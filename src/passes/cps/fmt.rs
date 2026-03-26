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
use super::ir::{Arg, Bind, BindNode, BuiltIn, Callable, Cont, CpsId, Expr, ExprKind, Ref, Lit, Param, Val, ValKind};

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

pub fn fmt_with(expr: &Expr<'_>, ctx: &Ctx<'_, '_>) -> String {
  ast::fmt::fmt_block(&to_node(expr, ctx))
}

pub fn fmt_with_mapped(expr: &Expr<'_>, ctx: &Ctx<'_, '_>, source_name: &str) -> (String, crate::sourcemap::SourceMap) {
  // Note: fmt_block flag not applied to mapped variants (used for codegen source maps, not debug output)
  ast::fmt::fmt_mapped(&to_node(expr, ctx), source_name)
}

pub fn fmt_with_mapped_content(expr: &Expr<'_>, ctx: &Ctx<'_, '_>, source_name: &str, content: &str) -> (String, crate::sourcemap::SourceMap) {
  ast::fmt::fmt_mapped_with_content(&to_node(expr, ctx), source_name, content)
}

/// Format without origin map — falls back to string-based category detection.
/// Used by tests that don't yet thread the prop graphs.
pub fn fmt(expr: &Expr<'_>) -> String {
  ast::fmt::fmt_block(&to_node_no_ctx(expr))
}

// ---------------------------------------------------------------------------
// Loc helpers
// ---------------------------------------------------------------------------

/// Dummy loc for purely synthetic nodes with no source origin.
fn dummy_loc() -> Loc {
  let p = Pos { idx: 0, line: 1, col: 0 };
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
  node(NodeKind::Patterns(exprs(params)), dummy_loc())
}

fn fn_node(params: Node<'static>, body: Vec<Node<'static>>, loc: Loc) -> Node<'static> {
  node(NodeKind::Fn { params: Box::new(params), sep: dummy_tok(), body: exprs(body) }, loc)
}

/// `fn: body` — state-only continuation (used in ·if branches). Synthetic; no source loc.
fn state_fn(body: Node<'static>) -> Node<'static> {
  fn_node(patterns(vec![]), vec![body], dummy_loc())
}


// ---------------------------------------------------------------------------
// Val → Node
// ---------------------------------------------------------------------------

/// Recover the source loc for a CPS node, falling back to dummy_loc().
fn ctx_loc(cps_id: CpsId, ctx: &Ctx<'_, '_>) -> Loc {
  ctx.ast_node(cps_id).map(|n| n.loc).unwrap_or_else(dummy_loc)
}

/// Render a Val to an AST node for use in an already-resolved position.
/// Uses origin map to recover names and source locs from the AST.
fn val_to_node(v: &Val<'_>, ctx: &Ctx<'_, '_>) -> Node<'static> {
  let loc = ctx_loc(v.id, ctx);
  match &v.kind {
    ValKind::Lit(lit) => lit_to_node(lit, loc),
    ValKind::Ref(Ref::Synth(bind_id)) => ident(&render_synth_name(*bind_id, ctx), loc),
    ValKind::Ref(Ref::Unresolved(_)) => ident(&render_unresolved_name(v.id, ctx), loc),
    ValKind::Panic => ident("·panic", dummy_loc()),
    ValKind::ContRef(id) => ident(&format!("·ƒ_{}", id.0), dummy_loc()),
    ValKind::BuiltIn(op) => ident(&render_builtin(op), dummy_loc()),
  }
}

fn lit_to_node(lit: &Lit<'_>, loc: Loc) -> Node<'static> {
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
    Lit::Str(s) => node(NodeKind::LitStr { open: dummy_tok(), close: dummy_tok(), content: s.to_string(), indent: 0 }, loc),
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
//   Ret(val)                    → ·ƒ_cont val, ·state
//
// Output name conventions:
//   source ident "foo"   → ·foo_<cps_id>
//   synth ident n        → ·$_<n>_<cps_id>
//   compiler temp        → ·v_<cps_id>   (no AST origin)
//   cont param           → ·ƒ_<cps_id>
//   builtins             → ·op_plus, ·seq_append, …
// ---------------------------------------------------------------------------

/// Render a CpsId's name: look up origin → AST node kind → pick rendering.
///   Ident("foo")   → ·foo_<id>
///   SynthIdent(n)  → ·$_<n>_<id>
///   no origin      → ·v_<id>
fn render_synth_name(cps_id: CpsId, ctx: &Ctx<'_, '_>) -> String {
  match ctx.ast_node(cps_id) {
    Some(node) => match &node.kind {
      NodeKind::Ident(s) => format!("·{}_{}", s, cps_id.0),
      NodeKind::SynthIdent(n) => format!("·$_{}_{}", n, cps_id.0),
      _ => format!("·v_{}", cps_id.0),
    },
    None => format!("·v_{}", cps_id.0),
  }
}

/// Render an unresolved ref as `·∅name` (source name) or `·∅_N` (no origin).
fn render_unresolved_name(cps_id: CpsId, ctx: &Ctx<'_, '_>) -> String {
  match ctx.ast_node(cps_id) {
    Some(node) => match &node.kind {
      NodeKind::Ident(s) => format!("·∅{}", s),
      NodeKind::SynthIdent(n) => format!("·∅$_{}", n),
      _ => format!("·∅_{}", cps_id.0),
    },
    None => format!("·∅_{}", cps_id.0),
  }
}

/// Render a Bind node's name using origin map.
fn render_bind_ctx(bind: &BindNode, ctx: &Ctx<'_, '_>) -> String {
  match bind.kind {
    Bind::SynthName => render_synth_name(bind.id, ctx),
    Bind::Synth     => format!("·v_{}", bind.id.0),
    Bind::Cont      => format!("·ƒ_{}", bind.id.0),
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
    BuiltIn::SeqAppend => "·seq_append".into(),
    BuiltIn::SeqConcat => "·seq_concat".into(),
    BuiltIn::RecPut    => "·rec_put".into(),
    BuiltIn::RecMerge  => "·rec_merge".into(),
    // String interpolation
    BuiltIn::StrFmt    => "·str_fmt".into(),
    // Closure construction
    BuiltIn::FnClosure => "·closure".into(),
    // Pattern matching primitives
    BuiltIn::MatchValue   => "·match_value".into(),
    BuiltIn::MatchSeq     => "·match_seq".into(),
    BuiltIn::MatchNext    => "·match_next".into(),
    BuiltIn::MatchDone    => "·match_done".into(),
    BuiltIn::MatchNotDone => "·match_not_done".into(),
    BuiltIn::MatchRest    => "·match_rest".into(),
    BuiltIn::MatchRec     => "·match_rec".into(),
    BuiltIn::MatchField   => "·match_field".into(),
    BuiltIn::MatchIf      => "·match_if".into(),
    BuiltIn::MatchApp     => "·match_app".into(),
    BuiltIn::MatchBlock   => "·match_block".into(),
    BuiltIn::MatchArm     => "·match_arm".into(),
    // Async/concurrency
    BuiltIn::Yield        => "·yield".into(),
  }
}

/// Render a `Cont` as a result-binding lambda for use in `·apply` / `·match_*` etc.
/// - `Cont::Expr(bind, body)` → `fn ·v_N: body`  (N from bind.id)
/// - `Cont::Ref(cont_id)` → `fn ·v_N: ·ƒ_cont ·v_N`  (cosmetic lambda sugar for the tail call)
///
/// For `Cont::Ref`, the result param name is synthesised from the cont_id for a stable
/// display; the `·ƒ_cont` name is fixed (all conts render as `·ƒ_cont` in param position).
fn render_cont<'src>(cont: &Cont<'src>, ctx: &Ctx<'_, '_>) -> Node<'static> {
  match cont {
    Cont::Expr { args, body } => {
      // Cont params are synthetic bindings — use their CpsId loc if available.
      let params: Vec<Node<'static>> = args.iter()
        .map(|b| ident(&render_bind_ctx(b, ctx), ctx_loc(b.id, ctx)))
        .collect();
      let body_node = to_node(body, ctx);
      fn_node(patterns(params), vec![body_node], dummy_loc())
    }
    Cont::Ref(cont_id) => {
      ident(&format!("·ƒ_{}", cont_id.0), dummy_loc())
    }
  }
}

/// Render a `body: Cont` field as the body expression of a `fn name:` lambda.
/// - `Cont::Expr { arg, body }` → render `body` as a normal expression (arg is unused here,
///   it's the continuation result binding handled by the parent's lambda param).
/// - `Cont::Ref(cont_id)` → render as `·ƒ_{cont_id} {name}` — a tail call to the cont,
///   passing the locally-bound `name` as the argument.
fn render_cont_body(cont: &Cont<'_>, bound_name: &str, ctx: &Ctx<'_, '_>) -> Node<'static> {
  match cont {
    Cont::Expr { body, .. } => to_node(body, ctx),
    Cont::Ref(cont_id) => {
      let cont_name = format!("·ƒ_{}", cont_id.0);
      apply(ident(&cont_name, dummy_loc()), vec![ident(bound_name, dummy_loc())], dummy_loc())
    }
  }
}

/// Render a `body: Cont` field as a plain expression for use in a no-arg `fn:` lambda
/// (e.g. MatchIf, MatchValue, MatchSeq, MatchNotDone, MatchRec).
/// - `Cont::Expr { body, .. }` → render `body`.
/// - `Cont::Ref(_)` → `·panic` (these nodes always chain to more expressions; Ref is
///   structurally unexpected here but we fall back gracefully).
fn render_cont_as_expr(cont: &Cont<'_>, ctx: &Ctx<'_, '_>) -> Node<'static> {
  match cont {
    Cont::Expr { body, .. } => to_node(body, ctx),
    Cont::Ref(cont_id) => {
      // A bare cont ref in a no-arg body position means "call this cont".
      // Render as `·ƒ_N _` — an application with a wildcard arg, since
      // a bare `·ƒ_N` is just a reference in Fink syntax, not a call.
      // Use a single ident token (with space) so the pretty-printer keeps it inline.
      ident(&format!("·ƒ_{} _", cont_id.0), dummy_loc())
    }
  }
}

pub fn to_node(expr: &Expr<'_>, ctx: &Ctx<'_, '_>) -> Node<'static> {
  // Best-effort loc for the expression itself — used for keyword/wrapper nodes.
  let expr_loc = ctx_loc(expr.id, ctx);

  match &expr.kind {
    ExprKind::LetVal { name, val, cont } => {
      let plain = render_bind_ctx(name, ctx);
      let name_loc = ctx_loc(name.id, ctx);
      let body_node = render_cont_body(cont, &plain, ctx);
      apply(ident("·let", expr_loc), vec![
        val_to_node(val, ctx),
        fn_node(patterns(vec![ident(&plain, name_loc)]), vec![body_node], dummy_loc()),
      ], expr_loc)
    }

    ExprKind::LetFn { name, params, fn_body, cont } => {
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
        fn_params.insert(0, ident(&label, dummy_loc()));
      }

      let body_node = render_cont_body(cont, &plain_name, ctx);
      apply(ident("·fn", expr_loc), vec![
        fn_node(patterns(fn_params), vec![to_node(fn_body, ctx)], expr_loc),
        fn_node(
          patterns(vec![ident(&plain_name, name_loc)]),
          vec![body_node],
          dummy_loc(),
        ),
      ], expr_loc)
    }

    ExprKind::App { func, args } => {
      // No-arg call to a cont — render as `·ƒ_N _` (tail jump, no value).
      if args.is_empty() {
        let func_node = match func {
          Callable::Val(func_val) => val_to_node(func_val, ctx),
          Callable::BuiltIn(op) => ident(&render_builtin(op), expr_loc),
        };
        return apply(func_node, vec![ident("_", dummy_loc())], expr_loc);
      }
      let func_node = match func {
        Callable::Val(func_val) => val_to_node(func_val, ctx),
        Callable::BuiltIn(op) => ident(&render_builtin(op), expr_loc),
      };
      // Match builtins with no-arg body use render_cont_as_expr (renders as `fn: body`).
      let is_noarg_match = matches!(func, Callable::BuiltIn(
        BuiltIn::MatchValue | BuiltIn::MatchNotDone | BuiltIn::MatchIf
      ));
      // Match builtins render as `·match_* args, cont` (no ·apply prefix).
      let is_match_builtin = is_noarg_match || matches!(func, Callable::BuiltIn(
        BuiltIn::MatchSeq | BuiltIn::MatchNext |
        BuiltIn::MatchDone | BuiltIn::MatchRest |
        BuiltIn::MatchRec | BuiltIn::MatchField |
        BuiltIn::MatchApp | BuiltIn::MatchBlock | BuiltIn::MatchArm
      ));
      if is_match_builtin {
        // Match builtins: render all args inline (Arg::Cont renders as lambdas).
        // For no-arg match builtins, the last Arg::Cont uses render_cont_as_expr.
        let arg_nodes: Vec<Node<'static>> = args.iter().enumerate().map(|(i, a)| match a {
          Arg::Val(v) => val_to_node(v, ctx),
          Arg::Spread(v) => spread_node(val_to_node(v, ctx), dummy_loc()),
          Arg::Cont(c) if is_noarg_match && i == args.len() - 1 =>
            state_fn(render_cont_as_expr(c, ctx)),
          Arg::Cont(c) => render_cont(c, ctx),
          Arg::Expr(e) => to_node(e, ctx),
        }).collect();
        apply(func_node, arg_nodes, expr_loc)
      } else {
        // Regular App: all args render normally (last Arg::Cont is the result cont).
        let arg_nodes: Vec<Node<'static>> = args.iter().map(|a| match a {
          Arg::Val(v) => val_to_node(v, ctx),
          Arg::Spread(v) => spread_node(val_to_node(v, ctx), dummy_loc()),
          Arg::Cont(c) => render_cont(c, ctx),
          Arg::Expr(e) => to_node(e, ctx),
        }).collect();
        apply(func_node, arg_nodes, expr_loc)
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

fn to_node_no_ctx(expr: &Expr<'_>) -> Node<'static> {
  // Build empty prop graphs as a dummy context.
  let origin: PropGraph<CpsId, Option<AstId>> = PropGraph::new();
  let ast_index: PropGraph<AstId, Option<&Node<'_>>> = PropGraph::new();
  let ctx = Ctx { origin: &origin, ast_index: &ast_index, captures: None };
  to_node(expr, &ctx)
}

