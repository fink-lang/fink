// Name resolution pass.
//
// Walks the CPS IR and resolves every Ref::Name to its Bind, producing a
// `PropGraph<CpsId, Option<CpsId>>` — the `resolves_to` map.
//
// (ref)--[:RESOLVES_TO]-->(bind)
//
// Each entry maps a Ref node's CpsId to the CpsId of the BindNode it refers to.
// `None` = unresolved name (error). Only Ref::Name nodes get entries;
// Ref::Gen already carry their target structurally.
//
// # Algorithm
//
// Maintain a scope map (`&str → CpsId`) while walking the tree.
// On each binding site (LetVal, LetFn, App result, params, etc.),
// insert the name into the scope. On each Ref::Name, look up the name
// and record the mapping. Scope is functional (clone on fn boundary).

use std::collections::HashMap;
use crate::ast::{AstId, Node as AstNode, NodeKind};
use crate::propgraph::PropGraph;
use super::cps::ir::{
  Arg, Bind, BindNode, Callable, CpsId, Expr, ExprKind,
  Param, Ref, Val, ValKind,
};

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

/// Output of the name resolution pass.
pub struct ResolveResult {
  /// Maps each Ref node's CpsId → the CpsId of the Bind it resolves to.
  /// Sized to the full CpsId space; most entries `None`.
  pub resolves_to: PropGraph<CpsId, Option<CpsId>>,
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
  let mut resolves_to: PropGraph<CpsId, Option<CpsId>> =
    PropGraph::with_size(node_count, None);
  let scope = HashMap::new();
  resolve_expr(expr, &scope, &ctx, &mut resolves_to);
  ResolveResult { resolves_to }
}

// ---------------------------------------------------------------------------
// Scope — map from source name to bind CpsId
// ---------------------------------------------------------------------------

type Scope<'src> = HashMap<&'src str, CpsId>;

/// Insert a BindNode into the scope (if it has a source name).
fn bind_to_scope<'src>(
  scope: &mut Scope<'src>,
  bind: &BindNode,
  ctx: &Ctx<'_, 'src>,
) {
  if let Bind::User = bind.kind {
    if let Some(name) = ctx.source_name(bind.id) {
      if name != "_" {
        scope.insert(name, bind.id);
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Resolve a Ref — look up in scope, record in resolves_to
// ---------------------------------------------------------------------------

fn resolve_val<'src>(
  val: &Val<'src>,
  scope: &Scope<'src>,
  ctx: &Ctx<'_, 'src>,
  resolves_to: &mut PropGraph<CpsId, Option<CpsId>>,
) {
  if let ValKind::Ref(ref_) = &val.kind {
    match ref_ {
      Ref::Name => {
        if let Some(name) = ctx.source_name(val.id) {
          if let Some(&bind_id) = scope.get(name) {
            resolves_to.set(val.id, Some(bind_id));
          }
          // else: unresolved — stays None
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
  scope: &Scope<'src>,
  ctx: &Ctx<'_, 'src>,
  resolves_to: &mut PropGraph<CpsId, Option<CpsId>>,
) {
  if let Callable::Val(val) = callable {
    resolve_val(val, scope, ctx, resolves_to);
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
  ctx: &Ctx<'_, 'src>,
) {
  use ExprKind::*;
  match &expr.kind {
    LetFn { name, body, .. } => {
      bind_to_scope(scope, name, ctx);
      collect_scope_names(body, scope, ctx);
    }
    LetVal { name, body, .. } => {
      bind_to_scope(scope, name, ctx);
      collect_scope_names(body, scope, ctx);
    }
    MatchLetVal { name, body, .. } => {
      bind_to_scope(scope, name, ctx);
      collect_scope_names(body, scope, ctx);
    }
    App { result, body, .. } => {
      bind_to_scope(scope, result, ctx);
      collect_scope_names(body, scope, ctx);
    }
    Yield { result, body, .. } => {
      bind_to_scope(scope, result, ctx);
      collect_scope_names(body, scope, ctx);
    }
    // Terminal or branching — stop collecting
    _ => {}
  }
}

fn resolve_expr<'src>(
  expr: &Expr<'src>,
  scope: &Scope<'src>,
  ctx: &Ctx<'_, 'src>,
  resolves_to: &mut PropGraph<CpsId, Option<CpsId>>,
) {
  use ExprKind::*;
  match &expr.kind {
    Ret(val) => {
      resolve_val(val, scope, ctx, resolves_to);
    }

    LetVal { name, val, body } => {
      resolve_val(val, scope, ctx, resolves_to);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, name, ctx);
      resolve_expr(body, &inner, ctx, resolves_to);
    }

    LetFn { name, params, fn_body, body, .. } => {
      // Fn bodies see all names at this scope level (hoisted), enabling
      // self- and mutual recursion. Collect all User bind names from the
      // entire continuation chain starting here.
      let mut hoisted = scope.clone();
      collect_scope_names(expr, &mut hoisted, ctx);

      // fn_body sees hoisted scope + params
      let mut fn_scope = hoisted.clone();
      for p in params {
        match p {
          Param::Name(b) | Param::Spread(b) => bind_to_scope(&mut fn_scope, b, ctx),
        }
      }
      resolve_expr(fn_body, &fn_scope, ctx, resolves_to);

      // continuation scope: sequential (only names defined so far)
      let mut cont_scope = scope.clone();
      bind_to_scope(&mut cont_scope, name, ctx);
      resolve_expr(body, &cont_scope, ctx, resolves_to);
    }

    LetRec { bindings, body } => {
      // All names visible in all fn_bodies and in body
      let mut rec_scope = scope.clone();
      for b in bindings {
        bind_to_scope(&mut rec_scope, &b.name, ctx);
      }
      for b in bindings {
        let mut fn_scope = rec_scope.clone();
        for p in &b.params {
          match p {
            Param::Name(n) | Param::Spread(n) => bind_to_scope(&mut fn_scope, n, ctx),
          }
        }
        resolve_expr(&b.fn_body, &fn_scope, ctx, resolves_to);
      }
      resolve_expr(body, &rec_scope, ctx, resolves_to);
    }

    App { func, args, result, body } => {
      resolve_callable(func, scope, ctx, resolves_to);
      for arg in args {
        match arg {
          Arg::Val(v) | Arg::Spread(v) => resolve_val(v, scope, ctx, resolves_to),
        }
      }
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, result, ctx);
      resolve_expr(body, &inner, ctx, resolves_to);
    }

    If { cond, then, else_ } => {
      resolve_val(cond, scope, ctx, resolves_to);
      resolve_expr(then, scope, ctx, resolves_to);
      resolve_expr(else_, scope, ctx, resolves_to);
    }

    Yield { value, result, body } => {
      resolve_val(value, scope, ctx, resolves_to);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, result, ctx);
      resolve_expr(body, &inner, ctx, resolves_to);
    }

    // -- Pattern lowering primitives --

    MatchLetVal { name, val, fail, body } => {
      resolve_val(val, scope, ctx, resolves_to);
      resolve_expr(fail, scope, ctx, resolves_to);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, name, ctx);
      resolve_expr(body, &inner, ctx, resolves_to);
    }

    MatchApp { func, args, fail, result, body } => {
      resolve_callable(func, scope, ctx, resolves_to);
      for v in args { resolve_val(v, scope, ctx, resolves_to); }
      resolve_expr(fail, scope, ctx, resolves_to);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, result, ctx);
      resolve_expr(body, &inner, ctx, resolves_to);
    }

    MatchIf { func, args, fail, body } => {
      resolve_callable(func, scope, ctx, resolves_to);
      for v in args { resolve_val(v, scope, ctx, resolves_to); }
      resolve_expr(fail, scope, ctx, resolves_to);
      resolve_expr(body, scope, ctx, resolves_to);
    }

    MatchValue { val, fail, body, .. } => {
      resolve_val(val, scope, ctx, resolves_to);
      resolve_expr(fail, scope, ctx, resolves_to);
      resolve_expr(body, scope, ctx, resolves_to);
    }

    MatchSeq { val, fail, body, .. } => {
      resolve_val(val, scope, ctx, resolves_to);
      resolve_expr(fail, scope, ctx, resolves_to);
      resolve_expr(body, scope, ctx, resolves_to);
    }

    MatchNext { fail, elem, body, .. } => {
      resolve_expr(fail, scope, ctx, resolves_to);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, elem, ctx);
      resolve_expr(body, &inner, ctx, resolves_to);
    }

    MatchDone { fail, result, body, .. } => {
      resolve_expr(fail, scope, ctx, resolves_to);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, result, ctx);
      resolve_expr(body, &inner, ctx, resolves_to);
    }

    MatchNotDone { fail, body, .. } => {
      resolve_expr(fail, scope, ctx, resolves_to);
      resolve_expr(body, scope, ctx, resolves_to);
    }

    MatchRest { fail, result, body, .. } => {
      resolve_expr(fail, scope, ctx, resolves_to);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, result, ctx);
      resolve_expr(body, &inner, ctx, resolves_to);
    }

    MatchRec { val, fail, body, .. } => {
      resolve_val(val, scope, ctx, resolves_to);
      resolve_expr(fail, scope, ctx, resolves_to);
      resolve_expr(body, scope, ctx, resolves_to);
    }

    MatchField { fail, elem, body, .. } => {
      resolve_expr(fail, scope, ctx, resolves_to);
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, elem, ctx);
      resolve_expr(body, &inner, ctx, resolves_to);
    }

    MatchBlock { params, fail, arm_params, arms, result, body } => {
      for v in params { resolve_val(v, scope, ctx, resolves_to); }
      resolve_expr(fail, scope, ctx, resolves_to);
      // Each arm gets the arm_params in scope
      for (arm, param) in arms.iter().zip(arm_params.iter()) {
        let mut arm_scope = scope.clone();
        bind_to_scope(&mut arm_scope, param, ctx);
        resolve_expr(arm, &arm_scope, ctx, resolves_to);
      }
      let mut inner = scope.clone();
      bind_to_scope(&mut inner, result, ctx);
      resolve_expr(body, &inner, ctx, resolves_to);
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use crate::parser::parse;
  use crate::ast::build_index;
  use crate::passes::cps::transform::lower_expr;
  use super::*;

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
