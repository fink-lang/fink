// cps::Expr → Node → Fink source pretty-printer.
//
// First-pass CPS: renders the structural IR directly — no ·load/·store/·scope
// synthesis. All names are in scope by construction (no forward refs in first
// pass). Scope resolution (name_res) is complete. Closure hoisting is next.
//
// Uses the CpsId→AstId origin map to recover source names from the AST,
// avoiding stringly-typed dispatch.

use crate::ast::{self, AstId, Exprs, Node, NodeKind};
use crate::lexer::{Loc, Pos, Token, TokenKind};
use crate::propgraph::PropGraph;
use super::ir::{Arg, Bind, BindNode, BuiltIn, Callable, Cont, ContKind, CpsId, Expr, ExprKind, Ref, Lit, Param, Val, ValKind};

// ---------------------------------------------------------------------------
// Formatter context — carries the prop graphs needed for origin lookups
// ---------------------------------------------------------------------------

/// Holds the origin map and AST index so the formatter can look up syntactic
/// category (operator/ident/prim) from CpsId without inspecting strings.
pub struct Ctx<'a, 'src> {
  pub origin: &'a PropGraph<CpsId, Option<AstId>>,
  pub ast_index: &'a PropGraph<AstId, Option<&'src Node<'src>>>,
  /// Optional capture graph — when present, LetFn nodes that are closures
  /// render with a leading `{cap: [x, y]}` param.
  pub captures: Option<&'a PropGraph<CpsId, Vec<(CpsId, crate::passes::cps::ir::Bind)>>>,
  /// Optional param role metadata — when present, params are rendered grouped:
  /// `{caps}, [user_params], cont` instead of a flat list.
  pub param_info: Option<&'a PropGraph<CpsId, Option<super::ir::ParamInfo>>>,
  /// Optional bind kind map — when present, refs to compiler-generated conts
  /// render with semantic names (·ret_N, ·succ_N, ·fail_N).
  pub bind_kinds: Option<&'a PropGraph<CpsId, Option<super::ir::Bind>>>,
}

impl<'a, 'src> Ctx<'a, 'src> {
  /// Look up the AST node that a CPS node was synthesized from.
  /// Returns None for compiler-generated nodes (prims, temps) or when the
  /// origin map is empty / doesn't cover this ID.
  fn ast_node(&self, cps_id: CpsId) -> Option<&'src Node<'src>> {
    self.origin.try_get(cps_id)
      .and_then(|opt| *opt)
      .and_then(|ast_id| self.ast_index.try_get(ast_id))
      .and_then(|opt| *opt)
  }

}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

pub fn fmt_with(expr: &Expr, ctx: &Ctx<'_, '_>) -> String {
  ast::fmt::fmt_block(&to_node(expr, ctx))
}

pub fn fmt_with_mapped(expr: &Expr, ctx: &Ctx<'_, '_>, source_name: &str) -> (String, crate::sourcemap::SourceMap) {
  // Note: fmt_block flag not applied to mapped variants (used for codegen source maps, not debug output)
  ast::fmt::fmt_mapped(&to_node(expr, ctx), source_name)
}

pub fn fmt_with_mapped_content(expr: &Expr, ctx: &Ctx<'_, '_>, source_name: &str, content: &str) -> (String, crate::sourcemap::SourceMap) {
  ast::fmt::fmt_mapped_with_content(&to_node(expr, ctx), source_name, content)
}

/// Format without origin map — falls back to string-based category detection.
/// Used by tests that don't yet thread the prop graphs.
pub fn fmt(expr: &Expr) -> String {
  ast::fmt::fmt_block(&to_node_no_ctx(expr))
}

// ---------------------------------------------------------------------------
// Loc helpers
// ---------------------------------------------------------------------------

/// Sentinel loc for purely synthetic nodes with no source origin.
/// Line 0 signals MappedWriter::mark to skip the mapping.
fn dummy_loc() -> Loc {
  let p = Pos { idx: 0, line: 0, col: 0 };
  Loc { start: p, end: p }
}

fn node(kind: NodeKind<'static>, loc: Loc) -> Node<'static> {
  Node::new(kind, loc)
}

fn dummy_tok() -> Token<'static> {
  Token { kind: TokenKind::Sep, loc: dummy_loc(), src: "" }
}

// ---------------------------------------------------------------------------
// AST builder helpers
// ---------------------------------------------------------------------------

fn ident(s: &str, loc: Loc) -> Node<'static> {
  let s: &'static str = Box::leak(s.to_string().into_boxed_str());
  node(NodeKind::Ident(s), loc)
}

fn spread_node(inner: Node<'static>, loc: Loc) -> Node<'static> {
  node(NodeKind::Spread { op: dummy_tok(), inner: Some(Box::new(inner)) }, loc)
}

fn exprs(items: Vec<Node<'static>>) -> Exprs<'static> {
  Exprs { items, seps: vec![] }
}

fn apply(func: Node<'static>, args: Vec<Node<'static>>, loc: Loc) -> Node<'static> {
  node(NodeKind::Apply { func: Box::new(func), args: exprs(args) }, loc)
}

fn patterns(params: Vec<Node<'static>>) -> Node<'static> {
  let loc = params.first().map(|p| p.loc).unwrap_or_else(dummy_loc);
  node(NodeKind::Patterns(exprs(params)), loc)
}

fn fn_node(params: Node<'static>, body: Vec<Node<'static>>, loc: Loc) -> Node<'static> {
  node(NodeKind::Fn { params: Box::new(params), sep: dummy_tok(), body: exprs(body) }, loc)
}

/// `fn: body` — state-only continuation (used in ·if branches).
fn state_fn(body: Node<'static>) -> Node<'static> {
  let loc = body.loc;
  fn_node(patterns(vec![]), vec![body], loc)
}


// ---------------------------------------------------------------------------
// Val → Node
// ---------------------------------------------------------------------------

/// Recover the source loc for a CPS node, falling back to dummy_loc().
fn ctx_loc(cps_id: CpsId, ctx: &Ctx<'_, '_>) -> Loc {
  ctx.ast_node(cps_id).map(|n| n.loc).unwrap_or_else(dummy_loc)
}

/// For a pipe-desugared App, return the `|` separator token loc for this call stage.
/// The App's origin points to the Pipe node; the func val's origin identifies which stage.
fn pipe_sep_loc(expr_id: CpsId, func_val: &Val, ctx: &Ctx<'_, '_>) -> Option<Loc> {
  let node = ctx.ast_node(expr_id)?;
  let NodeKind::Pipe(exprs) = &node.kind else { return None };
  let func_ast_id = ctx.origin.try_get(func_val.id).and_then(|o| *o)?;
  let stage_idx = exprs.items.iter().position(|item| item.id == func_ast_id)?;
  if stage_idx == 0 { return None; }
  exprs.seps.get(stage_idx - 1).map(|sep| sep.loc)
}

/// Render a Val to an AST node for use in an already-resolved position.
/// Uses origin map to recover names and source locs from the AST.
fn val_to_node(v: &Val, ctx: &Ctx<'_, '_>) -> Node<'static> {
  let loc = ctx_loc(v.id, ctx);
  match &v.kind {
    ValKind::Lit(lit) => lit_to_node(lit, loc),
    ValKind::Ref(Ref::Synth(bind_id)) => ident(&render_synth_name(*bind_id, ctx), loc),
    ValKind::Ref(Ref::Unresolved(_)) => ident(&render_unresolved_name(v.id, ctx), loc),
    ValKind::Panic => ident("·panic", loc),
    ValKind::ContRef(id) => ident(&render_synth_fallback(*id, ctx), ctx_loc(*id, ctx)),
    ValKind::BuiltIn(op) => {
      // For builtin ops whose origin is an InfixOp, use op.loc (e.g. `>` not `a > 1`).
      let op_loc = ctx.ast_node(v.id)
        .and_then(|n| match &n.kind {
          NodeKind::InfixOp { op, .. } | NodeKind::UnaryOp { op, .. } => Some(op.loc),
          _ => None,
        })
        .unwrap_or(loc);
      ident(&render_builtin(op), op_loc)
    }
  }
}

fn lit_to_node(lit: &Lit, loc: Loc) -> Node<'static> {
  match lit {
    Lit::Bool(b) => node(NodeKind::LitBool(*b), loc),
    Lit::Int(n) => {
      let s: &'static str = Box::leak(n.to_string().into_boxed_str());
      node(NodeKind::LitInt(s), loc)
    }
    Lit::Float(f) => {
      let s: &'static str = Box::leak(f.to_string().into_boxed_str());
      node(NodeKind::LitFloat(s), loc)
    }
    Lit::Decimal(f) => {
      let s: &'static str = Box::leak(format!("{}d", f).into_boxed_str());
      node(NodeKind::LitDecimal(s), loc)
    }
    Lit::Str(s) => node(NodeKind::LitStr { open: dummy_tok(), close: dummy_tok(), content: crate::strings::control_pics(s), indent: 0 }, loc),
    Lit::Seq   => node(NodeKind::LitSeq { open: dummy_tok(), close: dummy_tok(), items: Exprs::empty() }, loc),
    Lit::Rec   => node(NodeKind::LitRec { open: dummy_tok(), close: dummy_tok(), items: Exprs::empty() }, loc),
  }
}

// ---------------------------------------------------------------------------
// Expr → Node
//
// Rendering conventions:
//   LetVal { name, val, body }  → ·let val, fn name: body
//   LetFn  { name, params, ..}  → ·fn fn params: fn_body, fn name: body
//   App    { func, args, result, body } → ·apply func, args, ·state, fn result, ·state: body
//   Ret(val)                    → ·v_cont val, ·state
//
// Output name conventions:
//   source ident "foo"   → ·foo_<cps_id>
//   synth ident n        → ·$_<n>_<cps_id>
//   compiler temp        → ·v_<cps_id>   (no AST origin)
//   cont param           → ·v_<cps_id>
//   builtins             → ·op_plus, ·seq_prepend, …
// ---------------------------------------------------------------------------

/// Render a CpsId's name: look up origin → AST node kind → pick rendering.
///   Ident("foo")   → ·foo_<id>
///   SynthIdent(n)  → ·$_<n>_<id>
///   no origin      → ·v_<id>
fn render_synth_name(cps_id: CpsId, ctx: &Ctx<'_, '_>) -> String {
  match ctx.ast_node(cps_id) {
    Some(node) => match &node.kind {
      NodeKind::Ident(s) => format!("·{}_{}", s, cps_id.0),
      _ => render_synth_fallback(cps_id, ctx),
    },
    None => render_synth_fallback(cps_id, ctx),
  }
}

/// Render a compiler-generated node with no AST origin.
/// Checks bind_kinds for cont semantic names, falls back to ·v_N.
fn render_synth_fallback(cps_id: CpsId, ctx: &Ctx<'_, '_>) -> String {
  if let Some(bk) = ctx.bind_kinds
    && let Some(Some(kind)) = bk.try_get(cps_id) {
      return match kind {
        Bind::Cont(ContKind::Ret)  => format!("·ƒret_{}", cps_id.0),
        Bind::Cont(ContKind::Succ) => format!("·ƒsucc_{}", cps_id.0),
        Bind::Cont(ContKind::Fail) => format!("·ƒfail_{}", cps_id.0),
        _ => format!("·v_{}", cps_id.0),
      };
  }
  format!("·v_{}", cps_id.0)
}

/// Render an unresolved ref as `·∅name` (source name) or `·∅_N` (no origin).
fn render_unresolved_name(cps_id: CpsId, ctx: &Ctx<'_, '_>) -> String {
  match ctx.ast_node(cps_id) {
    Some(node) => match &node.kind {
      NodeKind::Ident(s) => format!("·∅{}", s),
      // SynthIdent should always be resolved — if we get here, origin tracking is broken
      NodeKind::SynthIdent(n) => format!("·⚠$_{}", n),
      _ => format!("·⚠_{}", cps_id.0),
    },
    None => format!("·⚠_{}", cps_id.0),
  }
}

/// Render a Bind node's name using origin map.
fn render_bind_ctx(bind: &BindNode, ctx: &Ctx<'_, '_>) -> String {
  match bind.kind {
    Bind::SynthName => render_synth_name(bind.id, ctx),
    Bind::Synth     => format!("·v_{}", bind.id.0),
    Bind::Cont(ContKind::Ret)  => format!("·ƒret_{}", bind.id.0),
    Bind::Cont(ContKind::Succ) => format!("·ƒsucc_{}", bind.id.0),
    Bind::Cont(ContKind::Fail) => format!("·ƒfail_{}", bind.id.0),
  }
}

/// Render a `BuiltIn` variant to a display name for the formatter.
/// Operators render as `·op_name`, prims as `·prim_name`.
pub fn render_builtin_name(op: &BuiltIn) -> String {
  render_builtin(op)
}

fn render_builtin(op: &BuiltIn) -> String {
  match op {
    // Arithmetic
    BuiltIn::Add    => "·op_plus".into(),
    BuiltIn::Sub    => "·op_minus".into(),
    BuiltIn::Mul    => "·op_mul".into(),
    BuiltIn::Div    => "·op_div".into(),
    BuiltIn::IntDiv => "·op_intdiv".into(),
    BuiltIn::Mod    => "·op_rem".into(),
    BuiltIn::IntMod => "·op_intmod".into(),
    BuiltIn::DivMod => "·op_divmod".into(),
    BuiltIn::Pow    => "·op_pow".into(),
    // Comparison
    BuiltIn::Eq     => "·op_eq".into(),
    BuiltIn::Neq    => "·op_neq".into(),
    BuiltIn::Lt     => "·op_lt".into(),
    BuiltIn::Lte    => "·op_lte".into(),
    BuiltIn::Gt     => "·op_gt".into(),
    BuiltIn::Gte    => "·op_gte".into(),
    BuiltIn::Cmp    => "·op_cmp".into(),
    // Logical
    BuiltIn::And    => "·op_and".into(),
    BuiltIn::Or     => "·op_or".into(),
    BuiltIn::Xor    => "·op_xor".into(),
    BuiltIn::Not    => "·op_not".into(),
    // Bitwise
    BuiltIn::BitAnd => "·op_bitand".into(),
    BuiltIn::BitXor => "·op_bitxor".into(),
    BuiltIn::Shl    => "·op_shl".into(),
    BuiltIn::Shr    => "·op_shr".into(),
    BuiltIn::RotL   => "·op_rotl".into(),
    BuiltIn::RotR   => "·op_rotr".into(),
    BuiltIn::BitNot => "·op_bitnot".into(),
    // Range
    BuiltIn::Range     => "·op_rngex".into(),
    BuiltIn::RangeIncl => "·op_rngin".into(),
    BuiltIn::In        => "·op_in".into(),
    BuiltIn::NotIn     => "·op_notin".into(),
    // Member access
    BuiltIn::Get       => "·op_dot".into(),
    // Data construction
    BuiltIn::SeqPrepend => "·seq_prepend".into(),
    BuiltIn::SeqConcat  => "·seq_concat".into(),
    BuiltIn::RecPut    => "·rec_put".into(),
    BuiltIn::RecMerge  => "·rec_merge".into(),
    // String interpolation
    BuiltIn::StrFmt    => "·str_fmt".into(),
    // Closure construction
    BuiltIn::FnClosure => "·closure".into(),
    // Type guards
    BuiltIn::IsSeqLike    => "·is_seq_like".into(),
    BuiltIn::IsRecLike    => "·is_rec_like".into(),
    // Collection primitives
    BuiltIn::SeqPop       => "·seq_pop".into(),
    BuiltIn::RecPop       => "·rec_pop".into(),
    BuiltIn::Empty        => "·empty".into(),
    // Legacy match primitives
    // Async/concurrency
    BuiltIn::Yield        => "·yield".into(),
    // Module
    BuiltIn::Export       => "·export".into(),
    BuiltIn::Import       => "·import".into(),
  }
}

/// Render a `Cont` as a result-binding lambda for use in `·apply` / `·match_*` etc.
/// - `Cont::Expr(bind, body)` → `fn ·v_N: body`  (N from bind.id)
/// - `Cont::Ref(cont_id)` → `fn ·v_N: ·v_cont ·v_N`  (cosmetic lambda sugar for the tail call)
///
/// For `Cont::Ref`, the result param name is synthesised from the cont_id for a stable
/// display; the `·v_cont` name is fixed (all conts render as `·v_cont` in param position).
fn render_cont(cont: &Cont, ctx: &Ctx<'_, '_>) -> Node<'static> {
  match cont {
    Cont::Expr { args, body } => {
      // Cont params are synthetic bindings — use their CpsId loc if available.
      let params: Vec<Node<'static>> = args.iter()
        .map(|b| ident(&render_bind_ctx(b, ctx), ctx_loc(b.id, ctx)))
        .collect();
      let body_node = to_node(body, ctx);
      // Use the first param's loc for the fn wrapper — the cont lambda originates
      // from the same source position as its parameter.
      let fn_loc = args.first().map(|b| ctx_loc(b.id, ctx)).unwrap_or_else(dummy_loc);
      fn_node(patterns(params), vec![body_node], fn_loc)
    }
    Cont::Ref(cont_id) => {
      ident(&render_synth_fallback(*cont_id, ctx), ctx_loc(*cont_id, ctx))
    }
  }
}

/// Render a `body: Cont` field as the body expression of a `fn name:` lambda.
/// - `Cont::Expr { body, .. }` → render `body`.
/// - `Cont::Ref(cont_id)` → render as `·ƒret_{cont_id} {name}` — a tail call to the cont.
fn render_cont_body(cont: &Cont, bound_name: &str, bound_id: CpsId, ctx: &Ctx<'_, '_>) -> Node<'static> {
  match cont {
    Cont::Expr { body, .. } => to_node(body, ctx),
    Cont::Ref(cont_id) => {
      let cont_loc = ctx_loc(*cont_id, ctx);
      let name_loc = ctx_loc(bound_id, ctx);
      let cont_name = render_synth_fallback(*cont_id, ctx);
      apply(ident(&cont_name, cont_loc), vec![ident(bound_name, name_loc)], cont_loc)
    }
  }
}


pub fn to_node(expr: &Expr, ctx: &Ctx<'_, '_>) -> Node<'static> {
  // Best-effort loc for the expression itself — used for keyword/wrapper nodes.
  let expr_loc = ctx_loc(expr.id, ctx);

  match &expr.kind {
    ExprKind::LetVal { name, val, cont } => {
      let plain = render_bind_ctx(name, ctx);
      let name_loc = ctx_loc(name.id, ctx);
      let body_node = render_cont_body(cont, &plain, name.id, ctx);
      // Map ·let to the = or |= token inside the Bind AST node.
      let let_loc = ctx.ast_node(expr.id)
        .and_then(|n| match &n.kind {
          NodeKind::Bind { op, .. } | NodeKind::BindRight { op, .. } => Some(op.loc),
          _ => None,
        })
        .unwrap_or(expr_loc);
      apply(ident("·let", let_loc), vec![
        val_to_node(val, ctx),
        fn_node(patterns(vec![ident(&plain, name_loc)]), vec![body_node], name_loc),
      ], let_loc)
    }

    ExprKind::LetFn { name, params, fn_body, cont, .. } => {
      let plain_name = render_bind_ctx(name, ctx);
      let name_loc = ctx_loc(name.id, ctx);
      let mut fn_params: Vec<Node<'static>> = params.iter()
        .map(|p| match p {
          Param::Name(n) => ident(&render_bind_ctx(n, ctx), ctx_loc(n.id, ctx)),
          Param::Spread(n) => spread_node(ident(&render_bind_ctx(n, ctx), ctx_loc(n.id, ctx)), ctx_loc(n.id, ctx)),
        })
        .collect();

      // If a capture graph is provided and this LetFn has captures, prepend
      // a `{cap: [x, y]}` annotation ident to the param list.
      if let Some(cap_graph) = ctx.captures
        && let Some(caps) = cap_graph.try_get(name.id)
        && !caps.is_empty()
      {
        let names: Vec<String> = caps.iter()
          .map(|(bind_id, _)| render_synth_name(*bind_id, ctx))
          .collect();
        let label = format!("{{cap: [{}]}}", names.join(", "));
        fn_params.insert(0, ident(&label, name_loc));
      }

      let body_node = render_cont_body(cont, &plain_name, name.id, ctx);
      apply(ident("·fn", expr_loc), vec![
        fn_node(patterns(fn_params), vec![to_node(fn_body, ctx)], expr_loc),
        fn_node(
          patterns(vec![ident(&plain_name, name_loc)]),
          vec![body_node],
          name_loc,
        ),
      ], expr_loc)
    }

    ExprKind::App { func, args } => {
      // No-arg call to a cont — render as `·v_N _` (tail jump, no value).
      if args.is_empty() {
        let func_node = match func {
          Callable::Val(func_val) => val_to_node(func_val, ctx),
          Callable::BuiltIn(op) => ident(&render_builtin(op), expr_loc),
        };
        return apply(func_node, vec![ident("_", expr_loc)], expr_loc);
      }
      // For builtin operators, map to the op token (e.g. `-` in `n - 1`).
      let builtin_loc = ctx.ast_node(expr.id)
        .and_then(|n| match (&n.kind, func) {
          (NodeKind::InfixOp { op, .. }, _) | (NodeKind::UnaryOp { op, .. }, _) => Some(op.loc),
          _ => None,
        })
        .unwrap_or(expr_loc);
      // For pipe-desugared calls, use the `|` sep token loc for the apply wrapper.
      let call_loc = if let Callable::Val(func_val) = func {
        pipe_sep_loc(expr.id, func_val, ctx).unwrap_or(expr_loc)
      } else {
        builtin_loc
      };
      let func_node = match func {
        Callable::Val(func_val) => val_to_node(func_val, ctx),
        Callable::BuiltIn(op) => ident(&render_builtin(op), builtin_loc),
      };
      // Collection builtins render as `·name args, fail, cont` (no ·apply prefix).
      let is_collection_builtin = matches!(func, Callable::BuiltIn(
        BuiltIn::IsSeqLike | BuiltIn::IsRecLike |
        BuiltIn::SeqPop | BuiltIn::RecPop | BuiltIn::Empty
      ));
      if is_collection_builtin {
        let arg_nodes: Vec<Node<'static>> = args.iter().map(|a| match a {
          Arg::Val(v) => val_to_node(v, ctx),
          Arg::Spread(v) => { let n = val_to_node(v, ctx); spread_node(n, ctx_loc(v.id, ctx)) },
          Arg::Cont(c) => render_cont(c, ctx),
          Arg::Expr(e) => to_node(e, ctx),
        }).collect();
        apply(func_node, arg_nodes, call_loc)
      } else {
        // Regular App: all args render normally (last Arg::Cont is the result cont).
        let arg_nodes: Vec<Node<'static>> = args.iter().map(|a| match a {
          Arg::Val(v) => val_to_node(v, ctx),
          Arg::Spread(v) => { let n = val_to_node(v, ctx); spread_node(n, ctx_loc(v.id, ctx)) },
          Arg::Cont(c) => render_cont(c, ctx),
          Arg::Expr(e) => to_node(e, ctx),
        }).collect();
        apply(func_node, arg_nodes, call_loc)
      }
    }

    ExprKind::If { cond, then, else_ } => {
      apply(ident("·if", expr_loc), vec![
        val_to_node(cond, ctx),
        state_fn(to_node(then, ctx)),
        state_fn(to_node(else_, ctx)),
      ], expr_loc)
    }


  }
}

// ---------------------------------------------------------------------------
// No-context fallback — uses string-based category detection
// ---------------------------------------------------------------------------

fn to_node_no_ctx(expr: &Expr) -> Node<'static> {
  // Build empty prop graphs as a dummy context.
  let origin: PropGraph<CpsId, Option<AstId>> = PropGraph::new();
  let ast_index: PropGraph<AstId, Option<&Node<'_>>> = PropGraph::new();
  let ctx = Ctx { origin: &origin, ast_index: &ast_index, captures: None, param_info: None, bind_kinds: None };
  to_node(expr, &ctx)
}

