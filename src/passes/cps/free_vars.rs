// TODO [deprecated]: this entire pass is subsumed by the name resolution pass.
// Once resolve produces `PropGraph<CpsId, Option<Resolution>>`, free vars are
// derivable from `Resolution::Captured` entries. Remove this file and the
// `free_vars` field on `LetFn` together.
//
// Free variable analysis pass.
//
// Annotates each `LetFn` with the CpsIds of Ref nodes it reads directly from
// the enclosing scope — i.e. refs in the fn body not covered by the fn's own
// params or by bindings settled within the same body.
//
// Names are recovered from the origin map (CpsId → AstId → AST ident).
//
// # Algorithm
//
// Walk the `LetFn.fn_body` and collect every `ValKind::Ref(RefKind::Name)`
// encountered, in first-encounter order, deduplicating via a seen set keyed
// by the source name (looked up through the origin map). Exclude:
//   - Names bound by the fn's own `params`
//   - Names settled by bindings *within the same fn body*
//   - The wildcard `_`
//
// Stop at nested `LetFn` boundaries — their free vars are their own concern
// (they have already been annotated by the time we process the outer fn,
// because we recurse depth-first / bottom-up).
//
// # Ordering
//
// The first ref encountered during a left-to-right walk of the body is
// listed first. This matches the order `·load` calls appear in the formatter
// output.

use std::collections::HashSet;
use crate::ast::{AstId, Node as AstNode, NodeKind};
use crate::propgraph::PropGraph;
use super::ir::{Arg, BindName, Callable, CpsId, Expr, ExprKind, FreeVar, Param, Val, ValKind, RefKind};

// ---------------------------------------------------------------------------
// Origin-based name lookup context
// ---------------------------------------------------------------------------

/// Minimal context for looking up source names from CpsIds.
struct Ctx<'a, 'src> {
  origin: &'a PropGraph<CpsId, Option<AstId>>,
  ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
}

impl<'a, 'src> Ctx<'a, 'src> {
  /// Recover the source name for a CPS node from its AST origin.
  fn source_name(&self, cps_id: CpsId) -> Option<&'src str> {
    let ast_id = (*self.origin.try_get(cps_id)?)?;
    let node = (*self.ast_index.try_get(ast_id)?)?;
    match &node.kind {
      NodeKind::Ident(s) => Some(s),
      _ => None,
    }
  }

  /// Recover the source name for a Bind node.
  fn bind_name(&self, bind: &super::ir::Bind) -> Option<&'src str> {
    match bind.kind {
      BindName::User => self.source_name(bind.id),
      BindName::Gen(_) => None,
    }
  }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Annotate every `LetFn` in `expr` with its free variables.
/// Operates bottom-up: inner fns are annotated before outer ones.
/// Requires origin map and AST index for name recovery.
#[deprecated(note = "will be subsumed by name resolution / static analysis pass")]
pub fn annotate<'src>(
  expr: Expr<'src>,
  origin: &PropGraph<CpsId, Option<AstId>>,
  ast_index: &PropGraph<AstId, Option<&'src AstNode<'src>>>,
) -> Expr<'src> {
  let ctx = Ctx { origin, ast_index };
  transform_expr(expr, &ctx)
}

// ---------------------------------------------------------------------------
// Recursive transform
// ---------------------------------------------------------------------------

fn transform_expr<'src>(expr: Expr<'src>, ctx: &Ctx<'_, 'src>) -> Expr<'src> {
  use ExprKind::*;
  let id = expr.id;
  let kind = match expr.kind {
    LetVal { name, val, body } => LetVal {
      name,
      val,
      body: Box::new(transform_expr(*body, ctx)),
    },

    LetFn { name, params, fn_body, body, .. } => {
      // Recurse into fn_body first (bottom-up).
      let fn_body = transform_expr(*fn_body, ctx);
      // Recurse into body (the continuation after the closure is created).
      let body = transform_expr(*body, ctx);
      // Compute free vars for this fn.
      let bound: HashSet<&str> = params.iter().filter_map(|p| match p {
        Param::Name(n) | Param::Spread(n) => match n.kind {
          BindName::User => {
            let s = ctx.source_name(n.id)?;
            if s == "_" { None } else { Some(s) }
          }
          BindName::Gen(_) => None,
        },
      }).collect();
      let mut seen: HashSet<&str> = HashSet::new();
      let mut free_vars: Vec<FreeVar> = vec![];
      collect_refs(&fn_body, &bound, &mut seen, &mut free_vars, ctx);
      LetFn { name, params, free_vars, fn_body: Box::new(fn_body), body: Box::new(body) }
    }

    LetRec { bindings, body } => LetRec {
      bindings: bindings.into_iter().map(|mut b| {
        b.fn_body = Box::new(transform_expr(*b.fn_body, ctx));
        b
      }).collect(),
      body: Box::new(transform_expr(*body, ctx)),
    },

    App { func, args, result, body } => App {
      func, args, result,
      body: Box::new(transform_expr(*body, ctx)),
    },

    If { cond, then, else_ } => If {
      cond,
      then: Box::new(transform_expr(*then, ctx)),
      else_: Box::new(transform_expr(*else_, ctx)),
    },

    Yield { value, result, body } => Yield {
      value, result,
      body: Box::new(transform_expr(*body, ctx)),
    },

    Ret(_) | Panic | FailCont => expr.kind,

    // Pattern lowering primitives — recurse into both fail and body.
    MatchLetVal { name, val, fail, body } => MatchLetVal {
      name, val,
      fail: Box::new(transform_expr(*fail, ctx)),
      body: Box::new(transform_expr(*body, ctx)),
    },
    MatchApp { func, args, fail, result, body } => MatchApp {
      func, args, result,
      fail: Box::new(transform_expr(*fail, ctx)),
      body: Box::new(transform_expr(*body, ctx)),
    },
    MatchIf { func, args, fail, body } => MatchIf {
      func, args,
      fail: Box::new(transform_expr(*fail, ctx)),
      body: Box::new(transform_expr(*body, ctx)),
    },
    MatchValue { val, lit, fail, body } => MatchValue {
      val, lit,
      fail: Box::new(transform_expr(*fail, ctx)),
      body: Box::new(transform_expr(*body, ctx)),
    },
    MatchSeq { val, cursor, fail, body } => MatchSeq {
      val, cursor,
      fail: Box::new(transform_expr(*fail, ctx)),
      body: Box::new(transform_expr(*body, ctx)),
    },
    MatchNext { val, cursor, next_cursor, fail, elem, body } => MatchNext {
      val, cursor, next_cursor, elem,
      fail: Box::new(transform_expr(*fail, ctx)),
      body: Box::new(transform_expr(*body, ctx)),
    },
    MatchDone { val, cursor, fail, result, body } => MatchDone {
      val, cursor, result,
      fail: Box::new(transform_expr(*fail, ctx)),
      body: Box::new(transform_expr(*body, ctx)),
    },
    MatchNotDone { val, cursor, fail, body } => MatchNotDone {
      val, cursor,
      fail: Box::new(transform_expr(*fail, ctx)),
      body: Box::new(transform_expr(*body, ctx)),
    },
    MatchRest { val, cursor, fail, result, body } => MatchRest {
      val, cursor, result,
      fail: Box::new(transform_expr(*fail, ctx)),
      body: Box::new(transform_expr(*body, ctx)),
    },
    MatchRec { val, cursor, fail, body } => MatchRec {
      val, cursor,
      fail: Box::new(transform_expr(*fail, ctx)),
      body: Box::new(transform_expr(*body, ctx)),
    },
    MatchField { val, cursor, next_cursor, field, fail, elem, body } => MatchField {
      val, cursor, next_cursor, field, elem,
      fail: Box::new(transform_expr(*fail, ctx)),
      body: Box::new(transform_expr(*body, ctx)),
    },
    MatchBlock { params, arm_params, fail, arms, result, body } => MatchBlock {
      params,
      arm_params,
      result,
      fail: Box::new(transform_expr(*fail, ctx)),
      arms: arms.into_iter().map(|a| transform_expr(a, ctx)).collect(),
      body: Box::new(transform_expr(*body, ctx)),
    },
  };
  Expr { id, kind }
}

// ---------------------------------------------------------------------------
// Ref collector — walks an Expr, stops at LetFn boundaries
// ---------------------------------------------------------------------------

/// Collect `Ref` CpsIds from `expr` into `out` (dedup via `seen` by source name),
/// excluding names in `bound` (params + settled names at this level).
/// Does NOT descend into `LetFn.fn_body` (its refs are its own).
fn collect_refs<'src>(
  expr: &Expr<'src>,
  bound: &HashSet<&'src str>,
  seen: &mut HashSet<&'src str>,
  out: &mut Vec<FreeVar>,
  ctx: &Ctx<'_, 'src>,
) {
  use ExprKind::*;
  match &expr.kind {
    Ret(val) => collect_ref_from_val(val, bound, seen, out, ctx),

    LetVal { val, body, name } => {
      collect_ref_from_val(val, bound, seen, out, ctx);
      let mut inner_bound = bound.clone();
      if let Some(s) = ctx.bind_name(name) {
        inner_bound.insert(s);
      }
      collect_refs(body, &inner_bound, seen, out, ctx);
    }

    // Stop at nested LetFn — its fn_body refs are its own.
    // Do continue into `body` (the continuation at our scope level).
    LetFn { body, .. } => {
      collect_refs(body, bound, seen, out, ctx);
    }

    LetRec { body, .. } => {
      collect_refs(body, bound, seen, out, ctx);
    }

    App { func, args, body, .. } => {
      collect_ref_from_callable(func, bound, seen, out, ctx);
      for arg in args {
        let v = match arg { Arg::Val(v) | Arg::Spread(v) => v };
        collect_ref_from_val(v, bound, seen, out, ctx);
      }
      collect_refs(body, bound, seen, out, ctx);
    }

    Yield { value, result, body } => {
      collect_ref_from_val(value, bound, seen, out, ctx);
      let mut inner_bound = bound.clone();
      if let Some(s) = ctx.bind_name(result) {
        inner_bound.insert(s);
      }
      collect_refs(body, &inner_bound, seen, out, ctx);
    }

    If { cond, then, else_ } => {
      collect_ref_from_val(cond, bound, seen, out, ctx);
      collect_refs(then, bound, seen, out, ctx);
      collect_refs(else_, bound, seen, out, ctx);
    }

    MatchLetVal { name, val, fail, body } => {
      collect_ref_from_val(val, bound, seen, out, ctx);
      collect_refs(fail, bound, seen, out, ctx);
      let mut inner = bound.clone();
      if let Some(s) = ctx.bind_name(name) { inner.insert(s); }
      collect_refs(body, &inner, seen, out, ctx);
    }
    MatchApp { func, args, result, fail, body } => {
      collect_ref_from_callable(func, bound, seen, out, ctx);
      for v in args { collect_ref_from_val(v, bound, seen, out, ctx); }
      collect_refs(fail, bound, seen, out, ctx);
      let mut inner = bound.clone();
      if let Some(s) = ctx.bind_name(result) { inner.insert(s); }
      collect_refs(body, &inner, seen, out, ctx);
    }
    MatchIf { func, args, fail, body } => {
      collect_ref_from_callable(func, bound, seen, out, ctx);
      for v in args { collect_ref_from_val(v, bound, seen, out, ctx); }
      collect_refs(fail, bound, seen, out, ctx);
      collect_refs(body, bound, seen, out, ctx);
    }
    MatchValue { val, fail, body, .. } => {
      collect_ref_from_val(val, bound, seen, out, ctx);
      collect_refs(fail, bound, seen, out, ctx);
      collect_refs(body, bound, seen, out, ctx);
    }
    MatchSeq { val, fail, body, .. } => {
      collect_ref_from_val(val, bound, seen, out, ctx);
      collect_refs(fail, bound, seen, out, ctx);
      collect_refs(body, bound, seen, out, ctx);
    }
    MatchNext { elem, fail, body, .. } => {
      collect_refs(fail, bound, seen, out, ctx);
      let mut inner = bound.clone();
      if let Some(s) = ctx.bind_name(elem) { inner.insert(s); }
      collect_refs(body, &inner, seen, out, ctx);
    }
    MatchDone { result, fail, body, .. } => {
      collect_refs(fail, bound, seen, out, ctx);
      let mut inner = bound.clone();
      if let Some(s) = ctx.bind_name(result) { inner.insert(s); }
      collect_refs(body, &inner, seen, out, ctx);
    }
    MatchNotDone { fail, body, .. } => {
      collect_refs(fail, bound, seen, out, ctx);
      collect_refs(body, bound, seen, out, ctx);
    }
    MatchRest { result, fail, body, .. } => {
      collect_refs(fail, bound, seen, out, ctx);
      let mut inner = bound.clone();
      if let Some(s) = ctx.bind_name(result) { inner.insert(s); }
      collect_refs(body, &inner, seen, out, ctx);
    }
    MatchRec { val, fail, body, .. } => {
      collect_ref_from_val(val, bound, seen, out, ctx);
      collect_refs(fail, bound, seen, out, ctx);
      collect_refs(body, bound, seen, out, ctx);
    }
    MatchField { elem, fail, body, .. } => {
      collect_refs(fail, bound, seen, out, ctx);
      let mut inner = bound.clone();
      if let Some(s) = ctx.bind_name(elem) { inner.insert(s); }
      collect_refs(body, &inner, seen, out, ctx);
    }
    MatchBlock { params, fail, arms, body, .. } => {
      for s in params { collect_ref_from_val(s, bound, seen, out, ctx); }
      collect_refs(fail, bound, seen, out, ctx);
      for arm in arms {
        collect_refs(arm, bound, seen, out, ctx);
      }
      collect_refs(body, bound, seen, out, ctx);
    }

    Panic | FailCont => {}
  }
}

fn collect_ref_from_val<'src>(
  val: &Val<'src>,
  bound: &HashSet<&'src str>,
  seen: &mut HashSet<&'src str>,
  out: &mut Vec<FreeVar>,
  ctx: &Ctx<'_, 'src>,
) {
  if let ValKind::Ref(ref_) = &val.kind {
    match &ref_.kind {
      RefKind::Name => {
        if let Some(n) = ctx.source_name(val.id) {
          if n != "_" && !bound.contains(n) && seen.insert(n) {
            out.push(val.id);
          }
        }
      }
      RefKind::Bind(_) => {
        // Gen param references — always bound in the enclosing fn, never free.
      }
    }
  }
}

/// Collect free-variable references from a `Callable`. `Op` variants have no
/// runtime references; only `Val` arms can introduce free vars.
fn collect_ref_from_callable<'src>(
  callable: &Callable<'src>,
  bound: &HashSet<&'src str>,
  seen: &mut HashSet<&'src str>,
  out: &mut Vec<FreeVar>,
  ctx: &Ctx<'_, 'src>,
) {
  if let Callable::Val(val) = callable {
    collect_ref_from_val(val, bound, seen, out, ctx);
  }
}


#[cfg(test)]
mod free_var_tests {
  use crate::parser::parse;
  use crate::ast::build_index;
  use crate::passes::cps::fmt::{fmt_with, Ctx};
  use crate::passes::cps::transform::lower_expr;
  #[allow(deprecated)]
  use super::annotate;

  #[allow(deprecated)]
  fn cps_free_vars(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let cps = lower_expr(&r.root);
        let annotated = annotate(cps.root, &cps.origin, &ast_index);
        let ctx = Ctx { origin: &cps.origin, ast_index: &ast_index };
        fmt_with(&annotated, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  test_macros::include_fink_tests!("src/passes/cps/test_cps_free_vars.fnk");
}
