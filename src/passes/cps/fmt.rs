// cps::Expr → Node → Fink source pretty-printer.
//
// Synthesizes ·store/·load/·scope/·state/·ƒ_cont from the clean structural IR.
// The output is valid runnable Fink — the visualization doubles as a runtime spec.
//
// Uses the CpsId→AstId origin map to recover syntactic category (operator vs
// ident vs prim) from the AST, avoiding stringly-typed dispatch. When a CPS
// node has no AST origin (compiler-generated prims), falls back to the string.

use crate::ast::{self, AstId, Node, NodeKind};
use crate::lexer::{Loc, Pos};
use crate::propgraph::PropGraph;
use super::ir::{Arg, Bind, BindNode, BuiltIn, Callable, CpsId, Expr, ExprKind, FreeVar, Ref, Lit, Param, Val, ValKind};

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

fn tagged(tag: &str, s: &str) -> Node<'static> {
  let s: &'static str = Box::leak(s.to_string().into_boxed_str());
  let str_node = node(NodeKind::LitStr(s.to_string()));
  let raw = node(NodeKind::StrRawTempl(vec![str_node]));
  apply(ident(tag), vec![raw])
}

fn id_tag(s: &str)   -> Node<'static> { tagged("·id",   s) }
fn op_tag(s: &str)   -> Node<'static> { tagged("·op",   s) }

/// `fn ·state: body` — state-only continuation (used in ·if branches).
fn state_fn(body: Node<'static>) -> Node<'static> {
  fn_node(patterns(vec![ident("·state")]), vec![body])
}

/// `fn name, ·state: body` — result continuation (used in ·apply and ·match_block).
fn result_cont(name: &str, body: Node<'static>) -> Node<'static> {
  fn_node(patterns(vec![ident(name), ident("·state")]), vec![body])
}

/// `fn local, ·scope: body` — scope continuation (used in ·load and ·store).
fn scope_cont(local: &str, body: Node<'static>) -> Node<'static> {
  fn_node(patterns(vec![ident(local), ident("·scope")]), vec![body])
}

/// `·yield value, ·state, fn result, ·state: body` — yield suspension point.
fn fmt_yield(value: &Val<'_>, result: &BindNode, body: &Expr<'_>, ctx: &Ctx<'_, '_>) -> Node<'static> {
  let cont = result_cont(&render_bind_ctx(result, ctx), to_node(body, ctx));
  with_loads(ctx, &[value], |resolved| {
    apply(ident("·yield"), vec![resolved.into_iter().next().unwrap(), ident("·state"), cont.clone()])
  })
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
    ValKind::Ref(Ref::Local) => ident(&render_ref_name_ctx(v.id, ctx)),
    ValKind::Ref(Ref::Gen(_)) => ident(&render_val_name(v, ctx)),
  }
}

/// Return the local name that a Val resolves to after loading.
/// Uses origin map to recover names from the AST.
fn resolved_name(v: &Val<'_>, ctx: &Ctx<'_, '_>) -> String {
  match &v.kind {
    ValKind::Ref(Ref::Name | Ref::Local) => render_ref_name_ctx(v.id, ctx),
    ValKind::Ref(Ref::Gen(_)) => render_val_name(v, ctx),
    ValKind::Lit(_) => String::new(),
  }
}

/// Render a Ref val's name using origin map.
/// For Gen temps: always `·v_N`. For Name: recovers source name from AST.
fn render_val_name(v: &Val<'_>, ctx: &Ctx<'_, '_>) -> String {
  if !matches!(v.kind, ValKind::Ref(Ref::Gen(_))) {
    if let Some((s, _)) = ctx.source_name(v.id) {
      return s.to_string();
    }
  }
  match &v.kind {
    ValKind::Ref(Ref::Gen(n)) => format!("·v_{}", n),
    ValKind::Ref(Ref::Name | Ref::Local) => unreachable!("Ref::Name/Local should always have an origin"),
    ValKind::Lit(_) => String::new(),
  }
}

/// Whether a Val needs a `·load` synthesis before use.
/// Only `Ref::Name` (user names) need scope lookup; Gen temps are local.
fn needs_load(v: &Val<'_>) -> bool {
  matches!(v.kind, ValKind::Ref(Ref::Name))
}

/// Synthesize a `·load` wrapping `body_node`:
///   ·load ·scope, id'name' | op'sym', fn local, ·scope: body_node
/// Only called for `Ref::Name` vals (gated by `needs_load`).
fn emit_load(cps_id: CpsId, local: &str, body_node: Node<'static>, ctx: &Ctx<'_, '_>) -> Node<'static> {
  let key_node = ref_tag_ctx(cps_id, ctx);
  apply(ident("·load"), vec![
    ident("·scope"),
    key_node,
    scope_cont(local, body_node),
  ])
}

/// Wrap `inner_node` in loads for every `Ref` val in `vals`.
/// Uses origin map for name rendering and tag selection.
fn with_loads<F>(ctx: &Ctx<'_, '_>, vals: &[&Val<'_>], inner: F) -> Node<'static>
where
  F: FnOnce(Vec<Node<'static>>) -> Node<'static>,
{
  // Collect which vals need loads, build the resolved name list.
  let resolved: Vec<(bool, String)> = vals.iter().map(|v| {
    (needs_load(v), resolved_name(v, ctx))
  }).collect();

  // Build inner node first (outermost continuation last = fold left).
  let inner_nodes: Vec<Node<'static>> = vals.iter().zip(resolved.iter())
    .map(|(v, (_, name))| {
      if name.is_empty() {
        val_to_node(v, ctx)  // literal
      } else {
        ident(name)     // already resolved (Ident or Key-after-load)
      }
    })
    .collect();
  let inner_node = inner(inner_nodes);

  // Wrap in loads right-to-left (innermost first in the fold).
  vals.iter().zip(resolved.iter()).rev()
    .fold(inner_node, |body, (v, (load, name))| {
      if *load {
        if let ValKind::Ref(ref_) = &v.kind {
          emit_load(v.id, name, body, ctx)
        } else {
          body
        }
      } else {
        body
      }
    })
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
// Synthesis conventions:
//   LetVal { name, val, body }  → ·store ·scope, id'name', val, fn name, ·scope: body
//   LetFn  { name, params, ..} → ·closure ·scope, fn params…, ·scope, ·state, ·ƒ_cont: fn_body,
//                                               fn name, ·chld_scope: body
//   App    { func, args, result, body } → ·apply func_loaded, arg…, ·state, fn result, ·state: body
//   Ret(val)                   → ·ƒ_cont val, ·state
//
// Output name conventions (sigil() is the single mapping point):
//   user names          → plain:  foo, bar
//   compiler temps      → ·v_0, ·fn_3
//   operator locals     → ·op_plus, ·op_eq
//   runtime primitives  → ·store, ·load, ·scope, ·state, ·ƒ_cont, ·apply, ·closure, …
//
// IR names are always plain strings — · never appears in the IR itself.
// ---------------------------------------------------------------------------

// Maps a BindName → rendered identifier (with · prefix for Gen).
// Only meaningful for Gen temps — User requires origin map lookup.
fn render_bind(name: Bind) -> String {
  match name {
    Bind::User => unreachable!("render_bind: User requires origin map"),
    Bind::Gen(n) => format!("·v_{}", n),
  }
}

/// Render a Bind node's name using origin map.
/// For User bindings: recovers the source name from the AST.
/// For Gen temps: always renders as `·v_N`.
fn render_bind_ctx(bind: &BindNode, ctx: &Ctx<'_, '_>) -> String {
  match bind.kind {
    Bind::Gen(n) => format!("·v_{}", n),
    Bind::User => ctx.source_name(bind.id)
      .expect("render_bind_ctx: User bind must have origin")
      .0.to_string(),
  }
}

// Maps a BindName → raw scope key (no · prefix).
// Only meaningful for Gen temps — User requires origin map lookup.
fn raw_bind(name: Bind) -> String {
  match name {
    Bind::User => unreachable!("raw_bind: User requires origin map"),
    Bind::Gen(n) => format!("v_{}", n),
  }
}

/// Raw scope key for a Bind node using origin map (no · prefix).
/// Used for id_tag() — the tag content is the storage key.
fn raw_bind_ctx(bind: &BindNode, ctx: &Ctx<'_, '_>) -> String {
  match bind.kind {
    Bind::Gen(n) => format!("v_{}", n),
    Bind::User => ctx.source_name(bind.id)
      .expect("raw_bind_ctx: User bind must have origin")
      .0.to_string(),
  }
}

/// Render a RefKind::Name for display.
/// Recovers the name from the AST via origin map.
fn render_ref_name_ctx(cps_id: CpsId, ctx: &Ctx<'_, '_>) -> String {
  match ctx.source_name(cps_id) {
    Some((s, true))  => sigil_op(s),
    Some((s, false)) => s.to_string(),
    None => unreachable!("render_ref_name_ctx: RefKind::Name must have origin"),
  }
}

/// Produce the correct tag node for a ref name.
/// Recovers the name from the AST via origin map.
fn ref_tag_ctx(cps_id: CpsId, ctx: &Ctx<'_, '_>) -> Node<'static> {
  match ctx.source_name(cps_id) {
    Some((s, true))  => op_tag(s),
    Some((s, false)) => id_tag(s),
    None => unreachable!("ref_tag_ctx: RefKind::Name must have origin"),
  }
}

/// Render a free variable CpsId to a display name.
/// Recovers the source name from the origin map.
fn render_free_var(fv: FreeVar, ctx: &Ctx<'_, '_>) -> String {
  match ctx.source_name(fv) {
    Some((s, true))  => sigil_op(s),
    Some((s, false)) => s.to_string(),
    None => unreachable!("render_free_var: free var must have origin"),
  }
}

/// Produce the scope tag for a Bind node using origin map.
fn bind_tag_ctx(bind: &BindNode, ctx: &Ctx<'_, '_>) -> Node<'static> {
  id_tag(&raw_bind_ctx(bind, ctx))
}

fn sigil_op(op: &str) -> String {
  // Operators are loaded under a readable local name: `·op_plus`, `·op_eq`, etc.
  let suffix = match op {
    "+"   => "plus",
    "-"   => "minus",
    "*"   => "mul",
    "/"   => "div",
    "%"   => "rem",
    "=="  => "eq",
    "!="  => "neq",
    "<"   => "lt",
    "<="  => "lte",
    ">"   => "gt",
    ">="  => "gte",
    "."   => "dot",
    "and" => "and",
    "or"  => "or",
    "not" => "not",
    "in"  => "in",
    ".."  => "rngex",
    "..."  => "rngin",
    _     => op,
  };
  format!("·op_{}", suffix)
}

/// Emit a synthetic `·load` wrapping for a `Callable::BuiltIn`.
/// Operators: `·load ·scope, ·op'+', fn ·op_plus, ·scope: body`
/// Prims: `·load ·scope, ·id'seq_append', fn ·seq_append, ·scope: body`
///
/// TODO: Remove synthetic ·load emission once downstream consumers (codegen)
/// handle Callable::BuiltIn directly. BuiltIns are compile-time known — they
/// don't need runtime scope lookup. The ·load wrapping exists only for
/// formatter output compatibility with the current test expectations.
fn emit_builtin_load(op: &BuiltIn, body_node: Node<'static>) -> Node<'static> {
  let local = render_builtin(op);
  let key_node = match builtin_source_sym(op) {
    Some(sym) => op_tag(sym),
    None      => id_tag(&local.strip_prefix('·').unwrap_or(&local)),
  };
  apply(ident("·load"), vec![
    ident("·scope"),
    key_node,
    scope_cont(&local, body_node),
  ])
}

/// The source symbol for an operator `BuiltIn`, or `None` for prims.
fn builtin_source_sym(op: &BuiltIn) -> Option<&'static str> {
  match op {
    BuiltIn::Add => Some("+"), BuiltIn::Sub => Some("-"), BuiltIn::Mul => Some("*"),
    BuiltIn::Div => Some("/"), BuiltIn::IntDiv => Some("//"), BuiltIn::Mod => Some("%"),
    BuiltIn::IntMod => Some("%%"), BuiltIn::DivMod => Some("/%"), BuiltIn::Pow => Some("**"),
    BuiltIn::Eq => Some("=="), BuiltIn::Neq => Some("!="), BuiltIn::Lt => Some("<"),
    BuiltIn::Lte => Some("<="), BuiltIn::Gt => Some(">"), BuiltIn::Gte => Some(">="),
    BuiltIn::Cmp => Some("><"),
    BuiltIn::And => Some("and"), BuiltIn::Or => Some("or"), BuiltIn::Xor => Some("xor"), BuiltIn::Not => Some("not"),
    BuiltIn::BitAnd => Some("&"), BuiltIn::BitXor => Some("^"),
    BuiltIn::Shl => Some("<<"), BuiltIn::Shr => Some(">>"), BuiltIn::RotL => Some("<<<"), BuiltIn::RotR => Some(">>>"),
    BuiltIn::BitNot => Some("~"),
    BuiltIn::Range => Some(".."), BuiltIn::RangeIncl => Some("..."), BuiltIn::In => Some("in"), BuiltIn::NotIn => Some("not in"),
    BuiltIn::Get => Some("."),
    // Prims have no source symbol
    BuiltIn::SeqAppend | BuiltIn::SeqConcat | BuiltIn::RecPut | BuiltIn::RecMerge | BuiltIn::StrFmt => None,
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
      with_loads(ctx, &[val], |resolved| {
        apply(ident("·ƒ_cont"), vec![resolved.into_iter().next().unwrap(), ident("·state")])
      })
    }

    ExprKind::Panic => ident("·panic"),
    ExprKind::FailCont => ident("·ƒ_fail"),

    ExprKind::MatchBlock { params, arm_params, fail, arms, result, body } => {
      let result_plain = render_bind_ctx(result, ctx);
      let result_fn = fn_node(
        patterns(vec![ident(&result_plain), ident("·scope"), ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      let arm_nodes: Vec<Node<'static>> = arms.iter().map(|arm| {
        let mut fn_params: Vec<Node<'static>> = arm_params.iter()
          .map(|p| ident(&render_bind_ctx(p, ctx)))
          .collect();
        fn_params.extend([ident("·scope"), ident("·state"), ident("·ƒ_cont"), ident("·ƒ_fail")]);
        fn_node(patterns(fn_params), vec![to_node(arm, ctx)])
      }).collect();
      let refs: Vec<&Val<'_>> = params.iter().collect();
      with_loads(ctx, &refs, |resolved| {
        let mut args = resolved;
        args.push(fail_node);
        args.push(ident("·state"));
        args.extend(arm_nodes.iter().map(|n| {
          apply(ident("·match_branch"), vec![n.clone()])
        }));
        args.push(result_fn);
        apply(ident("·match_block"), args)
      })
    }

    ExprKind::LetVal { name, val, body } => {
      let plain = render_bind_ctx(name, ctx);
      let store_node = apply(ident("·store"), vec![
        ident("·scope"),
        bind_tag_ctx(name, ctx),
        val_to_node(val, ctx),
        scope_cont(&plain, to_node(body, ctx)),
      ]);
      with_loads(ctx, &[val], |_| store_node)
    }

    ExprKind::LetFn { name, params, free_vars, fn_body, body } => {
      let plain_name = render_bind_ctx(name, ctx);
      let mut fn_params: Vec<Node<'static>> = params.iter()
        .map(|p| match p {
          Param::Name(n) => ident(&render_bind_ctx(n, ctx)),
          Param::Spread(n) => spread_node(ident(&render_bind_ctx(n, ctx))),
        })
        .collect();
      let scope_arg = if free_vars.is_empty() {
        ident("·scope")
      } else {
        let mut fields: Vec<Node<'static>> = vec![
          node(NodeKind::Spread(Some(Box::new(ident("·scope"))))),
        ];
        // free_vars are CpsIds — recover names from origin map.
        fields.extend(free_vars.iter().map(|fv| {
          let name = render_free_var(*fv, ctx);
          ident(&name)
        }));
        node(NodeKind::LitRec(fields))
      };
      fn_params.push(scope_arg);
      fn_params.push(ident("·state"));
      fn_params.push(ident("·ƒ_cont"));
      apply(ident("·closure"), vec![
        ident("·scope"),
        fn_node(patterns(fn_params), vec![to_node(fn_body, ctx)]),
        fn_node(
          patterns(vec![ident(&plain_name), ident("·chld_scope")]),
          vec![to_node(body, ctx)],
        ),
      ])
    }

    ExprKind::App { func, args, result, body } => {
      let result_plain = render_bind_ctx(result, ctx);
      let result_fn = result_cont(&result_plain, to_node(body, ctx));
      let is_spread: Vec<bool> = args.iter().map(|a| matches!(a, Arg::Spread(_))).collect();
      let arg_vals: Vec<&Val<'_>> = args.iter().map(|a| match a {
        Arg::Val(v) | Arg::Spread(v) => v,
      }).collect();
      match func {
        Callable::Val(func_val) => {
          let all_vals: Vec<&Val<'_>> = std::iter::once(func_val)
            .chain(arg_vals.iter().copied())
            .collect();
          with_loads(ctx, &all_vals, |mut resolved| {
            let func_node = resolved.remove(0);
            let mut apply_args: Vec<Node<'static>> = vec![func_node];
            apply_args.extend(resolved.into_iter()
              .zip(is_spread.iter())
              .map(|(n, &spread)| if spread { spread_node(n) } else { n }));
            apply_args.push(ident("·state"));
            apply_args.push(result_fn);
            apply(ident("·apply"), apply_args)
          })
        }
        Callable::BuiltIn(op) => {
          let op_local = render_builtin(op);
          let inner = with_loads(ctx, &arg_vals, |resolved| {
            let mut apply_args: Vec<Node<'static>> = vec![ident(&op_local)];
            apply_args.extend(resolved.into_iter()
              .zip(is_spread.iter())
              .map(|(n, &spread)| if spread { spread_node(n) } else { n }));
            apply_args.push(ident("·state"));
            apply_args.push(result_fn);
            apply(ident("·apply"), apply_args)
          });
          emit_builtin_load(op, inner)
        }
      }
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
      let store_node = apply(ident("·match_store"), vec![
        ident("·scope"),
        bind_tag_ctx(name, ctx),
        val_to_node(val, ctx),
        scope_cont(&plain, to_node(body, ctx)),
      ]);
      with_loads(ctx, &[val], |_| store_node)
    }

    ExprKind::MatchApp { func, args, fail, result, body } => {
      let result_str = render_bind_ctx(result, ctx);
      let cont = fn_node(
        patterns(vec![ident(&result_str), ident("·scope"), ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      let arg_vals: Vec<&Val<'_>> = args.iter().collect();
      match func {
        Callable::Val(func_val) => {
          let all_vals: Vec<&Val<'_>> = std::iter::once(func_val).chain(arg_vals).collect();
          with_loads(ctx, &all_vals, |mut resolved| {
            let func_node = resolved.remove(0);
            let mut apply_args = vec![func_node];
            apply_args.extend(resolved);
            apply_args.push(fail_node);
            apply_args.push(cont);
            apply(ident("·match_apply"), apply_args)
          })
        }
        Callable::BuiltIn(op) => {
          let op_local = render_builtin(op);
          let inner = with_loads(ctx, &arg_vals, |resolved| {
            let mut apply_args = vec![ident(&op_local)];
            apply_args.extend(resolved);
            apply_args.push(fail_node);
            apply_args.push(cont);
            apply(ident("·match_apply"), apply_args)
          });
          emit_builtin_load(op, inner)
        }
      }
    }

    ExprKind::MatchIf { func, args, fail, body } => {
      let cont = fn_node(
        patterns(vec![ident("·scope"), ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      let arg_vals: Vec<&Val<'_>> = args.iter().collect();
      match func {
        Callable::Val(func_val) => {
          let all_vals: Vec<&Val<'_>> = std::iter::once(func_val).chain(arg_vals).collect();
          with_loads(ctx, &all_vals, |mut resolved| {
            let func_node = resolved.remove(0);
            let mut apply_args = vec![func_node];
            apply_args.extend(resolved);
            apply_args.push(fail_node);
            apply_args.push(cont);
            apply(ident("·match_if"), apply_args)
          })
        }
        Callable::BuiltIn(op) => {
          let op_local = render_builtin(op);
          let inner = with_loads(ctx, &arg_vals, |resolved| {
            let mut apply_args = vec![ident(&op_local)];
            apply_args.extend(resolved);
            apply_args.push(fail_node);
            apply_args.push(cont);
            apply(ident("·match_if"), apply_args)
          });
          emit_builtin_load(op, inner)
        }
      }
    }

    ExprKind::MatchValue { val, lit, fail, body } => {
      let cont = fn_node(
        patterns(vec![ident("·scope"), ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      with_loads(ctx, &[val], |mut resolved| {
        let val_node = resolved.remove(0);
        apply(ident("·match_value"), vec![val_node, lit_to_node(lit), fail_node, cont])
      })
    }

    ExprKind::MatchSeq { val, cursor, fail, body } => {
      let cursor_name = cursor_name(*cursor);
      let body_node = to_node(body, ctx);
      let cont = fn_node(
        patterns(vec![ident(&cursor_name), ident("·scope"), ident("·state")]),
        vec![body_node],
      );
      let fail_node = to_node(fail, ctx);
      with_loads(ctx, &[val], |mut resolved| {
        let val_node = resolved.remove(0);
        apply(ident("·match_seq"), vec![val_node, fail_node, cont])
      })
    }

    ExprKind::MatchNext { cursor, next_cursor, fail, elem, body, .. } => {
      let cur = cursor_name(*cursor);
      let next = cursor_name(*next_cursor);
      let body_node = to_node(body, ctx);
      let elem_str = render_bind_ctx(elem, ctx);
      let cont = fn_node(
        patterns(vec![ident(&elem_str), ident(&next), ident("·scope"), ident("·state")]),
        vec![body_node],
      );
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_next"), vec![ident(&cur), fail_node, cont])
    }

    ExprKind::MatchDone { cursor, fail, result, body, .. } => {
      let cur = cursor_name(*cursor);
      let result_str = render_bind_ctx(result, ctx);
      let cont = fn_node(
        patterns(vec![ident(&result_str), ident("·scope"), ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_done"), vec![ident(&cur), fail_node, cont])
    }

    ExprKind::MatchNotDone { cursor, fail, body, .. } => {
      let cur = cursor_name(*cursor);
      let cont = fn_node(
        patterns(vec![ident("·scope"), ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_not_done"), vec![ident(&cur), fail_node, cont])
    }

    ExprKind::MatchRest { cursor, fail, result, body, .. } => {
      let cur = cursor_name(*cursor);
      let result_str = render_bind_ctx(result, ctx);
      let cont = fn_node(
        patterns(vec![ident(&result_str), ident("·scope"), ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      apply(ident("·match_rest"), vec![ident(&cur), fail_node, cont])
    }

    ExprKind::MatchRec { val, cursor, fail, body } => {
      let rec_name = cursor_name(*cursor);
      let cont = fn_node(
        patterns(vec![ident(&rec_name), ident("·scope"), ident("·state")]),
        vec![to_node(body, ctx)],
      );
      let fail_node = to_node(fail, ctx);
      with_loads(ctx, &[val], |mut resolved| {
        let val_node = resolved.remove(0);
        apply(ident("·match_rec"), vec![val_node, fail_node, cont])
      })
    }

    ExprKind::MatchField { cursor, next_cursor, field, fail, elem, body, .. } => {
      let cur = cursor_name(*cursor);
      let next = cursor_name(*next_cursor);
      let elem_str = render_bind_ctx(elem, ctx);
      let cont = fn_node(
        patterns(vec![ident(&elem_str), ident(&next), ident("·scope"), ident("·state")]),
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

