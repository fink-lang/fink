// Name resolution pass.
//
// Walks the CPS IR and resolves every Ref::Name to its Bind, producing:
//
//   resolution:   PropGraph<CpsId, Option<Resolution>>  — classified ref→bind
//   bind_scope:   PropGraph<CpsId, Option<CpsId>>       — bind → owning scope
//   parent_scope: PropGraph<CpsId, Option<CpsId>>       — scope → parent scope
//
// Scopes are identified by the CpsId of the node that introduces them
// (LetFn, match arm body, etc.). No separate ScopeId type.
//
// Classification:
//   Local     — ref and bind in the same scope
//   Captured  — bind across one or more LetFn boundaries (depth = count)
//   Recursive — fn body refs its own name
//   Unresolved — no binding found
//
// See docs/name-resolution-design.md for full design.

use std::collections::HashMap;
use crate::ast::{AstId, Node as AstNode, NodeKind};
use crate::propgraph::PropGraph;
use super::cps::ir::{
  Arg, Bind, BindNode, Callable, Cont, CpsId, Expr, ExprKind,
  Param, Ref, Val, ValKind,
};

// ---------------------------------------------------------------------------
// Resolution — classification of how a Ref resolves to a Bind
// ---------------------------------------------------------------------------

/// How a name reference resolves.
///
/// Every variant (except `Unresolved`) carries the CpsId of the Bind node
/// at the definition site, so downstream passes go straight from use → def.
/// No Global variant — scope is closed; builtins are pre-seeded Bind nodes.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
  /// Bind is in the same scope as the ref.
  Local(CpsId),
  /// Bind is across one or more fn boundaries. `depth` counts LetFn
  /// boundaries crossed (other scope boundaries don't count).
  /// `bind_kind` carries the original bind's kind (Name/Synth/Cont) so
  /// closure_lifting can create cap params with the correct WASM type.
  Captured { bind: CpsId, depth: u32, bind_kind: Bind },
  /// Ref inside a fn body resolves to the fn's own name (self-recursion).
  Recursive(CpsId),
  /// No binding found — free name (error in a closed scope).
  Unresolved,
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

/// Output of the name resolution pass.
pub struct ResolveResult {
  /// Classified resolution for each Ref::Name node.
  pub resolution: PropGraph<CpsId, Option<Resolution>>,
  /// Maps each bind's CpsId → CpsId of the scope-introducing node that owns it.
  pub bind_scope: PropGraph<CpsId, Option<CpsId>>,
  /// Maps each scope-introducing node's CpsId → CpsId of its parent scope.
  /// `None` for the root scope.
  pub parent_scope: PropGraph<CpsId, Option<CpsId>>,
  /// All captures per fn scope: direct (depth >= 1 refs in the fn body) and
  /// transitive (inner fn captures that cross this fn's boundary). Maps each
  /// LetFn scope CpsId → (bind CpsId, bind kind) pairs of all values the fn
  /// needs from outside. The bind kind is needed so closure_lifting creates
  /// cap params with the correct WASM type (Cont → ref $Cont, others → anyref).
  ///
  /// Downstream passes that transform the tree must recompute this by re-running
  /// name_res (lift_all already does this each iteration).
  pub captures: PropGraph<CpsId, Vec<(CpsId, Bind)>>,
}

impl ResolveResult {
  /// Returns true if any ref in this result resolves as `Captured`.
  /// Used by `lift_all` to decide whether another lifting pass is needed.
  pub fn any_captured(&self) -> bool {
    (0..self.resolution.len()).any(|i| {
      matches!(
        self.resolution.try_get(CpsId(i as u32)),
        Some(Some(Resolution::Captured { .. }))
      )
    })
  }
}

// ---------------------------------------------------------------------------
// Transitive capture computation
// ---------------------------------------------------------------------------

/// Compute all captures (direct + transitive) for each LetFn scope.
///
/// Direct: Captured { depth >= 1 } refs in the fn's immediate body.
/// Transitive: inner fn captures that cross this fn's boundary — the fn
/// doesn't directly reference the name but needs it threaded through.
///
/// Algorithm: bottom-up. Process each LetFn by first recursing into its
/// fn_body (so inner fn data is ready). Then collect direct captures and
/// propagate transitive captures from inner fns.
fn compute_captures<'src>(
  expr: &Expr<'src>,
  graphs: &Graphs,
  _ctx: &Ctx<'_, 'src>,
) -> PropGraph<CpsId, Vec<(CpsId, Bind)>> {
  let mut captures: PropGraph<CpsId, Vec<(CpsId, Bind)>> = PropGraph::new();
  walk_captures(expr, graphs, &mut captures);
  captures
}

/// Bottom-up walk: recurse into children first, then collect captures for this LetFn.
fn walk_captures(
  expr: &Expr<'_>,
  graphs: &Graphs,
  captures: &mut PropGraph<CpsId, Vec<(CpsId, Bind)>>,
) {
  use ExprKind::*;
  match &expr.kind {
    LetFn { name, fn_body, cont: cont, .. } => {
      // Recurse into fn_body first (bottom-up — inner fn captures ready first).
      walk_captures(fn_body, graphs, captures);

      let fn_scope = name.id;

      // Direct captures: Captured { depth >= 1 } refs in the immediate fn body
      // (conts only, not nested fn_bodies — same traversal as closure_capture).
      let mut direct: Vec<(CpsId, Bind)> = Vec::new();
      collect_direct_captured_binds(fn_body, graphs, &mut direct);
      for &(bind_id, bind_kind) in &direct {
        add_capture(captures, fn_scope, bind_id, bind_kind);
      }

      // Transitive captures: inner fn captures that cross this fn's boundary.
      let parent = graphs.parent_scope.try_get(fn_scope).and_then(|p| *p);
      if let Some(parent_scope) = parent {
        let mut deep_binds: Vec<(CpsId, Bind)> = Vec::new();
        collect_deep_captured_binds(fn_body, graphs, &mut deep_binds);
        for (bind_id, bind_kind) in deep_binds {
          if is_bind_outside_scope(bind_id, parent_scope, graphs) {
            add_capture(captures, parent_scope, bind_id, bind_kind);
          }
        }
      }

      // Recurse into continuation.
      if let Cont::Expr { body: b, .. } = cont {
        walk_captures(b, graphs, captures);
      }
    }
    LetVal { cont: Cont::Expr { body: b, .. }, .. } => {
      walk_captures(b, graphs, captures);
    }
    LetVal { .. } => {}
    App { args, .. } => {
      for arg in args {
        match arg {
          Arg::Cont(Cont::Expr { body, .. }) | Arg::Expr(body) => walk_captures(body, graphs, captures),
          _ => {}
        }
      }
    }
    If { then, else_, .. } => {
      walk_captures(then, graphs, captures);
      walk_captures(else_, graphs, captures);
    }
    _ => {}
  }
}

/// Collect direct Captured bind CpsIds from a fn body — walks conts only,
/// NOT nested fn_bodies (same scope semantics as closure_capture).
fn collect_direct_captured_binds(
  expr: &Expr<'_>,
  graphs: &Graphs,
  out: &mut Vec<(CpsId, Bind)>,
) {
  use ExprKind::*;
  match &expr.kind {
    LetVal { val, cont: cont, .. } => {
      check_captured_bind(val, graphs, out);
      if let Cont::Expr { body: b, .. } = cont { collect_direct_captured_binds(b, graphs, out); }
    }
    LetFn { cont: body, .. } => {
      // Don't descend into fn_body — those captures belong to the inner fn.
      if let Cont::Expr { body: b, .. } = body { collect_direct_captured_binds(b, graphs, out); }
    }
    App { func, args } => {
      if let Callable::Val(v) = func { check_captured_bind(v, graphs, out); }
      for arg in args {
        match arg {
          Arg::Val(v) | Arg::Spread(v) => check_captured_bind(v, graphs, out),
          Arg::Cont(Cont::Expr { body, .. }) | Arg::Expr(body) => collect_direct_captured_binds(body, graphs, out),
          Arg::Cont(Cont::Ref(cont_id)) => {
            if let Some(Some(Resolution::Captured { bind, bind_kind, .. })) = graphs.resolution.try_get(*cont_id)
              && !out.iter().any(|(id, _)| id == bind)
            { out.push((*bind, *bind_kind)); }
          }
        }
      }
    }
    If { cond, then, else_ } => {
      check_captured_bind(cond, graphs, out);
      collect_direct_captured_binds(then, graphs, out);
      collect_direct_captured_binds(else_, graphs, out);
    }
  }
}

/// Collect all Captured bind CpsIds from an expression, recursing into
/// everything including nested LetFn fn_bodies.
fn collect_deep_captured_binds(
  expr: &Expr<'_>,
  graphs: &Graphs,
  out: &mut Vec<(CpsId, Bind)>,
) {
  use ExprKind::*;
  match &expr.kind {
    LetVal { val, cont: cont, .. } => {
      check_captured_bind(val, graphs, out);
      if let Cont::Expr { body: b, .. } = cont { collect_deep_captured_binds(b, graphs, out); }
    }
    LetFn { fn_body, cont: body, .. } => {
      collect_deep_captured_binds(fn_body, graphs, out);
      if let Cont::Expr { body: b, .. } = body { collect_deep_captured_binds(b, graphs, out); }
    }
    App { func, args } => {
      if let Callable::Val(v) = func { check_captured_bind(v, graphs, out); }
      for arg in args {
        match arg {
          Arg::Val(v) | Arg::Spread(v) => check_captured_bind(v, graphs, out),
          Arg::Cont(Cont::Expr { body, .. }) | Arg::Expr(body) => collect_deep_captured_binds(body, graphs, out),
          Arg::Cont(Cont::Ref(cont_id)) => {
            if let Some(Some(Resolution::Captured { bind, bind_kind, .. })) = graphs.resolution.try_get(*cont_id)
              && !out.iter().any(|(id, _)| id == bind)
            { out.push((*bind, *bind_kind)); }
          }
        }
      }
    }
    If { cond, then, else_ } => {
      check_captured_bind(cond, graphs, out);
      collect_deep_captured_binds(then, graphs, out);
      collect_deep_captured_binds(else_, graphs, out);
    }
  }
}

fn check_captured_bind(val: &Val<'_>, graphs: &Graphs, out: &mut Vec<(CpsId, Bind)>) {
  let is_ref = matches!(&val.kind, ValKind::Ref(Ref::Name) | ValKind::Ref(Ref::Synth(_)));
  if is_ref
    && let Some(Some(Resolution::Captured { bind, bind_kind, .. })) = graphs.resolution.try_get(val.id)
    && !out.iter().any(|(id, _)| id == bind)
  {
    // For Synth/Cont captures, only include data-carrying refs (fn params).
    // Structural synth refs (LetFn names in the continuation scope) are always
    // accessible via collect_scope_names — don't need capture threading.
    // Fn params have bind_scope in a fn scope (has parent_scope entry).
    // Continuation binds have bind_scope in a continuation scope (no parent_scope).
    if *bind_kind != Bind::Name {
      let in_fn_scope = graphs.bind_scope.try_get(*bind)
        .and_then(|s| *s)
        .and_then(|scope| graphs.parent_scope.try_get(scope).and_then(|p| *p))
        .is_some();
      if !in_fn_scope { return; }
    }
    out.push((*bind, *bind_kind));
  }
}

/// Check if a bind is defined outside a given scope. Walk up from the bind's
/// own scope — if we reach scope_id, the bind is at or below scope_id (inside
/// it, not outside). If we reach the root without finding scope_id, the bind
/// is defined in a different branch of the scope tree (outside).
fn is_bind_outside_scope(bind_id: CpsId, scope_id: CpsId, graphs: &Graphs) -> bool {
  let bind_scope = match graphs.bind_scope.try_get(bind_id) {
    Some(Some(s)) => *s,
    _ => return true,
  };
  let mut current = bind_scope;
  for _ in 0..64 {
    if current == scope_id { return false; }
    match graphs.parent_scope.try_get(current) {
      Some(Some(parent)) => current = *parent,
      _ => return true,
    }
  }
  true
}

/// Add a bind CpsId to a scope's transitive capture list (if not already present).
fn add_capture(
  transitive: &mut PropGraph<CpsId, Vec<(CpsId, Bind)>>,
  scope_id: CpsId,
  bind_id: CpsId,
  bind_kind: Bind,
) {
  let idx: usize = scope_id.into();
  while transitive.len() <= idx {
    transitive.push(Vec::new());
  }
  let mut caps = transitive.try_get(scope_id).cloned().unwrap_or_default();
  if !caps.iter().any(|(id, _)| *id == bind_id) {
    caps.push((bind_id, bind_kind));
    transitive.set(scope_id, caps);
  }
}

// ---------------------------------------------------------------------------
// Name lookup context
// ---------------------------------------------------------------------------

/// Carries the origin map and AST index for recovering source names.
struct Ctx<'a, 'src> {
  origin: &'a PropGraph<CpsId, Option<AstId>>,
  ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
  synth_alias: &'a PropGraph<CpsId, Option<CpsId>>,
}

impl<'a, 'src> Ctx<'a, 'src> {
  fn source_name(&self, cps_id: CpsId) -> Option<&'src str> {
    let ast_id = (*self.origin.try_get(cps_id)?)?;
    let node = (*self.ast_index.try_get(ast_id)?)?;
    match &node.kind {
      NodeKind::Ident(s) => Some(s),
      _ => None,
    }
  }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Resolve every Ref::Name in `expr` to the BindNode it refers to.
/// Requires the origin map and AST index for name recovery.
pub fn resolve<'src>(
  expr: &Expr<'src>,
  origin: &PropGraph<CpsId, Option<AstId>>,
  ast_index: &PropGraph<AstId, Option<&'src AstNode<'src>>>,
  node_count: usize,
  synth_alias: &PropGraph<CpsId, Option<CpsId>>,
) -> ResolveResult {
  let ctx = Ctx { origin, ast_index, synth_alias };
  let mut graphs = Graphs {
    resolution: PropGraph::with_size(node_count, None),
    bind_scope: PropGraph::with_size(node_count, None),
    parent_scope: PropGraph::with_size(node_count, None),
  };
  let scope = Scope::new();
  // The root expr is a LetFn wrapping the module body; its CpsId is the root scope.
  let root_scope = expr.id;
  resolve_expr(expr, &scope, root_scope, None, 0, &ctx, &mut graphs);
  // Compute all captures (direct + transitive) from the resolution graph.
  let captures = compute_captures(expr, &graphs, &ctx);
  ResolveResult {
    resolution: graphs.resolution,
    bind_scope: graphs.bind_scope,
    parent_scope: graphs.parent_scope,
    captures,
  }
}

// ---------------------------------------------------------------------------
// Mutable output graphs
// ---------------------------------------------------------------------------

struct Graphs {
  resolution: PropGraph<CpsId, Option<Resolution>>,
  bind_scope: PropGraph<CpsId, Option<CpsId>>,
  parent_scope: PropGraph<CpsId, Option<CpsId>>,
}

// ---------------------------------------------------------------------------
// Scope — map from source name to bind CpsId + fn_depth
// ---------------------------------------------------------------------------

/// Each entry: name → (bind_id, fn_depth at bind site).
type ScopeMap<'src> = HashMap<&'src str, ScopeEntry>;
/// Synth/Cont bindings keyed by CpsId (no source name).
type SynthScopeMap = HashMap<CpsId, ScopeEntry>;

#[derive(Clone, Copy)]
struct ScopeEntry {
  bind_id: CpsId,
  fn_depth: u32,
  bind_kind: Bind,
}

/// Combined scope: name-keyed for Ref::Name, CpsId-keyed for Ref::Synth.
#[derive(Clone)]
struct Scope<'src> {
  names: ScopeMap<'src>,
  synths: SynthScopeMap,
}

impl<'src> Scope<'src> {
  fn new() -> Self {
    Self { names: ScopeMap::new(), synths: SynthScopeMap::new() }
  }
}

/// Insert a BindNode into the scope.
/// Name binds go into `names` (keyed by source name).
/// Synth/Cont binds go into `synths` (keyed by CpsId).
fn bind_to_scope<'src>(
  scope: &mut Scope<'src>,
  bind: &BindNode,
  scope_id: CpsId,
  fn_depth: u32,
  ctx: &Ctx<'_, 'src>,
  graphs: &mut Graphs,
) {
  let entry = ScopeEntry { bind_id: bind.id, fn_depth, bind_kind: bind.kind };
  match bind.kind {
    Bind::Name => {
      if let Some(name) = ctx.source_name(bind.id)
        && name != "_" {
          scope.names.insert(name, entry);
          graphs.bind_scope.set(bind.id, Some(scope_id));
      }
    }
    Bind::Synth | Bind::Cont => {
      scope.synths.insert(bind.id, entry);
      graphs.bind_scope.set(bind.id, Some(scope_id));
      // If this param has a synth alias (from closure_lifting), also register
      // the old CpsId so Ref::Synth(old_id) in the hoisted fn body resolves.
      if let Some(Some(old_id)) = ctx.synth_alias.try_get(bind.id) {
        scope.synths.insert(*old_id, entry);
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Resolve a Ref — look up in scope, record classification
// ---------------------------------------------------------------------------

/// Classify a ref: compute fn boundary crossings between ref and bind.
/// `self_bind` is the CpsId of the enclosing fn's own name bind (if any),
/// used to detect self-recursion.
fn classify(
  entry: &ScopeEntry,
  ref_fn_depth: u32,
  self_bind: Option<CpsId>,
) -> Resolution {
  let depth = ref_fn_depth - entry.fn_depth;
  if depth == 0 {
    Resolution::Local(entry.bind_id)
  } else if self_bind == Some(entry.bind_id) {
    Resolution::Recursive(entry.bind_id)
  } else {
    Resolution::Captured { bind: entry.bind_id, depth, bind_kind: entry.bind_kind }
  }
}

fn resolve_val<'src>(
  val: &Val<'src>,
  scope: &Scope<'src>,
  self_bind: Option<CpsId>,
  fn_depth: u32,
  ctx: &Ctx<'_, 'src>,
  graphs: &mut Graphs,
) {
  if let ValKind::Ref(ref_) = &val.kind {
    match ref_ {
      Ref::Name => {
        if let Some(name) = ctx.source_name(val.id) {
          if let Some(entry) = scope.names.get(name) {
            let resolution = classify(entry, fn_depth, self_bind);
            graphs.resolution.set(val.id, Some(resolution));
          } else {
            graphs.resolution.set(val.id, Some(Resolution::Unresolved));
          }
        }
      }
      Ref::Synth(bind_id) => {
        if let Some(entry) = scope.synths.get(bind_id) {
          let resolution = classify(entry, fn_depth, self_bind);
          graphs.resolution.set(val.id, Some(resolution));
        }
      }
    }
  }
}

fn resolve_callable<'src>(
  callable: &Callable<'src>,
  scope: &Scope<'src>,
  self_bind: Option<CpsId>,
  fn_depth: u32,
  ctx: &Ctx<'_, 'src>,
  graphs: &mut Graphs,
) {
  if let Callable::Val(val) = callable {
    resolve_val(val, scope, self_bind, fn_depth, ctx, graphs);
  }
}

// ---------------------------------------------------------------------------
// Recursive walk
// ---------------------------------------------------------------------------

/// Walk the continuation chain, collecting all User bind names.
/// These are the names that fn bodies at this scope level can see.
fn collect_scope_names<'src>(
  expr: &Expr<'src>,
  scope: &mut Scope<'src>,
  scope_id: CpsId,
  fn_depth: u32,
  ctx: &Ctx<'_, 'src>,
  graphs: &mut Graphs,
) {
  use ExprKind::*;
  match &expr.kind {
    LetFn { name, cont: cont, .. } => {
      bind_to_scope(scope, name, scope_id, fn_depth, ctx, graphs);
      if let Cont::Expr { body: body_expr, .. } = cont {
        collect_scope_names(body_expr, scope, scope_id, fn_depth, ctx, graphs);
      }
    }
    LetVal { name, cont: body, .. } => {
      bind_to_scope(scope, name, scope_id, fn_depth, ctx, graphs);
      if let Cont::Expr { body: body_expr, .. } = body {
        collect_scope_names(body_expr, scope, scope_id, fn_depth, ctx, graphs);
      }
    }
    App { args, .. } => {
      for arg in args {
        if let Arg::Cont(Cont::Expr { args: cont_args, body }) = arg {
          for ca in cont_args {
            bind_to_scope(scope, ca, scope_id, fn_depth, ctx, graphs);
          }
          collect_scope_names(body, scope, scope_id, fn_depth, ctx, graphs);
        }
      }
    }
    // Terminal or branching — stop collecting
    _ => {}
  }
}

/// Resolve names inside a continuation — bind its args into scope, then recurse into its body.
fn resolve_cont<'src>(
  cont: &Cont<'src>,
  scope: &Scope<'src>,
  current_scope: CpsId,
  self_bind: Option<CpsId>,
  fn_depth: u32,
  ctx: &Ctx<'_, 'src>,
  graphs: &mut Graphs,
) {
  match cont {
    Cont::Ref(cont_id) => {
      // Resolve Cont::Ref as a synth ref so capture analysis detects
      // when a cont ref crosses fn boundaries.
      if let Some(entry) = scope.synths.get(cont_id) {
        let resolution = classify(entry, fn_depth, self_bind);
        graphs.resolution.set(*cont_id, Some(resolution));
      }
    }
    Cont::Expr { args, body } => {
      let mut inner = scope.clone();
      for a in args {
        bind_to_scope(&mut inner, a, current_scope, fn_depth, ctx, graphs);
      }
      resolve_expr(body, &inner, current_scope, self_bind, fn_depth, ctx, graphs);
    }
  }
}

fn resolve_expr<'src>(
  expr: &Expr<'src>,
  scope: &Scope<'src>,
  current_scope: CpsId,
  self_bind: Option<CpsId>,
  fn_depth: u32,
  ctx: &Ctx<'_, 'src>,
  graphs: &mut Graphs,
) {
  use ExprKind::*;
  let sb = self_bind;
  match &expr.kind {
    LetVal { name, val, cont: cont } => {
      resolve_val(val, scope, sb, fn_depth, ctx, graphs);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, name, current_scope, fn_depth, ctx, graphs);
      if let Cont::Expr { body: body_expr, .. } = cont {
        resolve_expr(body_expr, &inner, current_scope, sb, fn_depth, ctx, graphs);
      }
    }

    LetFn { name, params, fn_body, cont: body } => {
      // Fn bodies see all names at this scope level (hoisted), enabling
      // self- and mutual recursion. Collect all User bind names from the
      // entire continuation chain starting here.
      let mut hoisted = scope.clone();
      collect_scope_names(expr, &mut hoisted, current_scope, fn_depth, ctx, graphs);

      // fn_body is a new scope, identified by the LetFn's name CpsId
      let fn_scope_id = name.id;
      graphs.parent_scope.set(fn_scope_id, Some(current_scope));

      // Determine self_bind for the fn body: the hoisted name that binds
      // this LetFn's result. The CPS transform produces the fn bind in the
      // continuation — either as LetVal or MatchLetVal. Extract it from
      // the continuation's first bind node (if it's Cont::Expr).
      let cont_bind_id = if let Cont::Expr { body: cont_body, .. } = body {
        match &cont_body.kind {
          ExprKind::LetVal { name: cn, .. } => Some(cn.id),
          _ => None,
        }
      } else {
        None
      };
      let fn_self_bind = cont_bind_id
        .and_then(|id| ctx.source_name(id))
        .and_then(|n| hoisted.names.get(n))
        .map(|entry| entry.bind_id);

      let mut fn_scope = hoisted.clone();
      for p in params {
        match p {
          Param::Name(b) | Param::Spread(b) =>
            bind_to_scope(&mut fn_scope, b, fn_scope_id, fn_depth + 1, ctx, graphs),
        }
      }
      resolve_expr(fn_body, &fn_scope, fn_scope_id, fn_self_bind, fn_depth + 1, ctx, graphs);

      // continuation scope: sequential (only names defined so far)
      let mut cont_scope = scope.clone();
      bind_to_scope(&mut cont_scope, name, current_scope, fn_depth, ctx, graphs);
      if let Cont::Expr { body: body_expr, .. } = body {
        resolve_expr(body_expr, &cont_scope, current_scope, sb, fn_depth, ctx, graphs);
      }
    }


    App { func, args } => {
      resolve_callable(func, scope, sb, fn_depth, ctx, graphs);
      for arg in args {
        match arg {
          Arg::Val(v) | Arg::Spread(v) =>
            resolve_val(v, scope, sb, fn_depth, ctx, graphs),
          Arg::Cont(cont) => resolve_cont(cont, scope, current_scope, sb, fn_depth, ctx, graphs),
          Arg::Expr(e) => resolve_expr(e, scope, current_scope, sb, fn_depth, ctx, graphs),
        }
      }
    }

    If { cond, then, else_ } => {
      resolve_val(cond, scope, sb, fn_depth, ctx, graphs);
      resolve_expr(then, scope, current_scope, sb, fn_depth, ctx, graphs);
      resolve_expr(else_, scope, current_scope, sb, fn_depth, ctx, graphs);
    }

  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use crate::parser::parse;
  use crate::ast::build_index;
  use crate::passes::cps::transform::lower_expr;
  use super::*;

  // -------------------------------------------------------------------------
  // Test output formatter — classified resolution
  // Produces: `(ref N, name) == (local (bind M, name)) in scope S` lines
  // -------------------------------------------------------------------------

  fn fmt_classified<'src>(
    expr: &Expr<'src>,
    result: &ResolveResult,
    ctx: &Ctx<'_, 'src>,
  ) -> String {
    let mut lines = Vec::new();
    collect_classified_lines(expr, result, ctx, &mut lines);
    lines.join("\n")
  }

  fn emit_classified_val<'src>(
    val: &Val<'src>,
    result: &ResolveResult,
    ctx: &Ctx<'_, 'src>,
    out: &mut Vec<String>,
  ) {
    if let ValKind::Ref(Ref::Name) = &val.kind {
      let ref_name = ctx.source_name(val.id).unwrap_or("?");
      match result.resolution.try_get(val.id) {
        Some(Some(Resolution::Local(bind_id))) => {
          let bind_name = ctx.source_name(*bind_id).unwrap_or("?");
          let scope = result.bind_scope.try_get(*bind_id)
            .and_then(|s| *s)
            .map(|s| s.0)
            .unwrap_or(0);
          out.push(format!(
            "(ref {}, {}) == (local (bind {}, {})) in scope {}",
            val.id.0, ref_name, bind_id.0, bind_name, scope
          ));
        }
        Some(Some(Resolution::Captured { bind, depth, .. })) => {
          let bind_name = ctx.source_name(*bind).unwrap_or("?");
          let scope = result.bind_scope.try_get(*bind)
            .and_then(|s| *s)
            .map(|s| s.0)
            .unwrap_or(0);
          out.push(format!(
            "(ref {}, {}) == (captured {}, (bind {}, {})) in scope {}",
            val.id.0, ref_name, depth, bind.0, bind_name, scope
          ));
        }
        Some(Some(Resolution::Recursive(bind_id))) => {
          let bind_name = ctx.source_name(*bind_id).unwrap_or("?");
          let scope = result.bind_scope.try_get(*bind_id)
            .and_then(|s| *s)
            .map(|s| s.0)
            .unwrap_or(0);
          out.push(format!(
            "(ref {}, {}) == (recursive (bind {}, {})) in scope {}",
            val.id.0, ref_name, bind_id.0, bind_name, scope
          ));
        }
        Some(Some(Resolution::Unresolved)) | Some(None) | None => {
          out.push(format!(
            "(ref {}, {}) == unresolved",
            val.id.0, ref_name
          ));
        }
      }
    }
  }

  fn emit_classified_callable<'src>(
    callable: &Callable<'src>,
    result: &ResolveResult,
    ctx: &Ctx<'_, 'src>,
    out: &mut Vec<String>,
  ) {
    if let Callable::Val(val) = callable {
      emit_classified_val(val, result, ctx, out);
    }
  }

  fn collect_classified_lines<'src>(
    expr: &Expr<'src>,
    result: &ResolveResult,
    ctx: &Ctx<'_, 'src>,
    out: &mut Vec<String>,
  ) {
    use ExprKind::*;
    match &expr.kind {
      LetVal { val, cont: cont, .. } => {
        emit_classified_val(val, result, ctx, out);
        if let Cont::Expr { body: body_expr, .. } = cont {
          collect_classified_lines(body_expr, result, ctx, out);
        }
      }

      LetFn { fn_body, cont: cont, .. } => {
        collect_classified_lines(fn_body, result, ctx, out);
        if let Cont::Expr { body: body_expr, .. } = cont {
          collect_classified_lines(body_expr, result, ctx, out);
        }
      }


      App { func, args } => {
        emit_classified_callable(func, result, ctx, out);
        for arg in args {
          match arg {
            Arg::Val(v) | Arg::Spread(v) => emit_classified_val(v, result, ctx, out),
            Arg::Cont(Cont::Expr { body, .. }) => collect_classified_lines(body, result, ctx, out),
            Arg::Cont(_) => {}
            Arg::Expr(e) => collect_classified_lines(e, result, ctx, out),
          }
        }
      }

      If { cond, then, else_ } => {
        emit_classified_val(cond, result, ctx, out);
        collect_classified_lines(then, result, ctx, out);
        collect_classified_lines(else_, result, ctx, out);
      }

    }
  }

  fn cps_resolve(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let cps = lower_expr(&r.root);
        let node_count = cps.origin.len();
        let empty_alias = crate::propgraph::PropGraph::new(); let result = resolve(&cps.root, &cps.origin, &ast_index, node_count, &empty_alias);
        let ctx = Ctx { origin: &cps.origin, ast_index: &ast_index, synth_alias: &empty_alias };
        fmt_classified(&cps.root, &result, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  /// Run parse → CPS → cont_lift → name_res. Emit BOTH Ref::Name and Ref::Synth
  /// resolution. Synth refs that don't have a resolution are reported as `unresolved`.
  fn cps_resolve_synth(src: &str) -> String {
    use crate::passes::lifting::lift;
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let cps = lower_expr(&r.root);
        let lifted = lift(cps, &ast_index);
        let node_count = lifted.origin.len();
        let empty_alias = crate::propgraph::PropGraph::new(); let result = resolve(&lifted.root, &lifted.origin, &ast_index, node_count, &empty_alias);
        let ctx = Ctx { origin: &lifted.origin, ast_index: &ast_index, synth_alias: &empty_alias };
        fmt_classified_with_synth(&lifted.root, &result, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  fn emit_synth_val(
    val: &Val<'_>,
    result: &ResolveResult,
    out: &mut Vec<String>,
  ) {
    if let ValKind::Ref(Ref::Synth(bind_id)) = &val.kind {
      match result.resolution.try_get(val.id) {
        Some(Some(Resolution::Local(resolved_id))) => {
          out.push(format!(
            "(synth {}, ·v_{}) == (local (bind {}))",
            val.id.0, bind_id.0, resolved_id.0
          ));
        }
        Some(Some(Resolution::Captured { bind, depth, .. })) => {
          out.push(format!(
            "(synth {}, ·v_{}) == (captured {}, (bind {}))",
            val.id.0, bind_id.0, depth, bind.0
          ));
        }
        _ => {
          out.push(format!(
            "(synth {}, ·v_{}) == unresolved",
            val.id.0, bind_id.0
          ));
        }
      }
    }
  }

  fn collect_classified_with_synth<'src>(
    expr: &Expr<'src>,
    result: &ResolveResult,
    ctx: &Ctx<'_, 'src>,
    out: &mut Vec<String>,
  ) {
    use ExprKind::*;
    match &expr.kind {
      LetVal { val, cont: cont, .. } => {
        emit_classified_val(val, result, ctx, out);
        emit_synth_val(val, result, out);
        if let Cont::Expr { body: body_expr, .. } = cont {
          collect_classified_with_synth(body_expr, result, ctx, out);
        }
      }
      LetFn { fn_body, cont: body, .. } => {
        collect_classified_with_synth(fn_body, result, ctx, out);
        if let Cont::Expr { body: body_expr, .. } = body {
          collect_classified_with_synth(body_expr, result, ctx, out);
        }
      }
      App { func, args } => {
        if let Callable::Val(val) = func {
          emit_classified_val(val, result, ctx, out);
          emit_synth_val(val, result, out);
        }
        for arg in args {
          match arg {
            Arg::Val(v) | Arg::Spread(v) => {
              emit_classified_val(v, result, ctx, out);
              emit_synth_val(v, result, out);
            }
            Arg::Cont(Cont::Expr { body, .. }) => collect_classified_with_synth(body, result, ctx, out),
            Arg::Cont(_) => {}
            Arg::Expr(e) => collect_classified_with_synth(e, result, ctx, out),
          }
        }
      }
      If { cond, then, else_ } => {
        emit_classified_val(cond, result, ctx, out);
        emit_synth_val(cond, result, out);
        collect_classified_with_synth(then, result, ctx, out);
        collect_classified_with_synth(else_, result, ctx, out);
      }
    }
  }

  fn fmt_classified_with_synth<'src>(
    expr: &Expr<'src>,
    result: &ResolveResult,
    ctx: &Ctx<'_, 'src>,
  ) -> String {
    let mut lines = Vec::new();
    collect_classified_with_synth(expr, result, ctx, &mut lines);
    lines.join("\n")
  }

  /// Run parse → CPS → name_res. Format transitive captures per fn scope.
  fn cps_resolve_transitive(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let cps = lower_expr(&r.root);
        let node_count = cps.origin.len();
        let empty_alias = crate::propgraph::PropGraph::new(); let result = resolve(&cps.root, &cps.origin, &ast_index, node_count, &empty_alias);
        let ctx = Ctx { origin: &cps.origin, ast_index: &ast_index, synth_alias: &empty_alias };
        fmt_transitive(&result, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  fn fmt_transitive(result: &ResolveResult, ctx: &Ctx<'_, '_>) -> String {
    let mut lines = Vec::new();
    for i in 0..result.captures.len() {
      let scope_id = CpsId(i as u32);
      if let Some(caps) = result.captures.try_get(scope_id) {
        if !caps.is_empty() {
          let binds: Vec<String> = caps.iter()
            .filter_map(|(bind_id, _)| {
              let name = ctx.source_name(*bind_id)?;
              Some(format!("(bind {}, {})", bind_id.0, name))
            })
            .collect();
          lines.push(format!("cap ·ƒ_{}, {}", scope_id.0, binds.join(", ")));
        }
      }
    }
    lines.join("\n")
  }

  test_macros::include_fink_tests!("src/passes/name_res/test_name_res.fnk");
  test_macros::include_fink_tests!("src/passes/name_res/test_name_res_synth.fnk");
  test_macros::include_fink_tests!("src/passes/name_res/test_name_res_transitive.fnk");
}
