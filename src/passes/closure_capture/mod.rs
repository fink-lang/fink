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
use crate::passes::cps::ir::{Arg, Bind, Callable, Cont, CpsId, CpsResult, Expr, ExprKind, Val};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Capture graph — maps each LetFn's name-bind CpsId to its ordered capture list.
/// Each entry is (bind CpsId, bind kind) of a captured value. The bind kind
/// determines the WASM type (Cont → ref $Cont, others → anyref).
/// Source names can be recovered via the origin map + AST index.
pub type CaptureGraph = PropGraph<CpsId, Vec<(CpsId, Bind)>>;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Build the capture graph for all LetFn nodes in `result`.
/// Uses the name_res captures prop graph, gated: fns with only transitive
/// captures (no direct captures) have their captures cleared for this round —
/// the multi-round lifting approach handles them.
pub fn analyse(
  result: &CpsResult<'_>,
  resolve: &ResolveResult,
) -> CaptureGraph {
  // Determine which fns have direct captures (Captured refs in immediate body).
  let mut direct_fns: std::collections::HashSet<CpsId> = std::collections::HashSet::new();
  collect_direct_capture_scopes(&result.root, resolve, &mut direct_fns);

  // Clone name_res captures, clearing transitive-only fns.
  let mut graph = CaptureGraph::new();
  for i in 0..resolve.captures.len() {
    let id = CpsId(i as u32);
    if let Some(caps) = resolve.captures.try_get(id) {
      if direct_fns.contains(&id) || caps.is_empty() {
        graph.push(caps.clone());
      } else {
        graph.push(Vec::new());
      }
    }
  }
  graph
}

/// Walk the tree to find LetFn scopes that have direct captures.
fn collect_direct_capture_scopes(
  expr: &Expr<'_>,
  resolve: &ResolveResult,
  out: &mut std::collections::HashSet<CpsId>,
) {
  use ExprKind::*;
  match &expr.kind {
    LetFn { name, fn_body, cont: cont, .. } => {
      if has_direct_captures(fn_body, resolve) {
        out.insert(name.id);
      }
      collect_direct_capture_scopes(fn_body, resolve, out);
      if let Cont::Expr { body: b, .. } = cont {
        collect_direct_capture_scopes(b, resolve, out);
      }
    }
    LetVal { cont: Cont::Expr { body: b, .. }, .. } => {
      collect_direct_capture_scopes(b, resolve, out);
    }
    LetVal { .. } => {}
    App { args, .. } => {
      for arg in args {
        match arg {
          Arg::Cont(Cont::Expr { body, .. }) | Arg::Expr(body) => collect_direct_capture_scopes(body, resolve, out),
          _ => {}
        }
      }
    }
    If { then, else_, .. } => {
      collect_direct_capture_scopes(then, resolve, out);
      collect_direct_capture_scopes(else_, resolve, out);
    }
    _ => {}
  }
}

/// Check if a fn body has any direct Captured refs (conts only, not nested fn_bodies).
fn has_direct_captures(expr: &Expr<'_>, resolve: &ResolveResult) -> bool {
  use ExprKind::*;
  match &expr.kind {
    LetVal { val, cont: cont, .. } => {
      is_captured_val(val, resolve) || matches!(cont, Cont::Expr { body: b, .. } if has_direct_captures(b, resolve))
    }
    LetFn { cont: body, .. } => {
      matches!(body, Cont::Expr { body: b, .. } if has_direct_captures(b, resolve))
    }
    App { func, args } => {
      if let Callable::Val(v) = func
        && is_captured_val(v, resolve)
      { return true; }
      args.iter().any(|a| match a {
        Arg::Val(v) | Arg::Spread(v) => is_captured_val(v, resolve),
        Arg::Cont(Cont::Expr { body, .. }) | Arg::Expr(body) => has_direct_captures(body, resolve),
        Arg::Cont(Cont::Ref(id)) => matches!(resolve.resolution.try_get(*id), Some(Some(Resolution::Captured { .. }))),
      })
    }
    If { cond, then, else_ } => {
      is_captured_val(cond, resolve) || has_direct_captures(then, resolve) || has_direct_captures(else_, resolve)
    }
  }
}

fn is_captured_val(val: &Val<'_>, resolve: &ResolveResult) -> bool {
  matches!(resolve.resolution.try_get(val.id), Some(Some(Resolution::Captured { .. })))
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
        let empty_alias = crate::propgraph::PropGraph::new(); let resolve_result = resolve(&cps.root, &cps.origin, &ast_index, node_count, &empty_alias);
        let cap_graph = analyse(&cps, &resolve_result);
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
