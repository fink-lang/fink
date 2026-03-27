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
  Arg, Bind, BindNode, BuiltIn, Callable, Cont, CpsId, CpsResult, Expr, ExprKind, Ref, Lit,
  Param, Val, ValKind,
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
    let bind = self.bind(Bind::Cont, origin);
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

  /// Allocate a cursor BindNode for seq/rec pattern traversal.
  pub fn fresh_cursor(&mut self, origin: Option<AstId>) -> BindNode {
    self.bind(Bind::Synth, origin)
  }

  /// Build an Expr with an auto-incrementing CpsId.
  fn expr<'src>(&mut self, kind: ExprKind<'src>, origin: Option<AstId>) -> Expr<'src> {
    let id = self.next_cps_id(origin);
    Expr { id, kind }
  }

  /// Build a Val with an auto-incrementing CpsId.
  fn val<'src>(&mut self, kind: ValKind<'src>, origin: Option<AstId>) -> Val<'src> {
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
fn ref_val<'src>(g: &mut Gen, _bind: Bind, bind_id: CpsId, origin: Option<AstId>) -> Val<'src> {
  g.val(ValKind::Ref(Ref::Synth(bind_id)), origin)
}

/// Create a Ref::Synth val for a source-level name reference (Ident or SynthIdent).
/// Looks up the ref's AstId in scope resolution to find the bind's pre-allocated CpsId.
fn scope_ref_val<'src>(g: &mut Gen, ref_ast_id: AstId) -> Val<'src> {
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

fn lit_val<'src>(g: &mut Gen, lit: Lit<'src>, origin: Option<AstId>) -> Val<'src> {
  g.val(ValKind::Lit(lit), origin)
}



/// Build an explicit tail call: `App(ContRef(cont_id), [val])`.
/// Replaces the implicit `Cont::Ref` shortcut so the val's origin is preserved in the propgraph.
fn tail_app<'src>(g: &mut Gen, cont_id: CpsId, val: Val<'src>, _origin: Option<AstId>) -> Expr<'src> {
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
fn wrap_val<'src>(g: &mut Gen, val: Val<'src>, origin: Option<AstId>) -> Expr<'src> {
  let cont_id = g.cont;
  tail_app(g, cont_id, val, origin)
}


/// Wrap a `Vec<Val>` as `Vec<Arg::Val>` — for internal primitives that never spread.
fn args_val<'src>(vals: Vec<Val<'src>>) -> Vec<Arg<'src>> {
  vals.into_iter().map(Arg::Val).collect()
}

// ---------------------------------------------------------------------------
// Core lowering — returns (value_produced, bindings_accumulated)
// ---------------------------------------------------------------------------

type Lower<'src> = (Val<'src>, Vec<Pending<'src>>);

fn lower<'src>(g: &mut Gen, node: &'src Node<'src>) -> Lower<'src> {
  let o = Some(node.id);
  match &node.kind {
    // ---- literals ----
    NodeKind::LitBool(b) => (lit_val(g, Lit::Bool(*b), o), vec![]),
    NodeKind::LitInt(s)  => (lit_val(g, Lit::Int(parse_int(s)), o), vec![]),
    NodeKind::LitFloat(s) => (lit_val(g, Lit::Float(parse_float(s)), o), vec![]),
    NodeKind::LitDecimal(s) => (lit_val(g, Lit::Decimal(parse_decimal(s)), o), vec![]),
    NodeKind::LitStr { content: s, .. } => (lit_val(g, Lit::Str(s), o), vec![]),

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

    // ---- yield: suspend execution, yield a value ----
    NodeKind::Yield(inner) => lower_yield(g, inner, o),

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
    NodeKind::Module(exprs) if exprs.items.len() == 1 => lower(g, &exprs.items[0]),
    NodeKind::Module(exprs) => lower_module_as_fn(g, &exprs.items, o),

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
  }
}

/// Lower a sequence of expressions and return an Expr for the whole sequence.
/// The last expression's value is returned to the current continuation.
fn lower_seq<'src>(g: &mut Gen, exprs: &'src [Node<'src>]) -> Expr<'src> {
  lower_seq_with_tail(g, exprs, Cont::Ref(g.cont))
}

fn lower_seq_with_tail<'src>(g: &mut Gen, exprs: &'src [Node<'src>], tail: Cont<'src>) -> Expr<'src> {
  assert!(!exprs.is_empty(), "empty expression sequence");
  let mut all_pending: Vec<Pending<'src>> = vec![];
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
// Yield
// ---------------------------------------------------------------------------

/// Lower `yield inner` — suspend execution, yield the inner value.
/// The continuation receives the resumed value bound to a fresh result.
fn lower_yield<'src>(g: &mut Gen, inner: &'src Node<'src>, origin: Option<AstId>) -> Lower<'src> {
  let (val, mut pending) = lower(g, inner);
  let result = g.fresh_result(origin);
  let (result_kind, result_id) = (result.kind, result.id);
  pending.push(Pending::Yield { value: val, result,  origin });
  (ref_val(g, result_kind, result_id, origin), pending)
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
) -> Vec<Pending<'src>> {
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
) -> Lower<'src> {
  let (val, mut pending) = lower(g, rhs);
  match &lhs.kind {
    NodeKind::Wildcard => {
      // _ discards the value — no store, just evaluate for side effects.
      (val, pending)
    }
    _ => {
      // All user binds (ident or pattern) are degenerate pattern matches.
      // lower_pat_lhs emits MatchLetVal for plain idents, Match* chains for patterns.
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
) -> Lower<'src> {
  let fn_name = g.fresh_fn(origin);
  let (fn_name_kind, fn_name_id) = (fn_name.kind, fn_name.id);
  let (mut param_names, deferred) = extract_params_with_gen(g, params);
  let (cont, prev_cont) = g.push_cont(origin);
  let fn_body = {
      let body = lower_seq(g, body);
      prepend_pat_binds(g, deferred, body)
    };
  g.pop_cont(prev_cont);
  param_names.push(Param::Name(cont));
  let pending = vec![Pending::Fn { name: fn_name, params: param_names, fn_body, origin }];
  (ref_val(g, fn_name_kind, fn_name_id, origin), pending)
}

/// Lower a Module node — zero-param function body, same as the old synthetic Fn wrapper.
fn lower_module_as_fn<'src>(
  g: &mut Gen,
  body: &'src [Node<'src>],
  origin: Option<AstId>,
) -> Lower<'src> {
  let fn_name = g.fresh_fn(origin);
  let (fn_name_kind, fn_name_id) = (fn_name.kind, fn_name.id);
  let (cont, prev_cont) = g.push_cont(origin);
  let fn_body = lower_seq(g, body);
  g.pop_cont(prev_cont);
  let pending = vec![Pending::Fn { name: fn_name, params: vec![Param::Name(cont)], fn_body, origin }];
  (ref_val(g, fn_name_kind, fn_name_id, origin), pending)
}

/// Lower a block group `(expr; expr)` — immediately-invoked zero-param closure.
/// Defines the closure then emits an App that calls it right away.
fn lower_iife<'src>(
  g: &mut Gen,
  params: &'src Node<'src>,
  body: &'src [Node<'src>],
  origin: Option<AstId>,
) -> Lower<'src> {
  let fn_name = g.fresh_fn(origin);
  let (mut param_names, deferred) = extract_params_with_gen(g, params);
  let (cont, prev_cont) = g.push_cont(origin);
  let fn_body = {
      let body = lower_seq(g, body);
      prepend_pat_binds(g, deferred, body)
    };
  g.pop_cont(prev_cont);
  param_names.push(Param::Name(cont));
  let result = g.fresh_result(origin);
  let (result_kind, result_id) = (result.kind, result.id);
  let fn_name_val = ref_val(g, fn_name.kind, fn_name.id, origin);
  let pending = vec![
    Pending::Fn { name: fn_name, params: param_names, fn_body, origin },
    Pending::App { func: Callable::Val(fn_name_val), args: args_val(vec![]), result, origin },
  ];
  (ref_val(g, result_kind, result_id, origin), pending)
}

/// Extract params from a fn params node, returning:
/// - the param list (with complex patterns replaced by fresh Synth names)
/// - a list of Pending entries to prepend to the fn body via wrap().
///
/// Complex destructuring params (e.g. `[1, ..b]`) are desugared to a fresh spread
/// param `·v_N` and a set of Match* pending entries that destructure it.
fn extract_params_with_gen<'src>(
  g: &mut Gen,
  params: &'src Node<'src>,
) -> (Vec<Param>, Vec<Pending<'src>>) {
  let mut param_list = vec![];
  let mut deferred: Vec<Pending<'src>> = vec![];
  let nodes = match &params.kind {
    NodeKind::Patterns(ps) => ps.items.as_slice(),
    _ => std::slice::from_ref(params),
  };
  for p in nodes {
    match &p.kind {
      NodeKind::Ident(_) => param_list.push(Param::Name(g.bind_name(p.id))),
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
      // Complex destructuring param — desugar to a fresh plain param + Match* lowering in body.
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

/// Wrap `body` in Match* nodes for each deferred pattern entry, innermost first.
fn prepend_pat_binds<'src>(g: &mut Gen, deferred: Vec<Pending<'src>>, body: Expr<'src>) -> Expr<'src> {
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
    NodeKind::Ident(_) => vec![Param::Name(g.bind_name(param.id))],
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
) -> Lower<'src> {
  let (func_val, mut pending) = lower(g, func);
  let mut arg_vals = vec![];
  for arg in args {
    let is_spread = matches!(arg.kind, NodeKind::Spread { .. });
    let inner = if is_spread {
      if let NodeKind::Spread { inner: Some(inner), .. } = &arg.kind { inner.as_ref() } else { arg }
    } else {
      arg
    };
    let (av, ap) = lower(g, inner);
    pending.extend(ap);
    arg_vals.push(if is_spread { Arg::Spread(av) } else { Arg::Val(av) });
  }
  let result = g.fresh_result(origin);
  let (result_kind, result_id) = (result.kind, result.id);
  pending.push(Pending::App { func: Callable::Val(func_val), args: arg_vals, result,  origin });
  (ref_val(g, result_kind, result_id, origin), pending)
}

// ---------------------------------------------------------------------------
// Pipe: `a | b | c` == `c (b a)`
// ---------------------------------------------------------------------------

fn lower_pipe<'src>(g: &mut Gen, stages: &'src [Node<'src>], origin: Option<AstId>) -> Lower<'src> {
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
) -> Lower<'src> {
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
) -> Lower<'src> {
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
) -> Lower<'src> {
  // `a < b < c` → `(a < b) and (b < c)`
  // Walk parts: collect Operand/Op pairs and emit pairwise comparisons.
  let mut pending: Vec<Pending<'src>> = vec![];
  let mut operands: Vec<Val<'src>> = vec![];
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
  let mut cmp_vals: Vec<Val<'src>> = vec![];
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
) -> Lower<'src> {
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
) -> Lower<'src> {
  let (lv, mut pending) = lower(g, lhs);
  let (rv, rp) = lower(g, rhs);
  pending.extend(rp);
  let result = g.fresh_result(origin);
  let (result_kind, result_id) = (result.kind, result.id);
  pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::Get), args: args_val(vec![lv, rv]), result,  origin });
  (ref_val(g, result_kind, result_id, origin), pending)
}

// ---------------------------------------------------------------------------
// Sequence literal: `[a, b, ..c]`
// ---------------------------------------------------------------------------

fn lower_lit_seq<'src>(g: &mut Gen, elems: &'src [Node<'src>], origin: Option<AstId>) -> Lower<'src> {
  let mut acc = lit_val(g, Lit::Seq, origin);
  let mut pending: Vec<Pending<'src>> = vec![];
  for elem in elems {
    let is_spread = matches!(elem.kind, NodeKind::Spread { .. });
    let inner = if is_spread {
      if let NodeKind::Spread { inner: Some(inner), .. } = &elem.kind { inner.as_ref() } else { elem }
    } else {
      elem
    };
    let (ev, ep) = lower(g, inner);
    pending.extend(ep);
    let op = if is_spread { BuiltIn::SeqConcat } else { BuiltIn::SeqAppend };
    let result = g.fresh_result(origin);
    let (result_kind, result_id) = (result.kind, result.id);
    pending.push(Pending::App { func: Callable::BuiltIn(op), args: args_val(vec![acc, ev]), result,  origin });
    acc = ref_val(g, result_kind, result_id, origin);
  }
  (acc, pending)
}

// ---------------------------------------------------------------------------
// Record literal: `{a, b: v, ..c}`
// ---------------------------------------------------------------------------

fn lower_lit_rec<'src>(g: &mut Gen, fields: &'src [Node<'src>], origin: Option<AstId>) -> Lower<'src> {
  let mut acc = lit_val(g, Lit::Rec, origin);
  let mut pending: Vec<Pending<'src>> = vec![];
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
          let key_lit = lit_val(g, Lit::Str(key), Some(field.id));
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
          let key_lit = lit_val(g, Lit::Str(key), Some(field.id));
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
        let key_lit = lit_val(g, Lit::Str(name), Some(field.id));
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

fn lower_str_templ<'src>(g: &mut Gen, parts: &'src [Node<'src>], origin: Option<AstId>) -> Lower<'src> {
  let mut pending: Vec<Pending<'src>> = vec![];
  let mut part_vals: Vec<Arg<'src>> = vec![];
  for part in parts {
    let (pv, pp) = lower(g, part);
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

fn lower_str_raw_templ<'src>(g: &mut Gen, parts: &'src [Node<'src>], origin: Option<AstId>) -> Lower<'src> {
  assert!(!parts.is_empty(), "empty raw string template");
  let (tag_fn, mut pending) = lower(g, &parts[0]);
  let mut part_vals: Vec<Arg<'src>> = vec![];
  for part in &parts[1..] {
    let (pv, pp) = lower(g, part);
    pending.extend(pp);
    part_vals.push(Arg::Val(pv));
  }
  let result = g.fresh_result(origin);
  let (result_kind, result_id) = (result.kind, result.id);
  pending.push(Pending::App { func: Callable::Val(tag_fn), args: part_vals, result,  origin });
  (ref_val(g, result_kind, result_id, origin), pending)
}

// ---------------------------------------------------------------------------
// Match
// ---------------------------------------------------------------------------

fn lower_match<'src>(
  g: &mut Gen,
  subjects: &'src [Node<'src>],
  arms: &'src [Node<'src>],
  origin: Option<AstId>,
) -> Lower<'src> {
  let mut pending: Vec<Pending<'src>> = vec![];

  let params: Vec<Val<'src>> = subjects.iter().map(|s| {
    let (v, sp) = lower(g, s);
    pending.extend(sp);
    v
  }).collect();
  let arm_params: Vec<BindNode> = params.iter().map(|_| g.fresh_result(origin)).collect();
  let mut arm_vals: Vec<Val<'src>> = vec![];
  for arm in arms {
    let (arm_val, arm_pending) = lower_match_arm(g, arm, &arm_params, origin);
    pending.extend(arm_pending);
    arm_vals.push(arm_val);
  }
  let result = g.fresh_result(origin);
  let (result_kind, result_id) = (result.kind, result.id);
  pending.push(Pending::MatchBlock { params, arm_vals, result,  origin });
  (ref_val(g, result_kind, result_id, origin), pending)
}

fn lower_match_arm<'src>(g: &mut Gen, arm: &'src Node<'src>, arm_params: &[BindNode], _origin: Option<AstId>) -> Lower<'src> {
  match &arm.kind {
    NodeKind::Arm { lhs, body, .. } => {
      let origin = Some(arm.id);
      let lhs_nodes: &[Node<'src>] = match &lhs.kind {
        NodeKind::Patterns(ps) => ps.items.as_slice(),
        _ => std::slice::from_ref(lhs),
      };

      // Allocate explicit Bind::Cont params for the matcher:
      //   [scrutinee_0, ..., fail_bind, succ_bind]
      // fail_bind: called on pattern mismatch (tries next arm)
      // succ_bind: called on success — this is the body cont ref
      // fail/succ conts are synthetic CPS machinery — no source origin.
      let fail_bind = g.bind(Bind::Cont, None);
      let succ_bind = g.bind(Bind::Cont, None);
      let succ_id   = succ_bind.id;

      // Lower patterns using the matcher's own scrutinee params (fresh per arm).
      // Scrutinee params are synthetic — no source origin.
      let matcher_scrutinee_params: Vec<BindNode> =
        arm_params.iter().map(|_| g.fresh_result(None)).collect();
      let mut arm_pending: Vec<Pending<'src>> = vec![];
      for (pat_node, param) in lhs_nodes.iter().zip(matcher_scrutinee_params.iter()) {
        let pat_origin = Some(pat_node.id);
        let scrutinee_val = ref_val(g, param.kind, param.id, pat_origin);
        lower_pat_lhs(g, pat_node, scrutinee_val, pat_origin, &mut arm_pending);
      }

      // Collect pattern-bound names — become explicit args of the body cont so
      // name_res and downstream passes see the binding set without tracing LetVal chains.
      let bound_names: Vec<BindNode> = arm_pending.iter().filter_map(|p| match p {
        Pending::MatchBind { name, .. } => Some(name.clone()),
        _ => None,
      }).collect();

      // Allocate block_cont_bind — last arg of body cont, called by arm body with result.
      let block_cont_bind = g.bind(Bind::Cont, None);
      let block_cont_id   = block_cont_bind.id;

      // Lower arm body with block_cont as the current cont so lower_seq terminates
      // by calling it rather than the outer ·ƒ_0.
      let prev_cont = g.cont;
      g.cont = block_cont_id;
      let arm_tail = lower_seq(g, &body.items);
      g.cont = prev_cont;

      // Body cont: [bound_names..., block_cont_bind]
      let mut body_args = bound_names;
      body_args.push(block_cont_bind);
      let arm_body = Cont::Expr { args: body_args, body: Box::new(arm_tail) };

      // Matcher cont: [scrutinee_params..., fail_bind, succ_bind]
      // Pattern chain uses fail_bind.id for failure, succ_bind.id for success.
      let matcher_body = if arm_pending.is_empty() {
        // Wildcard / no checks — matcher immediately jumps to succ.
        // Empty App to succ cont — renders as `·apply ·ƒ_succ`.
        let succ_val = g.val(ValKind::ContRef(succ_id), origin);
        g.expr(ExprKind::App {
          func: Callable::Val(succ_val),
          args: vec![],
        }, origin)
      } else {
        wrap_with_fail(g, arm_pending, Cont::Ref(succ_id), Some(fail_bind.id))
      };
      let mut matcher_args = matcher_scrutinee_params;
      matcher_args.push(fail_bind);
      matcher_args.push(succ_bind);
      let matcher = Cont::Expr { args: matcher_args, body: Box::new(matcher_body) };

      let result = g.fresh_result(origin);
      let (result_kind, result_id) = (result.kind, result.id);
      let pending = vec![Pending::MatchArm { matcher, body: arm_body, result, origin }];
      (ref_val(g, result_kind, result_id, origin), pending)
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
) -> Lower<'src> {
  let block_fn_name = g.fresh_fn(origin);
  let mut param_names = extract_params(g, params);
  let (cont, prev_cont) = g.push_cont(origin);
  let fn_body = lower_seq(g, body);
  g.pop_cont(prev_cont);
  param_names.push(Param::Name(cont));
  let (name_val, mut pending) = lower(g, name);
  let block_fn_val = ref_val(g, block_fn_name.kind, block_fn_name.id, origin);
  pending.push(Pending::Fn { name: block_fn_name, params: param_names, fn_body, origin });
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
enum Pending<'src> {
  Val { name: BindNode, val: Val<'src>, origin: Option<AstId> },
  Fn { name: BindNode, params: Vec<Param>, fn_body: Expr<'src>, origin: Option<AstId> },
  App { func: Callable<'src>, args: Vec<Arg<'src>>, result: BindNode, origin: Option<AstId> },
  MatchBlock { params: Vec<Val<'src>>, arm_vals: Vec<Val<'src>>, result: BindNode, origin: Option<AstId> },
  MatchArm { matcher: Cont<'src>, body: Cont<'src>, result: BindNode, origin: Option<AstId> },
  /// Pattern-lowered bind — emits MatchLetVal with ·panic as fail cont.
  MatchBind { name: BindNode, val: Val<'src>, origin: Option<AstId> },
  /// Pattern-lowered guard check — emits MatchIf with ·panic as fail cont.
  MatchGuard { func: Callable<'src>, args: Vec<Val<'src>>, origin: Option<AstId> },
  /// Literal equality check — emits MatchValue with ·panic as fail cont.
  MatchValue { val: Val<'src>, lit: Lit<'src>, origin: Option<AstId> },
  /// Seq pattern entry — emits MatchSeq with ·panic as fail cont.
  /// `cursor` is the outgoing cursor bind (arg 0 of the body cont).
  MatchSeq { val: Val<'src>, cursor: BindNode, origin: Option<AstId> },
  /// Pop head from seq — emits MatchNext with ·panic as fail cont.
  /// `elem` = extracted element bind (cont arg 0), `next_cursor` = advanced cursor bind (cont arg 1).
  MatchNext { val: Val<'src>, elem: BindNode, next_cursor: BindNode, origin: Option<AstId> },
  /// Seq pattern exhaustion — emits MatchDone with ·panic as fail cont.
  MatchDone { val: Val<'src>, result: BindNode, origin: Option<AstId> },
  /// Assert cursor non-empty — emits MatchNotDone with ·panic as fail cont.
  MatchNotDone { val: Val<'src>, origin: Option<AstId> },
  /// Bind remaining elements — emits MatchRest with ·panic as fail cont.
  MatchRest { val: Val<'src>, result: BindNode, origin: Option<AstId> },
  /// Rec pattern entry — emits MatchRec with ·panic as fail cont.
  /// `cursor` is the outgoing cursor bind (arg 0 of the body cont).
  MatchRec { val: Val<'src>, cursor: BindNode, origin: Option<AstId> },
  /// Extract named field from rec — emits MatchField with ·panic as fail cont.
  /// `elem` = field value bind (cont arg 0), `next_cursor` = advanced cursor bind (cont arg 1).
  MatchField { val: Val<'src>, field: &'src str, elem: BindNode, next_cursor: BindNode, origin: Option<AstId> },
  /// Yield — suspend execution, yield a value; result bound in continuation.
  Yield { value: Val<'src>, result: BindNode, origin: Option<AstId> },
}

impl<'src> Pending<'src> {
  fn origin(&self) -> Option<AstId> {
    match self {
      Pending::Val { origin, .. } | Pending::Fn { origin, .. } | Pending::App { origin, .. }
      | Pending::MatchBlock { origin, .. } | Pending::MatchArm { origin, .. } | Pending::MatchBind { origin, .. }
      | Pending::MatchGuard { origin, .. } | Pending::MatchValue { origin, .. }
      | Pending::MatchSeq { origin, .. } | Pending::MatchNext { origin, .. }
      | Pending::MatchDone { origin, .. } | Pending::MatchNotDone { origin, .. }
      | Pending::MatchRest { origin, .. } | Pending::MatchRec { origin, .. }
      | Pending::MatchField { origin, .. } | Pending::Yield { origin, .. } => *origin,
    }
  }
}

/// For `cont:`-typed pending items (App, MatchNext, etc.): when the current item is at
/// the leaf (`Cont::Ref`), use it directly; when non-leaf, wrap the inner body with the
/// pre-allocated `result` bind node.
fn cont_with_result<'src>(cont: Cont<'src>, result: BindNode) -> Cont<'src> {
  match cont {
    Cont::Ref(_) => cont,
    Cont::Expr { body, .. } => Cont::Expr { args: vec![result], body },
  }
}

fn cont_with_two_results<'src>(cont: Cont<'src>, first: BindNode, second: BindNode) -> Cont<'src> {
  match cont {
    Cont::Ref(_) => cont,
    Cont::Expr { body, .. } => Cont::Expr { args: vec![first, second], body },
  }
}

fn wrap<'src>(g: &mut Gen, bindings: Vec<Pending<'src>>, tail: Cont<'src>) -> Expr<'src> {
  wrap_with_fail(g, bindings, tail, None)
}

/// Like `wrap`, but with an explicit fail cont.
/// `fail_id`: `None` → emit `·panic`; `Some(id)` → emit a call to that cont.
/// Used for arm matchers inside a MatchBlock where `fail_id` is the matcher's fail param.
/// `tail` is the continuation for the innermost (last) binding.
/// Each non-leaf binding gets `Cont::Expr { args: vec![fresh], body: Box::new(next_expr) }`.
fn wrap_with_fail<'src>(
  g: &mut Gen,
  bindings: Vec<Pending<'src>>,
  tail: Cont<'src>,
  fail_id: Option<CpsId>,
) -> Expr<'src> {
  // Fold right-to-left. The accumulator starts as `tail: Cont` and becomes
  // `Expr` after the first (innermost) pending item is processed.
  // We use an enum to track whether we have a Cont (leaf) or Expr (non-leaf).
  enum Acc<'s> {
    Tail(Cont<'s>),
    Expr(Expr<'s>),
  }
  let make_fail_val = |g: &mut Gen, origin: Option<AstId>| -> Val<'src> {
    match fail_id {
      None     => g.val(ValKind::Panic, origin),
      Some(id) => g.val(ValKind::ContRef(id), origin),
    }
  };
  let acc = bindings.into_iter().rev().fold(Acc::Tail(tail), |acc, pending| {
    let cont: Cont<'src> = match acc {
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
      Pending::Fn { name, params, fn_body, origin } => g.expr(
        ExprKind::LetFn {
          name,
          params,
          fn_body: Box::new(fn_body),
          cont,
        },
        origin,
      ),
      Pending::App { func, args, result, origin } => g.expr(
        ExprKind::App { func, args: { let mut a = args; a.push(Arg::Cont(cont_with_result(cont, result))); a } },
        origin,
      ),
      Pending::MatchArm { matcher, body, result, origin } => {
        g.expr(
          ExprKind::App {
            func: Callable::BuiltIn(BuiltIn::MatchArm),
            args: vec![Arg::Cont(matcher), Arg::Cont(body), Arg::Cont(cont_with_result(cont, result))],
          },
          origin,
        )
      },
      Pending::MatchBlock { params, arm_vals, result, origin } => {
        let mut mb_args: Vec<Arg<'src>> = params.into_iter().map(Arg::Val).collect();
        mb_args.extend(arm_vals.into_iter().map(Arg::Val));
        mb_args.push(Arg::Cont(cont_with_result(cont, result)));
        g.expr(
          ExprKind::App { func: Callable::BuiltIn(BuiltIn::MatchBlock), args: mb_args },
          origin,
        )
      },
      Pending::MatchBind { name, val, origin } => {
        // MatchLetVal → plain LetVal (fail is always Panic for irrefutable binds)
        g.expr(
          ExprKind::LetVal { name, val: Box::new(val), cont },
          origin,
        )
      },
      Pending::MatchGuard { func, args, origin } => {
        let fail_val = make_fail_val(g, origin);
        let func_val = match func {
          Callable::Val(v) => v,
          Callable::BuiltIn(op) => g.val(ValKind::BuiltIn(op), origin),
        };
        let mut new_args: Vec<Arg<'src>> = vec![Arg::Val(func_val)];
        new_args.extend(args.into_iter().map(Arg::Val));
        new_args.push(Arg::Val(fail_val));
        new_args.push(Arg::Cont(cont));
        g.expr(
          ExprKind::App { func: Callable::BuiltIn(BuiltIn::MatchIf), args: new_args },
          origin,
        )
      },
      Pending::MatchValue { val, lit, origin } => {
        let fail_val = make_fail_val(g, origin);
        let lit_val = g.val(ValKind::Lit(lit), origin);
        g.expr(
          ExprKind::App {
            func: Callable::BuiltIn(BuiltIn::MatchValue),
            args: vec![Arg::Val(val), Arg::Val(lit_val), Arg::Val(fail_val), Arg::Cont(cont)],
          },
          origin,
        )
      },
      Pending::MatchSeq { val, cursor, origin } => {
        let fail_val = make_fail_val(g, origin);
        let body = match cont {
          Cont::Ref(_) => cont,
          Cont::Expr { body, .. } => Cont::Expr { args: vec![cursor], body },
        };
        g.expr(
          ExprKind::App {
            func: Callable::BuiltIn(BuiltIn::MatchSeq),
            args: vec![Arg::Val(val), Arg::Val(fail_val), Arg::Cont(body)],
          },
          origin,
        )
      },
      Pending::MatchNext { val, elem, next_cursor, origin } => {
        let fail_val = make_fail_val(g, origin);
        g.expr(
          ExprKind::App {
            func: Callable::BuiltIn(BuiltIn::MatchNext),
            args: vec![Arg::Val(val), Arg::Val(fail_val), Arg::Cont(cont_with_two_results(cont, elem, next_cursor))],
          },
          origin,
        )
      },
      Pending::MatchDone { val, result, origin } => {
        let fail_val = make_fail_val(g, origin);
        g.expr(
          ExprKind::App {
            func: Callable::BuiltIn(BuiltIn::MatchDone),
            args: vec![Arg::Val(val), Arg::Val(fail_val), Arg::Cont(cont_with_result(cont, result))],
          },
          origin,
        )
      },
      Pending::MatchNotDone { val, origin } => {
        let fail_val = make_fail_val(g, origin);
        g.expr(
          ExprKind::App {
            func: Callable::BuiltIn(BuiltIn::MatchNotDone),
            args: vec![Arg::Val(val), Arg::Val(fail_val), Arg::Cont(cont)],
          },
          origin,
        )
      },
      Pending::MatchRest { val, result, origin } => {
        let fail_val = make_fail_val(g, origin);
        g.expr(
          ExprKind::App {
            func: Callable::BuiltIn(BuiltIn::MatchRest),
            args: vec![Arg::Val(val), Arg::Val(fail_val), Arg::Cont(cont_with_result(cont, result))],
          },
          origin,
        )
      },
      Pending::MatchRec { val, cursor, origin } => {
        let fail_val = make_fail_val(g, origin);
        let body = match cont {
          Cont::Ref(_) => cont,
          Cont::Expr { body, .. } => Cont::Expr { args: vec![cursor], body },
        };
        g.expr(
          ExprKind::App {
            func: Callable::BuiltIn(BuiltIn::MatchRec),
            args: vec![Arg::Val(val), Arg::Val(fail_val), Arg::Cont(body)],
          },
          origin,
        )
      },
      Pending::MatchField { val, field, elem, next_cursor, origin } => {
        let fail_val = make_fail_val(g, origin);
        let field_val = g.val(ValKind::Lit(Lit::Str(field)), origin);
        g.expr(
          ExprKind::App {
            func: Callable::BuiltIn(BuiltIn::MatchField),
            args: vec![Arg::Val(val), Arg::Val(field_val), Arg::Val(fail_val), Arg::Cont(cont_with_two_results(cont, elem, next_cursor))],
          },
          origin,
        )
      },
      Pending::Yield { value, result, origin } => g.expr(
        ExprKind::App {
          func: Callable::BuiltIn(BuiltIn::Yield),
          args: vec![Arg::Val(value), Arg::Cont(cont_with_result(cont, result))],
        },
        origin,
      ),
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
  s.replace('_', "").parse().unwrap_or(0)
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

pub fn lower_module<'src>(exprs: &'src [Node<'src>], scope: &ScopeResult) -> CpsResult<'src> {
  let mut g = Gen::new(scope);
  if exprs.is_empty() {
    // Empty module: export nothing.
    let root = g.expr(ExprKind::App { func: Callable::BuiltIn(BuiltIn::Export), args: vec![] }, None);
    return CpsResult { root, origin: g.origin, bind_to_cps: g.bind_to_cps, synth_alias: crate::propgraph::PropGraph::new() };
  }

  // Collect simple top-level exports before lowering (bind_site_to_cps is populated at Gen::new).
  let export_ids: Vec<CpsId> = collect_module_exports(exprs, &g.bind_site_to_cps);

  // Build the terminal App: ·export ·export_0, ·export_1, ...
  let export_vals: Vec<Val<'src>> = export_ids.iter().map(|&cps_id| {
    let origin = g.origin.try_get(cps_id).and_then(|o| *o);
    g.val(ValKind::Ref(Ref::Synth(cps_id)), origin)
  }).collect();
  let export_args: Vec<Arg<'src>> = export_vals.into_iter().map(Arg::Val).collect();
  let terminal = g.expr(ExprKind::App {
    func: Callable::BuiltIn(BuiltIn::Export),
    args: export_args,
  }, None);

  // Lower the module body, using the exports terminal as the tail.
  let tail = Cont::Expr { args: vec![], body: Box::new(terminal) };
  let root = lower_seq_with_tail(&mut g, exprs, tail);
  CpsResult { root, origin: g.origin, bind_to_cps: g.bind_to_cps, synth_alias: crate::propgraph::PropGraph::new() }
}

/// Lower a single expression node (or a Module root) to CPS IR.
pub fn lower_expr<'src>(node: &'src Node<'src>, scope: &ScopeResult) -> CpsResult<'src> {
  let mut g = Gen::new(scope);
  let (val, pending) = lower(&mut g, node);
  let cont = g.cont;
  let root = if pending.is_empty() {
    wrap_val(&mut g, val, Some(node.id))
  } else {
    wrap(&mut g, pending, Cont::Ref(cont))
  };
  CpsResult { root, origin: g.origin, bind_to_cps: g.bind_to_cps, synth_alias: crate::propgraph::PropGraph::new() }
}

/// Recursively lower a pattern lhs node, appending Match* pending entries.
/// `val` is the scrutinee already lowered from the rhs.
/// Returns the Bind of the primary binding (used by the caller to construct Ret).
///
/// Implemented: Ident, Wildcard, BindRight, InfixOp (guard + range), Apply (→ MatchGuard/MatchIf),
///              LitInt/Float/Bool/Str (→ MatchValue), LitSeq (plain elems + Spread tail),
///              LitRec (fields + spread variants), Range (→ lower_range + MatchGuard w/ ·op_in).
/// TODO: Apply → MatchApp (after name resolution distinguishes predicate from constructor).
/// TODO(future): StrTempl pattern matching — e.g. `'hello ${name}'` in pattern position;
///               deferred, needs a string-matching primitive (·match_str_prefix or similar).
fn lower_pat_lhs<'src>(
  g: &mut Gen,
  lhs: &'src Node<'src>,
  val: Val<'src>,
  origin: Option<AstId>,
  pending: &mut Vec<Pending<'src>>,
) -> (Bind, CpsId) {
  match &lhs.kind {
    // Plain bind: `x = foo`
    NodeKind::Ident(_) => {
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
    // Evaluates the range as a value, then guards with `·op_in`.
    // Returns val's ref kind as a bind — range is a pure guard, no new binding allocated.
    NodeKind::InfixOp { op, lhs: start, rhs: end } if matches!(op.src, ".." | "...") => {
      let (range_val, rp) = lower_range(g, op.src, start, end, origin);
      pending.extend(rp);
      pending.push(Pending::MatchGuard { func: Callable::BuiltIn(BuiltIn::In), args: vec![val.clone(), range_val],  origin });
      match &val.kind {
        ValKind::Ref(Ref::Synth(cps_id)) => (Bind::Synth, *cps_id),
        _ => { let r = g.fresh_result(origin); (r.kind, r.id) }
      }
    }

    // Guarded bind: `a > 0 = foo` or `a > 0 or a < 9 = foo`
    // The innermost ident is the binding; the infix is the guard.
    NodeKind::InfixOp { op, lhs: guard_lhs, rhs: guard_rhs } => {
      let bind_ast_id = extract_bind_ast_id(guard_lhs);
      let bind = g.bind_name(bind_ast_id);
      let r = (bind.kind, bind.id);
      pending.push(Pending::MatchBind { name: bind, val,  origin });
      let (lv, lp) = lower(g, guard_lhs);
      let (rv, rp) = lower(g, guard_rhs);
      pending.extend(lp);
      pending.extend(rp);
      pending.push(Pending::MatchGuard { func: Callable::BuiltIn(BuiltIn::from_builtin_str(op.src)), args: vec![lv, rv],  origin });
      r
    }

    // Predicate guard: `is_even y`, `Ok b`, `foo 2, a, 3`
    // In pattern position, Apply args are either:
    //   - Ident/Wildcard — sub-pattern: binds to or discards `val` (the seq element)
    //   - Anything else  — expression: lowered normally and passed as-is to the guard
    // Exactly one arg should be an Ident/Wildcard (the "binding slot"); others are
    // literal/value args. All are assembled in order as arguments to MatchGuard.
    NodeKind::Apply { func, args } => {
      let mut arg_vals: Vec<Val<'src>> = vec![];
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

    // Literal equality: `1`, `'hello'`, `true` — emits MatchValue; no binding produced.
    // Returns val itself (the scrutinee) as the "result" for the caller — it's a check, not a bind.
    NodeKind::LitInt(s) => {
      pending.push(Pending::MatchValue { val: val.clone(), lit: Lit::Int(parse_int(s)),  origin });
      // MatchValue has no result binding; return a fresh slot so the caller can still chain.
      { let r = g.fresh_result(origin); (r.kind, r.id) }
    }
    NodeKind::LitFloat(s) => {
      pending.push(Pending::MatchValue { val: val.clone(), lit: Lit::Float(parse_float(s)),  origin });
      { let r = g.fresh_result(origin); (r.kind, r.id) }
    }
    NodeKind::LitBool(b) => {
      pending.push(Pending::MatchValue { val: val.clone(), lit: Lit::Bool(*b),  origin });
      { let r = g.fresh_result(origin); (r.kind, r.id) }
    }
    NodeKind::LitStr { content: s, .. } => {
      pending.push(Pending::MatchValue { val: val.clone(), lit: Lit::Str(s),  origin });
      { let r = g.fresh_result(origin); (r.kind, r.id) }
    }

    // Seq pattern: `[] = foo`, `[a, b] = foo`, `[a, []] = foo`, `[head, ..tail] = foo`
    NodeKind::LitSeq { items: elems, .. } => {
      let seq_cursor = g.fresh_cursor(origin);
      pending.push(Pending::MatchSeq { val: val.clone(), cursor: seq_cursor.clone(), origin });
      let mut cur = seq_cursor;
      let mut spread_seen = false;
      for elem_node in elems.items.iter() {
        match &elem_node.kind {
          // Spread element: `..` (discard non-empty) or `..name` (bind rest)
          NodeKind::Spread { inner, .. } => {
            spread_seen = true;
            let cur_val = ref_val(g, cur.kind, cur.id, origin);
            match inner {
              None => {
                // `[..]` — assert non-empty, discard rest
                pending.push(Pending::MatchNotDone { val: cur_val, origin });
              }
              Some(name_node) => {
                // `[..rest]` — bind remaining elements
                let result = g.fresh_result(origin);
                let (result_kind, result_id) = (result.kind, result.id);
                pending.push(Pending::MatchRest { val: cur_val, result, origin });
                // Bind the rest value to the name
                if let NodeKind::Ident(_) = &name_node.kind {
                  let bind = g.bind_name(name_node.id);
                  let rest_val = ref_val(g, result_kind, result_id, origin);
                  pending.push(Pending::MatchBind { name: bind, val: rest_val, origin });
                }
              }
            }
            // Spread must be last — stop processing elements
            break;
          }
          // Regular element: extract head, recurse
          _ => {
            let elem = g.fresh_result(origin);
            let (elem_kind, elem_id) = (elem.kind, elem.id);
            let next = g.fresh_cursor(origin);
            let cur_val = ref_val(g, cur.kind, cur.id, origin);
            pending.push(Pending::MatchNext { val: cur_val, elem, next_cursor: next.clone(), origin });
            cur = next;
            let elem_val = ref_val(g, elem_kind, elem_id, origin);
            lower_pat_lhs(g, elem_node, elem_val, Some(elem_node.id), pending);
          }
        }
      }
      // Only emit MatchDone if no spread consumed the tail
      if spread_seen {
        { let r = g.fresh_result(origin); (r.kind, r.id) }  // placeholder return; no MatchDone
      } else {
        let result = g.fresh_result(origin);
        let (result_kind, result_id) = (result.kind, result.id);
        let cur_val = ref_val(g, cur.kind, cur.id, origin);
        pending.push(Pending::MatchDone { val: cur_val, result, origin });
        (result_kind, result_id)
      }
    }

    // Rec pattern: `{} = foo`, `{x, y} = point`, `{bar, ..rest} = foo`, `{bar, ..{}} = foo`
    // Mirrors LitSeq lowering: open cursor with MatchRec, extract fields with MatchField,
    // close with MatchDone (closed/exact) or leave open (partial/open rest).
    NodeKind::LitRec { items: fields, .. } => {
      let rec_cursor = g.fresh_cursor(origin);
      pending.push(Pending::MatchRec { val: val.clone(), cursor: rec_cursor.clone(), origin });
      let mut cur = rec_cursor;
      let mut _spread_seen = false;
      for field_node in fields.items.iter() {
        match &field_node.kind {
          // Spread element: `..` (discard non-empty), `..rest` (bind rest), `..{}` (exact close)
          NodeKind::Spread { inner, .. } => {
            _spread_seen = true;
            let cur_val = ref_val(g, cur.kind, cur.id, origin);
            match inner {
              None => {
                // `{..}` — assert non-empty, discard rest (open partial match)
                pending.push(Pending::MatchNotDone { val: cur_val, origin });
              }
              Some(inner_node) => match &inner_node.kind {
                // `{..rest}` — bind remaining fields as a record
                NodeKind::Ident(_) => {
                  let result = g.fresh_result(origin);
                  let (result_kind, result_id) = (result.kind, result.id);
                  pending.push(Pending::MatchRest { val: cur_val, result, origin });
                  let bind = g.bind_name(inner_node.id);
                  let rest_val = ref_val(g, result_kind, result_id, origin);
                  pending.push(Pending::MatchBind { name: bind, val: rest_val, origin });
                }
                // `{..{sub_pat}}` — bind rest then destructure as a rec sub-pattern
                NodeKind::LitRec { .. } => {
                  let result = g.fresh_result(origin);
                  let (result_kind, result_id) = (result.kind, result.id);
                  pending.push(Pending::MatchRest { val: cur_val, result, origin });
                  let rest_val = ref_val(g, result_kind, result_id, origin);
                  lower_pat_lhs(g, inner_node, rest_val, Some(inner_node.id), pending);
                }
                _ => {}
              }
            }
            break;
          }
          // `{x}` shorthand — extract field named x, bind to x
          NodeKind::Ident(name) => {
            let field_origin = Some(field_node.id);
            let elem = g.fresh_result(origin);
            let (elem_kind, elem_id) = (elem.kind, elem.id);
            let next = g.fresh_cursor(origin);
            let cur_val = ref_val(g, cur.kind, cur.id, origin);
            pending.push(Pending::MatchField { val: cur_val, field: name, elem, next_cursor: next.clone(), origin: field_origin });
            cur = next;
            let bind = g.bind_name(field_node.id);
            let elem_val = ref_val(g, elem_kind, elem_id, origin);
            pending.push(Pending::MatchBind { name: bind, val: elem_val, origin });
          }
          // `{x: pat}` — extract field x, lower pat against extracted val
          // Parsed as Bind { lhs: Ident(key), rhs: pat } or Arm { lhs: [Ident(key)], body: [pat] }
          NodeKind::Bind { lhs, rhs: pat_node, .. } => {
            if let NodeKind::Ident(key) = &lhs.kind {
              let field_origin = Some(lhs.id);
              let elem = g.fresh_result(origin);
              let (elem_kind, elem_id) = (elem.kind, elem.id);
              let next = g.fresh_cursor(origin);
              let cur_val = ref_val(g, cur.kind, cur.id, origin);
              pending.push(Pending::MatchField { val: cur_val, field: key, elem, next_cursor: next.clone(), origin: field_origin });
              cur = next;
              let elem_val = ref_val(g, elem_kind, elem_id, origin);
              lower_pat_lhs(g, pat_node, elem_val, Some(pat_node.id), pending);
            }
          }
          NodeKind::Arm { lhs: arm_lhs, body: arm_body, .. } => {
            if let NodeKind::Ident(key) = &arm_lhs.kind
              && let Some(pat_node) = arm_body.items.last() {
                let field_origin = Some(arm_lhs.id);
                let elem = g.fresh_result(origin);
                let (elem_kind, elem_id) = (elem.kind, elem.id);
                let next = g.fresh_cursor(origin);
                let cur_val = ref_val(g, cur.kind, cur.id, origin);
                pending.push(Pending::MatchField { val: cur_val, field: key, elem, next_cursor: next.clone(), origin: field_origin });
                cur = next;
                let elem_val = ref_val(g, elem_kind, elem_id, origin);
                lower_pat_lhs(g, pat_node, elem_val, Some(pat_node.id), pending);
            }
          }
          _ => {}
        }
      }
      // Emit MatchDone only for `{}` (exact empty match). All other rec patterns
      // are structurally partial — records match even when extra fields are present.
      // Spread-terminated patterns (`..`, `..rest`, `..{}`) also omit MatchDone.
      if fields.items.is_empty() {
        let result = g.fresh_result(origin);
        let (result_kind, result_id) = (result.kind, result.id);
        let cur_val = ref_val(g, cur.kind, cur.id, origin);
        pending.push(Pending::MatchDone { val: cur_val, result, origin });
        (result_kind, result_id)
      } else {
        { let r = g.fresh_result(origin); (r.kind, r.id) }  // partial match — no cursor exhaustion check
      }
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

    // StrTempl in pattern position is deferred to a future version.
    // It needs a dedicated string-matching primitive (e.g. ·match_str_prefix) not yet designed.
    NodeKind::StrTempl { .. } => todo!("lower_pat_lhs: StrTempl pattern matching not yet implemented"),

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
mod tests {
  use super::*;
  use crate::parser::parse;
  use crate::passes::cps::ir::ExprKind;
  use crate::passes::scopes;

  fn lower_src(src: &str) -> CpsResult<'static> {
    let src: &'static str = Box::leak(src.to_string().into_boxed_str());
    let r: &'static crate::ast::ParseResult<'static> =
      Box::leak(Box::new(parse(src).expect("parse failed")));
    let scope = scopes::analyse(&r.root, r.node_count as usize, &[]);
    lower_expr(&r.root, &scope)
  }

  #[test]
  fn lower_lit_int() {
    let result = lower_src("42");
    // wrap_val now emits App(ContRef, [Lit]) directly — no LetVal wrapper.
    assert!(matches!(result.root.kind, ExprKind::App { .. }));
    if let ExprKind::App { args, .. } = &result.root.kind {
      assert!(matches!(args[0], Arg::Val(Val { kind: ValKind::Lit(Lit::Int(42)), .. })));
    }
  }

  #[test]
  fn lower_ident() {
    let result = lower_src("foo");
    // wrap_val now emits App(ContRef, [Ref]) directly — no LetVal wrapper.
    assert!(matches!(result.root.kind, ExprKind::App { .. }));
    if let ExprKind::App { args, .. } = &result.root.kind {
      assert!(matches!(args[0], Arg::Val(Val { kind: ValKind::Ref(_), .. })));
    }
  }

  #[test]
  fn lower_apply_simple() {
    let result = lower_src("foo bar");
    assert!(matches!(result.root.kind, ExprKind::App { .. }));
  }
}

#[cfg(test)]
mod cps_tests {
  use crate::parser::parse;
  use crate::ast::build_index;
  use crate::passes::cps::fmt::{fmt_with, Ctx};
  use crate::passes::scopes;
  use super::lower_expr;

  fn cps_expr(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let scope = scopes::analyse(&r.root, r.node_count as usize, &[]);
        let cps = lower_expr(&r.root, &scope);
        let ctx = Ctx { origin: &cps.origin, ast_index: &ast_index, captures: None };
        fmt_with(&cps.root, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  test_macros::include_fink_tests!("src/passes/cps/test_cps.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_cps_yield.fnk");
}

#[cfg(test)]
mod pat_tests {
  use crate::parser::parse;
  use crate::ast::build_index;
  use crate::passes::cps::fmt::{fmt_with, Ctx};
  use crate::passes::scopes;
  use super::lower_expr;

  fn cps_expr(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let scope = scopes::analyse(&r.root, r.node_count as usize, &[]);
        let cps = lower_expr(&r.root, &scope);
        let ctx = Ctx { origin: &cps.origin, ast_index: &ast_index, captures: None };
        fmt_with(&cps.root, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  test_macros::include_fink_tests!("src/passes/cps/test_cps_patterns.fnk");
}

#[cfg(test)]
mod module_tests {
  use crate::parser::parse;
  use crate::ast::{build_index, NodeKind};
  use crate::passes::cps::fmt::{fmt_with, Ctx};
  use crate::passes::scopes;
  use super::lower_module;

  fn cps_module(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let exprs = match &r.root.kind {
          NodeKind::Module(exprs) => exprs.items.as_slice(),
          _ => std::slice::from_ref(&r.root),
        };
        let scope = scopes::analyse(&r.root, r.node_count as usize, &["import"]);
        let cps = lower_module(exprs, &scope);
        let ctx = Ctx { origin: &cps.origin, ast_index: &ast_index, captures: None };
        fmt_with(&cps.root, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  test_macros::include_fink_tests!("src/passes/cps/test_cps_module.fnk");
}
