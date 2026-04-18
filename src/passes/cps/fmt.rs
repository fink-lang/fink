// cps::Expr → Node → Fink source pretty-printer.
//
// First-pass CPS: renders the structural IR directly — no ·load/·store/·scope
// synthesis. All names are in scope by construction (no forward refs in first
// pass). Scope resolution (name_res) is complete. Closure hoisting is next.
//
// Uses the CpsId→AstId origin map to recover source names from the AST,
// avoiding stringly-typed dispatch.
//
// Under the flat-AST refactor: nodes are appended into a transient
// `AstBuilder<'static>` per render call, finished into an `Ast<'static>`,
// and handed to the flipped `ast::fmt` API. The synthetic ids live only
// for the duration of the render.

use crate::ast::{self, Ast, AstBuilder, AstId, Exprs, NodeKind};
use crate::lexer::{Loc, Pos, Token, TokenKind};
use crate::propgraph::PropGraph;
use super::ir::{Arg, Bind, BindNode, BuiltIn, Callable, Cont, ContKind, CpsId, Expr, ExprKind, Ref, Lit, Param, Val, ValKind};

// ---------------------------------------------------------------------------
// Formatter context — carries the prop graphs needed for origin lookups
// ---------------------------------------------------------------------------

/// Holds the origin map and AST so the formatter can look up syntactic
/// category (operator/ident/prim) from CpsId without inspecting strings.
pub struct Ctx<'a, 'src> {
  pub origin: &'a PropGraph<CpsId, Option<AstId>>,
  /// The flat AST that the CPS lowering was produced from. Used to look
  /// up source nodes via `origin[cps_id] → ast_id → ast.nodes.get(id)`.
  pub ast: &'a Ast<'src>,
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
  /// Look up the AST node id that a CPS node was synthesized from.
  /// Returns None for compiler-generated nodes (prims, temps) or when the
  /// origin map is empty / doesn't cover this ID.
  fn ast_node_id(&self, cps_id: CpsId) -> Option<AstId> {
    self.origin.try_get(cps_id).and_then(|opt| *opt)
  }

  /// Look up the AST node that a CPS node was synthesized from.
  fn ast_node(&self, cps_id: CpsId) -> Option<&ast::Node<'src>> {
    let id = self.ast_node_id(cps_id)?;
    Some(self.ast.nodes.get(id))
  }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

pub fn fmt_with(expr: &Expr, ctx: &Ctx<'_, '_>) -> String {
  let ast = build_ast(expr, ctx);
  ast::fmt::fmt_block(&ast)
}

/// Format with native-form source map emission.
pub fn fmt_with_mapped_native(expr: &Expr, ctx: &Ctx<'_, '_>) -> (String, crate::sourcemap::native::SourceMap) {
  let ast = build_ast(expr, ctx);
  ast::fmt::fmt_mapped_native(&ast)
}

/// Format without origin map — falls back to string-based category detection.
/// Used by tests that don't yet thread the prop graphs.
pub fn fmt(expr: &Expr) -> String {
  let empty_origin: PropGraph<CpsId, Option<AstId>> = PropGraph::new();
  let empty_ast = Ast::empty();
  let ctx = Ctx {
    origin: &empty_origin,
    ast: &empty_ast,
    captures: None,
    param_info: None,
    bind_kinds: None,
  };
  let ast = build_ast(expr, &ctx);
  ast::fmt::fmt_block(&ast)
}

/// Build a transient `Ast<'static>` from a CPS expression by walking and
/// appending into a fresh arena. The returned Ast is consumed by the
/// caller (passed to one of the `ast::fmt::*` entry points).
fn build_ast(expr: &Expr, ctx: &Ctx<'_, '_>) -> Ast<'static> {
  let mut b = AstBuilder::new();
  let root = build_expr(&mut b, expr, ctx);
  b.finish(root)
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

fn dummy_tok() -> Token<'static> {
  Token { kind: TokenKind::Sep, loc: dummy_loc(), src: "" }
}

// ---------------------------------------------------------------------------
// AST builder helpers — append into the transient arena and return AstIds
// ---------------------------------------------------------------------------

fn b_ident(b: &mut AstBuilder<'static>, s: &str, loc: Loc) -> AstId {
  let s: &'static str = Box::leak(s.to_string().into_boxed_str());
  b.append(NodeKind::Ident(s), loc)
}

fn b_spread(b: &mut AstBuilder<'static>, inner: AstId, loc: Loc) -> AstId {
  b.append(NodeKind::Spread { op: dummy_tok(), inner: Some(inner) }, loc)
}

fn b_exprs(items: Vec<AstId>) -> Exprs<'static> {
  Exprs { items: items.into_boxed_slice(), seps: vec![] }
}

fn b_apply(b: &mut AstBuilder<'static>, func: AstId, args: Vec<AstId>, loc: Loc) -> AstId {
  b.append(NodeKind::Apply { func, args: b_exprs(args) }, loc)
}

fn b_patterns(b: &mut AstBuilder<'static>, params: Vec<AstId>) -> AstId {
  // Use the first param's loc; fall back to dummy.
  let loc = params.first().map(|&id| b.read(id).loc).unwrap_or_else(dummy_loc);
  b.append(NodeKind::Patterns(b_exprs(params)), loc)
}

fn b_fn(b: &mut AstBuilder<'static>, params: AstId, body: Vec<AstId>, loc: Loc) -> AstId {
  b.append(NodeKind::Fn { params, sep: dummy_tok(), body: b_exprs(body) }, loc)
}

/// `fn: body` — state-only continuation (used in ·if branches).
fn b_state_fn(b: &mut AstBuilder<'static>, body: AstId) -> AstId {
  let loc = b.read(body).loc;
  let empty_patterns = b_patterns(b, vec![]);
  b_fn(b, empty_patterns, vec![body], loc)
}


// ---------------------------------------------------------------------------
// Val → Node id
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
  let func_ast_id = ctx.ast_node_id(func_val.id)?;
  // Find which stage this func corresponds to by matching the AstId.
  let stage_idx = exprs.items.iter().position(|&item_id| item_id == func_ast_id)?;
  if stage_idx == 0 { return None; }
  exprs.seps.get(stage_idx - 1).map(|sep| sep.loc)
}

/// Render a Val into the transient arena and return its AstId.
fn build_val(b: &mut AstBuilder<'static>, v: &Val, ctx: &Ctx<'_, '_>) -> AstId {
  let loc = ctx_loc(v.id, ctx);
  match &v.kind {
    ValKind::Lit(lit) => build_lit(b, lit, loc),
    ValKind::Ref(Ref::Synth(bind_id)) => b_ident(b, &render_synth_name(*bind_id, ctx), loc),
    ValKind::Ref(Ref::Unresolved(_)) => b_ident(b, &render_unresolved_name(v.id, ctx), loc),
    ValKind::ContRef(id) => b_ident(b, &render_synth_fallback(*id, ctx), loc),
    ValKind::BuiltIn(op) => {
      // For builtin ops whose origin is an InfixOp, use op.loc (e.g. `>` not `a > 1`).
      let op_loc = ctx.ast_node(v.id)
        .and_then(|n| match &n.kind {
          NodeKind::InfixOp { op, .. } | NodeKind::UnaryOp { op, .. } => Some(op.loc),
          _ => None,
        })
        .unwrap_or(loc);
      b_ident(b, &render_builtin(op), op_loc)
    }
  }
}

fn build_lit(b: &mut AstBuilder<'static>, lit: &Lit, loc: Loc) -> AstId {
  match lit {
    Lit::Bool(v) => b.append(NodeKind::LitBool(*v), loc),
    Lit::Int(n) => {
      let s: &'static str = Box::leak(n.to_string().into_boxed_str());
      b.append(NodeKind::LitInt(s), loc)
    }
    Lit::Float(f) => {
      let s: &'static str = Box::leak(f.to_string().into_boxed_str());
      b.append(NodeKind::LitFloat(s), loc)
    }
    Lit::Decimal(f) => {
      let s: &'static str = Box::leak(format!("{}d", f).into_boxed_str());
      b.append(NodeKind::LitDecimal(s), loc)
    }
    Lit::Str(s) => b.append(
      NodeKind::LitStr {
        open: dummy_tok(),
        close: dummy_tok(),
        content: crate::strings::control_pics_bytes(s),
        indent: 0,
      },
      loc,
    ),
    Lit::Seq => b.append(
      NodeKind::LitSeq { open: dummy_tok(), close: dummy_tok(), items: Exprs::empty() },
      loc,
    ),
    Lit::Rec => b.append(
      NodeKind::LitRec { open: dummy_tok(), close: dummy_tok(), items: Exprs::empty() },
      loc,
    ),
  }
}

// ---------------------------------------------------------------------------
// Expr → Node id
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
    // Shifts / rotations
    BuiltIn::Shl    => "·op_shl".into(),
    BuiltIn::Shr    => "·op_shr".into(),
    BuiltIn::RotL   => "·op_rotl".into(),
    BuiltIn::RotR   => "·op_rotr".into(),
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
    BuiltIn::StrMatch     => "·str_match".into(),
    // Scheduling
    BuiltIn::Yield        => "·yield".into(),
    BuiltIn::Spawn        => "·spawn".into(),
    BuiltIn::Await        => "·await".into(),
    // Channels
    BuiltIn::Channel      => "·channel".into(),
    BuiltIn::Receive      => "·receive".into(),
    // IO
    BuiltIn::Read         => "·read".into(),
    // Module
    BuiltIn::Export       => "·export".into(),
    BuiltIn::Import       => "·import".into(),
    BuiltIn::FinkModule   => "·ƒink_module".into(),
    BuiltIn::Pub          => "·ƒpub".into(),
    BuiltIn::Panic        => "·panic".into(),
  }
}

/// Render a `Cont` as a result-binding lambda for use in `·apply` / `·match_*` etc.
/// - `Cont::Expr(bind, body)` → `fn ·v_N: body`  (N from bind.id)
/// - `Cont::Ref(cont_id)` → `fn ·v_N: ·v_cont ·v_N`  (cosmetic lambda sugar for the tail call)
///
/// `site_loc` is the source location of the surrounding call — used as
/// the anchor for a `Cont::Ref` render (the cont itself is a synthetic
/// token; its semantic position is the tail of the call being built).
fn build_cont(b: &mut AstBuilder<'static>, cont: &Cont, ctx: &Ctx<'_, '_>, site_loc: Loc) -> AstId {
  match cont {
    Cont::Expr { args, body } => {
      // Cont params are synthetic bindings — use their CpsId loc if available.
      let params: Vec<AstId> = args.iter()
        .map(|bn| b_ident(b, &render_bind_ctx(bn, ctx), ctx_loc(bn.id, ctx)))
        .collect();
      let body_id = build_expr(b, body, ctx);
      // Use the first param's loc for the fn wrapper — the cont lambda originates
      // from the same source position as its parameter.
      let fn_loc = args.first().map(|bn| ctx_loc(bn.id, ctx)).unwrap_or_else(dummy_loc);
      let pats = b_patterns(b, params);
      b_fn(b, pats, vec![body_id], fn_loc)
    }
    Cont::Ref(cont_id) => {
      b_ident(b, &render_synth_fallback(*cont_id, ctx), site_loc)
    }
  }
}

/// Render a `body: Cont` field as the body expression of a `fn name:` lambda.
/// - `Cont::Expr { body, .. }` → render `body`.
/// - `Cont::Ref(cont_id)` → render as `·ƒret_{cont_id} {name}` — a tail call to the cont.
fn build_cont_body(b: &mut AstBuilder<'static>, cont: &Cont, bound_name: &str, bound_id: CpsId, ctx: &Ctx<'_, '_>) -> AstId {
  match cont {
    Cont::Expr { body, .. } => build_expr(b, body, ctx),
    Cont::Ref(cont_id) => {
      let cont_loc = ctx_loc(*cont_id, ctx);
      let name_loc = ctx_loc(bound_id, ctx);
      let cont_name = render_synth_fallback(*cont_id, ctx);
      let cont_id = b_ident(b, &cont_name, cont_loc);
      let arg_id = b_ident(b, bound_name, name_loc);
      b_apply(b, cont_id, vec![arg_id], cont_loc)
    }
  }
}


pub fn build_expr(b: &mut AstBuilder<'static>, expr: &Expr, ctx: &Ctx<'_, '_>) -> AstId {
  // Best-effort loc for the expression itself — used for keyword/wrapper nodes.
  let expr_loc = ctx_loc(expr.id, ctx);

  match &expr.kind {
    ExprKind::LetVal { name, val, cont } => {
      let plain = render_bind_ctx(name, ctx);
      let name_loc = ctx_loc(name.id, ctx);
      let body_id = build_cont_body(b, cont, &plain, name.id, ctx);
      // Map ·let to the = or |= token inside the Bind AST node.
      let let_loc = ctx.ast_node(expr.id)
        .and_then(|n| match &n.kind {
          NodeKind::Bind { op, .. } | NodeKind::BindRight { op, .. } => Some(op.loc),
          _ => None,
        })
        .unwrap_or(expr_loc);
      let val_id = build_val(b, val, ctx);
      let name_id = b_ident(b, &plain, name_loc);
      let pats = b_patterns(b, vec![name_id]);
      let inner_fn = b_fn(b, pats, vec![body_id], name_loc);
      let let_ident = b_ident(b, "·let", let_loc);
      b_apply(b, let_ident, vec![val_id, inner_fn], let_loc)
    }

    ExprKind::LetFn { name, params, fn_body, cont, .. } => {
      let plain_name = render_bind_ctx(name, ctx);
      let name_loc = ctx_loc(name.id, ctx);
      let mut fn_param_ids: Vec<AstId> = params.iter()
        .map(|p| match p {
          Param::Name(n) => b_ident(b, &render_bind_ctx(n, ctx), ctx_loc(n.id, ctx)),
          Param::Spread(n) => {
            let inner = b_ident(b, &render_bind_ctx(n, ctx), ctx_loc(n.id, ctx));
            b_spread(b, inner, ctx_loc(n.id, ctx))
          }
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
        let label_id = b_ident(b, &label, name_loc);
        fn_param_ids.insert(0, label_id);
      }

      let body_id = build_cont_body(b, cont, &plain_name, name.id, ctx);
      let inner_fn_body = build_expr(b, fn_body, ctx);
      let fn_pats = b_patterns(b, fn_param_ids);
      let inner_fn = b_fn(b, fn_pats, vec![inner_fn_body], expr_loc);
      let name_ident = b_ident(b, &plain_name, name_loc);
      let outer_pats = b_patterns(b, vec![name_ident]);
      let outer_fn = b_fn(b, outer_pats, vec![body_id], name_loc);
      let fn_keyword = b_ident(b, "·fn", expr_loc);
      b_apply(b, fn_keyword, vec![inner_fn, outer_fn], expr_loc)
    }

    ExprKind::App { func, args } => {
      // No-arg call to a cont — render as `·v_N _` (tail jump, no value).
      if args.is_empty() {
        let func_id = match func {
          Callable::Val(func_val) => build_val(b, func_val, ctx),
          Callable::BuiltIn(op) => b_ident(b, &render_builtin(op), expr_loc),
        };
        let placeholder = b_ident(b, "_", expr_loc);
        return b_apply(b, func_id, vec![placeholder], expr_loc);
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
      let func_id = match func {
        Callable::Val(func_val) => build_val(b, func_val, ctx),
        Callable::BuiltIn(op) => b_ident(b, &render_builtin(op), builtin_loc),
      };
      let arg_ids: Vec<AstId> = args.iter().map(|a| match a {
        Arg::Val(v) => build_val(b, v, ctx),
        Arg::Spread(v) => {
          let n = build_val(b, v, ctx);
          b_spread(b, n, ctx_loc(v.id, ctx))
        }
        Arg::Cont(c) => build_cont(b, c, ctx, expr_loc),
        Arg::Expr(e) => build_expr(b, e, ctx),
      }).collect();
      b_apply(b, func_id, arg_ids, call_loc)
    }

    ExprKind::If { cond, then, else_ } => {
      let cond_id = build_val(b, cond, ctx);
      let then_id = build_expr(b, then, ctx);
      let else_id = build_expr(b, else_, ctx);
      let then_fn = b_state_fn(b, then_id);
      let else_fn = b_state_fn(b, else_id);
      let if_keyword = b_ident(b, "·if", expr_loc);
      b_apply(b, if_keyword, vec![cond_id, then_fn, else_fn], expr_loc)
    }
  }
}
