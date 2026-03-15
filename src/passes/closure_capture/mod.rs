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
use crate::passes::cps::ir::{Arg, Callable, CpsId, CpsResult, Expr, ExprKind, Ref, Val, ValKind};
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
  collect_expr(&result.root, resolve, &result.origin, ast_index, &mut graph);
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
  fn_depth: u32,
  captures: &mut Vec<&'src str>,
) {
  if let ValKind::Ref(Ref::Name) = &val.kind {
    if let Some(Resolution::Captured { depth, bind }) =
      resolve.resolution.try_get(val.id).and_then(|r| r.as_ref())
    {
      if *depth == fn_depth {
        if let Some(name) = source_name(*bind, origin, ast_index) {
          if !captures.contains(&name) {
            captures.push(name);
          }
        }
      }
    }
  }
}

fn collect_callable<'src>(
  callable: &Callable<'src>,
  resolve: &ResolveResult,
  origin: &PropGraph<CpsId, Option<AstId>>,
  ast_index: &PropGraph<AstId, Option<&'src crate::ast::Node<'src>>>,
  fn_depth: u32,
  captures: &mut Vec<&'src str>,
) {
  if let Callable::Val(v) = callable {
    collect_val(v, resolve, origin, ast_index, fn_depth, captures);
  }
}

/// Collect all Captured{depth} refs inside `expr`, at the given relative depth.
fn collect_captured_in_body<'src>(
  expr: &Expr<'src>,
  resolve: &ResolveResult,
  origin: &PropGraph<CpsId, Option<AstId>>,
  ast_index: &PropGraph<AstId, Option<&'src crate::ast::Node<'src>>>,
  fn_depth: u32,
  captures: &mut Vec<&'src str>,
) {
  use ExprKind::*;
  match &expr.kind {
    Ret(val) => collect_val(val, resolve, origin, ast_index, fn_depth, captures),

    LetVal { val, body, .. } => {
      collect_val(val, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(body, resolve, origin, ast_index, fn_depth, captures);
    }

    LetFn { fn_body, body, .. } => {
      // Don't descend into nested fn_body — that's a new scope boundary.
      // Only walk the continuation body at the same depth.
      collect_captured_in_body(fn_body, resolve, origin, ast_index, fn_depth + 1, captures);
      collect_captured_in_body(body, resolve, origin, ast_index, fn_depth, captures);
    }

    App { func, args, body, .. } => {
      collect_callable(func, resolve, origin, ast_index, fn_depth, captures);
      for arg in args {
        match arg {
          Arg::Val(v) | Arg::Spread(v) =>
            collect_val(v, resolve, origin, ast_index, fn_depth, captures),
        }
      }
      collect_captured_in_body(body, resolve, origin, ast_index, fn_depth, captures);
    }

    If { cond, then, else_ } => {
      collect_val(cond, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(then, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(else_, resolve, origin, ast_index, fn_depth, captures);
    }

    Yield { value, body, .. } => {
      collect_val(value, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(body, resolve, origin, ast_index, fn_depth, captures);
    }

    MatchLetVal { val, fail, body, .. } => {
      collect_val(val, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(fail, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(body, resolve, origin, ast_index, fn_depth, captures);
    }

    MatchApp { func, args, fail, body, .. } => {
      collect_callable(func, resolve, origin, ast_index, fn_depth, captures);
      for v in args { collect_val(v, resolve, origin, ast_index, fn_depth, captures); }
      collect_captured_in_body(fail, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(body, resolve, origin, ast_index, fn_depth, captures);
    }

    MatchIf { func, args, fail, body, .. } => {
      collect_callable(func, resolve, origin, ast_index, fn_depth, captures);
      for v in args { collect_val(v, resolve, origin, ast_index, fn_depth, captures); }
      collect_captured_in_body(fail, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(body, resolve, origin, ast_index, fn_depth, captures);
    }

    MatchValue { val, fail, body, .. } => {
      collect_val(val, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(fail, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(body, resolve, origin, ast_index, fn_depth, captures);
    }

    MatchSeq { val, fail, body, .. } => {
      collect_val(val, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(fail, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(body, resolve, origin, ast_index, fn_depth, captures);
    }

    MatchNext { fail, body, .. } => {
      collect_captured_in_body(fail, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(body, resolve, origin, ast_index, fn_depth, captures);
    }

    MatchDone { fail, body, .. } => {
      collect_captured_in_body(fail, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(body, resolve, origin, ast_index, fn_depth, captures);
    }

    MatchNotDone { fail, body, .. } => {
      collect_captured_in_body(fail, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(body, resolve, origin, ast_index, fn_depth, captures);
    }

    MatchRest { fail, body, .. } => {
      collect_captured_in_body(fail, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(body, resolve, origin, ast_index, fn_depth, captures);
    }

    MatchRec { val, fail, body, .. } => {
      collect_val(val, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(fail, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(body, resolve, origin, ast_index, fn_depth, captures);
    }

    MatchField { fail, body, .. } => {
      collect_captured_in_body(fail, resolve, origin, ast_index, fn_depth, captures);
      collect_captured_in_body(body, resolve, origin, ast_index, fn_depth, captures);
    }

    MatchBlock { params, fail, arms, body, .. } => {
      for v in params { collect_val(v, resolve, origin, ast_index, fn_depth, captures); }
      collect_captured_in_body(fail, resolve, origin, ast_index, fn_depth, captures);
      for arm in arms {
        collect_captured_in_body(arm, resolve, origin, ast_index, fn_depth, captures);
      }
      collect_captured_in_body(body, resolve, origin, ast_index, fn_depth, captures);
    }

    LetRec { bindings, body } => {
      for b in bindings {
        collect_captured_in_body(&b.fn_body, resolve, origin, ast_index, fn_depth + 1, captures);
      }
      collect_captured_in_body(body, resolve, origin, ast_index, fn_depth, captures);
    }

    Panic | FailCont => {}
  }
}

/// Walk the full IR, registering captures for every LetFn.
fn collect_expr<'src>(
  expr: &Expr<'src>,
  resolve: &ResolveResult,
  origin: &PropGraph<CpsId, Option<AstId>>,
  ast_index: &PropGraph<AstId, Option<&'src crate::ast::Node<'src>>>,
  graph: &mut CaptureGraph<'src>,
) {
  use ExprKind::*;
  match &expr.kind {
    LetFn { name, params, fn_body, body, .. } => {
      // Collect captures from fn_body at depth 1 relative to this fn.
      let mut caps: Vec<&'src str> = Vec::new();
      collect_captured_in_body(fn_body, resolve, origin, ast_index, 1, &mut caps);
      graph.set(name.id, caps);

      // Recurse into fn_body and continuation.
      collect_expr(fn_body, resolve, origin, ast_index, graph);
      collect_expr(body, resolve, origin, ast_index, graph);

      // Also register params' LetFn contributions (none — params are Bind nodes, not Exprs).
      let _ = params;
    }

    LetVal { body, .. } => collect_expr(body, resolve, origin, ast_index, graph),

    App { body, .. } => collect_expr(body, resolve, origin, ast_index, graph),

    If { then, else_, .. } => {
      collect_expr(then, resolve, origin, ast_index, graph);
      collect_expr(else_, resolve, origin, ast_index, graph);
    }

    Yield { body, .. } => collect_expr(body, resolve, origin, ast_index, graph),

    LetRec { bindings, body } => {
      for b in bindings {
        collect_expr(&b.fn_body, resolve, origin, ast_index, graph);
      }
      collect_expr(body, resolve, origin, ast_index, graph);
    }

    MatchLetVal { fail, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, graph);
      collect_expr(body, resolve, origin, ast_index, graph);
    }

    MatchApp { fail, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, graph);
      collect_expr(body, resolve, origin, ast_index, graph);
    }

    MatchIf { fail, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, graph);
      collect_expr(body, resolve, origin, ast_index, graph);
    }

    MatchValue { fail, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, graph);
      collect_expr(body, resolve, origin, ast_index, graph);
    }

    MatchSeq { fail, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, graph);
      collect_expr(body, resolve, origin, ast_index, graph);
    }

    MatchNext { fail, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, graph);
      collect_expr(body, resolve, origin, ast_index, graph);
    }

    MatchDone { fail, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, graph);
      collect_expr(body, resolve, origin, ast_index, graph);
    }

    MatchNotDone { fail, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, graph);
      collect_expr(body, resolve, origin, ast_index, graph);
    }

    MatchRest { fail, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, graph);
      collect_expr(body, resolve, origin, ast_index, graph);
    }

    MatchRec { fail, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, graph);
      collect_expr(body, resolve, origin, ast_index, graph);
    }

    MatchField { fail, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, graph);
      collect_expr(body, resolve, origin, ast_index, graph);
    }

    MatchBlock { fail, arms, body, .. } => {
      collect_expr(fail, resolve, origin, ast_index, graph);
      for arm in arms { collect_expr(arm, resolve, origin, ast_index, graph); }
      collect_expr(body, resolve, origin, ast_index, graph);
    }

    Ret(_) | Panic | FailCont => {}
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
