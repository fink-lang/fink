// Closure capture pass.
//
// Consumes the name resolution result and produces a capture graph:
// a PropGraph mapping each LetFn's name CpsId to its ordered capture list.
//
// A LetFn is a closure if any Ref inside its immediate body resolves to
// Resolution::Captured { depth: 1, .. } relative to that LetFn.
// Deeper captures (depth > 1) are propagated: intermediate LetFn nodes
// that must thread values inward also appear in the capture graph.
//
// Input:  CpsResult + ResolveResult
// Output: PropGraph<CpsId, Vec<&'src str>>  — capture list per LetFn name node
//         (empty vec = pure function)
//
// See docs/name-resolution-design.md for scope/resolution background.

use crate::propgraph::PropGraph;
use crate::passes::name_res::{Resolution, ResolveResult};
use crate::passes::cps::ir::{Arg, Callable, Cont, CpsId, CpsResult, Expr, ExprKind, Ref, Val, ValKind};
use crate::ast::{AstId, NodeKind};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Capture graph — maps each LetFn's name-bind CpsId to its ordered capture list.
/// Pure functions map to an empty Vec.
pub type CaptureGraph<'src> = PropGraph<CpsId, Vec<&'src str>>;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Build the capture graph for all LetFn nodes in `result`.
pub fn analyse<'src>(
  result: &CpsResult<'src>,
  resolve: &ResolveResult,
  ast_index: &PropGraph<AstId, Option<&'src crate::ast::Node<'src>>>,
) -> CaptureGraph<'src> {
  let node_count = result.origin.len();
  let mut graph: CaptureGraph<'src> = PropGraph::with_size(node_count, Vec::new());
  collect_expr(&result.root, resolve, &result.origin, ast_index, 0, &mut graph);
  graph
}

// ---------------------------------------------------------------------------
// Name recovery
// ---------------------------------------------------------------------------

fn source_name<'src>(
  cps_id: CpsId,
  origin: &PropGraph<CpsId, Option<AstId>>,
  ast_index: &PropGraph<AstId, Option<&'src crate::ast::Node<'src>>>,
) -> Option<&'src str> {
  let ast_id = (*origin.try_get(cps_id)?)?;
  let node = (*ast_index.try_get(ast_id)?)?;
  match &node.kind {
    NodeKind::Ident(s) => Some(s),
    _ => None,
  }
}

// ---------------------------------------------------------------------------
// Walk — collect captures per LetFn
// ---------------------------------------------------------------------------

fn collect_val<'src>(
  val: &Val<'src>,
  resolve: &ResolveResult,
  origin: &PropGraph<CpsId, Option<AstId>>,
  ast_index: &PropGraph<AstId, Option<&'src crate::ast::Node<'src>>>,
  captures: &mut Vec<&'src str>,
) {
  if let ValKind::Ref(Ref::Name) = &val.kind
    && let Some(Resolution::Captured { depth, bind }) =
      resolve.resolution.try_get(val.id).and_then(|r| r.as_ref())
  {
    // Any Captured ref seen in the fn body (with nested fn bodies skipped by
    // collect_captured_in_body) is directly captured by this fn.
    if *depth >= 1
      && let Some(name) = source_name(*bind, origin, ast_index)
      && !captures.contains(&name)
    {
      captures.push(name);
    }
  }
}

fn collect_callable<'src>(
  callable: &Callable<'src>,
  resolve: &ResolveResult,
  origin: &PropGraph<CpsId, Option<AstId>>,
  ast_index: &PropGraph<AstId, Option<&'src crate::ast::Node<'src>>>,
  captures: &mut Vec<&'src str>,
) {
  if let Callable::Val(v) = callable {
    collect_val(v, resolve, origin, ast_index, captures);
  }
}

/// Walk a Cont: if Ref, collect the val; if Expr, recurse into the body.
fn collect_cont<'src>(
  cont: &Cont<'src>,
  resolve: &ResolveResult,
  origin: &PropGraph<CpsId, Option<AstId>>,
  ast_index: &PropGraph<AstId, Option<&'src crate::ast::Node<'src>>>,
  captures: &mut Vec<&'src str>,
) {
  match cont {
    Cont::Ref(_cont_id) => {} // cont param ref — resolved by construction, no capture
    Cont::Expr { body, .. } => collect_captured_in_body(body, resolve, origin, ast_index, captures),
  }
}

/// Collect all refs directly captured by the enclosing fn (Captured { depth: 1 }).
/// Does not descend into nested LetFn bodies — those are handled by collect_expr.
fn collect_captured_in_body<'src>(
  expr: &Expr<'src>,
  resolve: &ResolveResult,
  origin: &PropGraph<CpsId, Option<AstId>>,
  ast_index: &PropGraph<AstId, Option<&'src crate::ast::Node<'src>>>,
  captures: &mut Vec<&'src str>,
) {
  use ExprKind::*;
  match &expr.kind {
    LetVal { val, body, .. } => {
      collect_val(val, resolve, origin, ast_index, captures);
      collect_cont(body, resolve, origin, ast_index, captures);
    }

    LetFn { body, .. } => {
      // Don't descend into fn_body — captures inside nested fns belong to those
      // fns and are registered separately by collect_expr. Only walk the
      // continuation (same scope level as the outer fn).
      collect_cont(body, resolve, origin, ast_index, captures);
    }

    App { func, args, cont } => {
      collect_callable(func, resolve, origin, ast_index, captures);
      for arg in args {
        match arg {
          Arg::Val(v) | Arg::Spread(v) => collect_val(v, resolve, origin, ast_index, captures),
        }
      }
      collect_cont(cont, resolve, origin, ast_index, captures);
    }

    If { cond, then, else_ } => {
      collect_val(cond, resolve, origin, ast_index, captures);
      collect_captured_in_body(then, resolve, origin, ast_index, captures);
      collect_captured_in_body(else_, resolve, origin, ast_index, captures);
    }

    Yield { value, cont } => {
      collect_val(value, resolve, origin, ast_index, captures);
      collect_cont(cont, resolve, origin, ast_index, captures);
    }

    MatchLetVal { val, fail, body, .. } => {
      collect_val(val, resolve, origin, ast_index, captures);
      collect_captured_in_body(fail, resolve, origin, ast_index, captures);
      collect_cont(body, resolve, origin, ast_index, captures);
    }

    MatchApp { func, args, fail, cont } => {
      collect_callable(func, resolve, origin, ast_index, captures);
      for v in args { collect_val(v, resolve, origin, ast_index, captures); }
      collect_captured_in_body(fail, resolve, origin, ast_index, captures);
      collect_cont(cont, resolve, origin, ast_index, captures);
    }

    MatchIf { func, args, fail, body, .. } => {
      collect_callable(func, resolve, origin, ast_index, captures);
      for v in args { collect_val(v, resolve, origin, ast_index, captures); }
      collect_captured_in_body(fail, resolve, origin, ast_index, captures);
      collect_cont(body, resolve, origin, ast_index, captures);
    }

    MatchValue { val, fail, body, .. } => {
      collect_val(val, resolve, origin, ast_index, captures);
      collect_captured_in_body(fail, resolve, origin, ast_index, captures);
      collect_cont(body, resolve, origin, ast_index, captures);
    }

    MatchSeq { val, fail, body, .. } => {
      collect_val(val, resolve, origin, ast_index, captures);
      collect_captured_in_body(fail, resolve, origin, ast_index, captures);
      collect_cont(body, resolve, origin, ast_index, captures);
    }

    MatchNext { fail, cont, .. } => {
      collect_captured_in_body(fail, resolve, origin, ast_index, captures);
      collect_cont(cont, resolve, origin, ast_index, captures);
    }

    MatchDone { fail, cont, .. } => {
      collect_captured_in_body(fail, resolve, origin, ast_index, captures);
      collect_cont(cont, resolve, origin, ast_index, captures);
    }

    MatchNotDone { fail, body, .. } => {
      collect_captured_in_body(fail, resolve, origin, ast_index, captures);
      collect_cont(body, resolve, origin, ast_index, captures);
    }

    MatchRest { fail, cont, .. } => {
      collect_captured_in_body(fail, resolve, origin, ast_index, captures);
      collect_cont(cont, resolve, origin, ast_index, captures);
    }

    MatchRec { val, fail, body, .. } => {
      collect_val(val, resolve, origin, ast_index, captures);
      collect_captured_in_body(fail, resolve, origin, ast_index, captures);
      collect_cont(body, resolve, origin, ast_index, captures);
    }

    MatchField { fail, cont, .. } => {
      collect_captured_in_body(fail, resolve, origin, ast_index, captures);
      collect_cont(cont, resolve, origin, ast_index, captures);
    }

    MatchArm { matcher, body } => {
      collect_cont(matcher, resolve, origin, ast_index, captures);
      collect_cont(body, resolve, origin, ast_index, captures);
    }

    MatchBlock { params, arms, cont, .. } => {
      for v in params { collect_val(v, resolve, origin, ast_index, captures); }
      for arm in arms { collect_captured_in_body(arm, resolve, origin, ast_index, captures); }
      collect_cont(cont, resolve, origin, ast_index, captures);
    }

    LetRec { body, .. } => {
      // Don't descend into rec fn bodies — handled by collect_expr.
      collect_cont(body, resolve, origin, ast_index, captures);
    }

    Panic | FailCont | FailRef(_) => {}
  }
}

/// Walk the full IR, registering captures for every LetFn.
/// `fn_depth` is the absolute fn nesting depth at this point (0 = module root).
#[allow(clippy::only_used_in_recursion)]
fn collect_expr<'src>(
  expr: &Expr<'src>,
  resolve: &ResolveResult,
  origin: &PropGraph<CpsId, Option<AstId>>,
  ast_index: &PropGraph<AstId, Option<&'src crate::ast::Node<'src>>>,
  fn_depth: u32,
  graph: &mut CaptureGraph<'src>,
) {
  use ExprKind::*;
  match &expr.kind {
    LetFn { name, params, fn_body, body, .. } => {
      // Collect all depth-1 captures from this fn's immediate body.
      let mut caps: Vec<&'src str> = Vec::new();
      collect_captured_in_body(fn_body, resolve, origin, ast_index, &mut caps);
      graph.set(name.id, caps);

      // Recurse into fn_body (deeper) and continuation (same depth).
      collect_expr(fn_body, resolve, origin, ast_index, fn_depth + 1, graph);
      if let Some(body_expr) = body.body() { collect_expr(body_expr, resolve, origin, ast_index, fn_depth, graph); }

      let _ = params;
    }

    LetVal { body, .. } => {
      if let Some(body_expr) = body.body() { collect_expr(body_expr, resolve, origin, ast_index, fn_depth, graph); }
    }

    App { cont, .. } => {
      if let Some(body) = cont.body() { collect_expr(body, resolve, origin, ast_index, fn_depth, graph); }
    }

    If { then, else_, .. } => {
      collect_expr(then, resolve, origin, ast_index, fn_depth, graph);
      collect_expr(else_, resolve, origin, ast_index, fn_depth, graph);
    }

    Yield { cont, .. } => {
      if let Some(body) = cont.body() { collect_expr(body, resolve, origin, ast_index, fn_depth, graph); }
    }

    LetRec { bindings, body } => {
      for b in bindings {
        collect_expr(&b.fn_body, resolve, origin, ast_index, fn_depth + 1, graph);
      }
      if let Some(body_expr) = body.body() { collect_expr(body_expr, resolve, origin, ast_index, fn_depth, graph); }
    }

    MatchLetVal { fail, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, fn_depth, graph);
      if let Some(body_expr) = body.body() { collect_expr(body_expr, resolve, origin, ast_index, fn_depth, graph); }
    }

    MatchApp { fail, cont, .. } => {
      collect_expr(fail, resolve, origin, ast_index, fn_depth, graph);
      if let Some(body) = cont.body() { collect_expr(body, resolve, origin, ast_index, fn_depth, graph); }
    }

    MatchIf { fail, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, fn_depth, graph);
      if let Some(body_expr) = body.body() { collect_expr(body_expr, resolve, origin, ast_index, fn_depth, graph); }
    }

    MatchValue { fail, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, fn_depth, graph);
      if let Some(body_expr) = body.body() { collect_expr(body_expr, resolve, origin, ast_index, fn_depth, graph); }
    }

    MatchSeq { fail, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, fn_depth, graph);
      if let Some(body_expr) = body.body() { collect_expr(body_expr, resolve, origin, ast_index, fn_depth, graph); }
    }

    MatchNext { fail, cont, .. } => {
      collect_expr(fail, resolve, origin, ast_index, fn_depth, graph);
      if let Some(body) = cont.body() { collect_expr(body, resolve, origin, ast_index, fn_depth, graph); }
    }

    MatchDone { fail, cont, .. } => {
      collect_expr(fail, resolve, origin, ast_index, fn_depth, graph);
      if let Some(body) = cont.body() { collect_expr(body, resolve, origin, ast_index, fn_depth, graph); }
    }

    MatchNotDone { fail, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, fn_depth, graph);
      if let Some(body_expr) = body.body() { collect_expr(body_expr, resolve, origin, ast_index, fn_depth, graph); }
    }

    MatchRest { fail, cont, .. } => {
      collect_expr(fail, resolve, origin, ast_index, fn_depth, graph);
      if let Some(body) = cont.body() { collect_expr(body, resolve, origin, ast_index, fn_depth, graph); }
    }

    MatchRec { fail, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, fn_depth, graph);
      if let Some(body_expr) = body.body() { collect_expr(body_expr, resolve, origin, ast_index, fn_depth, graph); }
    }

    MatchField { fail, cont, .. } => {
      collect_expr(fail, resolve, origin, ast_index, fn_depth, graph);
      if let Some(body) = cont.body() { collect_expr(body, resolve, origin, ast_index, fn_depth, graph); }
    }

    MatchArm { matcher, body } => {
      if let Some(e) = matcher.body() { collect_expr(e, resolve, origin, ast_index, fn_depth, graph); }
      if let Some(e) = body.body()    { collect_expr(e, resolve, origin, ast_index, fn_depth, graph); }
    }

    MatchBlock { arms, cont, .. } => {
      for arm in arms { collect_expr(arm, resolve, origin, ast_index, fn_depth, graph); }
      if let Some(body) = cont.body() { collect_expr(body, resolve, origin, ast_index, fn_depth, graph); }
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
  use crate::passes::cps::fmt::{fmt_with, Ctx};
  use crate::passes::name_res::resolve;
  use super::analyse;

  fn closure_capture(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let cps = lower_expr(&r.root);
        let node_count = cps.origin.len();
        let resolve_result = resolve(&cps.root, &cps.origin, &ast_index, node_count);
        let cap_graph = analyse(&cps, &resolve_result, &ast_index);
        let ctx = Ctx {
          origin: &cps.origin,
          ast_index: &ast_index,
          captures: Some(&cap_graph),
        };
        fmt_with(&cps.root, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  test_macros::include_fink_tests!("src/passes/closure_capture/test_closure_capture.fnk");
}
