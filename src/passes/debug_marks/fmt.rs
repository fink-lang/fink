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

/// Render marks as a single `marks <token>, <token>, …` Fink expression
/// — valid syntax so the extension's parser over `expect` blocks
/// accepts it without error, and its tokeniser produces one ident per
/// marker that can be mapped back to source via the companion sm.
///
/// Tokens are `<kind>_<cps_id>` (e.g. `any_42`). Leading sigils like
/// `·` would be valid idents in Fink source but the combination with
/// the `marks` prefix looks cleaner without them.
pub fn render_mapped_native(
  marks: &DebugMarks,
  lifted: &LiftedCps,
  desugared: &DesugaredAst<'_>,
) -> (String, SourceMap) {
  let mut out = MappedWriter::new();

  // Collect the stops in creation order so the output reflects CPS
  // emission sequence (roughly execution order).
  let mut rows: Vec<(CpsId, StopKind)> = Vec::new();
  for i in 0..marks.stops.len() {
    let id = CpsId(i as u32);
    if let Some(info) = *marks.stops.get(id) {
      rows.push((id, info.kind));
    }
  }

  if rows.is_empty() {
    return out.finish_native();
  }

  // One `marks` Apply with all tokens as args. Wrapped at a soft
  // width so blessed files stay within 80 cols even when nested
  // inside `expect | equals` (each level is 4 spaces of indent,
  // e.g. the rendered output itself lives 4 cols in). Still parses
  // as a single Apply node (`marks a, b, c, \n  d, e, …`).
  //
  // Budget of 70 bytes keeps us under 80 cols at 4-level nesting
  // while staying comfortable for top-level CLI output.
  const LINE_WIDTH: u32 = 70;
  const INDENT: &str = "  ";

  out.push_str("marks");
  out.push('\n');
  out.push_str(INDENT);
  let mut line_start = out.byte_pos();

  for (i, (id, kind)) in rows.iter().enumerate() {
    let token = format!("{}_{}", stop_kind_label(*kind), id.0);

    if i > 0 {
      out.push(',');
      // Budget: current-line bytes + ", " + next token.
      let prospective = out.byte_pos() + 2 + token.len() as u32;
      if prospective.saturating_sub(line_start) > LINE_WIDTH {
        out.push('\n');
        out.push_str(INDENT);
        line_start = out.byte_pos();
      } else {
        out.push(' ');
      }
    }

    // Source back-mapping for this marker ident.
    if let Some(Some(ast_id)) = lifted.result.origin.try_get(*id) {
      let loc = desugared.ast.nodes.get(*ast_id).loc;
      if loc.start.line > 0 {
        out.mark(loc);
      }
    }

    out.push_str(&token);
  }

  out.finish_native()
}

fn stop_kind_label(kind: StopKind) -> &'static str {
  match kind {
    StopKind::Any => "any",
  }
}
