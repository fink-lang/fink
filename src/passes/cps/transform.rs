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

use crate::ast::{Ast, AstId, CmpPart, Node, NodeKind};
use crate::propgraph::PropGraph;
use crate::passes::scopes::{BindId, BindInfo, BindOrigin, ScopeResult};
use super::ir::{
  Arg, Bind, BindNode, BuiltIn, Callable, Cont, ContKind, CpsFnKind, CpsId, CpsResult,
  Expr, ExprKind, Ref, Lit, Param, Val, ValKind,
};

// ---------------------------------------------------------------------------
// Node allocator
// ---------------------------------------------------------------------------

pub struct Gen<'scope, 'src> {
  /// The flat AST being lowered. Threaded as a field so every helper can
  /// look up nodes via `g.node(id)` without re-threading the
  /// `ast: &Ast` parameter through every call site.
  ast: &'scope Ast<'src>,
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

impl<'scope, 'src> Gen<'scope, 'src> {
  pub fn new(ast: &'scope Ast<'src>, scope: &'scope ScopeResult) -> Self {
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
    // Anchor it to the module body's last statement: the `·ƒret_N` token
    // represents "pass this value back to the caller", and the value in
    // question is whatever the last expression evaluates to. Fall back
    // to the Module node itself if the body is empty.
    let ret_origin = match &ast.nodes.get(ast.root).kind {
      NodeKind::Module { exprs, .. } => exprs.items.last().copied().unwrap_or(ast.root),
      _ => ast.root,
    };
    let cont_id: CpsId = origin.push(Some(ret_origin));
    Gen { ast, origin, bind_to_cps, bind_site_to_cps, resolution: &scope.resolution, binds: &scope.binds, cont: cont_id }
  }

  /// Look up a node in the AST.
  fn node(&self, id: AstId) -> &Node<'src> {
    self.ast.nodes.get(id)
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
///
/// Both the ContRef val and the App expr are synthesised here; their
/// origin is the incoming `origin` (the expression whose value is being
/// returned). That way source maps see the `·ƒret_N val` call anchored
/// to the returned expression, not to the cont's declaration site.
fn tail_app(g: &mut Gen, cont_id: CpsId, val: Val, origin: Option<AstId>) -> Expr {
  let cont_val = g.val(ValKind::ContRef(cont_id), origin);
  g.expr(ExprKind::App {
    func: Callable::Val(cont_val),
    args: vec![Arg::Val(val)],
  }, origin)
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

fn lower(g: &mut Gen, id: AstId) -> Lower {
  let o = Some(id);
  // Clone the kind to drop the ast borrow before recursive calls — same
  // pattern as scopes/partial. NodeKind clone is cheap (children are Copy
  // AstIds, tokens are Copy).
  let kind = g.node(id).kind.clone();
  match kind {
    // ---- literals ----
    NodeKind::LitBool(b) => (lit_val(g, Lit::Bool(b), o), vec![]),
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
    NodeKind::LitStr { content: s, .. } => (lit_val(g, Lit::Str(crate::strings::render(&s)), o), vec![]),

    // ---- identifier reference — resolved via scope analysis ----
    NodeKind::Ident(_) | NodeKind::SynthIdent(_) => (scope_ref_val(g, id), vec![]),

    // ---- wildcard ----
    NodeKind::Wildcard => (scope_ref_val(g, id), vec![]),

    // ---- group ----
    // A plain group `(expr)` is transparent.
    // A block group `(stmt; stmt)` parses to `Group(Fn { params: Patterns([]), body })` —
    // a zero-param closure that must be immediately invoked to produce a value.
    NodeKind::Group { inner, .. } => {
      let inner_kind = g.node(inner).kind.clone();
      match inner_kind {
        NodeKind::Fn { params, body, .. }
          if matches!(&g.node(params).kind, NodeKind::Patterns(ps) if ps.items.is_empty()) =>
        {
          let body_items: Vec<AstId> = body.items.to_vec();
          lower_iife(g, params, &body_items, o)
        }
        _ => lower(g, inner),
      }
    }

    // ---- try: lower transparently for now ----
    NodeKind::Try(inner) => lower(g, inner),

    // ---- bind: `name = rhs` ----
    NodeKind::Bind { lhs, rhs, .. } => lower_bind(g, lhs, rhs, o),

    // ---- bind-right: `rhs |= lhs` (swap) ----
    NodeKind::BindRight { lhs, rhs, .. } => lower_bind(g, rhs, lhs, o),

    // ---- fn: `fn params: body` ----
    NodeKind::Fn { params, body, .. } => {
      let body_items: Vec<AstId> = body.items.to_vec();
      lower_fn(g, params, &body_items, o)
    }

    // ---- apply: `func arg1 arg2` ----
    NodeKind::Apply { func, args } => {
      let arg_items: Vec<AstId> = args.items.to_vec();
      lower_apply(g, func, &arg_items, o)
    }

    // ---- pipe: `a | b | c` == `c (b a)` ----
    NodeKind::Pipe(stages) => {
      let stage_items: Vec<AstId> = stages.items.to_vec();
      lower_pipe(g, &stage_items, o)
    }

    // ---- infix op: `a + b` ----
    NodeKind::InfixOp { op, lhs, rhs } => lower_infix(g, op.src, lhs, rhs, o),

    // ---- unary op: `-a`, `not a` ----
    NodeKind::UnaryOp { op, operand } => lower_unary(g, op.src, operand, o),

    // ---- chained cmp: `a < b < c` ----
    NodeKind::ChainedCmp(parts) => lower_chained_cmp(g, &parts, o),

    // ---- member access: `lhs.rhs` ----
    NodeKind::Member { lhs, rhs, .. } => lower_member(g, lhs, rhs, o),

    // ---- sequence literal ----
    NodeKind::LitSeq { items: elems, .. } => {
      let items: Vec<AstId> = elems.items.to_vec();
      lower_lit_seq(g, &items, o)
    }

    // ---- record literal ----
    NodeKind::LitRec { items: fields, .. } => {
      let items: Vec<AstId> = fields.items.to_vec();
      lower_lit_rec(g, &items, o)
    }

    // ---- string template ----
    NodeKind::StrTempl { children: parts, .. } => {
      let parts: Vec<AstId> = parts.to_vec();
      lower_str_templ(g, &parts, o)
    }

    // ---- raw string template (tagged) ----
    NodeKind::StrRawTempl { children: parts, .. } => {
      let parts: Vec<AstId> = parts.to_vec();
      lower_str_raw_templ(g, &parts, o)
    }

    // ---- match ----
    NodeKind::Match { subjects, arms, .. } => {
      let subjs: Vec<AstId> = subjects.items.to_vec();
      let arm_items: Vec<AstId> = arms.items.to_vec();
      lower_match(g, &subjs, &arm_items, o)
    }

    // ---- block: `name params: body` ----
    NodeKind::Block { name, params, body, .. } => {
      let body_items: Vec<AstId> = body.items.to_vec();
      lower_block(g, name, params, &body_items, o)
    }

    // ---- module: single expression unwrapped; multiple as zero-param function ----
    NodeKind::Module { exprs, .. } if exprs.items.len() == 1 => lower(g, exprs.items[0]),
    NodeKind::Module { exprs, .. } => {
      let items: Vec<AstId> = exprs.items.to_vec();
      lower_module_as_fn(g, &items, o)
    }

    // ---- should not appear post-partial-pass ----
    NodeKind::Partial => panic!("Partial should be eliminated before CPS transform"),

    // ---- spread in expression position ----
    NodeKind::Spread { inner, .. } => {
      if let Some(inner_id) = inner {
        lower(g, inner_id)
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
fn lower_seq(g: &mut Gen, exprs: &[AstId]) -> Expr {
  lower_seq_with_tail(g, exprs, Cont::Ref(g.cont))
}

fn lower_seq_with_tail(g: &mut Gen, exprs: &[AstId], tail: Cont) -> Expr {
  assert!(!exprs.is_empty(), "empty expression sequence");
  let mut all_pending: Vec<Pending> = vec![];
  let n = exprs.len();
  for (i, &expr_id) in exprs.iter().enumerate() {
    let is_last = i + 1 == n;
    let o = Some(expr_id);
    if is_last {
      let (val, pending) = lower(g, expr_id);
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
      let kind = g.node(expr_id).kind.clone();
      match kind {
        // Bind introduces a name available in subsequent expressions.
        NodeKind::Bind { lhs, rhs, .. } | NodeKind::BindRight { rhs: lhs, lhs: rhs, .. } => {
          let pending = lower_bind_stmt(g, lhs, rhs, o);
          all_pending.extend(pending);
        }
        // Non-tail expression: evaluate, result discarded.
        _ => {
          let (val, pending) = lower(g, expr_id);
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
fn lower_bind_stmt(
  g: &mut Gen,
  lhs: AstId,
  rhs: AstId,
  origin: Option<AstId>,
) -> Vec<Pending> {
  let (val, mut pending) = lower(g, rhs);
  match &g.node(lhs).kind {
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
fn lower_bind(
  g: &mut Gen,
  lhs: AstId,
  rhs: AstId,
  origin: Option<AstId>,
) -> Lower {
  let (val, mut pending) = lower(g, rhs);
  match &g.node(lhs).kind {
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
      let lhs_kind = g.node(lhs).kind.clone();
      let result_origin = match lhs_kind {
        NodeKind::Ident(_) => Some(lhs),
        NodeKind::InfixOp { op, .. } if matches!(op.src, ".." | "...") => Some(rhs),
        NodeKind::InfixOp { .. } => Some(extract_bind_ast_id(g, lhs)),
        _ => origin,
      };
      (ref_val(g, bound_kind, bound_id, result_origin), pending)
    }
  }
}

// ---------------------------------------------------------------------------
// Function definition
// ---------------------------------------------------------------------------

fn lower_fn(
  g: &mut Gen,
  params: AstId,
  body: &[AstId],
  origin: Option<AstId>,
) -> Lower {
  let fn_name = g.fresh_fn(origin);
  let (fn_name_kind, fn_name_id) = (fn_name.kind, fn_name.id);
  let (mut param_names, deferred) = extract_params_with_gen(g, params);
  // The cont represents the return point of the function — semantically
  // anchored at the expression whose value flows into it (the last body
  // statement), not the whole fn node. Narrowing here makes hovering a
  // ·ƒret_N param declaration highlight the body's tail expression.
  let cont_origin = body.last().copied().map(Some).unwrap_or(origin);
  let (cont, prev_cont) = g.push_cont(cont_origin);
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
fn lower_module_as_fn(
  g: &mut Gen,
  body: &[AstId],
  origin: Option<AstId>,
) -> Lower {
  let fn_name = g.fresh_fn(origin);
  let (fn_name_kind, fn_name_id) = (fn_name.kind, fn_name.id);
  let cont_origin = body.last().copied().map(Some).unwrap_or(origin);
  let (cont, prev_cont) = g.push_cont(cont_origin);
  let fn_body = lower_seq(g, body);
  g.pop_cont(prev_cont);
  let pending = vec![Pending::Fn { name: fn_name, params: vec![Param::Name(cont)], fn_kind: CpsFnKind::CpsFunction, fn_body, origin }];
  (ref_val(g, fn_name_kind, fn_name_id, origin), pending)
}

/// Lower a block group `(expr; expr)` — immediately-invoked zero-param closure.
/// Defines the closure then emits an App that calls it right away.
fn lower_iife(
  g: &mut Gen,
  params: AstId,
  body: &[AstId],
  origin: Option<AstId>,
) -> Lower {
  let fn_name = g.fresh_fn(origin);
  let (mut param_names, deferred) = extract_params_with_gen(g, params);
  let cont_origin = body.last().copied().map(Some).unwrap_or(origin);
  let (cont, prev_cont) = g.push_cont(cont_origin);
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
fn extract_params_with_gen(
  g: &mut Gen,
  params: AstId,
) -> (Vec<Param>, Vec<Pending>) {
  let mut param_list = vec![];
  let mut deferred: Vec<Pending> = vec![];
  let params_kind = g.node(params).kind.clone();
  let nodes: Vec<AstId> = match params_kind {
    NodeKind::Patterns(ps) => ps.items.to_vec(),
    _ => vec![params],
  };
  for p in nodes {
    let p_kind = g.node(p).kind.clone();
    match p_kind {
      NodeKind::Ident(_) | NodeKind::SynthIdent(_) => param_list.push(Param::Name(g.bind_name(p))),
      NodeKind::Wildcard => param_list.push(Param::Name(g.bind(Bind::Synth, Some(p)))),
      NodeKind::Patterns(ps) => {
        for &inner in ps.items.iter() {
          param_list.push(Param::Name(g.bind_name(inner)));
        }
      }
      NodeKind::Spread { inner, .. } => {
        let bind = match inner {
          Some(inner_id) if matches!(g.node(inner_id).kind, NodeKind::Ident(_)) => g.bind_name(inner_id),
          _ => g.bind(Bind::Synth, Some(p)),
        };
        param_list.push(Param::Spread(bind));
      }
      // Complex destructuring param — desugar to a fresh plain param + pattern lowering in body.
      // The param receives a single value (not varargs); destructuring happens inside the fn.
      _ => {
        let param_name = g.fresh_result(Some(p));
        let (param_name_kind, param_name_id) = (param_name.kind, param_name.id);
        param_list.push(Param::Name(param_name));
        let param_val = ref_val(g, param_name_kind, param_name_id, Some(p));
        lower_pat_lhs(g, p, param_val, Some(p), &mut deferred);
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

fn extract_params(g: &mut Gen, params: AstId) -> Vec<Param> {
  let kind = g.node(params).kind.clone();
  match kind {
    NodeKind::Patterns(ps) => {
      let items: Vec<AstId> = ps.items.to_vec();
      items.into_iter().flat_map(|p| extract_param(g, p)).collect()
    }
    _ => extract_param(g, params),
  }
}

fn extract_param(g: &mut Gen, param: AstId) -> Vec<Param> {
  let origin = Some(param);
  let kind = g.node(param).kind.clone();
  match kind {
    NodeKind::Ident(_) | NodeKind::SynthIdent(_) => vec![Param::Name(g.bind_name(param))],
    NodeKind::Wildcard => vec![Param::Name(g.bind(Bind::Synth, origin))],
    NodeKind::Patterns(ps) => {
      let items: Vec<AstId> = ps.items.to_vec();
      items.into_iter().flat_map(|p| extract_param(g, p)).collect()
    }
    // `..rest` varargs param — trailing spread.
    NodeKind::Spread { inner, .. } => {
      let bind = match inner {
        Some(inner_id) if matches!(g.node(inner_id).kind, NodeKind::Ident(_)) => g.bind_name(inner_id),
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

fn lower_apply(
  g: &mut Gen,
  func: AstId,
  args: &[AstId],
  origin: Option<AstId>,
) -> Lower {
  let (func_val, mut pending) = lower(g, func);
  let mut arg_vals = vec![];
  for &arg in args {
    let arg_kind = g.node(arg).kind.clone();
    let is_spread = matches!(arg_kind, NodeKind::Spread { .. });
    let inner = if is_spread {
      if let NodeKind::Spread { inner: Some(inner), .. } = arg_kind { inner } else { arg }
    } else {
      arg
    };
    // Tagged template: build a list from raw parts and pass as spread arg.
    let inner_kind = g.node(inner).kind.clone();
    if let NodeKind::StrRawTempl { children, .. } = inner_kind {
      let mut acc = lit_val(g, Lit::Seq, origin);
      let parts: Vec<AstId> = children.to_vec();
      for part in parts.iter().rev() {
        let (pv, pp) = lower_str_part_raw(g, *part);
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
  // If the callable resolved to a compiler-known builtin (e.g. `import`,
  // `yield`, `spawn`, `channel`, ...), represent it as `Callable::BuiltIn`
  // rather than `Callable::Val`. This routes through the builtin
  // calling-convention branch in `wrap_with_fail` (cont-last, matching
  // the runtime-WAT convention these builtins require), instead of the
  // user convention (cont-first) that mangles val args when the result
  // is destructured.
  let func = match func_val.kind {
    ValKind::BuiltIn(op) => Callable::BuiltIn(op),
    _ => Callable::Val(func_val),
  };
  pending.push(Pending::App { func, args: arg_vals, result,  origin });
  (ref_val(g, result_kind, result_id, origin), pending)
}

// ---------------------------------------------------------------------------
// Pipe: `a | b | c` == `c (b a)`
// ---------------------------------------------------------------------------

fn lower_pipe(g: &mut Gen, stages: &[AstId], origin: Option<AstId>) -> Lower {
  assert!(!stages.is_empty(), "empty pipe");
  if stages.len() == 1 {
    return lower(g, stages[0]);
  }
  // Fold left: head | f | g → g (f head)
  let (mut acc_val, mut pending) = lower(g, stages[0]);
  for &stage in &stages[1..] {
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
  g: &mut Gen<'_, 'src>,
  op: &'src str,
  lhs: AstId,
  rhs: AstId,
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
  g: &mut Gen<'_, 'src>,
  op: &'src str,
  operand: AstId,
  origin: Option<AstId>,
) -> Lower {
  let (val, mut pending) = lower(g, operand);
  let result = g.fresh_result(origin);
  let (result_kind, result_id) = (result.kind, result.id);
  pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::from_builtin_str(op)), args: args_val(vec![val]), result,  origin });
  (ref_val(g, result_kind, result_id, origin), pending)
}

fn lower_chained_cmp<'src>(
  g: &mut Gen<'_, 'src>,
  parts: &[CmpPart<'src>],
  origin: Option<AstId>,
) -> Lower {
  // `a < b < c` → `(a < b) and (b < c)`
  // Walk parts: collect Operand/Op pairs and emit pairwise comparisons.
  let mut pending: Vec<Pending> = vec![];
  let mut operands: Vec<Val> = vec![];
  let mut ops: Vec<&'src str> = vec![];

  for part in parts {
    match part {
      CmpPart::Operand(node_id) => {
        let (val, p) = lower(g, *node_id);
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
  g: &mut Gen<'_, 'src>,
  op: &'src str,
  start: AstId,
  end: AstId,
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

fn lower_member(
  g: &mut Gen,
  lhs: AstId,
  rhs: AstId,
  origin: Option<AstId>,
) -> Lower {
  let (lv, mut pending) = lower(g, lhs);
  let rhs_kind = g.node(rhs).kind.clone();
  let rv = match rhs_kind {
    NodeKind::Ident(key) => lit_val(g, Lit::Str(key.as_bytes().to_vec()), Some(rhs)),
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
fn lower_lit_seq(g: &mut Gen, elems: &[AstId], origin: Option<AstId>) -> Lower {
  let mut acc = lit_val(g, Lit::Seq, origin);
  let mut pending: Vec<Pending> = vec![];
  for &elem in elems.iter().rev() {
    let elem_kind = g.node(elem).kind.clone();
    let is_spread = matches!(elem_kind, NodeKind::Spread { .. });
    let inner = if is_spread {
      if let NodeKind::Spread { inner: Some(inner_id), .. } = elem_kind { inner_id } else { elem }
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
    // Per-element origin so each ·seq_prepend / ·seq_concat maps to its
    // own element / spread node, not the whole literal.
    let elem_origin = Some(elem);
    let result = g.fresh_result(elem_origin);
    let (result_kind, result_id) = (result.kind, result.id);
    pending.push(Pending::App { func: Callable::BuiltIn(op), args, result, origin: elem_origin });
    acc = ref_val(g, result_kind, result_id, elem_origin);
  }
  (acc, pending)
}

// ---------------------------------------------------------------------------
// Record literal: `{a, b: v, ..c}`
// ---------------------------------------------------------------------------

fn lower_lit_rec(g: &mut Gen, fields: &[AstId], origin: Option<AstId>) -> Lower {
  let mut acc = lit_val(g, Lit::Rec, origin);
  let mut pending: Vec<Pending> = vec![];
  for &field in fields {
    // Per-field origin so each ·rec_put / ·rec_merge maps to its own
    // field / spread node, not the whole literal.
    let field_origin = Some(field);
    let field_kind = g.node(field).kind.clone();
    match field_kind {
      NodeKind::Spread { inner: Some(inner_id), .. } => {
        let (sv, sp) = lower(g, inner_id);
        pending.extend(sp);
        let result = g.fresh_result(field_origin);
        let (rk, ri) = (result.kind, result.id);
        pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::RecMerge), args: args_val(vec![acc, sv]), result, origin: field_origin });
        acc = ref_val(g, rk, ri, field_origin);
      }
      NodeKind::Bind { lhs, rhs, .. } => {
        let lhs_kind = g.node(lhs).kind.clone();
        if let NodeKind::Ident(key) = lhs_kind {
          let key_lit = lit_val(g, Lit::Str(key.as_bytes().to_vec()), Some(lhs));
          let (fv, fp) = lower(g, rhs);
          pending.extend(fp);
          let result = g.fresh_result(field_origin);
          let (rk, ri) = (result.kind, result.id);
          pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::RecPut), args: args_val(vec![acc, key_lit, fv]), result, origin: field_origin });
          acc = ref_val(g, rk, ri, field_origin);
        } else {
          // Computed key.
          let (kv, kp) = lower(g, lhs);
          let (fv, fp) = lower(g, rhs);
          pending.extend(kp);
          pending.extend(fp);
          let result = g.fresh_result(field_origin);
          let (rk, ri) = (result.kind, result.id);
          pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::RecPut), args: args_val(vec![acc, kv, fv]), result, origin: field_origin });
          acc = ref_val(g, rk, ri, field_origin);
        }
      }
      // `{foo: val}` parsed as Arm { lhs: Ident("foo"), body: [val] }
      NodeKind::Arm { lhs, body, .. } => {
        let key_id = lhs;
        let val_id = *body.items.last().expect("arm body empty");
        let key_kind = g.node(key_id).kind.clone();
        if let NodeKind::Ident(key) = key_kind {
          let key_lit = lit_val(g, Lit::Str(key.as_bytes().to_vec()), Some(key_id));
          let (fv, fp) = lower(g, val_id);
          pending.extend(fp);
          let result = g.fresh_result(field_origin);
          let (rk, ri) = (result.kind, result.id);
          pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::RecPut), args: args_val(vec![acc, key_lit, fv]), result, origin: field_origin });
          acc = ref_val(g, rk, ri, field_origin);
        } else {
          let (kv, kp) = lower(g, key_id);
          let (fv, fp) = lower(g, val_id);
          pending.extend(kp);
          pending.extend(fp);
          let result = g.fresh_result(field_origin);
          let (rk, ri) = (result.kind, result.id);
          pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::RecPut), args: args_val(vec![acc, kv, fv]), result, origin: field_origin });
          acc = ref_val(g, rk, ri, field_origin);
        }
      }
      NodeKind::Ident(name) => {
        // Shorthand `{foo}` == `{foo: foo}`
        let key_lit = lit_val(g, Lit::Str(name.as_bytes().to_vec()), Some(field));
        let id_val = scope_ref_val(g, field);
        let result = g.fresh_result(field_origin);
        let (rk, ri) = (result.kind, result.id);
        pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::RecPut), args: args_val(vec![acc, key_lit, id_val]), result, origin: field_origin });
        acc = ref_val(g, rk, ri, field_origin);
      }
      _ => {
        let (fv, fp) = lower(g, field);
        pending.extend(fp);
        let result = g.fresh_result(field_origin);
        let (rk, ri) = (result.kind, result.id);
        pending.push(Pending::App { func: Callable::BuiltIn(BuiltIn::RecMerge), args: args_val(vec![acc, fv]), result, origin: field_origin });
        acc = ref_val(g, rk, ri, field_origin);
      }
    }
  }
  (acc, pending)
}

// ---------------------------------------------------------------------------
// String template: `'hello ${name}'`
// ---------------------------------------------------------------------------

/// Lower a cooked string template part: LitStr segments go through
/// `strings::render` so escape sequences (\n \t \xNN \u{...}) become real
/// bytes, matching the plain LitStr path. Non-literal segments lower normally.
fn lower_str_part(g: &mut Gen, part: AstId) -> Lower {
  let kind = g.node(part).kind.clone();
  if let NodeKind::LitStr { content: s, .. } = kind {
    let o = Some(part);
    (lit_val(g, Lit::Str(crate::strings::render(&s)), o), vec![])
  } else {
    lower(g, part)
  }
}

/// Lower a raw string template part: LitStr segments stay as raw source
/// bytes. Used by tagged templates (`foo'...'`), where the tag is
/// responsible for any escape interpretation.
fn lower_str_part_raw(g: &mut Gen, part: AstId) -> Lower {
  let kind = g.node(part).kind.clone();
  if let NodeKind::LitStr { content: s, .. } = kind {
    let o = Some(part);
    (lit_val(g, Lit::Str(s.as_bytes().to_vec()), o), vec![])
  } else {
    lower(g, part)
  }
}

fn lower_str_templ(g: &mut Gen, parts: &[AstId], origin: Option<AstId>) -> Lower {
  let mut pending: Vec<Pending> = vec![];
  let mut part_vals: Vec<Arg> = vec![];
  for &part in parts {
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
fn lower_str_raw_templ(g: &mut Gen, parts: &[AstId], origin: Option<AstId>) -> Lower {
  // Single raw segment with no interpolation — return as a plain raw Lit::Str.
  if parts.len() == 1 {
    let (pv, pp) = lower_str_part_raw(g, parts[0]);
    return (pv, pp);
  }
  // Multiple segments (interpolation in raw template) — call StrFmt with raw parts.
  let mut pending: Vec<Pending> = vec![];
  let mut part_vals: Vec<Arg> = vec![];
  for &part in parts {
    let (pv, pp) = lower_str_part_raw(g, part);
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
fn lower_match(
  g: &mut Gen,
  subjects: &[AstId],
  arms: &[AstId],
  origin: Option<AstId>,
) -> Lower {
  let mut pending: Vec<Pending> = vec![];

  // Lower subject expressions.
  let subject_vals: Vec<Val> = subjects.iter().map(|&s| {
    let (v, sp) = lower(g, s);
    pending.extend(sp);
    v
  }).collect();

  // Lower each arm to its (mp_N, mb_N) components.
  let arm_cpss: Vec<ArmCps> = arms.iter().map(|&arm| lower_match_arm(g, arm, origin)).collect();

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
    // Initial: panic (runtime-backed builtin — traps via host_panic)
    g.expr(ExprKind::App { func: Callable::BuiltIn(BuiltIn::Panic), args: vec![] }, origin),
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
fn lower_match_arm(g: &mut Gen, arm: AstId, _origin: Option<AstId>) -> ArmCps {
  let arm_kind = g.node(arm).kind.clone();
  match arm_kind {
    NodeKind::Arm { lhs, body, .. } => {
      let arm_origin = Some(arm);
      let lhs_kind = g.node(lhs).kind.clone();
      let lhs_ids: Vec<AstId> = match lhs_kind {
        NodeKind::Patterns(ps) => ps.items.to_vec(),
        _ => vec![lhs],
      };

      // mp params: (succ, fail, subj_0, ...) — conts first.
      let mp_subj_params: Vec<BindNode> = lhs_ids.iter().map(|_| g.fresh_result(None)).collect();
      let mp_succ_param = g.bind(Bind::Cont(ContKind::Succ), None);
      let mp_succ_id = mp_succ_param.id;
      let mp_fail_param = g.bind(Bind::Cont(ContKind::Fail), None);
      let mp_fail_id = mp_fail_param.id;

      // Lower patterns against the mp scrutinee params.
      let mut arm_pending: Vec<Pending> = vec![];
      for (pat_id, param) in lhs_ids.iter().zip(mp_subj_params.iter()) {
        let pat_origin = Some(*pat_id);
        let scrutinee_val = ref_val(g, param.kind, param.id, pat_origin);
        lower_pat_lhs(g, *pat_id, scrutinee_val, pat_origin, &mut arm_pending);
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
      let body_items: Vec<AstId> = body.items.to_vec();
      let mb_body_expr = lower_seq(g, &body_items);
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

fn lower_block(
  g: &mut Gen,
  name: AstId,
  params: AstId,
  body: &[AstId],
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
  ///
  /// ```text
  ///   LetFn body = fn(bind_names...): <cont>
  ///   LetFn matcher = fn(subj, succ, fail): matcher_body
  ///   matcher(subject, body, panic)
  /// ```
  ///
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
      None     => g.val(ValKind::BuiltIn(BuiltIn::Panic), origin),
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
              g.val(ValKind::BuiltIn(BuiltIn::Panic), origin) // no binds — shouldn't reach here
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
/// Collect module-scope import declarations: url → [name, ...].
///
/// Matches the pattern `{foo, bar} = import './url'` at module scope.
/// Reads names directly from the AST LHS before CPS lowering, so the result
/// is stable even after lifting scatters the rec_pop chain into separate fns.
fn collect_module_imports(ast: &Ast<'_>, exprs: &[AstId]) -> std::collections::BTreeMap<String, Vec<String>> {
  use crate::passes::ast::NodeKind;
  let mut result: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
  for &expr_id in exprs {
    let NodeKind::Bind { lhs, rhs, .. } = &ast.nodes.get(expr_id).kind else { continue };
    let lhs = *lhs;
    let rhs = *rhs;
    // RHS must be `import 'url'`.
    let NodeKind::Apply { func, args } = &ast.nodes.get(rhs).kind else { continue };
    let func = *func;
    let arg_items: Vec<AstId> = args.items.to_vec();
    let NodeKind::Ident(name) = &ast.nodes.get(func).kind else { continue };
    if *name != "import" { continue; }
    let url = arg_items.iter().find_map(|&a| {
      if let NodeKind::LitStr { content, .. } = &ast.nodes.get(a).kind { Some(content.clone()) } else { None }
    });
    let Some(url) = url else { continue };
    // LHS must be a rec pattern `{foo, bar, ...}`.
    let NodeKind::LitRec { items, .. } = &ast.nodes.get(lhs).kind else { continue };
    let item_ids: Vec<AstId> = items.items.to_vec();
    let entry: &mut Vec<String> = result.entry(url).or_default();
    for item_id in item_ids {
      if let NodeKind::Ident(field) = &ast.nodes.get(item_id).kind {
        let s = field.to_string();
        if !entry.contains(&s) { entry.push(s); }
      }
    }
  }
  result
}

/// Collect the CpsIds of simple module-level exports: `name = <non-import expr>` where
/// lhs is a plain Ident. Pattern destructures and imports are excluded.
fn collect_module_exports(ast: &Ast<'_>, exprs: &[AstId], bind_site_to_cps: &std::collections::HashMap<u32, CpsId>) -> Vec<CpsId> {
  exprs.iter().filter_map(|&expr_id| {
    let NodeKind::Bind { lhs, rhs, .. } = &ast.nodes.get(expr_id).kind else { return None; };
    let lhs = *lhs;
    let rhs = *rhs;
    let NodeKind::Ident(_) = &ast.nodes.get(lhs).kind else { return None; };
    // Exclude imports: `{foo} = import './bar'` — rhs is Apply { func: Ident("import"), .. }
    if let NodeKind::Apply { func, .. } = &ast.nodes.get(rhs).kind {
      let func = *func;
      if let NodeKind::Ident(name) = &ast.nodes.get(func).kind
        && *name == "import" { return None; }
    }
    bind_site_to_cps.get(&lhs.0).copied()
  }).collect()
}

/// Collect every module-level binding leaf: `(cps_id, source_name)` pairs for
/// each Ident that appears as a binding site under a module-scope `Bind` LHS.
/// Includes destructure leaves (e.g. `x` from `{x} = {x: 42}`, `a`/`b` from
/// `[a, b] = ...`) and guard-pattern idents. Excludes import destructures.
///
/// Mirrors `scopes::pre_register_pattern_binds` — any Ident it would register
/// as a module-scope bind is a module local here.
fn collect_module_locals(
  ast: &Ast<'_>,
  exprs: &[AstId],
  bind_site_to_cps: &std::collections::HashMap<u32, CpsId>,
) -> Vec<(CpsId, String)> {
  let mut out = Vec::new();
  for &expr_id in exprs {
    let NodeKind::Bind { lhs, rhs, .. } = &ast.nodes.get(expr_id).kind else { continue; };
    let lhs = *lhs;
    let rhs = *rhs;
    // Exclude imports: `{foo} = import './bar'`.
    if let NodeKind::Apply { func, .. } = &ast.nodes.get(rhs).kind {
      let func = *func;
      if let NodeKind::Ident(name) = &ast.nodes.get(func).kind
        && *name == "import" { continue; }
    }
    walk_bind_lhs(ast, lhs, bind_site_to_cps, &mut out);
  }
  out
}

fn walk_bind_lhs(
  ast: &Ast<'_>,
  id: AstId,
  bind_site_to_cps: &std::collections::HashMap<u32, CpsId>,
  out: &mut Vec<(CpsId, String)>,
) {
  let kind = ast.nodes.get(id).kind.clone();
  match kind {
    NodeKind::Ident(name) => {
      if let Some(cps_id) = bind_site_to_cps.get(&id.0).copied() {
        out.push((cps_id, name.to_string()));
      }
    }
    NodeKind::LitSeq { items, .. }
    | NodeKind::LitRec { items, .. }
    | NodeKind::Patterns(items) => {
      for &item_id in items.items.iter() {
        walk_bind_lhs(ast, item_id, bind_site_to_cps, out);
      }
    }
    NodeKind::Spread { inner: Some(inner_id), .. } => {
      walk_bind_lhs(ast, inner_id, bind_site_to_cps, out);
    }
    NodeKind::Bind { lhs, .. } => {
      // Rec field: `{x: y}` — lhs is the binding target.
      walk_bind_lhs(ast, lhs, bind_site_to_cps, out);
    }
    NodeKind::BindRight { lhs, rhs, .. } => {
      walk_bind_lhs(ast, lhs, bind_site_to_cps, out);
      walk_bind_lhs(ast, rhs, bind_site_to_cps, out);
    }
    NodeKind::InfixOp { lhs, .. } => {
      // Guard pattern: `a > 0` — lhs is the bind.
      walk_bind_lhs(ast, lhs, bind_site_to_cps, out);
    }
    _ => {}
  }
}

pub fn lower_module<'src>(ast: &'src Ast<'src>, exprs: &[AstId], scope: &ScopeResult) -> CpsResult {
  let mut g = Gen::new(ast, scope);

  // The module-level continuation — ƒret. Pre-allocated by Gen::new.
  // The module body forwards its last expression's result to ƒret,
  // just like any function body.
  let cont_id = g.cont;
  let fret_bind = BindNode { id: cont_id, kind: Bind::Cont(ContKind::Ret) };

  // Collect which bindings are exported (module-level simple name = expr).
  let export_ids: std::collections::HashSet<CpsId> = if exprs.is_empty() {
    std::collections::HashSet::new()
  } else {
    collect_module_exports(ast, exprs, &g.bind_site_to_cps).into_iter().collect()
  };
  // Collect every module-level binding leaf (superset of exports; includes
  // destructure leaves like `x` from `{x} = ...`). Authoritative source for
  // which CpsIds become WASM globals.
  let module_locals: Vec<(CpsId, String)> = if exprs.is_empty() {
    Vec::new()
  } else {
    collect_module_locals(ast, exprs, &g.bind_site_to_cps)
  };
  // Collect module-scope import declarations from the AST before lowering.
  // Names must be read here — after lifting, the rec_pop chain is in separate fns.
  let module_imports = if exprs.is_empty() {
    std::collections::BTreeMap::new()
  } else {
    collect_module_imports(ast, exprs)
  };

  // Origin for module-level synthetic CPS nodes: the Module AST node itself.
  // Its loc covers the whole source, which is what `·ƒink_module` should
  // anchor to. Without this the root App is origin-less and renders as
  // unmapped in source-map output.
  let module_origin = Some(ast.root);

  let body = if exprs.is_empty() {
    // Empty module: call ƒret with no args.
    let cont_val = g.val(ValKind::ContRef(cont_id), module_origin);
    g.expr(ExprKind::App {
      func: Callable::Val(cont_val),
      args: vec![],
    }, module_origin)
  } else {
    // Lower the module body as a sequence — same as any function body.
    let body = lower_seq_with_tail(&mut g, exprs, Cont::Ref(cont_id));
    // Post-process: inject ·ƒpub calls after each exported LetVal binding.
    // TODO: move export detection to an AST desugaring pass; the CPS
    // transform shouldn't know about implicit module-level pub semantics.
    if export_ids.is_empty() { body } else { inject_pub_calls(&mut g, body, &export_ids) }
  };

  // Root: App(FinkModule, [Cont::Expr { args: [ƒret], body }])
  // The CPS root is a call — every module starts with an action.
  let root = g.expr(ExprKind::App {
    func: Callable::BuiltIn(BuiltIn::FinkModule),
    args: vec![Arg::Cont(Cont::Expr {
      args: vec![fret_bind],
      body: Box::new(body),
    })],
  }, module_origin);

  CpsResult { root, origin: g.origin, bind_to_cps: g.bind_to_cps, synth_alias: crate::propgraph::PropGraph::new(), param_info: crate::propgraph::PropGraph::new(), module_locals, module_imports }
}

/// Walk a lowered module body and wrap each exported LetVal's continuation
/// with a `·ƒpub name, fn: <original cont>` call. This injects per-binding
/// export side effects without changing the tail-forwarding structure.
fn inject_pub_calls(
  g: &mut Gen,
  expr: Expr,
  export_ids: &std::collections::HashSet<CpsId>,
) -> Expr {
  match expr.kind {
    ExprKind::LetVal { name, val, cont } => {
      let is_exported = export_ids.contains(&name.id);
      // Recurse into cont first.
      let cont = match cont {
        Cont::Ref(_) => cont,
        Cont::Expr { args, body } => {
          let body = inject_pub_calls(g, *body, export_ids);
          Cont::Expr { args, body: Box::new(body) }
        }
      };
      if is_exported {
        // Wrap: LetVal name = val; ·ƒpub name, fn: <original cont body>
        let origin = g.origin.try_get(name.id).and_then(|o| *o);
        let val_ref = g.val(ValKind::Ref(Ref::Synth(name.id)), origin);
        let cont_body = match cont {
          Cont::Expr { args, body } => {
            // Build: ·ƒpub val_ref, fn: <body>
            let pub_app = g.expr(ExprKind::App {
              func: Callable::BuiltIn(BuiltIn::Pub),
              args: vec![
                Arg::Val(val_ref),
                Arg::Cont(Cont::Expr { args: vec![], body }),
              ],
            }, origin);
            Cont::Expr { args, body: Box::new(pub_app) }
          }
          Cont::Ref(cont_id) => {
            // Cont is a direct ref — wrap in ƒpub then forward.
            let cont_val = g.val(ValKind::ContRef(cont_id), origin);
            let fwd_val = g.val(ValKind::Ref(Ref::Synth(name.id)), origin);
            let forward = g.expr(ExprKind::App {
              func: Callable::Val(cont_val),
              args: vec![Arg::Val(fwd_val)],
            }, origin);
            let pub_app = g.expr(ExprKind::App {
              func: Callable::BuiltIn(BuiltIn::Pub),
              args: vec![
                Arg::Val(val_ref),
                Arg::Cont(Cont::Expr { args: vec![], body: Box::new(forward) }),
              ],
            }, origin);
            Cont::Expr { args: vec![], body: Box::new(pub_app) }
          }
        };
        Expr { id: expr.id, kind: ExprKind::LetVal { name, val, cont: cont_body } }
      } else {
        Expr { id: expr.id, kind: ExprKind::LetVal { name, val, cont } }
      }
    }
    ExprKind::LetFn { name, params, fn_kind, fn_body, cont } => {
      // For CpsClosure (synthetic module-cont helpers, e.g. destructure success
      // wrappers), recurse into fn_body — these are continuations of the module
      // scope, not separate user scopes. For CpsFunction (user-defined fns), skip
      // fn_body — those are independent scopes.
      let fn_body = if fn_kind == CpsFnKind::CpsClosure {
        Box::new(inject_pub_calls(g, *fn_body, export_ids))
      } else {
        fn_body
      };
      let cont = match cont {
        Cont::Ref(_) => cont,
        Cont::Expr { args, body } => {
          let body = inject_pub_calls(g, *body, export_ids);
          Cont::Expr { args, body: Box::new(body) }
        }
      };
      Expr { id: expr.id, kind: ExprKind::LetFn { name, params, fn_kind, fn_body, cont } }
    }
    // App at module level — recurse into Cont::Expr args to find LetVals
    // that need ·ƒpub wrapping (e.g. `s = add 1, 2` produces an App whose
    // cont body contains the LetVal for `s`).
    ExprKind::App { func, args } => {
      let args = args.into_iter().map(|a| match a {
        Arg::Cont(Cont::Expr { args: ca, body }) => {
          let body = inject_pub_calls(g, *body, export_ids);
          Arg::Cont(Cont::Expr { args: ca, body: Box::new(body) })
        }
        other => other,
      }).collect();
      Expr { id: expr.id, kind: ExprKind::App { func, args } }
    }
    ExprKind::If { cond, then, else_ } => {
      let then = inject_pub_calls(g, *then, export_ids);
      let else_ = inject_pub_calls(g, *else_, export_ids);
      Expr { id: expr.id, kind: ExprKind::If { cond, then: Box::new(then), else_: Box::new(else_) } }
    }
  }
}

/// Lower a single expression node (or a Module root) to CPS IR.
pub fn lower_expr<'src>(ast: &'src Ast<'src>, id: AstId, scope: &ScopeResult) -> CpsResult {
  let mut g = Gen::new(ast, scope);
  let (val, pending) = lower(&mut g, id);
  let cont = g.cont;
  let root = if pending.is_empty() {
    wrap_val(&mut g, val, Some(id))
  } else {
    wrap(&mut g, pending, Cont::Ref(cont))
  };
  CpsResult { root, origin: g.origin, bind_to_cps: g.bind_to_cps, synth_alias: crate::propgraph::PropGraph::new(), param_info: crate::propgraph::PropGraph::new(), module_locals: Vec::new(), module_imports: std::collections::BTreeMap::new() }
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
  g: &mut Gen<'_, 'src>,
  val: Val,
  op: &'src str,
  start: AstId,
  end: AstId,
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
fn emit_seq_pattern(
  g: &mut Gen,
  val: Val,
  elems: &[AstId],
  origin: Option<AstId>,
  pending: &mut Vec<Pending>,
) -> (Bind, CpsId) {
  let subj_param = g.fresh_result(origin);
  let succ_param = g.bind(Bind::Cont(ContKind::Succ), None);
  let fail_param = g.bind(Bind::Cont(ContKind::Fail), None);

  // Separate front, spread, trailing.
  // Pattern shape: [front..., (..spread)?, trailing...]
  // Trailing elements only exist if there's a spread before them.
  // The spread's inner is `Option<AstId>` — None for bare `..`, Some for `..rest`.
  let mut regular: Vec<AstId> = vec![];
  let mut spread: Option<Option<AstId>> = None;
  let mut trailing: Vec<AstId> = vec![];
  for &elem in elems.iter() {
    if let NodeKind::Spread { inner, .. } = &g.node(elem).kind {
      spread = Some(*inner);
      continue;
    }
    if spread.is_some() {
      trailing.push(elem);
    } else {
      regular.push(elem);
    }
  }

  // Pre-allocate temp binds.
  let head_temps: Vec<BindNode> = regular.iter().map(|_| g.fresh_result(origin)).collect();
  let tail_temps: Vec<BindNode> = trailing.iter().map(|_| g.fresh_result(origin)).collect();

  // Pre-allocate a rest temp only for bound spread (`[..rest]`), not bare spread (`[..]`).
  let rest_temp: Option<BindNode> = match spread {
    Some(Some(_)) => Some(g.fresh_result(origin)),  // bound spread
    _ => None,                                       // no spread or bare spread
  };

  // Collect all bind_names that the body fn will receive.
  let mut bind_names: Vec<BindNode> = head_temps.clone();
  if let Some(rt) = &rest_temp {
    bind_names.push(rt.clone());
  }
  bind_names.extend(tail_temps.clone());

  // --- Build matcher body inside-out ---

  // Step 1: build the terminal expression (innermost).
  // This is either an Empty check (no spread), a SeqPop assert (bare spread),
  // or a plain succ call (bound spread).
  let binds = SeqBinds {
    head_temps: &head_temps,
    rest_temp: &rest_temp,
    tail_temps: &tail_temps,
  };
  let terminal = build_seq_terminal(
    g, &binds, spread,
    succ_param.id, fail_param.id, origin,
  );
  // Note: terminal.0 is the wrapped expression (Empty/SeqPop/LetVal).
  //       terminal.1 is the cursor_bind that the wrapped expression reads.
  //       When trailing exists, the cursor for the terminal is the LAST
  //       SeqPopBack's `init` output. When no trailing exists, it's the
  //       cursor produced by the front SeqPop fold (or the checked
  //       cursor when there are no front pops).

  // Step 2: if trailing patterns exist, wrap the terminal with a
  // SeqPopBack chain. Each SeqPopBack peels one element off the END of
  // the cursor; the LAST trailing pattern is popped first (it's at the
  // back of the list); the FIRST trailing pattern is popped last.
  // The final init (after all back-pops) is bound to rest_temp when the
  // spread is bound.
  let (inner_body_after_trailing, body_outer_cursor) = if !trailing.is_empty() {
    let outer_cursor = g.fresh_result(origin);
    let wrapped = fold_seq_pop_backs(
      g, &tail_temps, terminal.0, terminal.1, fail_param.id,
      outer_cursor.clone(), origin,
    );
    (wrapped, outer_cursor)
  } else {
    (terminal.0, terminal.1)
  };

  // Step 3: fold front pops over `head_temps`.
  // The IsSeqLike guard provides a checked cursor; SeqPops use that.
  let checked_param = g.fresh_result(origin);
  let inner_body = fold_seq_pops_to(
    g, &head_temps, inner_body_after_trailing, body_outer_cursor, fail_param.id,
    checked_param.clone(), origin,
  );

  // Step 4: wrap with IsSeqLike type guard.
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

  // Step 5: push sub-pattern pendings for each element (front + trailing).
  // The body fn receives the temps. For each regular element, lower_pat_lhs against the temp.
  for (i, &elem_id) in regular.iter().enumerate() {
    let temp_val = ref_val(g, head_temps[i].kind, head_temps[i].id, origin);
    lower_pat_lhs(g, elem_id, temp_val, Some(elem_id), pending);
  }
  for (i, &elem_id) in trailing.iter().enumerate() {
    let temp_val = ref_val(g, tail_temps[i].kind, tail_temps[i].id, origin);
    lower_pat_lhs(g, elem_id, temp_val, Some(elem_id), pending);
  }

  // Handle spread binding.
  if let Some(Some(name_id)) = spread {
    let rt = rest_temp.as_ref().unwrap();
    let rest_val = ref_val(g, rt.kind, rt.id, origin);
    if let NodeKind::Ident(_) = &g.node(name_id).kind {
      let bind = g.bind_name(name_id);
      pending.push(Pending::MatchBind { name: bind, val: rest_val, origin });
    }
  }

  // Return a placeholder bind (the last bind_name or a fresh one).
  let r = g.fresh_result(origin);
  (r.kind, r.id)
}

/// Bind groups carried by a seq pattern: front, optional spread bind,
/// trailing. Determines what `succ` is called with at the terminal.
struct SeqBinds<'a> {
  head_temps: &'a [BindNode],
  rest_temp: &'a Option<BindNode>,
  tail_temps: &'a [BindNode],
}

/// Build the terminal expression for a seq pattern matcher.
/// This is the innermost CPS expression before the SeqPop chain wraps it.
fn build_seq_terminal(
  g: &mut Gen,
  binds: &SeqBinds<'_>,
  spread: Option<Option<AstId>>,
  succ_id: CpsId,
  fail_id: CpsId,
  origin: Option<AstId>,
) -> (Expr, BindNode) {
  let head_temps = binds.head_temps;
  let rest_temp = binds.rest_temp;
  let tail_temps = binds.tail_temps;
  // Build succ(temps...) call. Order matches `bind_names`:
  // head_temps, rest_temp (if any), tail_temps.
  let succ_ref = g.val(ValKind::ContRef(succ_id), origin);
  let mut succ_args: Vec<Arg> = head_temps.iter()
    .map(|t| Arg::Val(g.val(ValKind::Ref(Ref::Synth(t.id)), origin)))
    .collect();
  if let Some(rt) = rest_temp {
    succ_args.push(Arg::Val(g.val(ValKind::Ref(Ref::Synth(rt.id)), origin)));
  }
  for tt in tail_temps {
    succ_args.push(Arg::Val(g.val(ValKind::Ref(Ref::Synth(tt.id)), origin)));
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
/// `body` is the inner expression that consumes the cursor at
/// `inner_cursor` (the last SeqPop's tail) and runs after all front pops.
/// Returns the fully wrapped body.
fn fold_seq_pops_to(
  g: &mut Gen,
  head_temps: &[BindNode],
  body: Expr,
  inner_cursor: BindNode,
  fail_id: CpsId,
  first_cursor: BindNode,
  origin: Option<AstId>,
) -> Expr {
  let mut body = body;
  let mut next_tail_bind = inner_cursor;

  if head_temps.is_empty() {
    // No elements — body's inner cursor needs to be wired to first_cursor.
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

/// Fold over tail_temps from LAST to FIRST, wrapping each SeqPopBack
/// around the terminal body. The outermost (first to run) SeqPopBack
/// reads from `outer_cursor` (the cursor produced by the front fold).
/// The innermost SeqPopBack writes its init to `inner_cursor` (the bind
/// the terminal_body references — typically allocated by
/// `build_seq_terminal`).
///
/// Pop order: the LAST trailing pattern is at the back of the list and
/// is popped FIRST; the FIRST trailing pattern is popped LAST.
///
/// Tree shape (for trailing = [a, b, c]):
///   SeqPopBack(outer_cursor, fail, fn (init1, c_val):   # OUTERMOST pops c
///     SeqPopBack(init1, fail, fn (init2, b_val):         # pops b
///       SeqPopBack(init2, fail, fn (inner_cursor, a_val): # INNERMOST pops a
///         <terminal_body, references inner_cursor>
///       )))
fn fold_seq_pop_backs(
  g: &mut Gen,
  tail_temps: &[BindNode],
  terminal_body: Expr,
  inner_cursor: BindNode,
  fail_id: CpsId,
  outer_cursor: BindNode,
  origin: Option<AstId>,
) -> Expr {
  assert!(!tail_temps.is_empty(), "fold_seq_pop_backs: empty tail_temps");
  let k = tail_temps.len();

  let mut body = terminal_body;
  // next_init_bind is the init that the CURRENT body expects as input.
  // For the innermost wrapping iteration, this is `inner_cursor`.
  let mut next_init_bind = inner_cursor;

  // Build from innermost (i = k-1, pops tail_temps[0]) outward
  // (i = 0, pops tail_temps[k-1]).
  for i in (0..k).rev() {
    // Trailing pattern bound by this pop: tail_temps[k-1-i].
    let last_temp = tail_temps[k - 1 - i].clone();

    // Cursor for this pop. Outermost (i == 0) uses outer_cursor.
    let cursor_bind = if i == 0 { outer_cursor.clone() } else { g.fresh_result(origin) };
    let cursor_ref = g.val(ValKind::Ref(Ref::Synth(cursor_bind.id)), origin);
    let fail_ref = g.val(ValKind::ContRef(fail_id), origin);

    let pop = g.expr(ExprKind::App {
      func: Callable::BuiltIn(BuiltIn::SeqPopBack),
      args: vec![
        Arg::Val(cursor_ref),
        Arg::Val(fail_ref),
        Arg::Cont(Cont::Expr {
          args: vec![next_init_bind, last_temp],
          body: Box::new(body),
        }),
      ],
    }, origin);

    body = pop;
    next_init_bind = cursor_bind;
  }

  body
}

/// Spread variant in a record pattern.
enum SpreadKind {
  BareNonEmpty,                      // `{..}`
  EmptyRest,                         // `{..{}}` — rest must be empty
  Bound(AstId),                      // `{..rest}`  — the rest binding ident
  SubPattern(AstId),                 // `{..{bar, spam}}` — the sub-pattern
}

/// Record pattern key: identifier name or computed expression.
enum RecKey<'src> {
  Ident(&'src str),
  Expr(AstId),
}

/// A record field extracted from the AST pattern: key + sub-pattern node.
struct RecField<'src> {
  key: RecKey<'src>,
  /// The sub-pattern node for the extracted value.
  /// For `{x}` shorthand: the Ident node itself.
  /// For `{x: pat}`: the pat node.
  pat: AstId,
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
  g: &mut Gen<'_, 'src>,
  val: Val,
  fields: &[AstId],
  origin: Option<AstId>,
  pending: &mut Vec<Pending>,
) -> (Bind, CpsId) {
  let subj_param = g.fresh_result(origin);
  let succ_param = g.bind(Bind::Cont(ContKind::Succ), None);
  let fail_param = g.bind(Bind::Cont(ContKind::Fail), None);

  // Parse field nodes into RecField structs and detect spread.
  let mut regular: Vec<RecField<'src>> = vec![];
  let mut spread: Option<SpreadKind> = None;

  for &field_id in fields.iter() {
    let field_kind = g.node(field_id).kind.clone();
    match field_kind {
      NodeKind::Spread { inner, .. } => {
        spread = Some(match inner {
          None => SpreadKind::BareNonEmpty,
          Some(inner_id) => {
            let inner_kind = g.node(inner_id).kind.clone();
            match inner_kind {
              NodeKind::Ident(_) => SpreadKind::Bound(inner_id),
              NodeKind::LitRec { items, .. } if items.items.is_empty() => SpreadKind::EmptyRest,
              NodeKind::LitRec { .. } => SpreadKind::SubPattern(inner_id),
              _ => SpreadKind::BareNonEmpty, // fallback
            }
          }
        });
        break;
      }
      NodeKind::Ident(name) => {
        regular.push(RecField { key: RecKey::Ident(name), pat: field_id, origin: Some(field_id) });
      }
      NodeKind::Bind { lhs, rhs: pat_id, .. } => {
        let lhs_kind = g.node(lhs).kind.clone();
        match lhs_kind {
          NodeKind::Ident(key) => {
            regular.push(RecField { key: RecKey::Ident(key), pat: pat_id, origin: Some(lhs) });
          }
          NodeKind::LitStr { content, .. } => {
            // Leak the String to make it 'src — small one-off cost during compile.
            let key: &'src str = Box::leak(content.into_boxed_str());
            regular.push(RecField { key: RecKey::Ident(key), pat: pat_id, origin: Some(lhs) });
          }
          _ => {}
        }
      }
      NodeKind::Arm { lhs: arm_lhs, body: arm_body, .. } => {
        if let Some(&pat_id) = arm_body.items.last() {
          let arm_lhs_kind = g.node(arm_lhs).kind.clone();
          match arm_lhs_kind {
            NodeKind::Ident(key) => {
              regular.push(RecField { key: RecKey::Ident(key), pat: pat_id, origin: Some(arm_lhs) });
            }
            NodeKind::LitStr { content, .. } => {
              let key: &'src str = Box::leak(content.into_boxed_str());
              regular.push(RecField { key: RecKey::Ident(key), pat: pat_id, origin: Some(arm_lhs) });
            }
            NodeKind::Group { inner, .. } => {
              regular.push(RecField { key: RecKey::Expr(inner), pat: pat_id, origin: Some(arm_lhs) });
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
  // Snapshot pat ids first to drop the borrow on `regular` / `g` for recursive calls.
  let pat_calls: Vec<(AstId, Option<AstId>)> = regular.iter().map(|f| (f.pat, f.origin)).collect();
  for (i, (pat_id, pat_origin)) in pat_calls.iter().enumerate() {
    let temp_val = ref_val(g, field_temps[i].kind, field_temps[i].id, origin);
    lower_pat_lhs(g, *pat_id, temp_val, *pat_origin, pending);
  }

  // Handle spread binding/sub-pattern.
  match spread {
    Some(SpreadKind::Bound(name_id)) => {
      let rt = rest_temp.as_ref().unwrap();
      let rest_val = ref_val(g, rt.kind, rt.id, origin);
      if let NodeKind::Ident(_) = &g.node(name_id).kind {
        let bind = g.bind_name(name_id);
        pending.push(Pending::MatchBind { name: bind, val: rest_val, origin });
      }
    }
    Some(SpreadKind::SubPattern(sub_pat_id)) => {
      let rt = rest_temp.as_ref().unwrap();
      let rest_val = ref_val(g, rt.kind, rt.id, origin);
      lower_pat_lhs(g, sub_pat_id, rest_val, Some(sub_pat_id), pending);
    }
    _ => {}
  }

  let r = g.fresh_result(origin);
  (r.kind, r.id)
}

/// Build the terminal expression for a rec pattern matcher.
fn build_rec_terminal(
  g: &mut Gen,
  field_temps: &[BindNode],
  rest_temp: &Option<BindNode>,
  spread: &Option<SpreadKind>,
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
  fields: &[RecField<'src>],
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
      RecKey::Expr(id) => lower(g, *id),
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
fn emit_str_templ_pattern(
  g: &mut Gen,
  val: Val,
  children: &[AstId],
  origin: Option<AstId>,
  pending: &mut Vec<Pending>,
) -> (Bind, CpsId) {
  // Parse children into (prefix_bytes, capture_id, suffix_bytes).
  // Valid shapes: [Expr], [LitStr, Expr], [Expr, LitStr], [LitStr, Expr, LitStr]
  // Template literal parts are escape-rendered like standalone string literals
  // so byte comparisons in str_match match the runtime representation of the
  // subject string (which is also rendered at the CPS LitStr lowering).
  let render = |s: &str| crate::strings::render(s);
  // Snapshot child kinds to drop the ast borrow during pattern matching.
  let child_kinds: Vec<NodeKind<'_>> = children.iter().map(|&id| g.node(id).kind.clone()).collect();
  let is_lit = |k: &NodeKind<'_>| matches!(k, NodeKind::LitStr { .. });
  let lit_content = |k: &NodeKind<'_>| -> Vec<u8> {
    if let NodeKind::LitStr { content, .. } = k { render(content) } else { unreachable!() }
  };
  let (prefix, capture_id, suffix): (Vec<u8>, AstId, Vec<u8>) = match (children, child_kinds.as_slice()) {
    ([id], [k0]) if !is_lit(k0) =>
      (Vec::new(), *id, Vec::new()),
    ([_, expr], [k0, _]) if is_lit(k0) =>
      (lit_content(&child_kinds[0]), *expr, Vec::new()),
    ([expr, _], [k0, k1]) if is_lit(k1) && !is_lit(k0) =>
      (Vec::new(), *expr, lit_content(&child_kinds[1])),
    ([_, expr, _], [k0, _, k2]) if is_lit(k0) && is_lit(k2) =>
      (lit_content(&child_kinds[0]), *expr, lit_content(&child_kinds[2])),
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
  lower_pat_lhs(g, capture_id, capture_val, Some(capture_id), pending)
}

fn lower_pat_lhs(
  g: &mut Gen,
  lhs: AstId,
  val: Val,
  origin: Option<AstId>,
  pending: &mut Vec<Pending>,
) -> (Bind, CpsId) {
  let kind = g.node(lhs).kind.clone();
  match kind {
    // Plain bind: `x = foo` or synthetic `·$_N` from partial desugaring
    NodeKind::Ident(_) | NodeKind::SynthIdent(_) => {
      let bind = g.bind_name(lhs);
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
      let bind_ast_id = extract_bind_ast_id(g, guard_lhs);
      let bind = g.bind_name(bind_ast_id);
      let r = (bind.kind, bind.id);

      // Guard-expression origin: the InfixOp node itself (`a > 0`), not
      // the outer bind context. Used for the op call and the If node
      // so hovering/stepping narrows to the condition.
      let guard_origin = Some(lhs);

      // Build matcher: fn(subj, succ, fail): op(subj, rhs, fn result: if result succ(subj) else fail)
      let subj_param = g.fresh_result(origin);
      let succ_param = g.bind(Bind::Cont(ContKind::Succ), None);
      let fail_param = g.bind(Bind::Cont(ContKind::Fail), None);

      // Lower the guard RHS (the comparison value, e.g. `0` in `a > 0`).
      // This is a pure expression, no scope dependency on the bind.
      let (rv, rp) = lower(g, guard_rhs);

      // Build the guard test: op(subj, rhs, fn result: if result then succ(subj) else fail())
      let subj_ref = ref_val(g, subj_param.kind, subj_param.id, origin);
      let result_bind = g.fresh_result(guard_origin);
      let result_ref = g.val(ValKind::Ref(Ref::Synth(result_bind.id)), guard_origin);

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
      }, guard_origin);

      // Build the op call: op(subj, rhs, fn result: if_expr)
      let op_builtin = BuiltIn::from_builtin_str(op.src);
      let guard_call = g.expr(ExprKind::App {
        func: Callable::BuiltIn(op_builtin),
        args: vec![
          Arg::Val(subj_ref),
          Arg::Val(rv),
          Arg::Cont(Cont::Expr { args: vec![result_bind], body: Box::new(if_expr) }),
        ],
      }, guard_origin);

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
      let arg_ids: Vec<AstId> = args.items.to_vec();
      let mut arg_vals: Vec<Val> = vec![];
      for arg in arg_ids {
        let arg_kind = g.node(arg).kind.clone();
        let arg_val = match arg_kind {
          NodeKind::Ident(_) | NodeKind::Wildcard => {
            let (bound_kind, bound_id) = lower_pat_lhs(g, arg, val.clone(), Some(arg), pending);
            ref_val(g, bound_kind, bound_id, Some(arg))
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
      emit_literal_pattern(g, val, Lit::Bool(b), origin, pending);
      { let r = g.fresh_result(origin); (r.kind, r.id) }
    }
    NodeKind::LitStr { content: s, .. } => {
      emit_literal_pattern(g, val, Lit::Str(crate::strings::render(&s)), origin, pending);
      { let r = g.fresh_result(origin); (r.kind, r.id) }
    }

    // Seq pattern: `[] = foo`, `[a, b] = foo`, `[a, []] = foo`, `[head, ..tail] = foo`
    // Emits a single PatternMatch whose matcher body chains SeqPop/Empty calls.
    NodeKind::LitSeq { items: elems, .. } => {
      let items: Vec<AstId> = elems.items.to_vec();
      emit_seq_pattern(g, val, &items, origin, pending)
    }

    // Rec pattern: `{} = foo`, `{x, y} = point`, `{bar, ..rest} = foo`, `{bar, ..{}} = foo`
    // Emits a single PatternMatch whose matcher body chains RecPop/Empty calls.
    NodeKind::LitRec { items: fields, .. } => {
      let items: Vec<AstId> = fields.items.to_vec();
      emit_rec_pattern(g, val, &items, origin, pending)
    }

    // Bind-right: `pat |= name` — bind val to `name`, then also destructure as `pat`.
    // e.g. `[b, c] |= d` binds the element as `d` and destructures it as `[b, c]`.
    NodeKind::BindRight { lhs: pat, rhs: name_node, .. } => {
      if !matches!(g.node(name_node).kind, NodeKind::Ident(_)) {
        panic!("lower_pat_lhs: BindRight rhs must be an Ident");
      }
      let bind = g.bind_name(name_node);
      pending.push(Pending::MatchBind { name: bind, val: val.clone(),  origin });
      lower_pat_lhs(g, pat, val, origin, pending)
    }

    // StrTempl pattern: `'prefix${capture}suffix' = val`
    // Validates exactly one interpolation with at most two literal parts (prefix, suffix).
    // Emits StrMatch(subj, prefix, suffix, fail, succ(capture)).
    NodeKind::StrTempl { children, .. } => {
      let children: Vec<AstId> = children.to_vec();
      emit_str_templ_pattern(g, val, &children, origin, pending)
    }

    other => todo!("lower_pat_lhs: pattern not yet implemented: {:?}", other),
  }
}


/// Extract the binding AstId from a pattern LHS.
/// Recurses through nested InfixOps to find the innermost ident.
fn extract_bind_ast_id(g: &Gen, id: AstId) -> AstId {
  match &g.node(id).kind {
    NodeKind::Ident(_) => id,
    NodeKind::InfixOp { lhs, .. } => {
      let lhs = *lhs;
      extract_bind_ast_id(g, lhs)
    }
    other => panic!("extract_bind_ast_id: expected ident in pattern lhs, got {:?}", other),
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod module_tests {
  use crate::passes::cps::fmt::Ctx;

  fn cps_module(src: &str) -> String {
    match crate::to_desugared(src, "test") {
      Ok(desugared) => {
        let cps = crate::passes::lower(&desugared);
        let bk = crate::passes::cps::ir::collect_bind_kinds(&cps.result.root);
        let ctx = Ctx { origin: &cps.result.origin, ast: &desugared.ast, captures: None, param_info: None, bind_kinds: Some(&bk) };
        let (output, srcmap) = crate::passes::cps::fmt::fmt_with_mapped_native(&cps.result.root, &ctx);
        let _ = src;
        let b64 = srcmap.encode_base64url();
        format!("{output}\n# sm:{b64}")
      }
      Err(e) => format!("ERROR: {e}"),
    }
  }

  test_macros::include_fink_tests!("src/passes/cps/test_module.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_literals.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_bindings.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_functions.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_operators.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_application.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_strings.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_collections.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_scheduling.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_patterns_bind.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_patterns_seq.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_patterns_rec.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_patterns_match.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_patterns_bindings.fnk");
  test_macros::include_fink_tests!("src/passes/cps/test_patterns_str.fnk");
}
