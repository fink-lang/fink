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
  Arg, Bind, BindNode, Callable, CpsId, Expr, ExprKind,
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
  Captured { bind: CpsId, depth: u32 },
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
  /// Maps each Ref::Name's CpsId → CpsId of the Bind it resolves to.
  /// Retained for backward compatibility with old test format.
  pub resolves_to: PropGraph<CpsId, Option<CpsId>>,
  /// Maps each bind's CpsId → CpsId of the scope-introducing node that owns it.
  pub bind_scope: PropGraph<CpsId, Option<CpsId>>,
  /// Maps each scope-introducing node's CpsId → CpsId of its parent scope.
  /// `None` for the root scope.
  pub parent_scope: PropGraph<CpsId, Option<CpsId>>,
}

// ---------------------------------------------------------------------------
// Name lookup context
// ---------------------------------------------------------------------------

/// Carries the origin map and AST index for recovering source names.
struct Ctx<'a, 'src> {
  origin: &'a PropGraph<CpsId, Option<AstId>>,
  ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
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
) -> ResolveResult {
  let ctx = Ctx { origin, ast_index };
  let mut graphs = Graphs {
    resolution: PropGraph::with_size(node_count, None),
    resolves_to: PropGraph::with_size(node_count, None),
    bind_scope: PropGraph::with_size(node_count, None),
    parent_scope: PropGraph::with_size(node_count, None),
  };
  let scope = ScopeMap::new();
  // The root expr is a LetFn wrapping the module body; its CpsId is the root scope.
  let root_scope = expr.id;
  resolve_expr(expr, &scope, root_scope, 0, &ctx, &mut graphs);
  ResolveResult {
    resolution: graphs.resolution,
    resolves_to: graphs.resolves_to,
    bind_scope: graphs.bind_scope,
    parent_scope: graphs.parent_scope,
  }
}

// ---------------------------------------------------------------------------
// Mutable output graphs
// ---------------------------------------------------------------------------

struct Graphs {
  resolution: PropGraph<CpsId, Option<Resolution>>,
  resolves_to: PropGraph<CpsId, Option<CpsId>>,
  bind_scope: PropGraph<CpsId, Option<CpsId>>,
  parent_scope: PropGraph<CpsId, Option<CpsId>>,
}

// ---------------------------------------------------------------------------
// Scope — map from source name to (bind CpsId, scope CpsId)
// ---------------------------------------------------------------------------

/// Each entry: name → (bind_id, scope_id where the bind lives).
type ScopeMap<'src> = HashMap<&'src str, ScopeEntry>;

#[derive(Clone, Copy)]
struct ScopeEntry {
  bind_id: CpsId,
  scope_id: CpsId,
  fn_depth: u32,
}

/// Insert a BindNode into the scope (if it has a source name).
fn bind_to_scope<'src>(
  scope: &mut ScopeMap<'src>,
  bind: &BindNode,
  scope_id: CpsId,
  fn_depth: u32,
  ctx: &Ctx<'_, 'src>,
  graphs: &mut Graphs,
) {
  if let Bind::User = bind.kind {
    if let Some(name) = ctx.source_name(bind.id) {
      if name != "_" {
        scope.insert(name, ScopeEntry { bind_id: bind.id, scope_id, fn_depth });
        graphs.bind_scope.set(bind.id, Some(scope_id));
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Resolve a Ref — look up in scope, record in resolves_to
// ---------------------------------------------------------------------------

/// Classify a ref: compute fn boundary crossings between ref and bind.
fn classify(
  entry: &ScopeEntry,
  ref_fn_depth: u32,
) -> Resolution {
  let depth = ref_fn_depth - entry.fn_depth;
  if depth == 0 {
    Resolution::Local(entry.bind_id)
  } else {
    Resolution::Captured { bind: entry.bind_id, depth }
  }
}

fn resolve_val<'src>(
  val: &Val<'src>,
  scope: &ScopeMap<'src>,
  fn_depth: u32,
  ctx: &Ctx<'_, 'src>,
  graphs: &mut Graphs,
) {
  if let ValKind::Ref(ref_) = &val.kind {
    match ref_ {
      Ref::Name => {
        if let Some(name) = ctx.source_name(val.id) {
          if let Some(entry) = scope.get(name) {
            let resolution = classify(entry, fn_depth);
            graphs.resolves_to.set(val.id, Some(entry.bind_id));
            graphs.resolution.set(val.id, Some(resolution));
          } else {
            graphs.resolution.set(val.id, Some(Resolution::Unresolved));
          }
        }
      }
      Ref::Gen(_) => {
        // Structural — already resolved by construction, skip.
      }
    }
  }
}

fn resolve_callable<'src>(
  callable: &Callable<'src>,
  scope: &ScopeMap<'src>,
  fn_depth: u32,
  ctx: &Ctx<'_, 'src>,
  graphs: &mut Graphs,
) {
  if let Callable::Val(val) = callable {
    resolve_val(val, scope, fn_depth, ctx, graphs);
  }
}

// ---------------------------------------------------------------------------
// Recursive walk
// ---------------------------------------------------------------------------

/// Walk the continuation chain, collecting all User bind names.
/// These are the names that fn bodies at this scope level can see.
fn collect_scope_names<'src>(
  expr: &Expr<'src>,
  scope: &mut ScopeMap<'src>,
  scope_id: CpsId,
  fn_depth: u32,
  ctx: &Ctx<'_, 'src>,
  graphs: &mut Graphs,
) {
  use ExprKind::*;
  match &expr.kind {
    LetFn { name, body, .. } => {
      bind_to_scope(scope, name, scope_id, fn_depth, ctx, graphs);
      collect_scope_names(body, scope, scope_id, fn_depth, ctx, graphs);
    }
    LetVal { name, body, .. } => {
      bind_to_scope(scope, name, scope_id, fn_depth, ctx, graphs);
      collect_scope_names(body, scope, scope_id, fn_depth, ctx, graphs);
    }
    MatchLetVal { name, body, .. } => {
      bind_to_scope(scope, name, scope_id, fn_depth, ctx, graphs);
      collect_scope_names(body, scope, scope_id, fn_depth, ctx, graphs);
    }
    App { result, body, .. } => {
      bind_to_scope(scope, result, scope_id, fn_depth, ctx, graphs);
      collect_scope_names(body, scope, scope_id, fn_depth, ctx, graphs);
    }
    Yield { result, body, .. } => {
      bind_to_scope(scope, result, scope_id, fn_depth, ctx, graphs);
      collect_scope_names(body, scope, scope_id, fn_depth, ctx, graphs);
    }
    // Terminal or branching — stop collecting
    _ => {}
  }
}

fn resolve_expr<'src>(
  expr: &Expr<'src>,
  scope: &ScopeMap<'src>,
  current_scope: CpsId,
  fn_depth: u32,
  ctx: &Ctx<'_, 'src>,
  graphs: &mut Graphs,
) {
  use ExprKind::*;
  match &expr.kind {
    Ret(val) => {
      resolve_val(val, scope, fn_depth, ctx, graphs);
    }

    LetVal { name, val, body } => {
      resolve_val(val, scope, fn_depth, ctx, graphs);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, name, current_scope, fn_depth, ctx, graphs);
      resolve_expr(body, &inner, current_scope, fn_depth, ctx, graphs);
    }

    LetFn { name, params, fn_body, body, .. } => {
      // Fn bodies see all names at this scope level (hoisted), enabling
      // self- and mutual recursion. Collect all User bind names from the
      // entire continuation chain starting here.
      let mut hoisted = scope.clone();
      collect_scope_names(expr, &mut hoisted, current_scope, fn_depth, ctx, graphs);

      // fn_body is a new scope, identified by the LetFn's name CpsId
      let fn_scope_id = name.id;
      graphs.parent_scope.set(fn_scope_id, Some(current_scope));

      let mut fn_scope = hoisted.clone();
      for p in params {
        match p {
          Param::Name(b) | Param::Spread(b) =>
            bind_to_scope(&mut fn_scope, b, fn_scope_id, fn_depth + 1, ctx, graphs),
        }
      }
      resolve_expr(fn_body, &fn_scope, fn_scope_id, fn_depth + 1, ctx, graphs);

      // continuation scope: sequential (only names defined so far)
      let mut cont_scope = scope.clone();
      bind_to_scope(&mut cont_scope, name, current_scope, fn_depth, ctx, graphs);
      resolve_expr(body, &cont_scope, current_scope, fn_depth, ctx, graphs);
    }

    LetRec { bindings, body } => {
      // All names visible in all fn_bodies and in body
      let mut rec_scope = scope.clone();
      for b in bindings {
        bind_to_scope(&mut rec_scope, &b.name, current_scope, fn_depth, ctx, graphs);
      }
      for b in bindings {
        let fn_scope_id = b.name.id;
        graphs.parent_scope.set(fn_scope_id, Some(current_scope));

        let mut fn_scope = rec_scope.clone();
        for p in &b.params {
          match p {
            Param::Name(n) | Param::Spread(n) =>
              bind_to_scope(&mut fn_scope, n, fn_scope_id, fn_depth + 1, ctx, graphs),
          }
        }
        resolve_expr(&b.fn_body, &fn_scope, fn_scope_id, fn_depth + 1, ctx, graphs);
      }
      resolve_expr(body, &rec_scope, current_scope, fn_depth, ctx, graphs);
    }

    App { func, args, result, body } => {
      resolve_callable(func, scope, fn_depth, ctx, graphs);
      for arg in args {
        match arg {
          Arg::Val(v) | Arg::Spread(v) =>
            resolve_val(v, scope, fn_depth, ctx, graphs),
        }
      }
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, result, current_scope, fn_depth, ctx, graphs);
      resolve_expr(body, &inner, current_scope, fn_depth, ctx, graphs);
    }

    If { cond, then, else_ } => {
      resolve_val(cond, scope, fn_depth, ctx, graphs);
      resolve_expr(then, scope, current_scope, fn_depth, ctx, graphs);
      resolve_expr(else_, scope, current_scope, fn_depth, ctx, graphs);
    }

    Yield { value, result, body } => {
      resolve_val(value, scope, fn_depth, ctx, graphs);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, result, current_scope, fn_depth, ctx, graphs);
      resolve_expr(body, &inner, current_scope, fn_depth, ctx, graphs);
    }

    // -- Pattern lowering primitives --

    MatchLetVal { name, val, fail, body } => {
      resolve_val(val, scope, fn_depth, ctx, graphs);
      resolve_expr(fail, scope, current_scope, fn_depth, ctx, graphs);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, name, current_scope, fn_depth, ctx, graphs);
      resolve_expr(body, &inner, current_scope, fn_depth, ctx, graphs);
    }

    MatchApp { func, args, fail, result, body } => {
      resolve_callable(func, scope, fn_depth, ctx, graphs);
      for v in args { resolve_val(v, scope, fn_depth, ctx, graphs); }
      resolve_expr(fail, scope, current_scope, fn_depth, ctx, graphs);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, result, current_scope, fn_depth, ctx, graphs);
      resolve_expr(body, &inner, current_scope, fn_depth, ctx, graphs);
    }

    MatchIf { func, args, fail, body } => {
      resolve_callable(func, scope, fn_depth, ctx, graphs);
      for v in args { resolve_val(v, scope, fn_depth, ctx, graphs); }
      resolve_expr(fail, scope, current_scope, fn_depth, ctx, graphs);
      resolve_expr(body, scope, current_scope, fn_depth, ctx, graphs);
    }

    MatchValue { val, fail, body, .. } => {
      resolve_val(val, scope, fn_depth, ctx, graphs);
      resolve_expr(fail, scope, current_scope, fn_depth, ctx, graphs);
      resolve_expr(body, scope, current_scope, fn_depth, ctx, graphs);
    }

    MatchSeq { val, fail, body, .. } => {
      resolve_val(val, scope, fn_depth, ctx, graphs);
      resolve_expr(fail, scope, current_scope, fn_depth, ctx, graphs);
      resolve_expr(body, scope, current_scope, fn_depth, ctx, graphs);
    }

    MatchNext { fail, elem, body, .. } => {
      resolve_expr(fail, scope, current_scope, fn_depth, ctx, graphs);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, elem, current_scope, fn_depth, ctx, graphs);
      resolve_expr(body, &inner, current_scope, fn_depth, ctx, graphs);
    }

    MatchDone { fail, result, body, .. } => {
      resolve_expr(fail, scope, current_scope, fn_depth, ctx, graphs);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, result, current_scope, fn_depth, ctx, graphs);
      resolve_expr(body, &inner, current_scope, fn_depth, ctx, graphs);
    }

    MatchNotDone { fail, body, .. } => {
      resolve_expr(fail, scope, current_scope, fn_depth, ctx, graphs);
      resolve_expr(body, scope, current_scope, fn_depth, ctx, graphs);
    }

    MatchRest { fail, result, body, .. } => {
      resolve_expr(fail, scope, current_scope, fn_depth, ctx, graphs);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, result, current_scope, fn_depth, ctx, graphs);
      resolve_expr(body, &inner, current_scope, fn_depth, ctx, graphs);
    }

    MatchRec { val, fail, body, .. } => {
      resolve_val(val, scope, fn_depth, ctx, graphs);
      resolve_expr(fail, scope, current_scope, fn_depth, ctx, graphs);
      resolve_expr(body, scope, current_scope, fn_depth, ctx, graphs);
    }

    MatchField { fail, elem, body, .. } => {
      resolve_expr(fail, scope, current_scope, fn_depth, ctx, graphs);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, elem, current_scope, fn_depth, ctx, graphs);
      resolve_expr(body, &inner, current_scope, fn_depth, ctx, graphs);
    }

    MatchBlock { params, fail, arm_params, arms, result, body } => {
      for v in params { resolve_val(v, scope, fn_depth, ctx, graphs); }
      resolve_expr(fail, scope, current_scope, fn_depth, ctx, graphs);
      // Each arm gets the arm_params in scope
      for (arm, param) in arms.iter().zip(arm_params.iter()) {
        let mut arm_scope = scope.clone();
        bind_to_scope(&mut arm_scope, param, current_scope, fn_depth, ctx, graphs);
        resolve_expr(arm, &arm_scope, current_scope, fn_depth, ctx, graphs);
      }
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, result, current_scope, fn_depth, ctx, graphs);
      resolve_expr(body, &inner, current_scope, fn_depth, ctx, graphs);
    }

    Panic | FailCont => {}
  }
}

// ---------------------------------------------------------------------------
// Test output formatter — produces `(ref N, name) == (bind M, name)` lines
// ---------------------------------------------------------------------------

fn fmt_resolutions<'src>(
  expr: &Expr<'src>,
  resolves_to: &PropGraph<CpsId, Option<CpsId>>,
  ctx: &Ctx<'_, 'src>,
) -> String {
  let mut lines = Vec::new();
  collect_resolution_lines(expr, resolves_to, ctx, &mut lines);
  lines.join("\n")
}

fn emit_val<'src>(
  val: &Val<'src>,
  resolves_to: &PropGraph<CpsId, Option<CpsId>>,
  ctx: &Ctx<'_, 'src>,
  out: &mut Vec<String>,
) {
  if let ValKind::Ref(Ref::Name) = &val.kind {
    let ref_name = ctx.source_name(val.id).unwrap_or("?");
    match resolves_to.try_get(val.id) {
      Some(&Some(bind_id)) => {
        let bind_name = ctx.source_name(bind_id).unwrap_or("?");
        out.push(format!(
          "(ref {}, {}) == (bind {}, {})",
          val.id.0, ref_name, bind_id.0, bind_name
        ));
      }
      _ => {
        out.push(format!(
          "(ref {}, {}) == (unresolved {})",
          val.id.0, ref_name, ref_name
        ));
      }
    }
  }
}

fn emit_callable<'src>(
  callable: &Callable<'src>,
  resolves_to: &PropGraph<CpsId, Option<CpsId>>,
  ctx: &Ctx<'_, 'src>,
  out: &mut Vec<String>,
) {
  if let Callable::Val(val) = callable {
    emit_val(val, resolves_to, ctx, out);
  }
}

fn collect_resolution_lines<'src>(
  expr: &Expr<'src>,
  resolves_to: &PropGraph<CpsId, Option<CpsId>>,
  ctx: &Ctx<'_, 'src>,
  out: &mut Vec<String>,
) {
  use ExprKind::*;
  match &expr.kind {
    Ret(val) => { emit_val(val, resolves_to, ctx, out); }

    LetVal { val, body, .. } => {
      emit_val(val, resolves_to, ctx, out);
      collect_resolution_lines(body, resolves_to, ctx, out);
    }

    LetFn { fn_body, body, .. } => {
      collect_resolution_lines(fn_body, resolves_to, ctx, out);
      collect_resolution_lines(body, resolves_to, ctx, out);
    }

    LetRec { bindings, body } => {
      for b in bindings {
        collect_resolution_lines(&b.fn_body, resolves_to, ctx, out);
      }
      collect_resolution_lines(body, resolves_to, ctx, out);
    }

    App { func, args, body, .. } => {
      emit_callable(func, resolves_to, ctx, out);
      for arg in args {
        match arg { Arg::Val(v) | Arg::Spread(v) => emit_val(v, resolves_to, ctx, out) }
      }
      collect_resolution_lines(body, resolves_to, ctx, out);
    }

    If { cond, then, else_ } => {
      emit_val(cond, resolves_to, ctx, out);
      collect_resolution_lines(then, resolves_to, ctx, out);
      collect_resolution_lines(else_, resolves_to, ctx, out);
    }

    Yield { value, body, .. } => {
      emit_val(value, resolves_to, ctx, out);
      collect_resolution_lines(body, resolves_to, ctx, out);
    }

    MatchLetVal { val, fail, body, .. } => {
      emit_val(val, resolves_to, ctx, out);
      collect_resolution_lines(fail, resolves_to, ctx, out);
      collect_resolution_lines(body, resolves_to, ctx, out);
    }
    MatchApp { func, args, fail, body, .. } => {
      emit_callable(func, resolves_to, ctx, out);
      for v in args { emit_val(v, resolves_to, ctx, out); }
      collect_resolution_lines(fail, resolves_to, ctx, out);
      collect_resolution_lines(body, resolves_to, ctx, out);
    }
    MatchIf { func, args, fail, body } => {
      emit_callable(func, resolves_to, ctx, out);
      for v in args { emit_val(v, resolves_to, ctx, out); }
      collect_resolution_lines(fail, resolves_to, ctx, out);
      collect_resolution_lines(body, resolves_to, ctx, out);
    }
    MatchValue { val, fail, body, .. } => {
      emit_val(val, resolves_to, ctx, out);
      collect_resolution_lines(fail, resolves_to, ctx, out);
      collect_resolution_lines(body, resolves_to, ctx, out);
    }
    MatchSeq { val, fail, body, .. } => {
      emit_val(val, resolves_to, ctx, out);
      collect_resolution_lines(fail, resolves_to, ctx, out);
      collect_resolution_lines(body, resolves_to, ctx, out);
    }
    MatchNext { fail, body, .. } => {
      collect_resolution_lines(fail, resolves_to, ctx, out);
      collect_resolution_lines(body, resolves_to, ctx, out);
    }
    MatchDone { fail, body, .. } => {
      collect_resolution_lines(fail, resolves_to, ctx, out);
      collect_resolution_lines(body, resolves_to, ctx, out);
    }
    MatchNotDone { fail, body, .. } => {
      collect_resolution_lines(fail, resolves_to, ctx, out);
      collect_resolution_lines(body, resolves_to, ctx, out);
    }
    MatchRest { fail, body, .. } => {
      collect_resolution_lines(fail, resolves_to, ctx, out);
      collect_resolution_lines(body, resolves_to, ctx, out);
    }
    MatchRec { val, fail, body, .. } => {
      emit_val(val, resolves_to, ctx, out);
      collect_resolution_lines(fail, resolves_to, ctx, out);
      collect_resolution_lines(body, resolves_to, ctx, out);
    }
    MatchField { fail, body, .. } => {
      collect_resolution_lines(fail, resolves_to, ctx, out);
      collect_resolution_lines(body, resolves_to, ctx, out);
    }
    MatchBlock { params, fail, arms, body, .. } => {
      for v in params { emit_val(v, resolves_to, ctx, out); }
      collect_resolution_lines(fail, resolves_to, ctx, out);
      for arm in arms {
        collect_resolution_lines(arm, resolves_to, ctx, out);
      }
      collect_resolution_lines(body, resolves_to, ctx, out);
    }

    Panic | FailCont => {}
  }
}

// ---------------------------------------------------------------------------
// Classified test output formatter
// Produces: `(ref N, name) == (local (bind M, name)) in scope S` lines
// ---------------------------------------------------------------------------

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
      Some(Some(Resolution::Captured { bind, depth })) => {
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
    Ret(val) => { emit_classified_val(val, result, ctx, out); }

    LetVal { val, body, .. } => {
      emit_classified_val(val, result, ctx, out);
      collect_classified_lines(body, result, ctx, out);
    }

    LetFn { fn_body, body, .. } => {
      collect_classified_lines(fn_body, result, ctx, out);
      collect_classified_lines(body, result, ctx, out);
    }

    LetRec { bindings, body } => {
      for b in bindings {
        collect_classified_lines(&b.fn_body, result, ctx, out);
      }
      collect_classified_lines(body, result, ctx, out);
    }

    App { func, args, body, .. } => {
      emit_classified_callable(func, result, ctx, out);
      for arg in args {
        match arg { Arg::Val(v) | Arg::Spread(v) => emit_classified_val(v, result, ctx, out) }
      }
      collect_classified_lines(body, result, ctx, out);
    }

    If { cond, then, else_ } => {
      emit_classified_val(cond, result, ctx, out);
      collect_classified_lines(then, result, ctx, out);
      collect_classified_lines(else_, result, ctx, out);
    }

    Yield { value, body, .. } => {
      emit_classified_val(value, result, ctx, out);
      collect_classified_lines(body, result, ctx, out);
    }

    MatchLetVal { val, fail, body, .. } => {
      emit_classified_val(val, result, ctx, out);
      collect_classified_lines(fail, result, ctx, out);
      collect_classified_lines(body, result, ctx, out);
    }
    MatchApp { func, args, fail, body, .. } => {
      emit_classified_callable(func, result, ctx, out);
      for v in args { emit_classified_val(v, result, ctx, out); }
      collect_classified_lines(fail, result, ctx, out);
      collect_classified_lines(body, result, ctx, out);
    }
    MatchIf { func, args, fail, body } => {
      emit_classified_callable(func, result, ctx, out);
      for v in args { emit_classified_val(v, result, ctx, out); }
      collect_classified_lines(fail, result, ctx, out);
      collect_classified_lines(body, result, ctx, out);
    }
    MatchValue { val, fail, body, .. } => {
      emit_classified_val(val, result, ctx, out);
      collect_classified_lines(fail, result, ctx, out);
      collect_classified_lines(body, result, ctx, out);
    }
    MatchSeq { val, fail, body, .. } => {
      emit_classified_val(val, result, ctx, out);
      collect_classified_lines(fail, result, ctx, out);
      collect_classified_lines(body, result, ctx, out);
    }
    MatchNext { fail, body, .. } => {
      collect_classified_lines(fail, result, ctx, out);
      collect_classified_lines(body, result, ctx, out);
    }
    MatchDone { fail, body, .. } => {
      collect_classified_lines(fail, result, ctx, out);
      collect_classified_lines(body, result, ctx, out);
    }
    MatchNotDone { fail, body, .. } => {
      collect_classified_lines(fail, result, ctx, out);
      collect_classified_lines(body, result, ctx, out);
    }
    MatchRest { fail, body, .. } => {
      collect_classified_lines(fail, result, ctx, out);
      collect_classified_lines(body, result, ctx, out);
    }
    MatchRec { val, fail, body, .. } => {
      emit_classified_val(val, result, ctx, out);
      collect_classified_lines(fail, result, ctx, out);
      collect_classified_lines(body, result, ctx, out);
    }
    MatchField { fail, body, .. } => {
      collect_classified_lines(fail, result, ctx, out);
      collect_classified_lines(body, result, ctx, out);
    }
    MatchBlock { params, fail, arms, body, .. } => {
      for v in params { emit_classified_val(v, result, ctx, out); }
      collect_classified_lines(fail, result, ctx, out);
      for arm in arms {
        collect_classified_lines(arm, result, ctx, out);
      }
      collect_classified_lines(body, result, ctx, out);
    }

    Panic | FailCont => {}
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

  fn cps_resolve(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let cps = lower_expr(&r.root);
        let node_count = cps.origin.len();
        let result = resolve(&cps.root, &cps.origin, &ast_index, node_count);
        let ctx = Ctx { origin: &cps.origin, ast_index: &ast_index };
        fmt_classified(&cps.root, &result, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  fn cps_name_res(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let cps = lower_expr(&r.root);
        let node_count = cps.origin.len();
        let result = resolve(&cps.root, &cps.origin, &ast_index, node_count);
        let ctx = Ctx { origin: &cps.origin, ast_index: &ast_index };
        fmt_resolutions(&cps.root, &result.resolves_to, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  test_macros::include_fink_tests!("src/passes/name_res/test_name_res.fnk");
}
