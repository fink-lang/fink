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
  /// Maps each bind's CpsId → CpsId of the scope-introducing node that owns it.
  pub bind_scope: PropGraph<CpsId, Option<CpsId>>,
  /// Maps each scope-introducing node's CpsId → CpsId of its parent scope.
  /// `None` for the root scope.
  pub parent_scope: PropGraph<CpsId, Option<CpsId>>,
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
    bind_scope: PropGraph::with_size(node_count, None),
    parent_scope: PropGraph::with_size(node_count, None),
  };
  let scope = ScopeMap::new();
  // The root expr is a LetFn wrapping the module body; its CpsId is the root scope.
  let root_scope = expr.id;
  resolve_expr(expr, &scope, root_scope, None, 0, &ctx, &mut graphs);
  ResolveResult {
    resolution: graphs.resolution,
    bind_scope: graphs.bind_scope,
    parent_scope: graphs.parent_scope,
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

#[derive(Clone, Copy)]
struct ScopeEntry {
  bind_id: CpsId,
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
  if let Bind::Name = bind.kind
    && let Some(name) = ctx.source_name(bind.id)
    && name != "_" {
      scope.insert(name, ScopeEntry { bind_id: bind.id, fn_depth });
      graphs.bind_scope.set(bind.id, Some(scope_id));
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
    Resolution::Captured { bind: entry.bind_id, depth }
  }
}

fn resolve_val<'src>(
  val: &Val<'src>,
  scope: &ScopeMap<'src>,
  self_bind: Option<CpsId>,
  fn_depth: u32,
  ctx: &Ctx<'_, 'src>,
  graphs: &mut Graphs,
) {
  if let ValKind::Ref(ref_) = &val.kind {
    match ref_ {
      Ref::Name => {
        if let Some(name) = ctx.source_name(val.id) {
          if let Some(entry) = scope.get(name) {
            let resolution = classify(entry, fn_depth, self_bind);
            graphs.resolution.set(val.id, Some(resolution));
          } else {
            graphs.resolution.set(val.id, Some(Resolution::Unresolved));
          }
        }
      }
      Ref::Synth(_) => {
        // Structural — already resolved by construction, skip.
      }
    }
  }
}

fn resolve_callable<'src>(
  callable: &Callable<'src>,
  scope: &ScopeMap<'src>,
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
      if let Cont::Expr { body: body_expr, .. } = body {
        collect_scope_names(body_expr, scope, scope_id, fn_depth, ctx, graphs);
      }
    }
    LetVal { name, body, .. } => {
      bind_to_scope(scope, name, scope_id, fn_depth, ctx, graphs);
      if let Cont::Expr { body: body_expr, .. } = body {
        collect_scope_names(body_expr, scope, scope_id, fn_depth, ctx, graphs);
      }
    }
    MatchLetVal { name, body, .. } => {
      bind_to_scope(scope, name, scope_id, fn_depth, ctx, graphs);
      if let Cont::Expr { body: body_expr, .. } = body {
        collect_scope_names(body_expr, scope, scope_id, fn_depth, ctx, graphs);
      }
    }
    App { args, .. } => {
      if let Some(Arg::Cont(Cont::Expr { args: cont_args, body })) = args.last() {
        bind_to_scope(scope, &cont_args[0], scope_id, fn_depth, ctx, graphs);
        collect_scope_names(body, scope, scope_id, fn_depth, ctx, graphs);
      }
    }
    Yield { cont: Cont::Expr { args, body }, .. } => {
      bind_to_scope(scope, &args[0], scope_id, fn_depth, ctx, graphs);
      collect_scope_names(body, scope, scope_id, fn_depth, ctx, graphs);
    }
    Yield { .. } => {}
    // Terminal or branching — stop collecting
    _ => {}
  }
}

fn resolve_expr<'src>(
  expr: &Expr<'src>,
  scope: &ScopeMap<'src>,
  current_scope: CpsId,
  self_bind: Option<CpsId>,
  fn_depth: u32,
  ctx: &Ctx<'_, 'src>,
  graphs: &mut Graphs,
) {
  use ExprKind::*;
  let sb = self_bind;
  match &expr.kind {
    LetVal { name, val, body } => {
      resolve_val(val, scope, sb, fn_depth, ctx, graphs);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, name, current_scope, fn_depth, ctx, graphs);
      if let Cont::Expr { body: body_expr, .. } = body {
        resolve_expr(body_expr, &inner, current_scope, sb, fn_depth, ctx, graphs);
      }
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

      // Determine self_bind for the fn body: the hoisted name that binds
      // this LetFn's result. The CPS transform produces the fn bind in the
      // continuation — either as LetVal or MatchLetVal. Extract it from
      // the continuation's first bind node (if it's Cont::Expr).
      let cont_bind_id = if let Cont::Expr { body: cont_body, .. } = body {
        match &cont_body.kind {
          ExprKind::LetVal { name: cn, .. } => Some(cn.id),
          ExprKind::MatchLetVal { name: cn, .. } => Some(cn.id),
          _ => None,
        }
      } else {
        None
      };
      let fn_self_bind = cont_bind_id
        .and_then(|id| ctx.source_name(id))
        .and_then(|n| hoisted.get(n))
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

    LetRec { bindings, body } => {
      // All names visible in all fn_bodies and in body
      let mut rec_scope = scope.clone();
      for b in bindings {
        bind_to_scope(&mut rec_scope, &b.name, current_scope, fn_depth, ctx, graphs);
      }
      for b in bindings {
        let fn_scope_id = b.name.id;
        graphs.parent_scope.set(fn_scope_id, Some(current_scope));

        let rec_self_bind = ctx.source_name(b.name.id)
          .and_then(|n| rec_scope.get(n))
          .map(|entry| entry.bind_id);

        let mut fn_scope = rec_scope.clone();
        for p in &b.params {
          match p {
            Param::Name(n) | Param::Spread(n) =>
              bind_to_scope(&mut fn_scope, n, fn_scope_id, fn_depth + 1, ctx, graphs),
          }
        }
        resolve_expr(&b.fn_body, &fn_scope, fn_scope_id, rec_self_bind, fn_depth + 1, ctx, graphs);
      }
      if let Cont::Expr { body: body_expr, .. } = body {
        resolve_expr(body_expr, &rec_scope, current_scope, sb, fn_depth, ctx, graphs);
      }
    }

    App { func, args } => {
      resolve_callable(func, scope, sb, fn_depth, ctx, graphs);
      for arg in args {
        match arg {
          Arg::Val(v) | Arg::Spread(v) =>
            resolve_val(v, scope, sb, fn_depth, ctx, graphs),
          Arg::Cont(_) | Arg::Expr(_) => {} // produced by match_lower, not present during name resolution
        }
      }
      if let Some(Arg::Cont(Cont::Expr { args: cont_args, body })) = args.last() {
        let mut inner = scope.clone();
        bind_to_scope(&mut inner, &cont_args[0], current_scope, fn_depth, ctx, graphs);
        resolve_expr(body, &inner, current_scope, sb, fn_depth, ctx, graphs);
      }
    }

    If { cond, then, else_ } => {
      resolve_val(cond, scope, sb, fn_depth, ctx, graphs);
      resolve_expr(then, scope, current_scope, sb, fn_depth, ctx, graphs);
      resolve_expr(else_, scope, current_scope, sb, fn_depth, ctx, graphs);
    }

    Yield { value, cont } => {
      resolve_val(value, scope, sb, fn_depth, ctx, graphs);
      if let Cont::Expr { args, body } = cont {
        let mut inner = scope.clone();
        bind_to_scope(&mut inner, &args[0], current_scope, fn_depth, ctx, graphs);
        resolve_expr(body, &inner, current_scope, sb, fn_depth, ctx, graphs);
      }
    }

    // -- Pattern lowering primitives --

    MatchLetVal { name, val, fail, body } => {
      resolve_val(val, scope, sb, fn_depth, ctx, graphs);
      resolve_expr(fail, scope, current_scope, sb, fn_depth, ctx, graphs);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, name, current_scope, fn_depth, ctx, graphs);
      if let Cont::Expr { body: body_expr, .. } = body {
        resolve_expr(body_expr, &inner, current_scope, sb, fn_depth, ctx, graphs);
      }
    }

    MatchApp { func, args, fail, cont } => {
      resolve_callable(func, scope, sb, fn_depth, ctx, graphs);
      for v in args { resolve_val(v, scope, sb, fn_depth, ctx, graphs); }
      resolve_expr(fail, scope, current_scope, sb, fn_depth, ctx, graphs);
      if let Cont::Expr { args: cont_args, body } = cont {
        let mut inner = scope.clone();
        bind_to_scope(&mut inner, &cont_args[0], current_scope, fn_depth, ctx, graphs);
        resolve_expr(body, &inner, current_scope, sb, fn_depth, ctx, graphs);
      }
    }

    MatchIf { func, args, fail, body } => {
      resolve_callable(func, scope, sb, fn_depth, ctx, graphs);
      for v in args { resolve_val(v, scope, sb, fn_depth, ctx, graphs); }
      resolve_expr(fail, scope, current_scope, sb, fn_depth, ctx, graphs);
      if let Cont::Expr { body: body_expr, .. } = body {
        resolve_expr(body_expr, scope, current_scope, sb, fn_depth, ctx, graphs);
      }
    }

    MatchValue { val, fail, body, .. } => {
      resolve_val(val, scope, sb, fn_depth, ctx, graphs);
      resolve_expr(fail, scope, current_scope, sb, fn_depth, ctx, graphs);
      if let Cont::Expr { body: body_expr, .. } = body {
        resolve_expr(body_expr, scope, current_scope, sb, fn_depth, ctx, graphs);
      }
    }

    MatchSeq { val, fail, body, .. } => {
      resolve_val(val, scope, sb, fn_depth, ctx, graphs);
      resolve_expr(fail, scope, current_scope, sb, fn_depth, ctx, graphs);
      if let Cont::Expr { body: body_expr, .. } = body {
        resolve_expr(body_expr, scope, current_scope, sb, fn_depth, ctx, graphs);
      }
    }

    MatchNext { val, fail, cont } => {
      resolve_val(val, scope, sb, fn_depth, ctx, graphs);
      resolve_expr(fail, scope, current_scope, sb, fn_depth, ctx, graphs);
      if let Cont::Expr { args, body } = cont {
        let mut inner = scope.clone();
        // args[0] = elem bind, args[1] = next_cursor bind (cursor is synthetic, no source name)
        bind_to_scope(&mut inner, &args[0], current_scope, fn_depth, ctx, graphs);
        resolve_expr(body, &inner, current_scope, sb, fn_depth, ctx, graphs);
      }
    }

    MatchDone { val, fail, cont } => {
      resolve_val(val, scope, sb, fn_depth, ctx, graphs);
      resolve_expr(fail, scope, current_scope, sb, fn_depth, ctx, graphs);
      if let Cont::Expr { args, body } = cont {
        let mut inner = scope.clone();
        bind_to_scope(&mut inner, &args[0], current_scope, fn_depth, ctx, graphs);
        resolve_expr(body, &inner, current_scope, sb, fn_depth, ctx, graphs);
      }
    }

    MatchNotDone { val, fail, body } => {
      resolve_val(val, scope, sb, fn_depth, ctx, graphs);
      resolve_expr(fail, scope, current_scope, sb, fn_depth, ctx, graphs);
      if let Cont::Expr { body: body_expr, .. } = body {
        resolve_expr(body_expr, scope, current_scope, sb, fn_depth, ctx, graphs);
      }
    }

    MatchRest { val, fail, cont } => {
      resolve_val(val, scope, sb, fn_depth, ctx, graphs);
      resolve_expr(fail, scope, current_scope, sb, fn_depth, ctx, graphs);
      if let Cont::Expr { args, body } = cont {
        let mut inner = scope.clone();
        bind_to_scope(&mut inner, &args[0], current_scope, fn_depth, ctx, graphs);
        resolve_expr(body, &inner, current_scope, sb, fn_depth, ctx, graphs);
      }
    }

    MatchRec { val, fail, body, .. } => {
      resolve_val(val, scope, sb, fn_depth, ctx, graphs);
      resolve_expr(fail, scope, current_scope, sb, fn_depth, ctx, graphs);
      if let Cont::Expr { body: body_expr, .. } = body {
        resolve_expr(body_expr, scope, current_scope, sb, fn_depth, ctx, graphs);
      }
    }

    MatchField { val, fail, cont, .. } => {
      resolve_val(val, scope, sb, fn_depth, ctx, graphs);
      resolve_expr(fail, scope, current_scope, sb, fn_depth, ctx, graphs);
      if let Cont::Expr { args, body } = cont {
        let mut inner = scope.clone();
        // args[0] = field val bind, args[1] = next_cursor bind (synthetic)
        bind_to_scope(&mut inner, &args[0], current_scope, fn_depth, ctx, graphs);
        resolve_expr(body, &inner, current_scope, sb, fn_depth, ctx, graphs);
      }
    }

    MatchArm { matcher, body } => {
      // matcher and body both execute in the arm scope (already established by MatchBlock).
      // matcher args are arm_params — already bound by MatchBlock, not re-bound here.
      // body args are a fresh result bind (the bridge from matcher to body) — not pattern vars.
      // Pattern vars are bound inside matcher's body via LetVal nodes; they're in scope already.
      if let Cont::Expr { body, .. } = matcher {
        resolve_expr(body, scope, current_scope, sb, fn_depth, ctx, graphs);
      }
      if let Cont::Expr { args, body } = body {
        let mut inner = scope.clone();
        for arg in args.iter() { bind_to_scope(&mut inner, arg, current_scope, fn_depth, ctx, graphs); }
        resolve_expr(body, &inner, current_scope, sb, fn_depth, ctx, graphs);
      }
    }

    MatchBlock { params, arm_params, arms, cont } => {
      for v in params { resolve_val(v, scope, sb, fn_depth, ctx, graphs); }
      // Each arm introduces its own scope, identified by the arm Expr's CpsId.
      // All arm_params (scrutinee binds) are available in each arm scope.
      for arm in arms {
        let arm_scope_id = arm.id;
        graphs.parent_scope.set(arm_scope_id, Some(current_scope));
        let mut arm_scope = scope.clone();
        for param in arm_params.iter() {
          bind_to_scope(&mut arm_scope, param, arm_scope_id, fn_depth, ctx, graphs);
        }
        resolve_expr(arm, &arm_scope, arm_scope_id, sb, fn_depth, ctx, graphs);
      }
      if let Cont::Expr { args, body } = cont {
        let mut inner = scope.clone();
        bind_to_scope(&mut inner, &args[0], current_scope, fn_depth, ctx, graphs);
        resolve_expr(body, &inner, current_scope, sb, fn_depth, ctx, graphs);
      }
    }

    Panic | FailCont | FailRef(_) => {}
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
      LetVal { val, body, .. } => {
        emit_classified_val(val, result, ctx, out);
        if let Cont::Expr { body: body_expr, .. } = body {
          collect_classified_lines(body_expr, result, ctx, out);
        }
      }

      LetFn { fn_body, body, .. } => {
        collect_classified_lines(fn_body, result, ctx, out);
        if let Cont::Expr { body: body_expr, .. } = body {
          collect_classified_lines(body_expr, result, ctx, out);
        }
      }

      LetRec { bindings, body } => {
        for b in bindings {
          collect_classified_lines(&b.fn_body, result, ctx, out);
        }
        if let Cont::Expr { body: body_expr, .. } = body {
          collect_classified_lines(body_expr, result, ctx, out);
        }
      }

      App { func, args } => {
        emit_classified_callable(func, result, ctx, out);
        for arg in args {
          match arg { Arg::Val(v) | Arg::Spread(v) => emit_classified_val(v, result, ctx, out), Arg::Cont(_) | Arg::Expr(_) => {} }
        }
        if let Some(Arg::Cont(Cont::Expr { body, .. })) = args.last() { collect_classified_lines(body, result, ctx, out); }
      }

      If { cond, then, else_ } => {
        emit_classified_val(cond, result, ctx, out);
        collect_classified_lines(then, result, ctx, out);
        collect_classified_lines(else_, result, ctx, out);
      }

      Yield { value, cont } => {
        emit_classified_val(value, result, ctx, out);
        if let Cont::Expr { body, .. } = cont { collect_classified_lines(body, result, ctx, out); }
      }

      MatchLetVal { val, fail, body, .. } => {
        emit_classified_val(val, result, ctx, out);
        collect_classified_lines(fail, result, ctx, out);
        if let Cont::Expr { body: body_expr, .. } = body { collect_classified_lines(body_expr, result, ctx, out); }
      }
      MatchApp { func, args, fail, cont } => {
        emit_classified_callable(func, result, ctx, out);
        for v in args { emit_classified_val(v, result, ctx, out); }
        collect_classified_lines(fail, result, ctx, out);
        if let Cont::Expr { body, .. } = cont { collect_classified_lines(body, result, ctx, out); }
      }
      MatchIf { func, args, fail, body } => {
        emit_classified_callable(func, result, ctx, out);
        for v in args { emit_classified_val(v, result, ctx, out); }
        collect_classified_lines(fail, result, ctx, out);
        if let Cont::Expr { body: body_expr, .. } = body { collect_classified_lines(body_expr, result, ctx, out); }
      }
      MatchValue { val, fail, body, .. } => {
        emit_classified_val(val, result, ctx, out);
        collect_classified_lines(fail, result, ctx, out);
        if let Cont::Expr { body: body_expr, .. } = body { collect_classified_lines(body_expr, result, ctx, out); }
      }
      MatchSeq { val, fail, body, .. } => {
        emit_classified_val(val, result, ctx, out);
        collect_classified_lines(fail, result, ctx, out);
        if let Cont::Expr { body: body_expr, .. } = body { collect_classified_lines(body_expr, result, ctx, out); }
      }
      MatchNext { fail, cont, .. } => {
        collect_classified_lines(fail, result, ctx, out);
        if let Cont::Expr { body, .. } = cont { collect_classified_lines(body, result, ctx, out); }
      }
      MatchDone { fail, cont, .. } => {
        collect_classified_lines(fail, result, ctx, out);
        if let Cont::Expr { body, .. } = cont { collect_classified_lines(body, result, ctx, out); }
      }
      MatchNotDone { fail, body, .. } => {
        collect_classified_lines(fail, result, ctx, out);
        if let Cont::Expr { body: body_expr, .. } = body { collect_classified_lines(body_expr, result, ctx, out); }
      }
      MatchRest { fail, cont, .. } => {
        collect_classified_lines(fail, result, ctx, out);
        if let Cont::Expr { body, .. } = cont { collect_classified_lines(body, result, ctx, out); }
      }
      MatchRec { val, fail, body, .. } => {
        emit_classified_val(val, result, ctx, out);
        collect_classified_lines(fail, result, ctx, out);
        if let Cont::Expr { body: body_expr, .. } = body { collect_classified_lines(body_expr, result, ctx, out); }
      }
      MatchField { fail, cont, .. } => {
        collect_classified_lines(fail, result, ctx, out);
        if let Cont::Expr { body, .. } = cont { collect_classified_lines(body, result, ctx, out); }
      }
      MatchArm { matcher, body } => {
        if let Cont::Expr { body, .. } = matcher { collect_classified_lines(body, result, ctx, out); }
        if let Cont::Expr { body, .. } = body    { collect_classified_lines(body, result, ctx, out); }
      }
      MatchBlock { params, arms, cont, .. } => {
        for v in params { emit_classified_val(v, result, ctx, out); }
        for arm in arms {
          collect_classified_lines(arm, result, ctx, out);
        }
        if let Cont::Expr { body, .. } = cont { collect_classified_lines(body, result, ctx, out); }
      }

      Panic | FailCont | FailRef(_) => {}
    }
  }

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

  test_macros::include_fink_tests!("src/passes/name_res/test_name_res.fnk");
}
