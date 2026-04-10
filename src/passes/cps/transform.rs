// TODO: Add named builder helpers for Expr construction (like cps_fmt.rs has).
//       Each ExprKind variant is currently built inline with verbose struct literal
//       syntax; extracting small fns would make callsites read like a DSL.
//
// AST → compiler-internal CPS IR transform.
//
// Produces `cps::Expr` trees — clean structural IR with no env/state plumbing.
// Also produces a `PropGraph<CpsId, Option<AstId>>` origin map that traces each
// CPS node back to the AST expression it was synthesized from.
//
// Transform strategy: bottom-up accumulation.
//   lower(node) → (Val, Vec<Binding>)
// where `Binding` is a deferred let-binding. Bindings are woven into an
// Expr chain by `wrap(val, bindings, tail)` which builds right-to-left:
//
//   wrap(v, [LetVal(a,x), LetFn(f,p,b)], Cont::Ref(·ƒ_cont))
//   → LetVal { name: a, val: x, body: LetFn { name: f, ... body: Cont::Ref(·ƒ_cont) } }
//
// This avoids the monomorphization explosion that closures-as-continuations
// cause in Rust's type system.

use crate::ast::{AstId, CmpPart, Node, NodeKind};
use crate::propgraph::PropGraph;
use crate::passes::scopes::{BindId, BindInfo, BindOrigin, ScopeResult};
use super::ir::{
  Arg, Bind, BindNode, BuiltIn, Callable, Cont, ContKind, CpsFnKind, CpsId, CpsResult,
  Expr, ExprKind, Ref, Lit, Param, Val, ValKind,
};

// ---------------------------------------------------------------------------
// Node allocator
// ---------------------------------------------------------------------------

pub struct Gen<'scope> {
  /// Maps each CPS node to its originating AST node (if any).
  /// Pre-filled for scope binds (CpsId 0..binds.len()); on-the-fly for compiler temps.
  origin: PropGraph<CpsId, Option<AstId>>,
  /// Maps each scope BindId to its pre-allocated CpsId (CpsId(bind_id.0)).
  bind_to_cps: PropGraph<BindId, CpsId>,
  /// Reverse map: bind-site AstId → pre-allocated CpsId.
  /// Populated from scope.binds during Gen::new.
  bind_site_to_cps: std::collections::HashMap<u32, CpsId>,
  /// Scope resolution: ref AstId → BindId. Used to emit Ref::Synth at ref sites.
  resolution: &'scope PropGraph<AstId, Option<BindId>>,
  /// Scope binds: BindId → BindInfo. Used to detect builtins.
  binds: &'scope PropGraph<BindId, BindInfo>,
  /// The current continuation — the `·ƒ_cont` in scope for the current function body.
  /// Set to the module-level cont at transform start; swapped per LetFn scope.
  cont: CpsId,
}

impl<'scope> Gen<'scope> {
  pub fn new(scope: &'scope ScopeResult) -> Self {
    let n = scope.binds.len();
    let mut origin: PropGraph<CpsId, Option<AstId>> = PropGraph::with_size(n, None);
    let mut bind_to_cps: PropGraph<BindId, CpsId> = PropGraph::new();
    let mut bind_site_to_cps: std::collections::HashMap<u32, CpsId> = std::collections::HashMap::new();

    // Pre-allocate CpsIds for all scope binds: CpsId(i) ↔ BindId(i).
    for i in 0..n {
      let bind_id = BindId(i as u32);
      let cps_id = CpsId(i as u32);
      let ast_id = match scope.binds.get(bind_id).origin {
        BindOrigin::Ast(ast_id) => {
          bind_site_to_cps.insert(ast_id.0, cps_id);
          Some(ast_id)
        }
        BindOrigin::Builtin(_) => None,
      };
      origin.set(cps_id, ast_id);
      bind_to_cps.push(cps_id);
    }

    // Allocate the module-level cont (·ƒ_halt) — first id after the pre-allocated range.
    let cont_id: CpsId = origin.push(None);
    Gen { origin, bind_to_cps, bind_site_to_cps, resolution: &scope.resolution, binds: &scope.binds, cont: cont_id }
  }

  /// Allocate a fresh cont BindNode, set it as the current cont, and return
  /// (the new cont BindNode, the previous cont id to restore after the fn body).
  pub fn push_cont(&mut self, origin: Option<AstId>) -> (BindNode, CpsId) {
    let bind = self.bind(Bind::Cont(ContKind::Ret), origin);
    let prev = self.cont;
    self.cont = bind.id;
    (bind, prev)
  }

  /// Restore the cont to a previously saved id (after leaving a fn scope).
  pub fn pop_cont(&mut self, prev: CpsId) {
    self.cont = prev;
  }

  pub fn fresh_fn(&mut self, origin: Option<AstId>) -> BindNode {
    self.bind(Bind::Synth, origin)
  }

  pub fn fresh_result(&mut self, origin: Option<AstId>) -> BindNode {
    self.bind(Bind::Synth, origin)
  }


  /// Build an Expr with an auto-incrementing CpsId.
  fn expr(&mut self, kind: ExprKind, origin: Option<AstId>) -> Expr {
    let id = self.next_cps_id(origin);
    Expr { id, kind }
  }

  /// Build a Val with an auto-incrementing CpsId.
  fn val(&mut self, kind: ValKind, origin: Option<AstId>) -> Val {
    let id = self.next_cps_id(origin);
    Val { id, kind }
  }

  /// Build a BindNode with an auto-incrementing CpsId.
  fn bind(&mut self, kind: Bind, origin: Option<AstId>) -> BindNode {
    let id = self.next_cps_id(origin);
    BindNode { id, kind }
  }

  /// Build a SynthName BindNode using the pre-allocated CpsId for the given AstId.
  /// The AstId must correspond to a binding site registered in the scope analysis.
  fn bind_name(&mut self, ast_id: AstId) -> BindNode {
    let cps_id = self.bind_site_to_cps.get(&ast_id.0).copied()
      .unwrap_or_else(|| panic!("bind_name: no CpsId for bind-site AstId {:?}", ast_id));
    BindNode { id: cps_id, kind: Bind::SynthName }
  }

  fn next_cps_id(&mut self, origin: Option<AstId>) -> CpsId {
    self.origin.push(origin)
  }
}

// ---------------------------------------------------------------------------
// Deferred bindings — accumulated bottom-up (full definition below)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a Ref::Synth val pointing at the given bind's CpsId.
fn ref_val(g: &mut Gen, _bind: Bind, bind_id: CpsId, origin: Option<AstId>) -> Val {
  g.val(ValKind::Ref(Ref::Synth(bind_id)), origin)
}

/// Create a Ref::Synth val for a source-level name reference (Ident or SynthIdent).
/// Looks up the ref's AstId in scope resolution to find the bind's pre-allocated CpsId.
fn scope_ref_val(g: &mut Gen, ref_ast_id: AstId) -> Val {
  match g.resolution.try_get(ref_ast_id).and_then(|opt| *opt) {
    Some(bind_id) => {
      // Builtins (e.g. `import`) → emit ValKind::BuiltIn.
      if let BindOrigin::Builtin(_) = g.binds.get(bind_id).origin {
        let op = BuiltIn::from_builtin_str(&g.binds.get(bind_id).name);
        return g.val(ValKind::BuiltIn(op), Some(ref_ast_id));
      }
      let cps_id = *g.bind_to_cps.get(bind_id);
      g.val(ValKind::Ref(Ref::Synth(cps_id)), Some(ref_ast_id))
    }
    None => {
      // No binding found in scope — emit an unresolved ref carrying the AstId for display.
      let cps_id = CpsId(ref_ast_id.0);
      g.val(ValKind::Ref(Ref::Unresolved(cps_id)), Some(ref_ast_id))
    }
  }
}

fn lit_val(g: &mut Gen, lit: Lit, origin: Option<AstId>) -> Val {
  g.val(ValKind::Lit(lit), origin)
}



/// Build an explicit tail call: `App(ContRef(cont_id), [val])`.
/// Replaces the implicit `Cont::Ref` shortcut so the val's origin is preserved in the propgraph.
fn tail_app(g: &mut Gen, cont_id: CpsId, val: Val, _origin: Option<AstId>) -> Expr {
  // ContRef val gets no origin — it references the cont param, whose origin
  // is already in the propgraph under cont_id. The App expr gets no origin
  // either — it's a synthetic tail call, not a user-written expression.
  let cont_val = g.val(ValKind::ContRef(cont_id), None);
  g.expr(ExprKind::App {
    func: Callable::Val(cont_val),
    args: vec![Arg::Val(val)],
  }, None)
}

/// Wrap a bare value as the tail of a function body.
/// Produces `App(ContRef(cont), [val])` — passes val directly to the cont.
fn wrap_val(g: &mut Gen, val: Val, origin: Option<AstId>) -> Expr {
  let cont_id = g.cont;
  tail_app(g, cont_id, val, origin)
}


/// Wrap a `Vec<Val>` as `Vec<Arg::Val>` — for internal primitives that never spread.
fn args_val(vals: Vec<Val>) -> Vec<Arg> {
  vals.into_iter().map(Arg::Val).collect()
}

// ---------------------------------------------------------------------------
// Core lowering — returns (value_produced, bindings_accumulated)
// ---------------------------------------------------------------------------

type Lower = (Val, Vec<Pending>);

fn lower<'src>(g: &mut Gen, node: &'src Node<'src>) -> Lower {
  let o = Some(node.id);
  match &node.kind {
    // ---- literals ----
    NodeKind::LitBool(b) => (lit_val(g, Lit::Bool(*b), o), vec![]),
    NodeKind::LitInt(s)  => {
      let n = parse_int(s);
      // Preserve -0 as f64 negative zero (i64 has no -0).
      let lit = if n == 0 && s.starts_with('-') {
        Lit::Float(-0.0_f64)
      } else {
        Lit::Int(n)
      };
      (lit_val(g, lit, o), vec![])
    }
    NodeKind::LitFloat(s) => (lit_val(g, Lit::Float(parse_float(s)), o), vec![]),
    NodeKind::LitDecimal(s) => (lit_val(g, Lit::Decimal(parse_decimal(s)), o), vec![]),
    NodeKind::LitStr { content: s, .. } => (lit_val(g, Lit::Str(crate::strings::render(s)), o), vec![]),

    // ---- identifier reference — resolved via scope analysis ----
    NodeKind::Ident(_) | NodeKind::SynthIdent(_) => (scope_ref_val(g, node.id), vec![]),

    // ---- wildcard ----
    NodeKind::Wildcard => (scope_ref_val(g, node.id), vec![]),

    // ---- group ----
    // A plain group `(expr)` is transparent.
    // A block group `(stmt; stmt)` parses to `Group(Fn { params: Patterns([]), body })` —
    // a zero-param closure that must be immediately invoked to produce a value.
    NodeKind::Group { inner, .. } => match &inner.kind {
      NodeKind::Fn { params, body, .. }
        if matches!(&params.kind, NodeKind::Patterns(ps) if ps.items.is_empty()) =>
      {
        lower_iife(g, params, &body.items, o)
      }
      _ => lower(g, inner),
    },

    // ---- try: lower transparently for now ----
    NodeKind::Try(inner) => lower(g, inner),

    // ---- bind: `name = rhs` ----
    NodeKind::Bind { lhs, rhs, .. } => lower_bind(g, lhs, rhs, o),

    // ---- bind-right: `rhs |= lhs` (swap) ----
    NodeKind::BindRight { lhs, rhs, .. } => lower_bind(g, rhs, lhs, o),

    // ---- fn: `fn params: body` ----
    NodeKind::Fn { params, body, .. } => lower_fn(g, params, &body.items, o),

    // ---- apply: `func arg1 arg2` ----
    NodeKind::Apply { func, args } => lower_apply(g, func, &args.items, o),

    // ---- pipe: `a | b | c` == `c (b a)` ----
    NodeKind::Pipe(stages) => lower_pipe(g, &stages.items, o),

    // ---- infix op: `a + b` ----
    NodeKind::InfixOp { op, lhs, rhs } => lower_infix(g, op.src, lhs, rhs, o),

    // ---- unary op: `-a`, `not a` ----
    NodeKind::UnaryOp { op, operand } => lower_unary(g, op.src, operand, o),

    // ---- chained cmp: `a < b < c` ----
    NodeKind::ChainedCmp(parts) => lower_chained_cmp(g, parts, o),

    // ---- member access: `lhs.rhs` ----
    NodeKind::Member { lhs, rhs, .. } => lower_member(g, lhs, rhs, o),

    // ---- sequence literal ----
    NodeKind::LitSeq { items: elems, .. } => lower_lit_seq(g, &elems.items, o),

    // ---- record literal ----
    NodeKind::LitRec { items: fields, .. } => lower_lit_rec(g, &fields.items, o),

    // ---- string template ----
    NodeKind::StrTempl { children: parts, .. } => lower_str_templ(g, parts, o),

    // ---- raw string template (tagged) ----
    NodeKind::StrRawTempl { children: parts, .. } => lower_str_raw_templ(g, parts, o),

    // ---- match ----
    NodeKind::Match { subjects, arms, .. } => lower_match(g, &subjects.items, &arms.items, o),

    // ---- block: `name params: body` ----
    NodeKind::Block { name, params, body, .. } => lower_block(g, name, params, &body.items, o),

    // ---- module: single expression unwrapped; multiple as zero-param function ----
    NodeKind::Module { exprs, .. } if exprs.items.len() == 1 => lower(g, &exprs.items[0]),
    NodeKind::Module { exprs, .. } => lower_module_as_fn(g, &exprs.items, o),

    // ---- should not appear post-partial-pass ----
    NodeKind::Partial => panic!("Partial should be eliminated before CPS transform"),

    // ---- spread in expression position ----
    NodeKind::Spread { inner, .. } => {
      if let Some(inner) = inner {
        lower(g, inner)
      } else {
        panic!("Bare spread in expression position")
      }
    }

    // ---- structural nodes lowered via their parents ----
    NodeKind::Patterns(_) => panic!("Patterns node lowered via fn/match"),
    NodeKind::Arm { .. }  => panic!("Arm node lowered via lower_match"),
    NodeKind::Token(_) => panic!("Token node should not reach CPS transform"),
  }
}

/// Lower a sequence of expressions and return an Expr for the whole sequence.
/// The last expression's value is returned to the current continuation.
fn lower_seq<'src>(g: &mut Gen, exprs: &'src [Node<'src>]) -> Expr {
  lower_seq_with_tail(g, exprs, Cont::Ref(g.cont))
}

fn lower_seq_with_tail<'src>(g: &mut Gen, exprs: &'src [Node<'src>], tail: Cont) -> Expr {
  assert!(!exprs.is_empty(), "empty expression sequence");
  let mut all_pending: Vec<Pending> = vec![];
  let n = exprs.len();
  for (i, expr) in exprs.iter().enumerate() {
    let is_last = i + 1 == n;
    let o = Some(expr.id);
    if is_last {
      let (val, pending) = lower(g, expr);
      let last_has_pendings = !pending.is_empty();
      all_pending.extend(pending);
      if all_pending.is_empty() {
        // Bare atom at tail — pass val directly to cont.
        return match tail {
          Cont::Ref(cont_id) => tail_app(g, cont_id, val, o),
          tail => {
            let name = g.fresh_result(o);
            g.expr(ExprKind::LetVal { name, val: Box::new(val), cont: tail }, o)
          }
        };
      }
      // When the last expression is a standalone ref (no pendings of its own),
      // build an explicit App so the ref Val's CpsId origin is preserved.
      // When the last expression produced pendings (e.g. a call), the val is a
      // result ref — its origin flows through the pending chain naturally.
      let explicit_tail = if !last_has_pendings {
        match tail {
          Cont::Ref(cont_id) => {
            let app = tail_app(g, cont_id, val, o);
            Cont::Expr { args: vec![], body: Box::new(app) }
          }
          tail => tail,
        }
      } else {
        tail
      };
      return wrap(g, all_pending, explicit_tail);
    } else {
      match &expr.kind {
        // Bind introduces a name available in subsequent expressions.
        NodeKind::Bind { lhs, rhs, .. } | NodeKind::BindRight { rhs: lhs, lhs: rhs, .. } => {
          let pending = lower_bind_stmt(g, lhs, rhs, o);
          all_pending.extend(pending);
        }
        // Non-tail expression: evaluate, result discarded.
        _ => {
          let (val, pending) = lower(g, expr);
          all_pending.extend(pending);
          let discard = g.fresh_result(o);
          all_pending.push(Pending::Val { name: discard, val, origin: o });
        }
      }
    }
  }
  unreachable!()
}

// ---------------------------------------------------------------------------
// Bind
// ---------------------------------------------------------------------------

/// Lower a bind statement (not last in a block) — returns the `Pending` that
/// introduces the binding, so subsequent statements can use the name.
fn lower_bind_stmt<'src>(
  g: &mut Gen,
  lhs: &'src Node<'src>,
  rhs: &'src Node<'src>,
  origin: Option<AstId>,
) -> Vec<Pending> {
  let (val, mut pending) = lower(g, rhs);
  match &lhs.kind {
    NodeKind::Wildcard => {
      // _ discards — no store, just evaluate for side effects.
    }
    _ => {
      // All user binds (ident or pattern) are degenerate pattern matches.
      lower_pat_lhs(g, lhs, val, origin, &mut pending);
    }
  }
  pending
}

/// Lower a bind expression (the result IS the bound value — last in block or standalone).
fn lower_bind<'src>(
  g: &mut Gen,
  lhs: &'src Node<'src>,
  rhs: &'src Node<'src>,
  origin: Option<AstId>,
) -> Lower {
  let (val, mut pending) = lower(g, rhs);
  match &lhs.kind {
    NodeKind::Wildcard => {
      // _ discards the value — no store, just evaluate for side effects.
      (val, pending)
    }
    _ => {
      // All user binds (ident or pattern) are degenerate pattern matches.
      // lower_pat_lhs emits MatchBind for plain idents, PatternMatch for complex patterns.
      let (bound_kind, bound_id) = lower_pat_lhs(g, lhs, val, origin, &mut pending);
      // Origin for the result val: recover the bound ident's AstId.
      // - Plain ident (`x = rhs`): lhs is the ident itself
      // - Guarded bind (`a > 0 = rhs`): extract the innermost ident from the guard
      // - Range (`0..10 = rhs`): pure guard, result is the rhs value
      // - Structural patterns (Seq/Rec): result is a Synth temp — origin unused
      let result_origin = match &lhs.kind {
        NodeKind::Ident(_) => Some(lhs.id),
        NodeKind::InfixOp { op, .. } if matches!(op.src, ".." | "...") => Some(rhs.id),
        NodeKind::InfixOp { .. } => Some(extract_bind_ast_id(lhs)),
        _ => origin,
      };
      (ref_val(g, bound_kind, bound_id, result_origin), pending)
    }
  }
}

// ---------------------------------------------------------------------------
// Function definition
// ---------------------------------------------------------------------------

fn lower_fn<'src>(
  g: &mut Gen,
  params: &'src Node<'src>,
  body: &'src [Node<'src>],
  origin: Option<AstId>,
) -> Lower {
  let fn_name = g.fresh_fn(origin);
  let (fn_name_kind, fn_name_id) = (fn_name.kind, fn_name.id);
  let (mut param_names, deferred) = extract_params_with_gen(g, params);
  let (cont, prev_cont) = g.push_cont(origin);
  let fn_body = {
      let body = lower_seq(g, body);
      prepend_pat_binds(g, deferred, body)
    };
  g.pop_cont(prev_cont);
  param_names.insert(0, Param::Name(cont));
  let pending = vec![Pending::Fn { name: fn_name, params: param_names, fn_kind: CpsFnKind::CpsFunction, fn_body, origin }];
  (ref_val(g, fn_name_kind, fn_name_id, origin), pending)
}

/// Lower a Module node — zero-param function body, same as the old synthetic Fn wrapper.
fn lower_module_as_fn<'src>(
  g: &mut Gen,
  body: &'src [Node<'src>],
  origin: Option<AstId>,
) -> Lower {
  let fn_name = g.fresh_fn(origin);
  let (fn_name_kind, fn_name_id) = (fn_name.kind, fn_name.id);
  let (cont, prev_cont) = g.push_cont(origin);
  let fn_body = lower_seq(g, body);
  g.pop_cont(prev_cont);
  let pending = vec![Pending::Fn { name: fn_name, params: vec![Param::Name(cont)], fn_kind: CpsFnKind::CpsFunction, fn_body, origin }];
  (ref_val(g, fn_name_kind, fn_name_id, origin), pending)
}

/// Lower a block group `(expr; expr)` — immediately-invoked zero-param closure.
/// Defines the closure then emits an App that calls it right away.
fn lower_iife<'src>(
  g: &mut Gen,
  params: &'src Node<'src>,
  body: &'src [Node<'src>],
  origin: Option<AstId>,
) -> Lower {
  let fn_name = g.fresh_fn(origin);
  let (mut param_names, deferred) = extract_params_with_gen(g, params);
  let (cont, prev_cont) = g.push_cont(origin);
  let fn_body = {
      let body = lower_seq(g, body);
      prepend_pat_binds(g, deferred, body)
    };
  g.pop_cont(prev_cont);
  param_names.insert(0, Param::Name(cont));
  let result = g.fresh_result(origin);
  let (result_kind, result_id) = (result.kind, result.id);
  let fn_name_val = ref_val(g, fn_name.kind, fn_name.id, origin);
  let pending = vec![
    Pending::Fn { name: fn_name, params: param_names, fn_kind: CpsFnKind::CpsFunction, fn_body, origin },
    Pending::App { func: Callable::Val(fn_name_val), args: args_val(vec![]), result, origin },
  ];
  (ref_val(g, result_kind, result_id, origin), pending)
}

/// Extract params from a fn params node, returning:
/// - the param list (with complex patterns replaced by fresh Synth names)
/// - a list of Pending entries to prepend to the fn body via wrap().
///
/// Complex destructuring params (e.g. `[1, ..b]`) are desugared to a fresh spread
/// param `·v_N` and a set of PatternMatch/MatchBind pending entries that destructure it.
fn extract_params_with_gen<'src>(
  g: &mut Gen,
  params: &'src Node<'src>,
) -> (Vec<Param>, Vec<Pending>) {
  let mut param_list = vec![];
  let mut deferred: Vec<Pending> = vec![];
  let nodes = match &params.kind {
    NodeKind::Patterns(ps) => ps.items.as_slice(),
    _ => std::slice::from_ref(params),
  };
  for p in nodes {
    match &p.kind {
      NodeKind::Ident(_) | NodeKind::SynthIdent(_) => param_list.push(Param::Name(g.bind_name(p.id))),
      NodeKind::Wildcard => param_list.push(Param::Name(g.bind(Bind::Synth, Some(p.id)))),
      NodeKind::Patterns(ps) => {
        for inner in &ps.items {
          param_list.push(Param::Name(g.bind_name(inner.id)));
        }
      }
      NodeKind::Spread { inner, .. } => {
        let bind = match inner.as_deref() {
          Some(node @ Node { kind: NodeKind::Ident(_), .. }) => g.bind_name(node.id),
          _ => g.bind(Bind::Synth, Some(p.id)),
        };
        param_list.push(Param::Spread(bind));
      }
      // Complex destructuring param — desugar to a fresh plain param + pattern lowering in body.
      // The param receives a single value (not varargs); destructuring happens inside the fn.
      _ => {
        let param_name = g.fresh_result(Some(p.id));
        let (param_name_kind, param_name_id) = (param_name.kind, param_name.id);
        param_list.push(Param::Name(param_name));
        let param_val = ref_val(g, param_name_kind, param_name_id, Some(p.id));
        lower_pat_lhs(g, p, param_val, Some(p.id), &mut deferred);
      }
    }
  }
  (param_list, deferred)
}

/// Wrap `body` in pattern nodes for each deferred pattern entry, innermost first.
fn prepend_pat_binds(g: &mut Gen, deferred: Vec<Pending>, body: Expr) -> Expr {
  if deferred.is_empty() { return body; }
  let arg = g.fresh_result(None);
  wrap(g, deferred, Cont::Expr { args: vec![arg], body: Box::new(body) })
}

fn extract_params<'src>(g: &mut Gen, params: &'src Node<'src>) -> Vec<Param> {
  match &params.kind {
    NodeKind::Patterns(ps) => ps.items.iter().flat_map(|p| extract_param(g, p)).collect(),
    _ => extract_param(g, params),
  }
}

fn extract_param<'src>(g: &mut Gen, param: &'src Node<'src>) -> Vec<Param> {
  let origin = Some(param.id);
  match &param.kind {
    NodeKind::Ident(_) | NodeKind::SynthIdent(_) => vec![Param::Name(g.bind_name(param.id))],
    NodeKind::Wildcard => vec![Param::Name(g.bind(Bind::Synth, origin))],
    NodeKind::Patterns(ps) => ps.items.iter().flat_map(|p| extract_param(g, p)).collect(),
    // `..rest` varargs param — trailing spread.
    NodeKind::Spread { inner, .. } => {
      let bind = match inner.as_deref() {
        Some(node @ Node { kind: NodeKind::Ident(_), .. }) => g.bind_name(node.id),
        _ => g.bind(Bind::Synth, origin),
      };
      vec![Param::Spread(bind)]
    }
    // Complex destructuring params (e.g. `fn [a, b]: …`) — not yet implemented.
    _ => vec![],
  }
}

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------

fn lower_apply<'src>(
  g: &mut Gen,
  func: &'src Node<'src>,
  args: &'src [Node<'src>],
  origin: Option<AstId>,
) -> Lower {
  let (func_val, mut pending) = lower(g, func);
  let mut arg_vals = vec![];
  for arg in args {
    let is_spread = matches!(arg.kind, NodeKind::Spread { .. });
    let inner = if is_spread {
      if let NodeKind::Spread { inner: Some(inner), .. } = &arg.kind { inner.as_ref() } else { arg }
    } else {
      arg
    };
    // Tagged template: build a list from raw parts and pass as spread arg.
    if let NodeKind::StrRawTempl { children, .. } = &inner.kind {
      let mut acc = lit_val(g, Lit::Seq, origin);
      for part in children.iter().rev() {
        let (pv, pp) = lower_str_part(g, part);
        pending.extend(pp);
        let result = g.fresh_result(origin);
        let (result_kind, result_id) = (result.kind, result.id);
        pending.push(Pending::App {
          func: Callable::BuiltIn(BuiltIn::SeqPrepend),
          args: args_val(vec![pv, acc]),
          result,
          origin,
        });
        acc = ref_val(g, result_kind, result_id, origin);
      }
      arg_vals.push(Arg::Spread(acc));
      continue;
    }
    let (av, ap) = lower(g, inner);
    pending.extend(ap);
    arg_vals.push(if is_spread { Arg::Spread(av) } else { Arg::Val(av) });
  }
  let result = g.fresh_result(origin);
  let (result_kind, result_id) = (result.kind, result.id);
  let func = Callable::Val(func_val);
  pending.push(Pending::App { func, args: arg_vals, result,  origin });
  (ref_val(g, result_kind, result_id, origin), pending)
}

// ---------------------------------------------------------------------------
// Pipe: `a | b | c` == `c (b a)`
// ---------------------------------------------------------------------------

fn lower_pipe<'src>(g: &mut Gen, stages: &'src [Node<'src>], origin: Option<AstId>) -> Lower {
  assert!(!stages.is_empty(), "empty pipe");
  if stages.len() == 1 {
    return lower(g, &stages[0]);
  }
  // Fold left: head | f | g → g (f head)
  let (mut acc_val, mut pending) = lower(g, &stages[0]);
  for stage in &stages[1..] {
    let (func_val, sp) = lower(g, stage);
    pending.extend(sp);
    let result = g.fresh_result(origin);
    let (result_kind, result_id) = (result.kind, result.id);
    pending.push(Pending::App { func: Callable::Val(func_val), args: args_val(vec![acc_val]), result,  origin });
    acc_val = ref_val(g, result_kind, result_id, origin);
  }
  (acc_val, pending)
}

// ---------------------------------------------------------------------------
// Infix, unary, chained cmp
// ---------------------------------------------------------------------------

fn lower_infix<'src>(
  g: &mut Gen,
  op: &'src str,
  lhs: &'src Node<'src>,
  rhs: &'src Node<'src>,
  origin: Option<AstId>,
) -> Lower {
  if matches!(op, ".." | "...") {
    return lower_range(g, op, lhs, rhs, origin);
  }
  let (lv, mut pending) = lower(g, lhs);
  let (rv, rp) = lower(g, rhs);
  pending.extend(rp);
  let result = g.fresh_result(origin);
  let (result_kind, result_id) = (result.kind, result.id);
  pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::from_builtin_str(op)), args: args_val(vec![lv, rv]), result,  origin });
  (ref_val(g, result_kind, result_id, origin), pending)
}

fn lower_unary<'src>(
  g: &mut Gen,
  op: &'src str,
  operand: &'src Node<'src>,
  origin: Option<AstId>,
) -> Lower {
  let (val, mut pending) = lower(g, operand);
  let result = g.fresh_result(origin);
  let (result_kind, result_id) = (result.kind, result.id);
  pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::from_builtin_str(op)), args: args_val(vec![val]), result,  origin });
  (ref_val(g, result_kind, result_id, origin), pending)
}

fn lower_chained_cmp<'src>(
  g: &mut Gen,
  parts: &'src [CmpPart<'src>],
  origin: Option<AstId>,
) -> Lower {
  // `a < b < c` → `(a < b) and (b < c)`
  // Walk parts: collect Operand/Op pairs and emit pairwise comparisons.
  let mut pending: Vec<Pending> = vec![];
  let mut operands: Vec<Val> = vec![];
  let mut ops: Vec<&'src str> = vec![];

  for part in parts {
    match part {
      CmpPart::Operand(node) => {
        let (val, p) = lower(g, node);
        pending.extend(p);
        operands.push(val);
      }
      CmpPart::Op(op) => ops.push(op.src),
    }
  }

  // Now operands: [a, b, c], ops: [<, <]
  // Emit: cmp0 = a < b; cmp1 = b < c; result = cmp0 and cmp1
  let mut cmp_vals: Vec<Val> = vec![];
  for (i, op) in ops.iter().enumerate() {
    let lv = operands[i].clone();
    let rv = operands[i + 1].clone();
    let cmp_result = g.fresh_result(origin);
    let (cmp_result_kind, cmp_result_id) = (cmp_result.kind, cmp_result.id);
    pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::from_builtin_str(op)), args: args_val(vec![lv, rv]), result: cmp_result,  origin });
    cmp_vals.push(ref_val(g, cmp_result_kind, cmp_result_id, origin));
  }

  // And all comparison results together.
  let mut acc = cmp_vals.remove(0);
  for cv in cmp_vals {
    let and_result = g.fresh_result(origin);
    let (and_result_kind, and_result_id) = (and_result.kind, and_result.id);
    pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::And), args: args_val(vec![acc, cv]), result: and_result,  origin });
    acc = ref_val(g, and_result_kind, and_result_id, origin);
  }
  (acc, pending)
}

// ---------------------------------------------------------------------------
// Range
// ---------------------------------------------------------------------------

fn lower_range<'src>(
  g: &mut Gen,
  op: &'src str,
  start: &'src Node<'src>,
  end: &'src Node<'src>,
  origin: Option<AstId>,
) -> Lower {
  let (sv, mut pending) = lower(g, start);
  let (ev, ep) = lower(g, end);
  pending.extend(ep);
  let result = g.fresh_result(origin);
  let (result_kind, result_id) = (result.kind, result.id);
  pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::from_builtin_str(op)), args: args_val(vec![sv, ev]), result,  origin });
  (ref_val(g, result_kind, result_id, origin), pending)
}

// ---------------------------------------------------------------------------
// Member access
// ---------------------------------------------------------------------------

fn lower_member<'src>(
  g: &mut Gen,
  lhs: &'src Node<'src>,
  rhs: &'src Node<'src>,
  origin: Option<AstId>,
) -> Lower {
  let (lv, mut pending) = lower(g, lhs);
  let rv = match &rhs.kind {
    NodeKind::Ident(key) => lit_val(g, Lit::Str(key.as_bytes().to_vec()), Some(rhs.id)),
    _ => {
      let (v, rp) = lower(g, rhs);
      pending.extend(rp);
      v
    }
  };
  let result = g.fresh_result(origin);
  let (result_kind, result_id) = (result.kind, result.id);
  pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::Get), args: args_val(vec![lv, rv]), result, origin });
  (ref_val(g, result_kind, result_id, origin), pending)
}

// ---------------------------------------------------------------------------
// Sequence literal: `[a, b, ..c]`
// ---------------------------------------------------------------------------

// Build right-to-left: SeqPrepend(1, SeqPrepend(2, SeqPrepend(3, []))) — O(1) cons each.
// Spreads use SeqConcat(spread, acc) — prepend spread list onto accumulator.
fn lower_lit_seq<'src>(g: &mut Gen, elems: &'src [Node<'src>], origin: Option<AstId>) -> Lower {
  let mut acc = lit_val(g, Lit::Seq, origin);
  let mut pending: Vec<Pending> = vec![];
  for elem in elems.iter().rev() {
    let is_spread = matches!(elem.kind, NodeKind::Spread { .. });
    let inner = if is_spread {
      if let NodeKind::Spread { inner: Some(inner), .. } = &elem.kind { inner.as_ref() } else { elem }
    } else {
      elem
    };
    let (ev, ep) = lower(g, inner);
    pending.extend(ep);
    let (op, args) = if is_spread {
      // SeqConcat(spread, acc) — prepend spread onto accumulator
      (BuiltIn::SeqConcat, args_val(vec![ev, acc]))
    } else {
      // SeqPrepend(val, acc) — cons val onto front
      (BuiltIn::SeqPrepend, args_val(vec![ev, acc]))
    };
    let result = g.fresh_result(origin);
    let (result_kind, result_id) = (result.kind, result.id);
    pending.push(Pending::App { func: Callable::BuiltIn(op), args, result, origin });
    acc = ref_val(g, result_kind, result_id, origin);
  }
  (acc, pending)
}

// ---------------------------------------------------------------------------
// Record literal: `{a, b: v, ..c}`
// ---------------------------------------------------------------------------

fn lower_lit_rec<'src>(g: &mut Gen, fields: &'src [Node<'src>], origin: Option<AstId>) -> Lower {
  let mut acc = lit_val(g, Lit::Rec, origin);
  let mut pending: Vec<Pending> = vec![];
  for field in fields {
    match &field.kind {
      NodeKind::Spread { inner: Some(inner), .. } => {
        let (sv, sp) = lower(g, inner);
        pending.extend(sp);
        let result = g.fresh_result(origin);
        let (rk, ri) = (result.kind, result.id);
        pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::RecMerge), args: args_val(vec![acc, sv]), result,  origin });
        acc = ref_val(g, rk, ri, origin);
      }
      NodeKind::Bind { lhs, rhs, .. } => {
        if let NodeKind::Ident(key) = &lhs.kind {
          let key_lit = lit_val(g, Lit::Str(key.as_bytes().to_vec()), Some(field.id));
          let (fv, fp) = lower(g, rhs);
          pending.extend(fp);
          let result = g.fresh_result(origin);
          let (rk, ri) = (result.kind, result.id);
          pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::RecPut), args: args_val(vec![acc, key_lit, fv]), result,  origin });
          acc = ref_val(g, rk, ri, origin);
        } else {
          // Computed key.
          let (kv, kp) = lower(g, lhs);
          let (fv, fp) = lower(g, rhs);
          pending.extend(kp);
          pending.extend(fp);
          let result = g.fresh_result(origin);
          let (rk, ri) = (result.kind, result.id);
          pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::RecPut), args: args_val(vec![acc, kv, fv]), result,  origin });
          acc = ref_val(g, rk, ri, origin);
        }
      }
      // `{foo: val}` parsed as Arm { lhs: Ident("foo"), body: [val] }
      NodeKind::Arm { lhs, body, .. } => {
        let key_node = &**lhs;
        let val_node = body.items.last().expect("arm body empty");
        if let NodeKind::Ident(key) = &key_node.kind {
          let key_lit = lit_val(g, Lit::Str(key.as_bytes().to_vec()), Some(field.id));
          let (fv, fp) = lower(g, val_node);
          pending.extend(fp);
          let result = g.fresh_result(origin);
          let (rk, ri) = (result.kind, result.id);
          pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::RecPut), args: args_val(vec![acc, key_lit, fv]), result,  origin });
          acc = ref_val(g, rk, ri, origin);
        } else {
          let (kv, kp) = lower(g, key_node);
          let (fv, fp) = lower(g, val_node);
          pending.extend(kp);
          pending.extend(fp);
          let result = g.fresh_result(origin);
          let (rk, ri) = (result.kind, result.id);
          pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::RecPut), args: args_val(vec![acc, kv, fv]), result,  origin });
          acc = ref_val(g, rk, ri, origin);
        }
      }
      NodeKind::Ident(name) => {
        // Shorthand `{foo}` == `{foo: foo}`
        let key_lit = lit_val(g, Lit::Str(name.as_bytes().to_vec()), Some(field.id));
        let id_val = scope_ref_val(g, field.id);
        let result = g.fresh_result(origin);
        let (rk, ri) = (result.kind, result.id);
        pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::RecPut), args: args_val(vec![acc, key_lit, id_val]), result,  origin });
        acc = ref_val(g, rk, ri, origin);
      }
      _ => {
        let (fv, fp) = lower(g, field);
        pending.extend(fp);
        let result = g.fresh_result(origin);
        let (rk, ri) = (result.kind, result.id);
        pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::RecMerge), args: args_val(vec![acc, fv]), result,  origin });
        acc = ref_val(g, rk, ri, origin);
      }
    }
  }
  (acc, pending)
}

// ---------------------------------------------------------------------------
// String template: `'hello ${name}'`
// ---------------------------------------------------------------------------

/// Lower a string template part: LitStr segments stay raw (escape processing
/// is handled by str_fmt at runtime), everything else lowers normally.
fn lower_str_part<'src>(g: &mut Gen, part: &'src Node<'src>) -> Lower {
  if let NodeKind::LitStr { content: s, .. } = &part.kind {
    let o = Some(part.id);
    (lit_val(g, Lit::Str(s.as_bytes().to_vec()), o), vec![])
  } else {
    lower(g, part)
  }
}

fn lower_str_templ<'src>(g: &mut Gen, parts: &'src [Node<'src>], origin: Option<AstId>) -> Lower {
  let mut pending: Vec<Pending> = vec![];
  let mut part_vals: Vec<Arg> = vec![];
  for part in parts {
    let (pv, pp) = lower_str_part(g, part);
    pending.extend(pp);
    part_vals.push(Arg::Val(pv));
  }
  let result = g.fresh_result(origin);
  let (result_kind, result_id) = (result.kind, result.id);
  pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::StrFmt), args: part_vals, result,  origin });
  (ref_val(g, result_kind, result_id, origin), pending)
}

// ---------------------------------------------------------------------------
// Raw string template (tagged): `tag'...'`
// First element of `parts` is the tag function; rest are string segments.
// ---------------------------------------------------------------------------

/// Lower a raw string template (tagged): all segments stay raw.
/// The tag function is NOT in `parts` — the parser wraps tagged templates as
/// `Apply(tag, [StrRawTempl])`, so the tag is handled by `lower_apply`.
/// This function just lowers the segments with escape sequences preserved.
fn lower_str_raw_templ<'src>(g: &mut Gen, parts: &'src [Node<'src>], origin: Option<AstId>) -> Lower {
  // Single raw segment with no interpolation — return as a plain raw Lit::Str.
  if parts.len() == 1 {
    let (pv, pp) = lower_str_part(g, &parts[0]);
    return (pv, pp);
  }
  // Multiple segments (interpolation in raw template) — call StrFmt with raw parts.
  let mut pending: Vec<Pending> = vec![];
  let mut part_vals: Vec<Arg> = vec![];
  for part in parts {
    let (pv, pp) = lower_str_part(g, part);
    pending.extend(pp);
    part_vals.push(Arg::Val(pv));
  }
  let result = g.fresh_result(origin);
  let (result_kind, result_id) = (result.kind, result.id);
  pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::StrFmt), args: part_vals, result,  origin });
  (ref_val(g, result_kind, result_id, origin), pending)
}

// ---------------------------------------------------------------------------
// Match
// ---------------------------------------------------------------------------

/// Components of a lowered match arm — raw pieces used by lower_match
/// to build the fail-chain directly.
///
/// mp_N = fn(subj, succ, fail): mp_body  — tests pattern, calls succ/fail
/// mb_N = fn(..binds, k): mb_body        — body: names bindings, calls k with result
///
/// Calling convention (succ-first, mirrors if then/else):
///   mp_N(subj, succ, fail)
///   mb_N(..binds, k)
struct ArmCps {
  mp_name: BindNode,
  mp_params: Vec<Param>,
  mp_body: Expr,
  mb_name: BindNode,
  mb_params: Vec<Param>,
  mb_body: Expr,
  origin: Option<AstId>,
}

/// Lower a match expression into a fail-chain of (mp_N, mb_N) pairs.
///
/// Emits: mb_1/mp_1 ... mb_N/mp_N as Pending::Fn, then
/// a match-block wrapper fn m_0(subj, k): mp_1(subj, k, fn: ... mp_N(subj, k, fn: panic))
/// followed by m_0(subjects, outer_cont) as Pending::App.
///
/// Downstream sees only LetFn + App + If — no MatchBlock/MatchArm builtins.
fn lower_match<'src>(
  g: &mut Gen,
  subjects: &'src [Node<'src>],
  arms: &'src [Node<'src>],
  origin: Option<AstId>,
) -> Lower {
  let mut pending: Vec<Pending> = vec![];

  // Lower subject expressions.
  let subject_vals: Vec<Val> = subjects.iter().map(|s| {
    let (v, sp) = lower(g, s);
    pending.extend(sp);
    v
  }).collect();

  // Lower each arm to its (mp_N, mb_N) components.
  let arm_cpss: Vec<ArmCps> = arms.iter().map(|arm| lower_match_arm(g, arm, origin)).collect();

  // Emit mb_N and mp_N LetFns as Pending::Fn for each arm.
  for arm in &arm_cpss {
    pending.push(Pending::Fn {
      name: arm.mb_name.clone(),
      params: arm.mb_params.clone(),
      fn_kind: CpsFnKind::CpsClosure,
      fn_body: arm.mb_body.clone(),
      origin: arm.origin,
    });
    pending.push(Pending::Fn {
      name: arm.mp_name.clone(),
      params: arm.mp_params.clone(),
      fn_kind: CpsFnKind::CpsFunction,
      fn_body: arm.mp_body.clone(),
      origin: arm.origin,
    });
  }

  // Build the match-block wrapper: m_0(subj, k): mp_1(subj, k, fn: mp_2(subj, k, fn: panic))
  // m_0 takes (subjects..., k) and threads k as the succ cont through the fail-chain.
  let m0_subj_params: Vec<BindNode> = subject_vals.iter().map(|_| g.fresh_result(origin)).collect();
  let m0_k_param = g.bind(Bind::Cont(ContKind::Ret), None);
  let m0_k_id = m0_k_param.id;

  // Build fail-chain right-to-left. Start with panic, wrap each arm from last to first.
  let chain: Expr = arm_cpss.iter().rev().fold(
    // Initial: panic (unreachable — no arms matched)
    { let pv = g.val(ValKind::Panic, origin); g.expr(ExprKind::App { func: Callable::Val(pv), args: vec![] }, origin) },
    |fail_expr, arm| {
      // Build: mp_N(k, fn: fail_expr, subj_0, ...) — conts first
      let mp_ref = g.val(ValKind::Ref(Ref::Synth(arm.mp_name.id)), origin);
      let k_val = g.val(ValKind::ContRef(m0_k_id), origin);
      let mut call_args: Vec<Arg> = vec![
        Arg::Val(k_val),
        Arg::Cont(Cont::Expr { args: vec![], body: Box::new(fail_expr) }),
      ];
      call_args.extend(m0_subj_params.iter().map(|p| {
        Arg::Val(ref_val(g, p.kind, p.id, origin))
      }));
      g.expr(ExprKind::App { func: Callable::Val(mp_ref), args: call_args }, origin)
    }
  );

  let mut m0_params: Vec<Param> = vec![Param::Name(m0_k_param)];
  m0_params.extend(m0_subj_params.iter().map(|p| Param::Name(p.clone())));
  let m0_name = g.fresh_result(origin);
  pending.push(Pending::Fn {
    name: m0_name.clone(),
    params: m0_params,
    fn_kind: CpsFnKind::CpsFunction,
    fn_body: chain,
    origin,
  });

  // Final call: m_0(subjects..., result_cont)
  // result is the value flowing out of the match; the outer cont receives it via App result bind.
  let result = g.fresh_result(origin);
  let (result_kind, result_id) = (result.kind, result.id);
  let m0_ref = g.val(ValKind::Ref(Ref::Synth(m0_name.id)), origin);
  let call_args: Vec<Arg> = subject_vals.into_iter().map(Arg::Val).collect();
  pending.push(Pending::App {
    func: Callable::Val(m0_ref),
    args: call_args,
    result,
    origin,
  });
  (ref_val(g, result_kind, result_id, origin), pending)
}

/// Lower a single match arm into its matcher (mp) and body (mb) components.
///
/// mp = fn(subj, succ, fail): tests pattern; on success calls mb(binds, succ); on failure calls fail()
/// mb = fn(..binds, k): body expression, calls k with result
///
/// Calling convention: mp(subj, succ, fail) — succ-first like if-then-else.
fn lower_match_arm<'src>(g: &mut Gen, arm: &'src Node<'src>, _origin: Option<AstId>) -> ArmCps {
  match &arm.kind {
    NodeKind::Arm { lhs, body, .. } => {
      let arm_origin = Some(arm.id);
      let lhs_nodes: &[Node<'src>] = match &lhs.kind {
        NodeKind::Patterns(ps) => ps.items.as_slice(),
        _ => std::slice::from_ref(lhs),
      };

      // mp params: (succ, fail, subj_0, ...) — conts first.
      let mp_subj_params: Vec<BindNode> = lhs_nodes.iter().map(|_| g.fresh_result(None)).collect();
      let mp_succ_param = g.bind(Bind::Cont(ContKind::Succ), None);
      let mp_succ_id = mp_succ_param.id;
      let mp_fail_param = g.bind(Bind::Cont(ContKind::Fail), None);
      let mp_fail_id = mp_fail_param.id;

      // Lower patterns against the mp scrutinee params.
      let mut arm_pending: Vec<Pending> = vec![];
      for (pat_node, param) in lhs_nodes.iter().zip(mp_subj_params.iter()) {
        let pat_origin = Some(pat_node.id);
        let scrutinee_val = ref_val(g, param.kind, param.id, pat_origin);
        lower_pat_lhs(g, pat_node, scrutinee_val, pat_origin, &mut arm_pending);
      }

      // Collect bound names — these become mb params so name_res sees them.
      let bound_names: Vec<BindNode> = arm_pending.iter().filter_map(|p| match p {
        Pending::MatchBind { name, .. } => Some(name.clone()),
        _ => None,
      }).collect();

      // mb params: (k, ..binds) — cont first, then bound values.
      let mb_k_param = g.bind(Bind::Cont(ContKind::Ret), None);
      let mb_k_id = mb_k_param.id;
      let prev_cont = g.cont;
      g.cont = mb_k_id;
      let mb_body_expr = lower_seq(g, &body.items);
      g.cont = prev_cont;
      let mut mb_params: Vec<Param> = vec![Param::Name(mb_k_param)];
      mb_params.extend(bound_names.iter().map(|b| Param::Name(b.clone())));
      let mb_name = g.fresh_result(arm_origin);

      // Build mp body: test pattern, call mb(binds, succ) on success, fail() on failure.
      // The succ cont IS the outer continuation (passed through from m_0).
      let mp_body: Expr = {
        // Success call: mb_N(succ, bound_vals...) — cont first
        let mb_ref = g.val(ValKind::Ref(Ref::Synth(mb_name.id)), arm_origin);
        let succ_val = g.val(ValKind::ContRef(mp_succ_id), arm_origin);
        let fail_val = g.val(ValKind::ContRef(mp_fail_id), arm_origin);

        if arm_pending.is_empty() {
          // Wildcard: mp calls mb(succ) directly — no binds, no test.
          g.expr(ExprKind::App {
            func: Callable::Val(mb_ref),
            args: vec![Arg::Val(succ_val)],
          }, arm_origin)

        } else if arm_pending.iter().all(|p| matches!(p, Pending::MatchBind { .. })) {
          // Bind-only: mp calls mb(succ, scrutinees...).
          let bind_vals: Vec<Val> = arm_pending.iter().filter_map(|p| match p {
            Pending::MatchBind { val, .. } => Some(val.clone()),
            _ => None,
          }).collect();
          let mut mb_args: Vec<Arg> = vec![Arg::Val(succ_val)];
          mb_args.extend(bind_vals.into_iter().map(Arg::Val));
          g.expr(ExprKind::App { func: Callable::Val(mb_ref), args: mb_args }, arm_origin)

        } else if arm_pending.len() == 1 && matches!(arm_pending[0], Pending::PatternMatch { .. }) {
          // PatternMatch — rewire its matcher to use mp's succ/fail.
          // The inner matcher calls succ() on success. We need succ() → mb_N(outer_k).
          // Emit: LetFn mb_succ_wrapper() = mb_N(outer_k)
          //       LetFn inner_mp(subj, succ, fail) = inner_mp_body
          //       inner_mp(scrutinee, mb_succ_wrapper, fail)
          let pm = arm_pending.into_iter().next().unwrap();
          if let Pending::PatternMatch { matcher_name, matcher_params, matcher_body, .. } = pm {
            let inner_ref = g.val(ValKind::Ref(Ref::Synth(matcher_name.id)), arm_origin);
            let scrutinee = ref_val(g, mp_subj_params[0].kind, mp_subj_params[0].id, arm_origin);

            // Zero-arg wrapper: mb_succ_wrapper() = mb_N(outer_k)
            let wrapper_name = g.fresh_result(arm_origin);
            let wrapper_ref = g.val(ValKind::Ref(Ref::Synth(wrapper_name.id)), arm_origin);
            let mb_succ_body = g.expr(ExprKind::App {
              func: Callable::Val(mb_ref),
              args: vec![Arg::Val(succ_val)],
            }, arm_origin);

            // Call inner_mp(wrapper_ref, fail_val, scrutinee) — conts first
            let call = g.expr(ExprKind::App {
              func: Callable::Val(inner_ref),
              args: vec![Arg::Val(wrapper_ref), Arg::Val(fail_val), Arg::Val(scrutinee)],
            }, arm_origin);

            // LetFn inner_mp(params) = matcher_body; call
            let with_inner_mp = g.expr(ExprKind::LetFn {
              name: matcher_name,
              params: matcher_params,
              fn_kind: CpsFnKind::CpsClosure,
              fn_body: Box::new(matcher_body),
              cont: Cont::Expr { args: vec![], body: Box::new(call) },
            }, arm_origin);

            // LetFn mb_succ_wrapper() = mb_succ_body; with_inner_mp
            g.expr(ExprKind::LetFn {
              name: wrapper_name,
              params: vec![],
              fn_kind: CpsFnKind::CpsClosure,
              fn_body: Box::new(mb_succ_body),
              cont: Cont::Expr { args: vec![], body: Box::new(with_inner_mp) },
            }, arm_origin)
          } else {
            unreachable!()
          }

        } else {
          // Mixed or structural — legacy path using wrap_with_fail.
          // Build success call: mb(succ, binds) — cont first
          let bind_vals: Vec<Val> = arm_pending.iter().filter_map(|p| match p {
            Pending::MatchBind { val, .. } => Some(val.clone()),
            _ => None,
          }).collect();
          let mut mb_args: Vec<Arg> = vec![Arg::Val(succ_val)];
          mb_args.extend(bind_vals.into_iter().map(Arg::Val));
          let mb_call = g.expr(ExprKind::App { func: Callable::Val(mb_ref), args: mb_args }, arm_origin);
          let succ_cont = Cont::Expr { args: vec![], body: Box::new(mb_call) };
          wrap_with_fail(g, arm_pending, succ_cont, Some(mp_fail_id))
        }
      };

      let mut mp_params: Vec<Param> = vec![Param::Name(mp_succ_param), Param::Name(mp_fail_param)];
      mp_params.extend(mp_subj_params.iter().map(|p| Param::Name(p.clone())));
      let mp_name = g.fresh_result(arm_origin);

      ArmCps {
        mp_name,
        mp_params,
        mp_body,
        mb_name,
        mb_params,
        mb_body: mb_body_expr,
        origin: arm_origin,
      }
    }
    _ => panic!("lower_match_arm: expected Arm node"),
  }
}

// ---------------------------------------------------------------------------
// Block: `name params: body`
// ---------------------------------------------------------------------------

fn lower_block<'src>(
  g: &mut Gen,
  name: &'src Node<'src>,
  params: &'src Node<'src>,
  body: &'src [Node<'src>],
  origin: Option<AstId>,
) -> Lower {
  let block_fn_name = g.fresh_fn(origin);
  let mut param_names = extract_params(g, params);
  let (cont, prev_cont) = g.push_cont(origin);
  let fn_body = lower_seq(g, body);
  g.pop_cont(prev_cont);
  param_names.insert(0, Param::Name(cont));
  let (name_val, mut pending) = lower(g, name);
  let block_fn_val = ref_val(g, block_fn_name.kind, block_fn_name.id, origin);
  pending.push(Pending::Fn { name: block_fn_name, params: param_names, fn_kind: CpsFnKind::CpsFunction, fn_body, origin });
  let result = g.fresh_result(origin);
  let (result_kind, result_id) = (result.kind, result.id);
  pending.push(Pending::App {
    func: Callable::Val(name_val),
    args: args_val(vec![block_fn_val]),
    result,
    origin,
  });
  (ref_val(g, result_kind, result_id, origin), pending)
}

// ---------------------------------------------------------------------------
// Wrap — builds the Expr chain from Pending bindings
// ---------------------------------------------------------------------------

// Extend Pending to handle App and Match, which need a body (the next expression).
enum Pending {
  Val { name: BindNode, val: Val, origin: Option<AstId> },
  Fn { name: BindNode, params: Vec<Param>, fn_kind: CpsFnKind, fn_body: Expr, origin: Option<AstId> },
  App { func: Callable, args: Vec<Arg>, result: BindNode, origin: Option<AstId> },
  /// Pattern-lowered bind — emits plain LetVal (fail is always ·panic for irrefutable binds).
  MatchBind { name: BindNode, val: Val, origin: Option<AstId> },
  /// Pattern-lowered guard check — emits func(args) + If with ·panic as fail cont.
  /// Used by Apply patterns (predicate guards like `is_even y`, `Ok b`).
  MatchGuard { func: Callable, args: Vec<Val>, origin: Option<AstId> },
  /// Pattern match — matcher function applied to subject.
  /// Emits: LetFn body = fn(bind_names...): <cont>
  ///        LetFn matcher = fn(subj, succ, fail): matcher_body
  ///        matcher(subject, body, panic)
  /// The matcher tests with temps only; succ forwards values to the body.
  PatternMatch {
    subject: Val,
    bind_names: Vec<BindNode>,
    matcher_name: BindNode,
    matcher_params: Vec<Param>,
    matcher_body: Expr,
    origin: Option<AstId>,
  },
}

impl Pending {
  fn origin(&self) -> Option<AstId> {
    match self {
      Pending::Val { origin, .. } | Pending::Fn { origin, .. } | Pending::App { origin, .. }
      | Pending::MatchBind { origin, .. }
      | Pending::MatchGuard { origin, .. }
      | Pending::PatternMatch { origin, .. } => *origin,
    }
  }
}

/// For `cont:`-typed pending items (App, etc.): when the current item is at
/// the leaf (`Cont::Ref`), use it directly; when non-leaf, wrap the inner body with the
/// pre-allocated `result` bind node.
fn cont_with_result(cont: Cont, result: BindNode) -> Cont {
  match cont {
    Cont::Ref(_) => cont,
    Cont::Expr { body, .. } => Cont::Expr { args: vec![result], body },
  }
}


fn wrap(g: &mut Gen, bindings: Vec<Pending>, tail: Cont) -> Expr {
  wrap_with_fail(g, bindings, tail, None)
}

/// Like `wrap`, but with an explicit fail cont.
/// `fail_id`: `None` → emit `·panic`; `Some(id)` → emit a call to that cont.
/// Used for arm matchers where `fail_id` is the matcher's fail param.
/// `tail` is the continuation for the innermost (last) binding.
/// Each non-leaf binding gets `Cont::Expr { args: vec![fresh], body: Box::new(next_expr) }`.
fn wrap_with_fail(
  g: &mut Gen,
  bindings: Vec<Pending>,
  tail: Cont,
  fail_id: Option<CpsId>,
) -> Expr {
  // Fold right-to-left. The accumulator starts as `tail: Cont` and becomes
  // `Expr` after the first (innermost) pending item is processed.
  // We use an enum to track whether we have a Cont (leaf) or Expr (non-leaf).
  enum Acc {
    Tail(Cont),
    Expr(Expr),
  }
  let make_fail_val = |g: &mut Gen, origin: Option<AstId>| -> Val {
    match fail_id {
      None     => g.val(ValKind::Panic, origin),
      Some(id) => g.val(ValKind::ContRef(id), origin),
    }
  };
  let acc = bindings.into_iter().rev().fold(Acc::Tail(tail), |acc, pending| {
    let cont: Cont = match acc {
      Acc::Tail(cont) => cont,
      Acc::Expr(inner) => {
        // TODO: when Pending::Fn is followed by Pending::MatchBind (plain ident
        // bind like `add = fn a, b: ...`), the fresh_result Synth bind here is
        // redundant — the MatchBind's Name bind could go directly into the
        // LetFn body Cont::Expr args, avoiding the extra LetVal indirection.
        // Currently produces: LetFn { body: Cont::Expr [Synth] → LetVal [Name] }
        // Could produce:      LetFn { body: Cont::Expr [Name] → rest }
        // Codegen currently compensates with a val_alias map (wasm/codegen.rs)
        // that follows these LetVal rebinding chains — remove that once this
        // is fixed here.
        let arg = g.fresh_result(pending.origin());
        Cont::Expr { args: vec![arg], body: Box::new(inner) }
      }
    };
    Acc::Expr(match pending {
      Pending::Val { name, val, origin } => g.expr(
        ExprKind::LetVal { name, val: Box::new(val), cont },
        origin,
      ),
      Pending::Fn { name, params, fn_kind, fn_body, origin } => g.expr(
        ExprKind::LetFn {
          name,
          params,
          fn_kind,
          fn_body: Box::new(fn_body),
          cont,
        },
        origin,
      ),
      Pending::App { func, args, result, origin } => {
        let cont_arg = Arg::Cont(cont_with_result(cont, result));
        let args = match &func {
          // User function calls: cont first
          Callable::Val(_) => { let mut a = vec![cont_arg]; a.extend(args); a }
          // Builtin calls: cont last (runtime WAT convention)
          Callable::BuiltIn(_) => { let mut a = args; a.push(cont_arg); a }
        };
        g.expr(ExprKind::App { func, args }, origin)
      }
      Pending::MatchBind { name, val, origin } => {
        // Plain LetVal (fail is always Panic for irrefutable binds)
        g.expr(
          ExprKind::LetVal { name, val: Box::new(val), cont },
          origin,
        )
      },
      Pending::MatchGuard { func, args, origin } => {
        // Guard check: call func(args...) → if result then cont else fail.
        // Inlined as plain App + If (no dedicated guard builtin).
        let fail_val = make_fail_val(g, origin);

        // Build: fail()
        let fail_call = g.expr(ExprKind::App {
          func: Callable::Val(fail_val),
          args: vec![],
        }, origin);

        // Build: <cont body> — inline the continuation as the then-branch.
        // The fold may attach a fresh_result param that MatchGuard doesn't use;
        // just inline the body directly (the unused bind is harmless).
        let succ_body = match cont {
          Cont::Ref(cont_id) => {
            let cont_ref = g.val(ValKind::ContRef(cont_id), origin);
            g.expr(ExprKind::App {
              func: Callable::Val(cont_ref),
              args: vec![],
            }, origin)
          }
          Cont::Expr { body, .. } => *body,
        };

        // Build: if result then succ_body else fail()
        let result_bind = g.fresh_result(origin);
        let result_ref = g.val(ValKind::Ref(Ref::Synth(result_bind.id)), origin);
        let if_expr = g.expr(ExprKind::If {
          cond: Box::new(result_ref),
          then: Box::new(succ_body),
          else_: Box::new(fail_call),
        }, origin);

        // Build: func(fn result: if_expr, args...) — cont first
        let mut call_args: Vec<Arg> = vec![Arg::Cont(Cont::Expr { args: vec![result_bind], body: Box::new(if_expr) })];
        call_args.extend(args.into_iter().map(Arg::Val));
        g.expr(
          ExprKind::App { func, args: call_args },
          origin,
        )
      },
      Pending::PatternMatch { subject, bind_names, matcher_name, matcher_params, matcher_body, origin } => {
        // Emit: LetFn body = fn(bind_names...): <cont>
        //       LetFn matcher = fn(subj, succ, fail): matcher_body
        //       matcher(subject, body, panic)
        let body_name = g.fresh_result(origin);
        let body_ref = g.val(ValKind::Ref(Ref::Synth(body_name.id)), origin);
        let matcher_ref = g.val(ValKind::Ref(Ref::Synth(matcher_name.id)), origin);
        let fail_val = make_fail_val(g, origin);

        // Build: matcher(body, panic, subject) — conts first
        let call = g.expr(
          ExprKind::App {
            func: Callable::Val(matcher_ref),
            args: vec![Arg::Val(body_ref), Arg::Val(fail_val), Arg::Val(subject)],
          },
          origin,
        );

        // Build: LetFn matcher = fn(succ, fail, subj): matcher_body; <call>
        let with_matcher = g.expr(
          ExprKind::LetFn {
            name: matcher_name,
            params: matcher_params,
            fn_kind: CpsFnKind::CpsClosure,
            fn_body: Box::new(matcher_body),
            cont: Cont::Expr { args: vec![], body: Box::new(call) },
          },
          origin,
        );

        // Build: LetFn body = fn(bind_names...): <cont>; <with_matcher>
        // The body's fn receives the matched values as params,
        // then continues with the rest of the sequence (cont).
        let body_body = match cont {
          Cont::Ref(cont_id) => {
            // Forward first bind through the cont ref (for single-bind compat)
            let bind_ref = if let Some(first) = bind_names.first() {
              g.val(ValKind::Ref(Ref::Synth(first.id)), origin)
            } else {
              g.val(ValKind::Panic, origin) // no binds — shouldn't reach here
            };
            let cont_ref = g.val(ValKind::ContRef(cont_id), origin);
            g.expr(ExprKind::App {
              func: Callable::Val(cont_ref),
              args: vec![Arg::Val(bind_ref)],
            }, origin)
          }
          Cont::Expr { body, .. } => *body,
        };
        let body_params: Vec<Param> = bind_names.into_iter().map(Param::Name).collect();
        g.expr(
          ExprKind::LetFn {
            name: body_name,
            params: body_params,
            fn_kind: CpsFnKind::CpsClosure,
            fn_body: Box::new(body_body),
            cont: Cont::Expr { args: vec![], body: Box::new(with_matcher) },
          },
          origin,
        )
      },
    })
  });
  match acc {
    Acc::Expr(e) => e,
    Acc::Tail(_) => unreachable!("wrap_with_fail called with empty bindings"),
  }
}


// ---------------------------------------------------------------------------
// Numeric helpers
// ---------------------------------------------------------------------------

fn parse_int(s: &str) -> i64 {
  let s = s.replace('_', "");
  let (negative, s) = match s.strip_prefix('-') {
    Some(rest) => (true, rest.to_string()),
    None => (false, s.strip_prefix('+').unwrap_or(&s).to_string()),
  };
  let val = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
    i64::from_str_radix(hex, 16).unwrap_or(0)
  } else if let Some(oct) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) {
    i64::from_str_radix(oct, 8).unwrap_or(0)
  } else if let Some(bin) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
    i64::from_str_radix(bin, 2).unwrap_or(0)
  } else {
    s.parse().unwrap_or(0)
  };
  if negative { -val } else { val }
}

fn parse_float(s: &str) -> f64 {
  s.replace('_', "").parse().unwrap_or(0.0)
}

fn parse_decimal(s: &str) -> f64 {
  let s = s.strip_suffix('d').unwrap_or(s);
  s.replace('_', "").parse().unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Lower a top-level block of statements (module body).
/// Collect the CpsIds of simple module-level exports: `name = <non-import expr>` where
/// lhs is a plain Ident. Pattern destructures and imports are excluded.
fn collect_module_exports(exprs: &[Node<'_>], bind_site_to_cps: &std::collections::HashMap<u32, CpsId>) -> Vec<CpsId> {
  exprs.iter().filter_map(|expr| {
    let NodeKind::Bind { lhs, rhs, .. } = &expr.kind else { return None; };
    let NodeKind::Ident(_) = &lhs.kind else { return None; };
    // Exclude imports: `{foo} = import './bar'` — rhs is Apply { func: Ident("import"), .. }
    if let NodeKind::Apply { func, .. } = &rhs.kind
      && let NodeKind::Ident(name) = &func.kind
      && *name == "import" { return None; }
    bind_site_to_cps.get(&lhs.id.0).copied()
  }).collect()
}

pub fn lower_module<'src>(exprs: &'src [Node<'src>], scope: &ScopeResult) -> CpsResult {
  let mut g = Gen::new(scope);
  if exprs.is_empty() {
    // Empty module: export nothing.
    let root = g.expr(ExprKind::App { func: Callable::BuiltIn(BuiltIn::Export), args: vec![] }, None);
    return CpsResult { root, origin: g.origin, bind_to_cps: g.bind_to_cps, synth_alias: crate::propgraph::PropGraph::new(), param_info: crate::propgraph::PropGraph::new() };
  }

  // Collect simple top-level exports before lowering (bind_site_to_cps is populated at Gen::new).
  let export_ids: Vec<CpsId> = collect_module_exports(exprs, &g.bind_site_to_cps);

  // Build the terminal App: ·export ·export_0, ·export_1, ...
  let export_vals: Vec<Val> = export_ids.iter().map(|&cps_id| {
    let origin = g.origin.try_get(cps_id).and_then(|o| *o);
    g.val(ValKind::Ref(Ref::Synth(cps_id)), origin)
  }).collect();
  let export_args: Vec<Arg> = export_vals.into_iter().map(Arg::Val).collect();
  let terminal = g.expr(ExprKind::App {
    func: Callable::BuiltIn(BuiltIn::Export),
    args: export_args,
  }, None);

  // Lower the module body, using the exports terminal as the tail.
  let tail = Cont::Expr { args: vec![], body: Box::new(terminal) };
  let root = lower_seq_with_tail(&mut g, exprs, tail);
  CpsResult { root, origin: g.origin, bind_to_cps: g.bind_to_cps, synth_alias: crate::propgraph::PropGraph::new(), param_info: crate::propgraph::PropGraph::new() }
}

/// Lower a single expression node (or a Module root) to CPS IR.
pub fn lower_expr<'src>(node: &'src Node<'src>, scope: &ScopeResult) -> CpsResult {
  let mut g = Gen::new(scope);
  let (val, pending) = lower(&mut g, node);
  let cont = g.cont;
  let root = if pending.is_empty() {
    wrap_val(&mut g, val, Some(node.id))
  } else {
    wrap(&mut g, pending, Cont::Ref(cont))
  };
  CpsResult { root, origin: g.origin, bind_to_cps: g.bind_to_cps, synth_alias: crate::propgraph::PropGraph::new(), param_info: crate::propgraph::PropGraph::new() }
}

/// Recursively lower a pattern lhs node, appending MatchBind/PatternMatch pending entries.
/// `val` is the scrutinee already lowered from the rhs.
/// Returns the Bind of the primary binding (used by the caller to construct Ret).
///
/// Implemented: Ident, Wildcard, BindRight, InfixOp (guard + range), Apply (→ MatchGuard),
///              LitInt/Float/Bool/Str (→ PatternMatch), LitSeq (plain elems + Spread tail),
///              LitRec (fields + spread variants), Range (→ lower_range + MatchGuard w/ ·op_in).
/// TODO: Apply → constructor destructuring (after name resolution distinguishes predicate from constructor).
/// TODO(future): StrTempl pattern matching — e.g. `'hello ${name}'` in pattern position;
///               deferred, needs a string-matching primitive (·match_str_prefix or similar).
/// Emit a PatternMatch for a range check: `0..10` or `0...10`.
/// Matcher: fn(subj, succ, fail): op_rngex(start, end, fn range: op_in(subj, range, fn result: if result succ() else fail()))
/// Range matches don't produce bindings — succ is called with no args.
fn emit_range_pattern<'src>(
  g: &mut Gen,
  val: Val,
  op: &'src str,
  start: &'src Node<'src>,
  end: &'src Node<'src>,
  origin: Option<AstId>,
  pending: &mut Vec<Pending>,
) {
  let subj_param = g.fresh_result(origin);
  let succ_param = g.bind(Bind::Cont(ContKind::Succ), None);
  let fail_param = g.bind(Bind::Cont(ContKind::Fail), None);

  let subj_ref = ref_val(g, subj_param.kind, subj_param.id, origin);

  // succ() — no args for range matches
  let succ_ref = g.val(ValKind::ContRef(succ_param.id), origin);
  let succ_call = g.expr(ExprKind::App {
    func: Callable::Val(succ_ref),
    args: vec![],
  }, origin);

  let fail_ref = g.val(ValKind::ContRef(fail_param.id), origin);
  let fail_call = g.expr(ExprKind::App {
    func: Callable::Val(fail_ref),
    args: vec![],
  }, origin);

  let result_bind = g.fresh_result(origin);
  let result_ref = g.val(ValKind::Ref(Ref::Synth(result_bind.id)), origin);
  let if_expr = g.expr(ExprKind::If {
    cond: Box::new(result_ref),
    then: Box::new(succ_call),
    else_: Box::new(fail_call),
  }, origin);

  // Lower the range: op_rngex/op_rngin(start, end) → range_val
  let (range_val, range_pending) = lower_range(g, op, start, end, origin);

  let in_call = g.expr(ExprKind::App {
    func: Callable::BuiltIn(BuiltIn::In),
    args: vec![
      Arg::Val(subj_ref),
      Arg::Val(range_val),
      Arg::Cont(Cont::Expr { args: vec![result_bind], body: Box::new(if_expr) }),
    ],
  }, origin);

  let matcher_body = if range_pending.is_empty() {
    in_call
  } else {
    wrap(g, range_pending, Cont::Expr { args: vec![], body: Box::new(in_call) })
  };

  let matcher_name = g.fresh_result(origin);
  let bind_name = g.fresh_result(origin); // dummy — range produces no named binding
  pending.push(Pending::PatternMatch {
    subject: val,
    bind_names: vec![bind_name],
    matcher_name,
    matcher_params: vec![
      Param::Name(succ_param),
      Param::Name(fail_param),
      Param::Name(subj_param),
    ],
    matcher_body,
    origin,
  });
}

/// Emit a PatternMatch for a literal equality check.
/// Matcher: fn(subj, succ, fail): op_eq(subj, lit, fn result: if result then succ() else fail())
/// Literal matches don't produce bindings — succ is called with no args.
fn emit_literal_pattern(
  g: &mut Gen,
  val: Val,
  lit: Lit,
  origin: Option<AstId>,
  pending: &mut Vec<Pending>,
) {
  let subj_param = g.fresh_result(origin);
  let succ_param = g.bind(Bind::Cont(ContKind::Succ), None);
  let fail_param = g.bind(Bind::Cont(ContKind::Fail), None);

  let subj_ref = ref_val(g, subj_param.kind, subj_param.id, origin);
  let lit_val = g.val(ValKind::Lit(lit), origin);
  let result_bind = g.fresh_result(origin);
  let result_ref = g.val(ValKind::Ref(Ref::Synth(result_bind.id)), origin);

  // succ() — no args for literal matches
  let succ_ref = g.val(ValKind::ContRef(succ_param.id), origin);
  let succ_call = g.expr(ExprKind::App {
    func: Callable::Val(succ_ref),
    args: vec![],
  }, origin);

  let fail_ref = g.val(ValKind::ContRef(fail_param.id), origin);
  let fail_call = g.expr(ExprKind::App {
    func: Callable::Val(fail_ref),
    args: vec![],
  }, origin);

  let if_expr = g.expr(ExprKind::If {
    cond: Box::new(result_ref),
    then: Box::new(succ_call),
    else_: Box::new(fail_call),
  }, origin);

  let eq_call = g.expr(ExprKind::App {
    func: Callable::BuiltIn(BuiltIn::Eq),
    args: vec![
      Arg::Val(subj_ref),
      Arg::Val(lit_val),
      Arg::Cont(Cont::Expr { args: vec![result_bind], body: Box::new(if_expr) }),
    ],
  }, origin);

  let matcher_name = g.fresh_result(origin);
  // Literal matches don't produce bindings — body takes no params.
  pending.push(Pending::PatternMatch {
    subject: val,
    bind_names: vec![],
    matcher_name,
    matcher_params: vec![
      Param::Name(succ_param),
      Param::Name(fail_param),
      Param::Name(subj_param),
    ],
    matcher_body: eq_call,
    origin,
  });
}

/// Emit a PatternMatch for a sequence destructure: `[a, b]`, `[head, ..tail]`, etc.
///
/// Matcher body chains `SeqPop` calls to extract elements, ending with `Empty` check (if no spread)
/// or passing the rest cursor through (if spread). Succ receives all extracted temps.
/// After the PatternMatch, recursive `lower_pat_lhs` calls handle sub-patterns for each element.
///
/// Strategy: build the matcher body inside-out (from the terminal expression outward),
/// folding right-to-left over the element list. Each `SeqPop` wraps the previous body.
fn emit_seq_pattern<'src>(
  g: &mut Gen,
  val: Val,
  elems: &'src [Node<'src>],
  origin: Option<AstId>,
  pending: &mut Vec<Pending>,
) -> (Bind, CpsId) {
  let subj_param = g.fresh_result(origin);
  let succ_param = g.bind(Bind::Cont(ContKind::Succ), None);
  let fail_param = g.bind(Bind::Cont(ContKind::Fail), None);

  // Separate regular elements from the trailing spread (if any).
  let mut regular: Vec<&'src Node<'src>> = vec![];
  let mut spread: Option<&'src Option<Box<Node<'src>>>> = None;
  for elem in elems.iter() {
    if let NodeKind::Spread { inner, .. } = &elem.kind {
      spread = Some(inner);
      break;
    }
    regular.push(elem);
  }

  // Pre-allocate temp binds: one per regular element.
  let head_temps: Vec<BindNode> = regular.iter().map(|_| g.fresh_result(origin)).collect();

  // Pre-allocate a rest temp only for bound spread (`[..rest]`), not bare spread (`[..]`).
  let rest_temp: Option<BindNode> = match &spread {
    Some(Some(_)) => Some(g.fresh_result(origin)),  // bound spread
    _ => None,                                       // no spread or bare spread
  };

  // Collect all bind_names that the body fn will receive.
  let mut bind_names: Vec<BindNode> = head_temps.clone();
  if let Some(rt) = &rest_temp {
    bind_names.push(rt.clone());
  }

  // --- Build matcher body inside-out ---

  // Step 1: build the terminal expression (innermost).
  // This is either an Empty check (no spread), a SeqPop assert (bare spread),
  // or a plain succ call (bound spread).
  let terminal = build_seq_terminal(g, &head_temps, &rest_temp, &spread, succ_param.id, fail_param.id, origin);

  // Step 2: fold right — wrap each regular element's SeqPop around the body.
  // The IsSeqLike guard provides a checked cursor; SeqPops use that.
  let checked_param = g.fresh_result(origin);
  let inner_body = fold_seq_pops(
    g, &head_temps, terminal, fail_param.id, checked_param.clone(), origin,
  );

  // Step 3: wrap with IsSeqLike type guard.
  let subj_ref = g.val(ValKind::Ref(Ref::Synth(subj_param.id)), origin);
  let fail_ref = g.val(ValKind::ContRef(fail_param.id), origin);
  let fail_call = g.expr(ExprKind::App {
    func: Callable::Val(fail_ref), args: vec![],
  }, origin);
  let final_body = g.expr(ExprKind::App {
    func: Callable::BuiltIn(BuiltIn::IsSeqLike),
    args: vec![
      Arg::Val(subj_ref),
      Arg::Cont(Cont::Expr { args: vec![checked_param], body: Box::new(inner_body) }),
      Arg::Cont(Cont::Expr { args: vec![], body: Box::new(fail_call) }),
    ],
  }, origin);

  let matcher_name = g.fresh_result(origin);
  pending.push(Pending::PatternMatch {
    subject: val,
    bind_names: bind_names.clone(),
    matcher_name,
    matcher_params: vec![
      Param::Name(succ_param),
      Param::Name(fail_param),
      Param::Name(subj_param),
    ],
    matcher_body: final_body,
    origin,
  });

  // Step 4: push sub-pattern pendings for each element.
  // The body fn receives the temps. For each regular element, lower_pat_lhs against the temp.
  for (i, elem_node) in regular.iter().enumerate() {
    let temp_val = ref_val(g, head_temps[i].kind, head_temps[i].id, origin);
    lower_pat_lhs(g, elem_node, temp_val, Some(elem_node.id), pending);
  }

  // Handle spread binding.
  if let Some(Some(name_node)) = &spread {
    let rt = rest_temp.as_ref().unwrap();
    let rest_val = ref_val(g, rt.kind, rt.id, origin);
    if let NodeKind::Ident(_) = &name_node.kind {
      let bind = g.bind_name(name_node.id);
      pending.push(Pending::MatchBind { name: bind, val: rest_val, origin });
    }
  }

  // Return a placeholder bind (the last bind_name or a fresh one).
  let r = g.fresh_result(origin);
  (r.kind, r.id)
}

/// Build the terminal expression for a seq pattern matcher.
/// This is the innermost CPS expression before the SeqPop chain wraps it.
fn build_seq_terminal<'src>(
  g: &mut Gen,
  head_temps: &[BindNode],
  rest_temp: &Option<BindNode>,
  spread: &Option<&'src Option<Box<Node<'src>>>>,
  succ_id: CpsId,
  fail_id: CpsId,
  origin: Option<AstId>,
) -> (Expr, BindNode) {
  // Build succ(temps...) call.
  let succ_ref = g.val(ValKind::ContRef(succ_id), origin);
  let mut succ_args: Vec<Arg> = head_temps.iter()
    .map(|t| Arg::Val(g.val(ValKind::Ref(Ref::Synth(t.id)), origin)))
    .collect();
  if let Some(rt) = rest_temp {
    succ_args.push(Arg::Val(g.val(ValKind::Ref(Ref::Synth(rt.id)), origin)));
  }
  let succ_call = g.expr(ExprKind::App {
    func: Callable::Val(succ_ref),
    args: succ_args,
  }, origin);

  // The terminal needs a cursor_bind — the bind that receives the cursor value
  // from the last SeqPop's tail (or subj_param if no elements).
  let cursor_bind = g.fresh_result(origin);

  match spread {
    None => {
      // No spread: empty(cursor, fn e: if e then succ(...) else fail())
      let fail_ref = g.val(ValKind::ContRef(fail_id), origin);
      let fail_call = g.expr(ExprKind::App {
        func: Callable::Val(fail_ref), args: vec![],
      }, origin);
      let result_bind = g.fresh_result(origin);
      let result_ref = g.val(ValKind::Ref(Ref::Synth(result_bind.id)), origin);
      let if_expr = g.expr(ExprKind::If {
        cond: Box::new(result_ref),
        then: Box::new(succ_call),
        else_: Box::new(fail_call),
      }, origin);
      let cursor_ref = g.val(ValKind::Ref(Ref::Synth(cursor_bind.id)), origin);
      let empty_call = g.expr(ExprKind::App {
        func: Callable::BuiltIn(BuiltIn::Empty),
        args: vec![
          Arg::Val(cursor_ref),
          Arg::Cont(Cont::Expr { args: vec![result_bind], body: Box::new(if_expr) }),
        ],
      }, origin);
      (empty_call, cursor_bind)
    }
    Some(None) => {
      // Bare spread `..`: assert remaining cursor is non-empty by doing one more seq_pop.
      // seq_pop(cursor, fail, fn _, _: succ(...))
      let fail_ref = g.val(ValKind::ContRef(fail_id), origin);
      let discard_h = g.fresh_result(origin);
      let discard_t = g.fresh_result(origin);
      let cursor_ref = g.val(ValKind::Ref(Ref::Synth(cursor_bind.id)), origin);
      let pop_call = g.expr(ExprKind::App {
        func: Callable::BuiltIn(BuiltIn::SeqPop),
        args: vec![
          Arg::Val(cursor_ref),
          Arg::Val(fail_ref),
          Arg::Cont(Cont::Expr { args: vec![discard_h, discard_t], body: Box::new(succ_call) }),
        ],
      }, origin);
      (pop_call, cursor_bind)
    }
    Some(Some(_)) => {
      // Bound spread `..rest`: cursor IS the rest. Just call succ.
      // The rest_temp is already in succ_args. We need to wire cursor_bind → rest_temp.
      // Emit: let rest_temp = cursor_bind; succ(...)
      let rt = rest_temp.as_ref().unwrap();
      let cursor_ref = g.val(ValKind::Ref(Ref::Synth(cursor_bind.id)), origin);
      let let_rest = g.expr(ExprKind::LetVal {
        name: rt.clone(),
        val: Box::new(cursor_ref),
        cont: Cont::Expr { args: vec![], body: Box::new(succ_call) },
      }, origin);
      (let_rest, cursor_bind)
    }
  }
}

/// Fold right over head_temps, wrapping each SeqPop around the body.
/// `first_cursor` is used as the outermost cursor (typically subj_param).
/// Returns the fully wrapped body.
fn fold_seq_pops(
  g: &mut Gen,
  head_temps: &[BindNode],
  terminal: (Expr, BindNode),
  fail_id: CpsId,
  first_cursor: BindNode,
  origin: Option<AstId>,
) -> Expr {
  let (mut body, mut next_tail_bind) = terminal;

  if head_temps.is_empty() {
    // No elements — terminal's cursor_bind needs to be wired to first_cursor.
    // Emit a LetVal alias only in this case.
    let cursor_ref = g.val(ValKind::Ref(Ref::Synth(first_cursor.id)), origin);
    return g.expr(ExprKind::LetVal {
      name: next_tail_bind,
      val: Box::new(cursor_ref),
      cont: Cont::Expr { args: vec![], body: Box::new(body) },
    }, origin);
  }

  // Fold from last element to first.
  for i in (0..head_temps.len()).rev() {
    // For the outermost pop (i == 0), use first_cursor directly.
    let cursor_bind = if i == 0 { first_cursor.clone() } else { g.fresh_result(origin) };
    let cursor_ref = g.val(ValKind::Ref(Ref::Synth(cursor_bind.id)), origin);
    let fail_ref = g.val(ValKind::ContRef(fail_id), origin);

    let pop = g.expr(ExprKind::App {
      func: Callable::BuiltIn(BuiltIn::SeqPop),
      args: vec![
        Arg::Val(cursor_ref),
        Arg::Val(fail_ref),
        Arg::Cont(Cont::Expr {
          args: vec![head_temps[i].clone(), next_tail_bind],
          body: Box::new(body),
        }),
      ],
    }, origin);

    body = pop;
    next_tail_bind = cursor_bind;
  }

  body
}

/// Spread variant in a record pattern.
enum SpreadKind<'a, 'src> {
  BareNonEmpty,                      // `{..}`
  EmptyRest,                         // `{..{}}` — rest must be empty
  Bound(&'a Node<'src>),             // `{..rest}`
  SubPattern(&'a Node<'src>),        // `{..{bar, spam}}`
}

/// Record pattern key: identifier name or computed expression.
enum RecKey<'a, 'src> {
  Ident(&'src str),
  Expr(&'a Node<'src>),
}

/// A record field extracted from the AST pattern: key + sub-pattern node.
struct RecField<'a, 'src> {
  key: RecKey<'a, 'src>,
  /// The sub-pattern node for the extracted value.
  /// For `{x}` shorthand: the Ident node itself.
  /// For `{x: pat}`: the pat node.
  pat: &'a Node<'src>,
  origin: Option<AstId>,
}

/// Emit a PatternMatch for a record destructure: `{x, y}`, `{bar, ..rest}`, etc.
///
/// Matcher body chains `RecPop` calls to extract named fields, ending with:
/// - No terminal check for partial matches (`{x, y}` — extra fields OK)
/// - `Empty` check for `{..{}}` (exact match after extracting all named fields)
/// - `Empty` inverted for `{..}` (assert non-empty rest)
/// - Plain succ for `{..rest}` (bind remaining)
///
/// After the PatternMatch, recursive `lower_pat_lhs` calls handle sub-patterns.
fn emit_rec_pattern<'src>(
  g: &mut Gen,
  val: Val,
  fields: &'src [Node<'src>],
  origin: Option<AstId>,
  pending: &mut Vec<Pending>,
) -> (Bind, CpsId) {
  let subj_param = g.fresh_result(origin);
  let succ_param = g.bind(Bind::Cont(ContKind::Succ), None);
  let fail_param = g.bind(Bind::Cont(ContKind::Fail), None);

  // Parse field nodes into RecField structs and detect spread.
  let mut regular: Vec<RecField<'_, 'src>> = vec![];
  let mut spread: Option<SpreadKind<'_, 'src>> = None;

  for field_node in fields.iter() {
    match &field_node.kind {
      NodeKind::Spread { inner, .. } => {
        spread = Some(match inner {
          None => SpreadKind::BareNonEmpty,
          Some(inner_node) => match &inner_node.kind {
            NodeKind::Ident(_) => SpreadKind::Bound(inner_node),
            NodeKind::LitRec { items, .. } if items.items.is_empty() => SpreadKind::EmptyRest,
            NodeKind::LitRec { .. } => SpreadKind::SubPattern(inner_node),
            _ => SpreadKind::BareNonEmpty, // fallback
          }
        });
        break;
      }
      NodeKind::Ident(name) => {
        regular.push(RecField { key: RecKey::Ident(name), pat: field_node, origin: Some(field_node.id) });
      }
      NodeKind::Bind { lhs, rhs: pat_node, .. } => {
        match &lhs.kind {
          NodeKind::Ident(key) => {
            regular.push(RecField { key: RecKey::Ident(key), pat: pat_node, origin: Some(lhs.id) });
          }
          NodeKind::LitStr { content, .. } => {
            regular.push(RecField { key: RecKey::Ident(content), pat: pat_node, origin: Some(lhs.id) });
          }
          _ => {}
        }
      }
      NodeKind::Arm { lhs: arm_lhs, body: arm_body, .. } => {
        if let Some(pat_node) = arm_body.items.last() {
          match &arm_lhs.kind {
            NodeKind::Ident(key) => {
              regular.push(RecField { key: RecKey::Ident(key), pat: pat_node, origin: Some(arm_lhs.id) });
            }
            NodeKind::LitStr { content, .. } => {
              regular.push(RecField { key: RecKey::Ident(content), pat: pat_node, origin: Some(arm_lhs.id) });
            }
            NodeKind::Group { inner, .. } => {
              regular.push(RecField { key: RecKey::Expr(inner), pat: pat_node, origin: Some(arm_lhs.id) });
            }
            _ => {}
          }
        }
      }
      _ => {}
    }
  }

  // Pre-allocate temp binds: one per regular field.
  let field_temps: Vec<BindNode> = regular.iter().map(|_| g.fresh_result(origin)).collect();

  // Pre-allocate rest temp if spread binds or sub-patterns.
  let rest_temp: Option<BindNode> = match &spread {
    Some(SpreadKind::Bound(_) | SpreadKind::SubPattern(_)) => Some(g.fresh_result(origin)),
    _ => None,
  };

  // Collect bind_names for the body fn.
  let mut bind_names: Vec<BindNode> = field_temps.clone();
  if let Some(rt) = &rest_temp {
    bind_names.push(rt.clone());
  }
  // For partial match with no spread and no fields, we need at least a dummy bind.
  if bind_names.is_empty() {
    bind_names.push(g.fresh_result(origin));
  }

  // --- Build matcher body inside-out ---

  // Step 1: terminal expression.
  let terminal = build_rec_terminal(g, &field_temps, &rest_temp, &spread, succ_param.id, fail_param.id, origin);

  // Step 2: fold right — wrap each field's RecPop.
  // The IsRecLike guard provides a checked cursor; RecPops use that.
  let checked_param = g.fresh_result(origin);
  let inner_body = fold_rec_pops(
    g, &regular, &field_temps, terminal, fail_param.id, checked_param.clone(), origin,
  );

  // Step 3: wrap with IsRecLike type guard.
  let subj_ref = g.val(ValKind::Ref(Ref::Synth(subj_param.id)), origin);
  let fail_ref = g.val(ValKind::ContRef(fail_param.id), origin);
  let fail_call = g.expr(ExprKind::App {
    func: Callable::Val(fail_ref), args: vec![],
  }, origin);
  let final_body = g.expr(ExprKind::App {
    func: Callable::BuiltIn(BuiltIn::IsRecLike),
    args: vec![
      Arg::Val(subj_ref),
      Arg::Cont(Cont::Expr { args: vec![checked_param], body: Box::new(inner_body) }),
      Arg::Cont(Cont::Expr { args: vec![], body: Box::new(fail_call) }),
    ],
  }, origin);

  let matcher_name = g.fresh_result(origin);
  pending.push(Pending::PatternMatch {
    subject: val,
    bind_names: bind_names.clone(),
    matcher_name,
    matcher_params: vec![
      Param::Name(succ_param),
      Param::Name(fail_param),
      Param::Name(subj_param),
    ],
    matcher_body: final_body,
    origin,
  });

  // Step 4: push sub-pattern pendings for each field.
  for (i, field) in regular.iter().enumerate() {
    let temp_val = ref_val(g, field_temps[i].kind, field_temps[i].id, origin);
    lower_pat_lhs(g, field.pat, temp_val, field.origin, pending);
  }

  // Handle spread binding/sub-pattern.
  match &spread {
    Some(SpreadKind::Bound(name_node)) => {
      let rt = rest_temp.as_ref().unwrap();
      let rest_val = ref_val(g, rt.kind, rt.id, origin);
      if let NodeKind::Ident(_) = &name_node.kind {
        let bind = g.bind_name(name_node.id);
        pending.push(Pending::MatchBind { name: bind, val: rest_val, origin });
      }
    }
    Some(SpreadKind::SubPattern(sub_pat_node)) => {
      let rt = rest_temp.as_ref().unwrap();
      let rest_val = ref_val(g, rt.kind, rt.id, origin);
      lower_pat_lhs(g, sub_pat_node, rest_val, Some(sub_pat_node.id), pending);
    }
    _ => {}
  }

  let r = g.fresh_result(origin);
  (r.kind, r.id)
}

/// Build the terminal expression for a rec pattern matcher.
fn build_rec_terminal<'src>(
  g: &mut Gen,
  field_temps: &[BindNode],
  rest_temp: &Option<BindNode>,
  spread: &Option<SpreadKind<'_, 'src>>,
  succ_id: CpsId,
  fail_id: CpsId,
  origin: Option<AstId>,
) -> (Expr, BindNode) {
  // Build succ(temps...) call.
  let succ_ref = g.val(ValKind::ContRef(succ_id), origin);
  let mut succ_args: Vec<Arg> = field_temps.iter()
    .map(|t| Arg::Val(g.val(ValKind::Ref(Ref::Synth(t.id)), origin)))
    .collect();
  if let Some(rt) = rest_temp {
    succ_args.push(Arg::Val(g.val(ValKind::Ref(Ref::Synth(rt.id)), origin)));
  }
  // For empty partial match (no fields, no spread), still need succ() call.
  let succ_call = g.expr(ExprKind::App {
    func: Callable::Val(succ_ref),
    args: succ_args,
  }, origin);

  let cursor_bind = g.fresh_result(origin);

  match spread {
    // `{}` — exact empty match: empty(cursor, fn e: if e then succ() else fail())
    None if field_temps.is_empty() => {
      let fail_ref = g.val(ValKind::ContRef(fail_id), origin);
      let fail_call = g.expr(ExprKind::App {
        func: Callable::Val(fail_ref), args: vec![],
      }, origin);
      let result_bind = g.fresh_result(origin);
      let result_ref = g.val(ValKind::Ref(Ref::Synth(result_bind.id)), origin);
      let if_expr = g.expr(ExprKind::If {
        cond: Box::new(result_ref),
        then: Box::new(succ_call),
        else_: Box::new(fail_call),
      }, origin);
      let cursor_ref = g.val(ValKind::Ref(Ref::Synth(cursor_bind.id)), origin);
      let empty_call = g.expr(ExprKind::App {
        func: Callable::BuiltIn(BuiltIn::Empty),
        args: vec![
          Arg::Val(cursor_ref),
          Arg::Cont(Cont::Expr { args: vec![result_bind], body: Box::new(if_expr) }),
        ],
      }, origin);
      (empty_call, cursor_bind)
    }

    // `{x, y}` — partial match (fields extracted, extra OK): just call succ.
    None => {
      (succ_call, cursor_bind)
    }

    // `{bar, ..{}}` — rest must be empty: empty(cursor, fn e: if e then succ(...) else fail())
    Some(SpreadKind::EmptyRest) => {
      let fail_ref = g.val(ValKind::ContRef(fail_id), origin);
      let fail_call = g.expr(ExprKind::App {
        func: Callable::Val(fail_ref), args: vec![],
      }, origin);
      let result_bind = g.fresh_result(origin);
      let result_ref = g.val(ValKind::Ref(Ref::Synth(result_bind.id)), origin);
      let if_expr = g.expr(ExprKind::If {
        cond: Box::new(result_ref),
        then: Box::new(succ_call),
        else_: Box::new(fail_call),
      }, origin);
      let cursor_ref = g.val(ValKind::Ref(Ref::Synth(cursor_bind.id)), origin);
      let empty_call = g.expr(ExprKind::App {
        func: Callable::BuiltIn(BuiltIn::Empty),
        args: vec![
          Arg::Val(cursor_ref),
          Arg::Cont(Cont::Expr { args: vec![result_bind], body: Box::new(if_expr) }),
        ],
      }, origin);
      (empty_call, cursor_bind)
    }

    // `{bar, ..}` — assert non-empty rest: empty(cursor, fn e: if e then fail() else succ(...))
    Some(SpreadKind::BareNonEmpty) => {
      let fail_ref = g.val(ValKind::ContRef(fail_id), origin);
      let fail_call = g.expr(ExprKind::App {
        func: Callable::Val(fail_ref), args: vec![],
      }, origin);
      let result_bind = g.fresh_result(origin);
      let result_ref = g.val(ValKind::Ref(Ref::Synth(result_bind.id)), origin);
      // Note: inverted — if empty then FAIL, else succ.
      let if_expr = g.expr(ExprKind::If {
        cond: Box::new(result_ref),
        then: Box::new(fail_call),
        else_: Box::new(succ_call),
      }, origin);
      let cursor_ref = g.val(ValKind::Ref(Ref::Synth(cursor_bind.id)), origin);
      let empty_call = g.expr(ExprKind::App {
        func: Callable::BuiltIn(BuiltIn::Empty),
        args: vec![
          Arg::Val(cursor_ref),
          Arg::Cont(Cont::Expr { args: vec![result_bind], body: Box::new(if_expr) }),
        ],
      }, origin);
      (empty_call, cursor_bind)
    }

    // `{..rest}` or `{bar, ..rest}` — bind rest cursor.
    Some(SpreadKind::Bound(_)) | Some(SpreadKind::SubPattern(_)) => {
      let rt = rest_temp.as_ref().unwrap();
      let cursor_ref = g.val(ValKind::Ref(Ref::Synth(cursor_bind.id)), origin);
      let let_rest = g.expr(ExprKind::LetVal {
        name: rt.clone(),
        val: Box::new(cursor_ref),
        cont: Cont::Expr { args: vec![], body: Box::new(succ_call) },
      }, origin);
      (let_rest, cursor_bind)
    }
  }
}

/// Fold right over record fields, wrapping each RecPop around the body.
/// `first_cursor` is used as the outermost cursor (typically subj_param).
fn fold_rec_pops<'src>(
  g: &mut Gen,
  fields: &[RecField<'_, 'src>],
  field_temps: &[BindNode],
  terminal: (Expr, BindNode),
  fail_id: CpsId,
  first_cursor: BindNode,
  origin: Option<AstId>,
) -> Expr {
  let (mut body, mut next_tail_bind) = terminal;

  if fields.is_empty() {
    let cursor_ref = g.val(ValKind::Ref(Ref::Synth(first_cursor.id)), origin);
    return g.expr(ExprKind::LetVal {
      name: next_tail_bind,
      val: Box::new(cursor_ref),
      cont: Cont::Expr { args: vec![], body: Box::new(body) },
    }, origin);
  }

  for i in (0..fields.len()).rev() {
    let cursor_bind = if i == 0 { first_cursor.clone() } else { g.fresh_result(origin) };
    let cursor_ref = g.val(ValKind::Ref(Ref::Synth(cursor_bind.id)), origin);
    let (field_key_val, key_pending) = match &fields[i].key {
      RecKey::Ident(name) => (g.val(ValKind::Lit(Lit::Str(name.as_bytes().to_vec())), origin), vec![]),
      RecKey::Expr(node) => lower(g, node),
    };
    let fail_ref = g.val(ValKind::ContRef(fail_id), origin);

    let pop = g.expr(ExprKind::App {
      func: Callable::BuiltIn(BuiltIn::RecPop),
      args: vec![
        Arg::Val(cursor_ref),
        Arg::Val(field_key_val),
        Arg::Val(fail_ref),
        Arg::Cont(Cont::Expr {
          args: vec![field_temps[i].clone(), next_tail_bind],
          body: Box::new(body),
        }),
      ],
    }, origin);

    body = prepend_pat_binds(g, key_pending, pop);
    next_tail_bind = cursor_bind;
  }

  body
}

/// String template pattern: `'prefix${capture}suffix' = val`
/// Validates exactly one interpolation with at most two literal parts.
/// Emits StrMatch(subj, prefix, suffix, fail, succ(capture)).
fn emit_str_templ_pattern<'src>(
  g: &mut Gen,
  val: Val,
  children: &'src [Node<'src>],
  origin: Option<AstId>,
  pending: &mut Vec<Pending>,
) -> (Bind, CpsId) {
  // Parse children into (prefix_bytes, capture_node, suffix_bytes).
  // Valid shapes: [Expr], [LitStr, Expr], [Expr, LitStr], [LitStr, Expr, LitStr]
  // Template literal parts are escape-rendered like standalone string literals
  // so byte comparisons in str_match match the runtime representation of the
  // subject string (which is also rendered at the CPS LitStr lowering).
  let render = |s: &str| crate::strings::render(s);
  let (prefix, capture_node, suffix): (Vec<u8>, _, Vec<u8>) = match children {
    [expr] if !matches!(expr.kind, NodeKind::LitStr { .. }) =>
      (Vec::new(), expr, Vec::new()),
    [lit, expr] if matches!(lit.kind, NodeKind::LitStr { .. }) => {
      let NodeKind::LitStr { content, .. } = &lit.kind else { unreachable!() };
      (render(content), expr, Vec::new())
    }
    [expr, lit] if matches!(lit.kind, NodeKind::LitStr { .. })
               && !matches!(expr.kind, NodeKind::LitStr { .. }) => {
      let NodeKind::LitStr { content, .. } = &lit.kind else { unreachable!() };
      (Vec::new(), expr, render(content))
    }
    [lit1, expr, lit2]
      if matches!(lit1.kind, NodeKind::LitStr { .. })
      && matches!(lit2.kind, NodeKind::LitStr { .. }) => {
      let NodeKind::LitStr { content: c1, .. } = &lit1.kind else { unreachable!() };
      let NodeKind::LitStr { content: c2, .. } = &lit2.kind else { unreachable!() };
      (render(c1), expr, render(c2))
    }
    _ => panic!("emit_str_templ_pattern: expected single capture with at most two literal parts"),
  };

  let subj_param = g.fresh_result(origin);
  let succ_param = g.bind(Bind::Cont(ContKind::Succ), None);
  let fail_param = g.bind(Bind::Cont(ContKind::Fail), None);

  // The capture temp — what succ receives.
  let capture_temp = g.fresh_result(origin);

  // Build matcher body: ·str_match subj, prefix, suffix, fail, fn capture: succ(capture)
  let subj_ref = g.val(ValKind::Ref(Ref::Synth(subj_param.id)), origin);
  let prefix_val = g.val(ValKind::Lit(Lit::Str(prefix)), origin);
  let suffix_val = g.val(ValKind::Lit(Lit::Str(suffix)), origin);
  let fail_ref = g.val(ValKind::ContRef(fail_param.id), origin);

  let succ_ref = g.val(ValKind::ContRef(succ_param.id), origin);
  let capture_ref = g.val(ValKind::Ref(Ref::Synth(capture_temp.id)), origin);
  let succ_call = g.expr(ExprKind::App {
    func: Callable::Val(succ_ref),
    args: vec![Arg::Val(capture_ref)],
  }, origin);

  let matcher_body = g.expr(ExprKind::App {
    func: Callable::BuiltIn(BuiltIn::StrMatch),
    args: vec![
      Arg::Val(subj_ref),
      Arg::Val(prefix_val),
      Arg::Val(suffix_val),
      Arg::Val(fail_ref),
      Arg::Cont(Cont::Expr { args: vec![capture_temp.clone()], body: Box::new(succ_call) }),
    ],
  }, origin);

  let matcher_name = g.fresh_result(origin);
  pending.push(Pending::PatternMatch {
    subject: val,
    bind_names: vec![capture_temp.clone()],
    matcher_name,
    matcher_params: vec![
      Param::Name(succ_param),
      Param::Name(fail_param),
      Param::Name(subj_param),
    ],
    matcher_body,
    origin,
  });

  // Lower the capture expression as a sub-pattern (could be ident, nested pattern, etc.)
  let capture_val = ref_val(g, capture_temp.kind, capture_temp.id, origin);
  lower_pat_lhs(g, capture_node, capture_val, Some(capture_node.id), pending)
}

fn lower_pat_lhs<'src>(
  g: &mut Gen,
  lhs: &'src Node<'src>,
  val: Val,
  origin: Option<AstId>,
  pending: &mut Vec<Pending>,
) -> (Bind, CpsId) {
  match &lhs.kind {
    // Plain bind: `x = foo` or synthetic `·$_N` from partial desugaring
    NodeKind::Ident(_) | NodeKind::SynthIdent(_) => {
      let bind = g.bind_name(lhs.id);
      let r = (bind.kind, bind.id);
      pending.push(Pending::MatchBind { name: bind, val,  origin });
      r
    }

    // Wildcard: `_` — no binding; pass the val through as-is for guard args.
    // Val must be a Ref (always true when called from Apply arg lowering).
    NodeKind::Wildcard => {
      match &val.kind {
        ValKind::Ref(Ref::Synth(cps_id)) => (Bind::Synth, *cps_id),
        _ => panic!("lower_pat_lhs: Wildcard with non-Ref val"),
      }
    }

    // Range pattern: `0..10` or `0...10` — assert val is in range; no binding produced.
    // Emits a PatternMatch: matcher function tests op_in(subj, range), succ called with no args.
    NodeKind::InfixOp { op, lhs: start, rhs: end } if matches!(op.src, ".." | "...") => {
      emit_range_pattern(g, val, op.src, start, end, origin, pending);
      { let r = g.fresh_result(origin); (r.kind, r.id) }
    }

    // Guarded bind: `a > 0 = foo` or `a > 0 or a < 9 = foo`
    // Emits a PatternMatch: matcher function tests with temps, succ forwards value.
    NodeKind::InfixOp { op, lhs: guard_lhs, rhs: guard_rhs } => {
      // The bind name for the body (succ param gives it its name).
      let bind_ast_id = extract_bind_ast_id(guard_lhs);
      let bind = g.bind_name(bind_ast_id);
      let r = (bind.kind, bind.id);

      // Build matcher: fn(subj, succ, fail): op(subj, rhs, fn result: if result succ(subj) else fail)
      let subj_param = g.fresh_result(origin);
      let succ_param = g.bind(Bind::Cont(ContKind::Succ), None);
      let fail_param = g.bind(Bind::Cont(ContKind::Fail), None);

      // Lower the guard RHS (the comparison value, e.g. `0` in `a > 0`).
      // This is a pure expression, no scope dependency on the bind.
      let (rv, rp) = lower(g, guard_rhs);

      // Build the guard test: op(subj, rhs, fn result: if result then succ(subj) else fail())
      let subj_ref = ref_val(g, subj_param.kind, subj_param.id, origin);
      let result_bind = g.fresh_result(origin);
      let result_ref = g.val(ValKind::Ref(Ref::Synth(result_bind.id)), origin);

      let succ_ref = g.val(ValKind::ContRef(succ_param.id), origin);
      let succ_call = g.expr(ExprKind::App {
        func: Callable::Val(succ_ref),
        args: vec![Arg::Val(subj_ref.clone())],
      }, origin);

      let fail_ref = g.val(ValKind::ContRef(fail_param.id), origin);
      let fail_call = g.expr(ExprKind::App {
        func: Callable::Val(fail_ref),
        args: vec![],
      }, origin);

      let if_expr = g.expr(ExprKind::If {
        cond: Box::new(result_ref),
        then: Box::new(succ_call),
        else_: Box::new(fail_call),
      }, origin);

      // Build the op call: op(subj, rhs, fn result: if_expr)
      let op_builtin = BuiltIn::from_builtin_str(op.src);
      let guard_call = g.expr(ExprKind::App {
        func: Callable::BuiltIn(op_builtin),
        args: vec![
          Arg::Val(subj_ref),
          Arg::Val(rv),
          Arg::Cont(Cont::Expr { args: vec![result_bind], body: Box::new(if_expr) }),
        ],
      }, origin);

      // If the RHS lowering produced pendings (e.g. function calls), wrap them.
      let matcher_body = if rp.is_empty() {
        guard_call
      } else {
        wrap(g, rp, Cont::Expr { args: vec![], body: Box::new(guard_call) })
      };

      let matcher_name = g.fresh_result(origin);
      pending.push(Pending::PatternMatch {
        subject: val,
        bind_names: vec![bind],
        matcher_name,
        matcher_params: vec![
          Param::Name(succ_param),
          Param::Name(fail_param),
          Param::Name(subj_param),
        ],
        matcher_body,
        origin,
      });

      r
    }

    // Predicate guard: `is_even y`, `Ok b`, `foo 2, a, 3`
    // In pattern position, Apply args are either:
    //   - Ident/Wildcard — sub-pattern: binds to or discards `val` (the seq element)
    //   - Anything else  — expression: lowered normally and passed as-is to the guard
    // Exactly one arg should be an Ident/Wildcard (the "binding slot"); others are
    // literal/value args. All are assembled in order as arguments to MatchGuard.
    NodeKind::Apply { func, args } => {
      let mut arg_vals: Vec<Val> = vec![];
      for arg in args.items.iter() {
        let arg_val = match &arg.kind {
          NodeKind::Ident(_) | NodeKind::Wildcard => {
            let (bound_kind, bound_id) = lower_pat_lhs(g, arg, val.clone(), Some(arg.id), pending);
            ref_val(g, bound_kind, bound_id, Some(arg.id))
          }
          _ => {
            let (v, ap) = lower(g, arg);
            pending.extend(ap);
            v
          }
        };
        arg_vals.push(arg_val);
      }
      let (func_val, func_pending) = lower(g, func);
      pending.extend(func_pending);
      pending.push(Pending::MatchGuard { func: Callable::Val(func_val), args: arg_vals,  origin });
      { let r = g.fresh_result(origin); (r.kind, r.id) }
    }

    // Literal equality: `1`, `'hello'`, `true` — emits PatternMatch with op_eq test. No binding produced.
    NodeKind::LitInt(s) => {
      let n = parse_int(s);
      let lit = if n == 0 && s.starts_with('-') {
        Lit::Float(-0.0_f64)
      } else {
        Lit::Int(n)
      };
      emit_literal_pattern(g, val, lit, origin, pending);
      { let r = g.fresh_result(origin); (r.kind, r.id) }
    }
    NodeKind::LitFloat(s) => {
      emit_literal_pattern(g, val, Lit::Float(parse_float(s)), origin, pending);
      { let r = g.fresh_result(origin); (r.kind, r.id) }
    }
    NodeKind::LitBool(b) => {
      emit_literal_pattern(g, val, Lit::Bool(*b), origin, pending);
      { let r = g.fresh_result(origin); (r.kind, r.id) }
    }
    NodeKind::LitStr { content: s, .. } => {
      emit_literal_pattern(g, val, Lit::Str(crate::strings::render(s)), origin, pending);
      { let r = g.fresh_result(origin); (r.kind, r.id) }
    }

    // Seq pattern: `[] = foo`, `[a, b] = foo`, `[a, []] = foo`, `[head, ..tail] = foo`
    // Emits a single PatternMatch whose matcher body chains SeqPop/Empty calls.
    NodeKind::LitSeq { items: elems, .. } => {
      emit_seq_pattern(g, val, &elems.items, origin, pending)
    }

    // Rec pattern: `{} = foo`, `{x, y} = point`, `{bar, ..rest} = foo`, `{bar, ..{}} = foo`
    // Emits a single PatternMatch whose matcher body chains RecPop/Empty calls.
    NodeKind::LitRec { items: fields, .. } => {
      emit_rec_pattern(g, val, &fields.items, origin, pending)
    }

    // Bind-right: `pat |= name` — bind val to `name`, then also destructure as `pat`.
    // e.g. `[b, c] |= d` binds the element as `d` and destructures it as `[b, c]`.
    NodeKind::BindRight { lhs: pat, rhs: name_node, .. } => {
      if !matches!(name_node.kind, NodeKind::Ident(_)) {
        panic!("lower_pat_lhs: BindRight rhs must be an Ident");
      }
      let bind = g.bind_name(name_node.id);
      pending.push(Pending::MatchBind { name: bind, val: val.clone(),  origin });
      lower_pat_lhs(g, pat, val, origin, pending)
    }

    // StrTempl pattern: `'prefix${capture}suffix' = val`
    // Validates exactly one interpolation with at most two literal parts (prefix, suffix).
    // Emits StrMatch(subj, prefix, suffix, fail, succ(capture)).
    NodeKind::StrTempl { children, .. } => {
      emit_str_templ_pattern(g, val, children, origin, pending)
    }

    _ => todo!("lower_pat_lhs: pattern not yet implemented: {:?}", lhs.kind),
  }
}


/// Extract the binding AstId from a pattern LHS.
/// Recurses through nested InfixOps to find the innermost ident.
fn extract_bind_ast_id<'src>(node: &'src Node<'src>) -> AstId {
  match &node.kind {
    NodeKind::Ident(_) => node.id,
    NodeKind::InfixOp { lhs, .. } => extract_bind_ast_id(lhs),
    _ => panic!("extract_bind_ast_id: expected ident in pattern lhs, got {:?}", node.kind),
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod cps_tests {
  use crate::passes::cps::fmt::Ctx;

  fn cps_expr(src: &str) -> String {
    match crate::to_desugared(src, "test") {
      Ok(desugared) => {
        let cps = super::lower_expr(&desugared.result.root, &desugared.scope);
        let bk = super::super::ir::collect_bind_kinds(&cps.root);
        let ctx = Ctx { origin: &cps.origin, ast_index: &desugared.ast_index, captures: None, param_info: None, bind_kinds: Some(&bk) };
        let (output, srcmap) = crate::passes::cps::fmt::fmt_with_mapped_content(&cps.root, &ctx, "test", src);
        let json = srcmap.to_json();
        let b64 = crate::sourcemap::base64_encode(json.as_bytes());
        format!("{output}\n#sourcemaps:{b64}")
      }
      Err(e) => format!("ERROR: {e}"),
    }
  }

  test_macros::include_fink_tests!("src/passes/cps/test_literals.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_bindings.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_functions.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_operators.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_application.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_strings.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_collections.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_scheduling.fnk");
}

#[cfg(test)]
mod pat_tests {
  use crate::passes::cps::fmt::Ctx;

  fn cps_expr(src: &str) -> String {
    match crate::to_desugared(src, "test") {
      Ok(desugared) => {
        let cps = super::lower_expr(&desugared.result.root, &desugared.scope);
        let bk = super::super::ir::collect_bind_kinds(&cps.root);
        let ctx = Ctx { origin: &cps.origin, ast_index: &desugared.ast_index, captures: None, param_info: None, bind_kinds: Some(&bk) };
        let (output, srcmap) = crate::passes::cps::fmt::fmt_with_mapped_content(&cps.root, &ctx, "test", src);
        let json = srcmap.to_json();
        let b64 = crate::sourcemap::base64_encode(json.as_bytes());
        format!("{output}\n#sourcemaps:{b64}")
      }
      Err(e) => format!("ERROR: {e}"),
    }
  }

  test_macros::include_fink_tests!("src/passes/cps/test_patterns_bind.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_patterns_seq.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_patterns_rec.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_patterns_match.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_patterns_bindings.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_patterns_str.fnk");
}

#[cfg(test)]
mod module_tests {
  use crate::passes::cps::fmt::Ctx;

  fn cps_module(src: &str) -> String {
    match crate::to_desugared(src, "test") {
      Ok(desugared) => {
        let cps = crate::passes::lower(&desugared);
        let bk = crate::passes::cps::ir::collect_bind_kinds(&cps.result.root);
        let ctx = Ctx { origin: &cps.result.origin, ast_index: &desugared.ast_index, captures: None, param_info: None, bind_kinds: Some(&bk) };
        let (output, srcmap) = crate::passes::cps::fmt::fmt_with_mapped_content(&cps.result.root, &ctx, "test", src);
        let json = srcmap.to_json();
        let b64 = crate::sourcemap::base64_encode(json.as_bytes());
        format!("{output}\n#sourcemaps:{b64}")
      }
      Err(e) => format!("ERROR: {e}"),
    }
  }

  test_macros::include_fink_tests!("src/passes/cps/test_module.fnk");
}
