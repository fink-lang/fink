// Debug-marker pass — decides which CPS nodes the interactive debugger
// should stop at.
//
// The policy ("what's a step-stop?") lives here and only here. Downstream
// consumers (WASM emit, DWARF, DAP's breakpoint resolver) read
// `DebugMarks` and decide how to realise the stops — they don't re-derive
// the policy.
//
// Skeleton: currently marks nothing. A later commit picks an initial
// policy (probably something like "App sites of user fns") and populates
// `stops` in-pass.
//
// Design notes (2026-04-19 session): see
// `.brain/.scratch/sourcemap-phase-b-status.md` for the path that led
// here. Key reframe: we target CPS-node-granularity step stops (one
// stop per meaningful expression), not line or instruction granularity.

pub mod fmt;

use crate::ast::NodeKind;
use crate::lexer::Loc;
use crate::passes::cps::ir::{Arg, Bind, Callable, Cont, CpsId, Expr, ExprKind, Val, ValKind};
use crate::propgraph::PropGraph;

/// One realised step-stop in the linked WASM binary.
///
/// Produced downstream of `analyse` once the emitter has placed each
/// marked CpsId's instruction in the binary. Consumed by the DAP to
/// install breakpoints and to map a stopped PC back to a source `Loc`.
///
/// `wasm_pc` is an absolute byte offset into the linked WASM binary
/// (Step 1 plumbing only — populated as empty for now).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarkRecord {
  pub wasm_pc: u32,
  pub cps_id: CpsId,
  pub source: Loc,
}

/// Output of the debug-marker pass.
///
/// Every CpsId that the debugger should stop at carries `Some(StopInfo)`;
/// others carry `None`. Dense PropGraph keyed by CpsId so consumers can
/// query in O(1) at emit time.
#[derive(Clone)]
pub struct DebugMarks {
  pub stops: PropGraph<CpsId, Option<StopInfo>>,
}

/// Metadata about a single step-stop.
///
/// `kind` classifies *why* this CpsId is a stop — useful for test output
/// (so reviewers can see "stop because guard, stop because call, …") and
/// potentially for DAP to distinguish e.g. step-in eligibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopInfo {
  pub kind: StopKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopKind {
  /// The CPS node's AST origin is an "expression-level" construct the
  /// user would reasonably stop at — binding, call, comparison, branch.
  Expr,
  /// The CPS node is an App to a Ret-kind continuation — the moment a
  /// value flows back to a caller / the host. Used so bare-literal
  /// returns still get a stop even though the literal itself is skipped.
  Return,
}

/// Compute debug marks for a lifted CPS result.
///
/// First policy cut:
///
/// 1. CpsIds whose AST origin is an expression-level node (Bind, Apply,
///    InfixOp, UnaryOp, ChainedCmp, If, Member, Try, Pipe) become
///    `StopKind::Expr`.
/// 2. CpsIds whose `ExprKind::App` callable is a `Cont::Ref` (or
///    equivalent `ValKind::ContRef`) to a `Bind::Cont(ContKind::Ret)`
///    become `StopKind::Return`. Catches bare-literal statements where
///    the value flows directly into the enclosing Ret cont.
///
/// Rule (2) wins if both apply — "returning with this value" is more
/// user-meaningful than "this is some expression node."
///
/// This is deliberately a starting point; we'll carve further once
/// we see it in the extension.
pub fn analyse(
  lifted: &crate::passes::LiftedCps,
  desugared: &crate::passes::DesugaredAst<'_>,
) -> DebugMarks {
  let size = lifted.result.origin.len();
  let mut stops: PropGraph<CpsId, Option<StopInfo>> = PropGraph::with_size(size, None);

  // Rule (1): scan all CpsIds, mark by AST origin kind.
  for i in 0..size {
    let id = CpsId(i as u32);
    let Some(Some(ast_id)) = lifted.result.origin.try_get(id) else { continue };
    let node = desugared.ast.nodes.get(*ast_id);
    if node.loc.start.line == 0 { continue }
    if is_expr_stop_kind(&node.kind) {
      stops.set(id, Some(StopInfo { kind: StopKind::Expr }));
    }
  }

  // Rule (2): walk the CPS tree, mark App nodes whose callable is a
  // Ret-kind cont ref. Overrides rule (1) where both match.
  let bind_kinds = crate::passes::cps::ir::collect_bind_kinds(&lifted.result.root);
  let cont_is_ret = |bind_id: CpsId| -> bool {
    matches!(
      bind_kinds.try_get(bind_id).and_then(|b| *b),
      Some(Bind::Cont(crate::passes::cps::ir::ContKind::Ret))
    )
  };
  walk_exprs(&lifted.result.root, &mut |expr| {
    if let ExprKind::App { func, .. } = &expr.kind
      && let Callable::Val(Val { kind: ValKind::ContRef(cont_id), .. }) = func
      && cont_is_ret(*cont_id)
    {
      stops.set(expr.id, Some(StopInfo { kind: StopKind::Return }));
    }
  });

  DebugMarks { stops }
}

fn is_expr_stop_kind(kind: &NodeKind<'_>) -> bool {
  matches!(
    kind,
    NodeKind::Bind { .. }
      | NodeKind::Apply { .. }
      | NodeKind::InfixOp { .. }
      | NodeKind::UnaryOp { .. }
      | NodeKind::ChainedCmp(_)
      | NodeKind::Match { .. }
      | NodeKind::Member { .. }
      | NodeKind::Try(_)
      | NodeKind::Pipe(_)
  )
}

/// Walk every `Expr` node in the CPS tree, invoking `visit` on each.
/// Recurses through LetFn/LetVal conts, If branches, Cont::Expr bodies,
/// and Arg::Cont / Arg::Expr sub-expressions.
fn walk_exprs(root: &Expr, visit: &mut impl FnMut(&Expr)) {
  visit(root);
  match &root.kind {
    ExprKind::LetFn { fn_body, cont, .. } => {
      walk_exprs(fn_body, visit);
      walk_cont(cont, visit);
    }
    ExprKind::LetVal { cont, .. } => walk_cont(cont, visit),
    ExprKind::App { args, .. } => {
      for arg in args {
        match arg {
          Arg::Cont(c) => walk_cont(c, visit),
          Arg::Expr(e) => walk_exprs(e, visit),
          Arg::Val(_) | Arg::Spread(_) => {}
        }
      }
    }
    ExprKind::If { then, else_, .. } => {
      walk_exprs(then, visit);
      walk_exprs(else_, visit);
    }
  }
}

fn walk_cont(cont: &Cont, visit: &mut impl FnMut(&Expr)) {
  if let Cont::Expr { body, .. } = cont {
    walk_exprs(body, visit);
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use test_macros::include_fink_tests;

  #[allow(unused)]
  fn marks(src: &str) -> String {
    let src_owned = src.to_string();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || marks_inner(&src_owned))) {
      Ok(s) => s,
      Err(e) => {
        let msg = if let Some(s) = e.downcast_ref::<&str>() {
          (*s).to_string()
        } else if let Some(s) = e.downcast_ref::<String>() {
          s.clone()
        } else {
          "<unknown panic>".to_string()
        };
        format!("PANIC: {msg}")
      }
    }
  }

  fn marks_inner(src: &str) -> String {
    match crate::to_lifted(src, "test") {
      Ok((lifted, desugared)) => {
        let debug_marks = super::analyse(&lifted, &desugared);
        let (output, srcmap) = super::fmt::render_mapped_native(&debug_marks, &lifted, &desugared);
        let b64 = srcmap.encode_base64url();
        if output.is_empty() {
          // No stops yet — still emit the sm line (empty) so the
          // harness's shape is stable once policy lands.
          format!("# sm:{b64}")
        } else {
          format!("{output}\n# sm:{b64}")
        }
      }
      Err(e) => format!("ERROR: {e}"),
    }
  }

  include_fink_tests!("src/passes/debug_marks/test_debug_marks.fnk");
}
