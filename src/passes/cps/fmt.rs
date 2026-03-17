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
  pub captures: Option<&'a PropGraph<CpsId, Vec<&'src str>>>,
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
      NodeKind::InfixOp { op, .. } => Some((op.src, true)),
      NodeKind::UnaryOp { op, .. } => Some((op.src, true)),
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

fn dummy_tok() -> Token<'static> {
  Token { kind: TokenKind::Sep, loc: loc(), src: "" }
}

// ---------------------------------------------------------------------------
// AST builder helpers
// ---------------------------------------------------------------------------

fn ident(s: &str) -> Node<'static> {
  let s: &'static str = Box::leak(s.to_string().into_boxed_str());
  node(NodeKind::Ident(s))
}

fn spread_node(inner: Node<'static>) -> Node<'static> {
  node(NodeKind::Spread { op: dummy_tok(), inner: Some(Box::new(inner)) })
}

fn exprs(items: Vec<Node<'static>>) -> Exprs<'static> {
  Exprs { items, seps: vec![] }
}

fn apply(func: Node<'static>, args: Vec<Node<'static>>) -> Node<'static> {
  node(NodeKind::Apply { func: Box::new(func), args: exprs(args) })
}

fn patterns(params: Vec<Node<'static>>) -> Node<'static> {
  node(NodeKind::Patterns(exprs(params)))
}

fn fn_node(params: Node<'static>, body: Vec<Node<'static>>) -> Node<'static> {
  node(NodeKind::Fn { params: Box::new(params), sep: dummy_tok(), body: exprs(body) })
}

/// `fn: body` — state-only continuation (used in ·if branches).
fn state_fn(body: Node<'static>) -> Node<'static> {
  fn_node(patterns(vec![]), vec![body])
}

/// `fn name: body` — result continuation (used in ·apply and ·match_block).
fn result_cont(name: &str, body: Node<'static>) -> Node<'static> {
  fn_node(patterns(vec![ident(name)]), vec![body])
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
    ValKind::Ref(Ref::Synth(bind_id)) => ident(&format!("·v_{}", bind_id.0)),
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
    Lit::Str(s) => node(NodeKind::LitStr { open: dummy_tok(), close: dummy_tok(), content: s.to_string() }),
    Lit::Seq   => node(NodeKind::LitSeq { open: dummy_tok(), close: dummy_tok(), items: Exprs::empty() }),
    Lit::Rec   => node(NodeKind::LitRec { open: dummy_tok(), close: dummy_tok(), items: Exprs::empty() }),
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
/// For Name bindings: recovers the source name from the AST.
/// For Synth temps: always renders as `·v_N`.
fn render_bind_ctx(bind: &BindNode, ctx: &Ctx<'_, '_>) -> String {
  match bind.kind {
    Bind::Synth => format!("·v_{}", bind.id.0),
    Bind::Cont => format!("·ƒ_{}", bind.id.0),
    Bind::Name => ctx.source_name(bind.id)
      .expect("render_bind_ctx: Name bind must have origin")
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
    // Closure construction
    BuiltIn::FnClosure => "·fn_closure".into(),
  }
}

/// Unwrap a `Cont::Expr` for formatting. Panics on `Cont::Ref` — the formatter
/// Unwrap an inline continuation for rendering.
/// Panics on `Cont::Ref` — callers that need to handle both must match directly.
fn cont_expr<'a, 'src>(cont: &'a Cont<'src>) -> (&'a BindNode, &'a Expr<'src>) {
  match cont {
    Cont::Expr(bind, body) => (bind, body),
    Cont::Ref(_) => panic!("cont_expr: unexpected Cont::Ref — caller must handle Ref directly"),
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
    Cont::Expr(bind, body) => {
      let name = render_bind_ctx(bind, ctx);
      result_cont(&name, to_node(body, ctx))
    }
    Cont::Ref(cont_id) => {
      // Cosmetic: synthesise `fn ·v_N: ·ƒ_N ·v_N`.
      // `·v_N` is a display-only result param; `·ƒ_N` names the cont by its CpsId.
      let result_name = format!("·v_{}", cont_id.0);
      let cont_name = format!("·ƒ_{}", cont_id.0);
      let body = apply(ident(&cont_name), vec![ident(&result_name)]);
      result_cont(&result_name, body)
    }
  }
}

pub fn to_node(expr: &Expr<'_>, ctx: &Ctx<'_, '_>) -> Node<'static> {
  match &expr.kind {
    ExprKind::Yield { value, cont } => {
      let cont_node = render_cont(cont, ctx);
      apply(ident("·yield"), vec![val_to_node(value, ctx), cont_node])
    }

    ExprKind::Ret(val, cont_id) => {
      let cont_name = format!("·ƒ_{}", cont_id.0);
      apply(ident(&cont_name), vec![val_to_node(val, ctx)])
    }

    ExprKind::Panic => ident("·panic"),
    ExprKind::FailCont => ident("·ƒ_fail"),

    ExprKind::MatchBlock { params, arm_params, fail, arms, cont } => {
      let result_fn = render_cont(cont, ctx);
      let fail_node = to_node(fail, ctx);
      let arm_nodes: Vec<Node<'static>> = arms.iter().map(|arm| {
        let mut fn_params: Vec<Node<'static>> = arm_params.iter()
          .map(|p| ident(&render_bind_ctx(p, ctx)))
          .collect();
        fn_params.extend([ident("·ƒ_cont"), ident("·ƒ_fail")]);
        fn_node(patterns(fn_params), vec![to_node(arm, ctx)])
      }).collect();
      let mut args: Vec<Node<'static>> = params.iter().map(|v| val_to_node(v, ctx)).collect();
      args.push(fail_node);
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

    ExprKind::LetFn { name, params, cont, fn_body, body } => {
      let plain_name = render_bind_ctx(name, ctx);
      let mut fn_params: Vec<Node<'static>> = params.iter()
        .map(|p| match p {
          Param::Name(n) => ident(&render_bind_ctx(n, ctx)),
          Param::Spread(n) => spread_node(ident(&render_bind_ctx(n, ctx))),
        })
        .collect();

      // Append the explicit cont param (·ƒ_cont) — always last.
      fn_params.push(ident(&render_bind_ctx(cont, ctx)));

      // If a capture graph is provided and this LetFn has captures, prepend
      // a `{cap: [x, y]}` annotation ident to the param list.
      if let Some(cap_graph) = ctx.captures
        && let Some(caps) = cap_graph.try_get(name.id)
        && !caps.is_empty()
      {
        let inner = caps.join(", ");
        let label = format!("{{cap: [{}]}}", inner);
        fn_params.insert(0, ident(&label));
      }

      apply(ident("·fn"), vec![
        fn_node(patterns(fn_params), vec![to_node(fn_body, ctx)]),
        fn_node(
          patterns(vec![ident(&plain_name)]),
          vec![to_node(body, ctx)],
        ),
      ])
    }

    ExprKind::App { func, args, cont } => {
      let result_fn = render_cont(cont, ctx);
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

    ExprKind::MatchApp { func, args, fail, cont } => {
      let cont_node = render_cont(cont, ctx);
      let fail_node = to_node(fail, ctx);
      let func_node = match func {
        Callable::Val(func_val) => val_to_node(func_val, ctx),
        Callable::BuiltIn(op) => ident(&render_builtin(op)),
      };
      let mut apply_args = vec![func_node];
      apply_args.extend(args.iter().map(|v| val_to_node(v, ctx)));
      apply_args.push(fail_node);
      apply_args.push(cont_node);
      apply(ident("·match_apply"), apply_args)
    }

    ExprKind::MatchIf { func, args, fail, body } => {
      let cont = fn_node(
        patterns(vec![]),
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
        patterns(vec![]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_value"), vec![val_to_node(val, ctx), lit_to_node(lit), fail_node, cont])
    }

    ExprKind::MatchSeq { val, cursor, fail, body } => {
      let cursor_name = cursor_name(*cursor);
      let cont = fn_node(
        patterns(vec![ident(&cursor_name)]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_seq"), vec![val_to_node(val, ctx), fail_node, cont])
    }

    ExprKind::MatchNext { cursor, next_cursor, fail, cont, .. } => {
      let cur = cursor_name(*cursor);
      let next = cursor_name(*next_cursor);
      // MatchNext cont always carries the elem binding + next cursor — must be Expr.
      let (elem, body) = cont_expr(cont);
      let elem_str = render_bind_ctx(elem, ctx);
      let cont_node = fn_node(
        patterns(vec![ident(&elem_str), ident(&next)]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_next"), vec![ident(&cur), fail_node, cont_node])
    }

    ExprKind::MatchDone { cursor, fail, cont, .. } => {
      let cur = cursor_name(*cursor);
      let cont_node = render_cont(cont, ctx);
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_done"), vec![ident(&cur), fail_node, cont_node])
    }

    ExprKind::MatchNotDone { cursor, fail, body, .. } => {
      let cur = cursor_name(*cursor);
      let cont = fn_node(
        patterns(vec![]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_not_done"), vec![ident(&cur), fail_node, cont])
    }

    ExprKind::MatchRest { cursor, fail, cont, .. } => {
      let cur = cursor_name(*cursor);
      let cont_node = render_cont(cont, ctx);
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_rest"), vec![ident(&cur), fail_node, cont_node])
    }

    ExprKind::MatchRec { val, cursor, fail, body } => {
      let rec_name = cursor_name(*cursor);
      let cont = fn_node(
        patterns(vec![ident(&rec_name)]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_rec"), vec![val_to_node(val, ctx), fail_node, cont])
    }

    ExprKind::MatchField { cursor, next_cursor, field, fail, cont, .. } => {
      let cur = cursor_name(*cursor);
      let next = cursor_name(*next_cursor);
      // MatchField cont always carries the field binding + next cursor — must be Expr.
      let (elem, body) = cont_expr(cont);
      let elem_str = render_bind_ctx(elem, ctx);
      let cont_node = fn_node(
        patterns(vec![ident(&elem_str), ident(&next)]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      let field_lit = node(NodeKind::LitStr { open: dummy_tok(), close: dummy_tok(), content: field.to_string() });
      apply(ident("·match_field"), vec![ident(&cur), field_lit, fail_node, cont_node])
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

