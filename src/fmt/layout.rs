// Stage 1 — layout: AST → AST with canonical locs
//
// **Temporary stub during the flat-AST refactor.** The original layout
// pass was 1476 lines of recursive `&Node` walking that built owning
// trees via `clone()`/`Box::new`. Porting it to the flat-arena shape
// is queued as a follow-up — see git history for the legacy
// implementation.
//
// This stub returns the input AST unchanged so the `fmt2` CLI command
// compiles. The resulting "formatting" is whatever locs the parser
// produced, which means `print::print` will reproduce the original
// source roughly verbatim (no max-width wrapping, no canonical
// indentation, no apply expansion).
//
// Original design notes preserved for the eventual real port:
//
// Walks the input AST and produces a clone with locs rewritten to canonical
// positions that satisfy the formatting rules in FmtConfig.
//
// The pass operates in two modes:
//
//   Preserve mode  — input has locs (idx > 0 or line > 1). The existing
//                    layout is kept unless it violates a hard rule.
//   Canonical mode — input has no locs (all idx/line/col == 0). Canonical
//                    default representation is produced.
//
// Hard rules that trigger reformatting even in preserve mode:
//   1. Wrong indentation depth — normalised to indent_width * depth spaces.
//   2. Apply with ≥2 direct (ungrouped) args where any arg is itself an
//      ungrouped Apply — expand to one arg per indented line.
//   3. Apply whose single arg is a multi-arg bare Apply — expand.
//   4. Any line exceeds max_width — break args/ops to new lines.
//   5. LitRec where any field value contains a Fn with a body block — expand
//      to one field per line.

use crate::ast::Ast;
use super::FmtConfig;

/// Run the layout pass on `ast`.
///
/// **Stub implementation:** clones the input AST unchanged.
pub fn layout<'src>(ast: &Ast<'src>, _cfg: &FmtConfig) -> Ast<'src> {
    ast.clone()
}
