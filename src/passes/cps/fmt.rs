// cps::Expr → Node → Fink source pretty-printer.
//
// First-pass CPS: renders the structural IR directly — no ·load/·store/·scope
// synthesis. All names are in scope by construction (no forward refs in first
// pass). Scope resolution and closure conversion are deferred to later passes.
//
// Uses the CpsId→AstId origin map to recover source names from the AST,
// avoiding stringly-typed dispatch.

use crate::ast::{self, AstId, Node, NodeKind};
use crate::lexer::{Loc, Pos};
use crate::propgraph::PropGraph;
use super::ir::{Arg, Bind, BindNode, BuiltIn, Callable, CpsId, Expr, ExprKind, Ref, Lit, Param, Val, ValKind};

// ---------------------------------------------------------------------------
// Formatter context — carries the prop graphs needed for origin lookups
// ---------------------------------------------------------------------------

/// Holds the origin map and AST index so the formatter can look up syntactic
/// category (operator/ident/prim) from CpsId without inspecting strings.
pub struct Ctx<'a, 'src> {
  pub origin: &'a PropGraph<CpsId, Option<AstId>>,
  pub ast_index: &'a PropGraph<AstId, Option<&'src Node<'src>>>,
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

  /// Recover the source name for a CPS node from its AST origin.
  /// Returns the name string and whether it's an operator.
  /// Returns None for compiler-generated nodes with no AST origin.
  fn source_name(&self, cps_id: CpsId) -> Option<(&'src str, bool)> {
    let node = self.ast_node(cps_id)?;
    match &node.kind {
      NodeKind::Ident(s) => Some((s, false)),
      NodeKind::InfixOp { op, .. } => Some((op, true)),
      NodeKind::UnaryOp { op, .. } => Some((op, true)),
      NodeKind::ChainedCmp(_) => None,  // chained cmp has multiple ops, handled by transform
      _ => None,
    }
  }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

pub fn fmt_with(expr: &Expr<'_>, ctx: &Ctx<'_, '_>) -> String {
  ast::fmt::fmt(&to_node(expr, ctx))
}

/// Format without origin map — falls back to string-based category detection.
/// Used by tests that don't yet thread the prop graphs.
pub fn fmt(expr: &Expr<'_>) -> String {
  ast::fmt::fmt(&to_node_no_ctx(expr))
}

/// Render a cursor index as a formatter name (`·m_N`).
/// Shared for both seq and rec cursors — the IR uses a plain `u32`.
fn cursor_name(idx: u32) -> String {
  format!("·m_{}", idx)
}

// ---------------------------------------------------------------------------
// Dummy loc — all reconstructed AST nodes use this
// ---------------------------------------------------------------------------

fn loc() -> Loc {
  let p = Pos { idx: 0, line: 1, col: 0 };
  Loc { start: p, end: p }
}

fn node(kind: NodeKind<'static>) -> Node<'static> {
  Node::new(kind, loc())
}

// ---------------------------------------------------------------------------
// AST builder helpers
// ---------------------------------------------------------------------------

fn ident(s: &str) -> Node<'static> {
  let s: &'static str = Box::leak(s.to_string().into_boxed_str());
  node(NodeKind::Ident(s))
}

fn spread_node(inner: Node<'static>) -> Node<'static> {
  node(NodeKind::Spread(Some(Box::new(inner))))
}

fn apply(func: Node<'static>, args: Vec<Node<'static>>) -> Node<'static> {
  node(NodeKind::Apply { func: Box::new(func), args })
}

fn patterns(params: Vec<Node<'static>>) -> Node<'static> {
  node(NodeKind::Patterns(params))
}

fn fn_node(params: Node<'static>, body: Vec<Node<'static>>) -> Node<'static> {
  node(NodeKind::Fn { params: Box::new(params), body })
}

/// `fn ·state: body` — state-only continuation (used in ·if branches).
fn state_fn(body: Node<'static>) -> Node<'static> {
  fn_node(patterns(vec![ident("·state")]), vec![body])
}

/// `fn name, ·state: body` — result continuation (used in ·apply and ·match_block).
fn result_cont(name: &str, body: Node<'static>) -> Node<'static> {
  fn_node(patterns(vec![ident(name), ident("·state")]), vec![body])
}

/// `·yield value, ·state, fn result, ·state: body` — yield suspension point.
fn fmt_yield(value: &Val<'_>, result: &BindNode, body: &Expr<'_>, ctx: &Ctx<'_, '_>) -> Node<'static> {
  let cont = result_cont(&render_bind_ctx(result, ctx), to_node(body, ctx));
  apply(ident("·yield"), vec![val_to_node(value, ctx), ident("·state"), cont])
}

// ---------------------------------------------------------------------------
// Val → Node
// ---------------------------------------------------------------------------

/// Render a Val to an AST node for use in an already-resolved position.
/// Uses origin map to recover names from the AST.
fn val_to_node(v: &Val<'_>, ctx: &Ctx<'_, '_>) -> Node<'static> {
  match &v.kind {
    ValKind::Lit(lit) => lit_to_node(lit),
    ValKind::Ref(Ref::Name) => ident(&render_ref_name_ctx(v.id, ctx)),
    ValKind::Ref(Ref::Gen(bind_id)) => ident(&format!("·v_{}", bind_id.0)),
  }
}

/// Render a Ref val's name using origin map.
/// For Gen temps: renders as `·v_{bind_cps_id}`. For Name: recovers source name from AST.
fn render_val_name(v: &Val<'_>, ctx: &Ctx<'_, '_>) -> String {
  match &v.kind {
    ValKind::Ref(Ref::Gen(bind_id)) => format!("·v_{}", bind_id.0),
    ValKind::Ref(Ref::Name) => {
      ctx.source_name(v.id)
        .expect("Ref::Name should always have an origin")
        .0.to_string()
    }
    ValKind::Lit(_) => String::new(),
  }
}

fn lit_to_node(lit: &Lit<'_>) -> Node<'static> {
  match lit {
    Lit::Bool(b) => node(NodeKind::LitBool(*b)),
    Lit::Int(n) => {
      let s: &'static str = Box::leak(n.to_string().into_boxed_str());
      node(NodeKind::LitInt(s))
    }
    Lit::Float(f) => {
      let s: &'static str = Box::leak(f.to_string().into_boxed_str());
      node(NodeKind::LitFloat(s))
    }
    Lit::Decimal(f) => {
      let s: &'static str = Box::leak(format!("{}d", f).into_boxed_str());
      node(NodeKind::LitDecimal(s))
    }
    Lit::Str(s) => node(NodeKind::LitStr(s.to_string())),
    Lit::Seq   => node(NodeKind::LitSeq(vec![])),
    Lit::Rec   => node(NodeKind::LitRec(vec![])),
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
//   user names          → plain:  foo, bar
//   compiler temps      → ·v_{cps_id}
//   builtins            → ·op_plus, ·seq_append, …
// ---------------------------------------------------------------------------

/// Render a Bind node's name using origin map.
/// For User bindings: recovers the source name from the AST.
/// For Gen temps: always renders as `·v_N`.
fn render_bind_ctx(bind: &BindNode, ctx: &Ctx<'_, '_>) -> String {
  match bind.kind {
    Bind::Gen => format!("·v_{}", bind.id.0),
    Bind::User => ctx.source_name(bind.id)
      .expect("render_bind_ctx: User bind must have origin")
      .0.to_string(),
  }
}

/// Render a Ref::Name for display.
/// Recovers the name from the AST via origin map.
fn render_ref_name_ctx(cps_id: CpsId, ctx: &Ctx<'_, '_>) -> String {
  match ctx.source_name(cps_id) {
    Some((s, _)) => s.to_string(),
    None => unreachable!("render_ref_name_ctx: Ref::Name must have origin"),
  }
}

/// Render a `BuiltIn` variant to a display name for the formatter.
/// Operators render as `·op_name`, prims as `·prim_name`.
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
  }
}

pub fn to_node(expr: &Expr<'_>, ctx: &Ctx<'_, '_>) -> Node<'static> {
  match &expr.kind {
    ExprKind::Yield { value, result, body } => fmt_yield(value, result, body, ctx),

    ExprKind::Ret(val) => {
      apply(ident("·ƒ_cont"), vec![val_to_node(val, ctx), ident("·state")])
    }

    ExprKind::Panic => ident("·panic"),
    ExprKind::FailCont => ident("·ƒ_fail"),

    ExprKind::MatchBlock { params, arm_params, fail, arms, result, body } => {
      let result_plain = render_bind_ctx(result, ctx);
      let result_fn = fn_node(
        patterns(vec![ident(&result_plain), ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      let arm_nodes: Vec<Node<'static>> = arms.iter().map(|arm| {
        let mut fn_params: Vec<Node<'static>> = arm_params.iter()
          .map(|p| ident(&render_bind_ctx(p, ctx)))
          .collect();
        fn_params.extend([ident("·state"), ident("·ƒ_cont"), ident("·ƒ_fail")]);
        fn_node(patterns(fn_params), vec![to_node(arm, ctx)])
      }).collect();
      let mut args: Vec<Node<'static>> = params.iter().map(|v| val_to_node(v, ctx)).collect();
      args.push(fail_node);
      args.push(ident("·state"));
      args.extend(arm_nodes.iter().map(|n| {
        apply(ident("·match_branch"), vec![n.clone()])
      }));
      args.push(result_fn);
      apply(ident("·match_block"), args)
    }

    ExprKind::LetVal { name, val, body } => {
      let plain = render_bind_ctx(name, ctx);
      apply(ident("·let"), vec![
        val_to_node(val, ctx),
        fn_node(patterns(vec![ident(&plain)]), vec![to_node(body, ctx)]),
      ])
    }

    ExprKind::LetFn { name, params, fn_body, body, .. } => {
      let plain_name = render_bind_ctx(name, ctx);
      let fn_params: Vec<Node<'static>> = params.iter()
        .map(|p| match p {
          Param::Name(n) => ident(&render_bind_ctx(n, ctx)),
          Param::Spread(n) => spread_node(ident(&render_bind_ctx(n, ctx))),
        })
        .collect();
      apply(ident("·fn"), vec![
        fn_node(patterns(fn_params), vec![to_node(fn_body, ctx)]),
        fn_node(
          patterns(vec![ident(&plain_name)]),
          vec![to_node(body, ctx)],
        ),
      ])
    }

    ExprKind::App { func, args, result, body } => {
      let result_plain = render_bind_ctx(result, ctx);
      let result_fn = result_cont(&result_plain, to_node(body, ctx));
      let arg_nodes: Vec<Node<'static>> = args.iter().map(|a| match a {
        Arg::Val(v) => val_to_node(v, ctx),
        Arg::Spread(v) => spread_node(val_to_node(v, ctx)),
      }).collect();
      let func_node = match func {
        Callable::Val(func_val) => val_to_node(func_val, ctx),
        Callable::BuiltIn(op) => ident(&render_builtin(op)),
      };
      let mut apply_args: Vec<Node<'static>> = vec![func_node];
      apply_args.extend(arg_nodes);
      apply_args.push(ident("·state"));
      apply_args.push(result_fn);
      apply(ident("·apply"), apply_args)
    }

    ExprKind::If { cond, then, else_ } => {
      apply(ident("·if"), vec![
        val_to_node(cond, ctx),
        state_fn(to_node(then, ctx)),
        state_fn(to_node(else_, ctx)),
      ])
    }

    ExprKind::LetRec { .. } => unreachable!("LetRec should not reach the formatter before SCC analysis"),

    ExprKind::MatchLetVal { name, val, body, .. } => {
      let plain = render_bind_ctx(name, ctx);
      apply(ident("·let"), vec![
        val_to_node(val, ctx),
        fn_node(patterns(vec![ident(&plain)]), vec![to_node(body, ctx)]),
      ])
    }

    ExprKind::MatchApp { func, args, fail, result, body } => {
      let result_str = render_bind_ctx(result, ctx);
      let cont = fn_node(
        patterns(vec![ident(&result_str), ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      let func_node = match func {
        Callable::Val(func_val) => val_to_node(func_val, ctx),
        Callable::BuiltIn(op) => ident(&render_builtin(op)),
      };
      let mut apply_args = vec![func_node];
      apply_args.extend(args.iter().map(|v| val_to_node(v, ctx)));
      apply_args.push(fail_node);
      apply_args.push(cont);
      apply(ident("·match_apply"), apply_args)
    }

    ExprKind::MatchIf { func, args, fail, body } => {
      let cont = fn_node(
        patterns(vec![ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      let func_node = match func {
        Callable::Val(func_val) => val_to_node(func_val, ctx),
        Callable::BuiltIn(op) => ident(&render_builtin(op)),
      };
      let mut apply_args = vec![func_node];
      apply_args.extend(args.iter().map(|v| val_to_node(v, ctx)));
      apply_args.push(fail_node);
      apply_args.push(cont);
      apply(ident("·match_if"), apply_args)
    }

    ExprKind::MatchValue { val, lit, fail, body } => {
      let cont = fn_node(
        patterns(vec![ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_value"), vec![val_to_node(val, ctx), lit_to_node(lit), fail_node, cont])
    }

    ExprKind::MatchSeq { val, cursor, fail, body } => {
      let cursor_name = cursor_name(*cursor);
      let cont = fn_node(
        patterns(vec![ident(&cursor_name), ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_seq"), vec![val_to_node(val, ctx), fail_node, cont])
    }

    ExprKind::MatchNext { cursor, next_cursor, fail, elem, body, .. } => {
      let cur = cursor_name(*cursor);
      let next = cursor_name(*next_cursor);
      let elem_str = render_bind_ctx(elem, ctx);
      let cont = fn_node(
        patterns(vec![ident(&elem_str), ident(&next), ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_next"), vec![ident(&cur), fail_node, cont])
    }

    ExprKind::MatchDone { cursor, fail, result, body, .. } => {
      let cur = cursor_name(*cursor);
      let result_str = render_bind_ctx(result, ctx);
      let cont = fn_node(
        patterns(vec![ident(&result_str), ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_done"), vec![ident(&cur), fail_node, cont])
    }

    ExprKind::MatchNotDone { cursor, fail, body, .. } => {
      let cur = cursor_name(*cursor);
      let cont = fn_node(
        patterns(vec![ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_not_done"), vec![ident(&cur), fail_node, cont])
    }

    ExprKind::MatchRest { cursor, fail, result, body, .. } => {
      let cur = cursor_name(*cursor);
      let result_str = render_bind_ctx(result, ctx);
      let cont = fn_node(
        patterns(vec![ident(&result_str), ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_rest"), vec![ident(&cur), fail_node, cont])
    }

    ExprKind::MatchRec { val, cursor, fail, body } => {
      let rec_name = cursor_name(*cursor);
      let cont = fn_node(
        patterns(vec![ident(&rec_name), ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_rec"), vec![val_to_node(val, ctx), fail_node, cont])
    }

    ExprKind::MatchField { cursor, next_cursor, field, fail, elem, body, .. } => {
      let cur = cursor_name(*cursor);
      let next = cursor_name(*next_cursor);
      let elem_str = render_bind_ctx(elem, ctx);
      let cont = fn_node(
        patterns(vec![ident(&elem_str), ident(&next), ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      let field_lit = node(NodeKind::LitStr(field.to_string()));
      apply(ident("·match_field"), vec![ident(&cur), field_lit, fail_node, cont])
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
  let ctx = Ctx { origin: &origin, ast_index: &ast_index };
  to_node(expr, &ctx)
}

