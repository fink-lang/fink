// Rendering for `DebugMarks` — produces one token per stop with a
// native-form source map so the vscode-fink extension can decorate
// source spans the same way it does for CPS/lifting tests.
//
// Output shape: one line per step-stop CpsId, formatted as
// `s_<kind>#<cps_id>`. An accompanying `MappedWriter` records the
// source range each token was generated from, encoded as a `# sm:`
// base64url blob by the test harness (see mod.rs tests).
//
// Skeleton: the pass marks nothing today, so output is empty. The
// harness still produces a (trivial) sm blob so the shape is stable
// once real stops arrive.

use crate::passes::LiftedCps;
use crate::passes::DesugaredAst;
use crate::passes::cps::ir::CpsId;
use crate::sourcemap::MappedWriter;
use crate::sourcemap::native::SourceMap;

use super::{DebugMarks, StopKind};

pub fn render_mapped_native(
  marks: &DebugMarks,
  lifted: &LiftedCps,
  desugared: &DesugaredAst<'_>,
) -> (String, SourceMap) {
  let mut out = MappedWriter::new();
  let mut first = true;

  // CpsIds iterate in definition order; stops emitted in the same order
  // so the test output reflects creation sequence, which roughly tracks
  // execution order at the CPS level.
  for i in 0..marks.stops.len() {
    let id = CpsId(i as u32);
    let Some(info) = *marks.stops.get(id) else { continue };

    // Mark the output position back to the source span that produced
    // this CpsId. `origin[id] -> ast_id -> node.loc` is the chain.
    if let Some(Some(ast_id)) = lifted.result.origin.try_get(id) {
      let loc = desugared.ast.nodes.get(*ast_id).loc;
      if loc.start.line > 0 {
        out.mark(loc);
      }
    }

    if !first {
      out.push('\n');
    }
    first = false;

    let token = format!("s_{}#{}", stop_kind_label(info.kind), id.0);
    out.push_str(&token);
  }

  out.finish_native()
}

fn stop_kind_label(kind: StopKind) -> &'static str {
  match kind {
    StopKind::Placeholder => "stop",
  }
}
