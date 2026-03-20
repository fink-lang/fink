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
    ValKind::Panic => ident("·panic"),
    ValKind::ContRef(id) => ident(&format!("·ƒ_{}", id.0)),
    ValKind::BuiltIn(op) => ident(&render_builtin(op)),
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
      let params: Vec<Node<'static>> = args.iter()
        .map(|b| ident(&render_bind_ctx(b, ctx)))
        .collect();
      let body_node = to_node(body, ctx);
      fn_node(patterns(params), vec![body_node])
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
      apply(ident(&cont_name), vec![ident(bound_name)])
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
      ident(&format!("·ƒ_{} _", cont_id.0))
    }
  }
}

pub fn to_node(expr: &Expr<'_>, ctx: &Ctx<'_, '_>) -> Node<'static> {
  match &expr.kind {
    ExprKind::Yield { value, cont } => {
      let cont_node = render_cont(cont, ctx);
      apply(ident("·yield"), vec![val_to_node(value, ctx), cont_node])
    }

    ExprKind::LetVal { name, val, body } => {
      let plain = render_bind_ctx(name, ctx);
      let body_node = render_cont_body(body, &plain, ctx);
      apply(ident("·let"), vec![
        val_to_node(val, ctx),
        fn_node(patterns(vec![ident(&plain)]), vec![body_node]),
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

      let body_node = render_cont_body(body, &plain_name, ctx);
      apply(ident("·fn"), vec![
        fn_node(patterns(fn_params), vec![to_node(fn_body, ctx)]),
        fn_node(
          patterns(vec![ident(&plain_name)]),
          vec![body_node],
        ),
      ])
    }

    ExprKind::App { func, args } => {
      // No-arg call to a cont — render as `·ƒ_N _` (tail jump, no value).
      if args.is_empty() {
        let func_node = match func {
          Callable::Val(func_val) => val_to_node(func_val, ctx),
          Callable::BuiltIn(op) => ident(&render_builtin(op)),
        };
        return apply(func_node, vec![ident("_")]);
      }
      let func_node = match func {
        Callable::Val(func_val) => val_to_node(func_val, ctx),
        Callable::BuiltIn(op) => ident(&render_builtin(op)),
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
          Arg::Spread(v) => spread_node(val_to_node(v, ctx)),
          Arg::Cont(c) if is_noarg_match && i == args.len() - 1 =>
            state_fn(render_cont_as_expr(c, ctx)),
          Arg::Cont(c) => render_cont(c, ctx),
          Arg::Expr(e) => to_node(e, ctx),
        }).collect();
        apply(func_node, arg_nodes)
      } else {
        // Regular App: all args render normally (last Arg::Cont is the result cont).
        let arg_nodes: Vec<Node<'static>> = args.iter().map(|a| match a {
          Arg::Val(v) => val_to_node(v, ctx),
          Arg::Spread(v) => spread_node(val_to_node(v, ctx)),
          Arg::Cont(c) => render_cont(c, ctx),
          Arg::Expr(e) => to_node(e, ctx),
        }).collect();
        let mut apply_args: Vec<Node<'static>> = vec![func_node];
        apply_args.extend(arg_nodes);
        apply(ident("·apply"), apply_args)
      }
    }

    ExprKind::If { cond, then, else_ } => {
      apply(ident("·if"), vec![
        val_to_node(cond, ctx),
        state_fn(to_node(then, ctx)),
        state_fn(to_node(else_, ctx)),
      ])
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

