// Fink formatter — two-stage pipeline:
//
//   raw AST  ──[layout]──►  formatted AST  ──[print]──►  String
//
// Stage 1 — layout (layout.rs):
//   Traverses the input AST and produces a new `Ast` with canonical locs
//   that satisfy the formatting rules (max line width, indentation, etc.).
//
// Stage 2 — print (print.rs):
//   Takes an `Ast` whose locs are already canonical and materialises the
//   source string by placing token bytes at their loc positions. No formatting
//   decisions are made here — it is the identity observer of the loc contract.
//
// Origin mapping is not wired. `print` derives source maps from the per-token
// `Loc.start` values — an identity map of the formatted output. If the
// formatter ever needs to attribute reflowed spans back to pre-layout source
// (e.g. to power a fix-it style refactor against the original file), the
// layout pass would need to carry a `PropGraph<AstId, AstId>` from output
// node id back to input node id. No current consumer needs that, so it is
// not built.

pub mod layout;
pub mod print;

/// Configuration for the formatter.
#[derive(Debug, Clone)]
pub struct FmtConfig {
    /// Maximum line width in bytes before wrapping is triggered.
    pub max_width: u32,
    /// Number of spaces per indentation level.
    pub indent: u32,
}

impl Default for FmtConfig {
    fn default() -> Self {
        Self { max_width: 80, indent: 2 }
    }
}
