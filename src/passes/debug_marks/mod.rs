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

use crate::passes::cps::ir::CpsId;
use crate::propgraph::PropGraph;

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
  /// Bootstrap policy — mark every CpsId that has an AST origin.
  /// Saturated: way too many stops for a real debugger. Serves to
  /// exercise the harness + extension decoration end-to-end. Real
  /// policy commits will carve this down by AST node kind.
  Any,
}

/// Compute debug marks for a lifted CPS result.
///
/// Bootstrap policy: any CpsId with an AST origin whose `Loc` is
/// source-bearing (`start.line > 0`) becomes a stop. That's a
/// deliberately crude starting point so the harness emits visible
/// output we can review in the extension; later commits replace this
/// with a real policy keyed on AST node kind.
pub fn analyse(lifted: &crate::passes::LiftedCps, desugared: &crate::passes::DesugaredAst<'_>) -> DebugMarks {
  let size = lifted.result.origin.len();
  let mut stops: PropGraph<CpsId, Option<StopInfo>> = PropGraph::with_size(size, None);
  for i in 0..size {
    let id = CpsId(i as u32);
    let Some(Some(ast_id)) = lifted.result.origin.try_get(id) else { continue };
    let loc = desugared.ast.nodes.get(*ast_id).loc;
    if loc.start.line == 0 { continue }
    stops.set(id, Some(StopInfo { kind: StopKind::Any }));
  }
  DebugMarks { stops }
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
