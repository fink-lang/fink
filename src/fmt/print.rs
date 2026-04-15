// Stage 2 — AST → String
//
// **Temporary stub during the flat-AST refactor.** The original print
// pass was 459 lines of `&Node` walking that placed token bytes at
// their canonical loc positions to materialise the formatted source.
// Porting it to the flat-arena shape is queued as a follow-up — see
// git history for the legacy implementation.
//
// This stub delegates to `ast::fmt::fmt` (and its mapped variants) —
// the s-expression printer's regular formatting output. The result
// is **not** the canonical Stage-2 layout, just whatever ast::fmt
// produces for the input. Good enough to keep the `fmt2` CLI command
// compiling; the real port preserves canonical-mode wrapping and
// max-width breaks.

use crate::ast::Ast;
use crate::sourcemap::SourceMap;

/// Render a formatted AST to a String.
/// **Stub:** delegates to `ast::fmt::fmt`.
pub fn print(ast: &Ast<'_>) -> String {
    crate::passes::ast::fmt::fmt(ast)
}

/// Render a formatted AST to a String and a Source Map.
/// **Stub:** delegates to `ast::fmt::fmt_mapped`.
pub fn print_mapped(ast: &Ast<'_>, source_name: &str) -> (String, SourceMap) {
    crate::passes::ast::fmt::fmt_mapped(ast, source_name)
}

/// Like `print_mapped` but embeds the original source content in the map.
/// **Stub:** delegates to `ast::fmt::fmt_mapped_with_content`.
pub fn print_mapped_with_content(ast: &Ast<'_>, source_name: &str, content: &str) -> (String, SourceMap) {
    crate::passes::ast::fmt::fmt_mapped_with_content(ast, source_name, content)
}
