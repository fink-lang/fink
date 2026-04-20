//! ƒink source-code formatter — two-stage pipeline:
//!
//! ```text
//!   raw AST  ──[layout]──►  formatted AST  ──[print]──►  String
//! ```
//!
//! [`layout`] traverses the input AST and produces a new `Ast` with
//! canonical locs that satisfy the formatting rules (max line width,
//! indentation). [`print`][mod@print] takes an `Ast` whose locs are
//! already canonical and materialises the source string by placing token
//! bytes at their loc positions — no formatting decisions, just the
//! identity observer of the loc contract.
//!
//! Origin mapping is not wired. `print` derives source maps from
//! per-token `Loc.start` values — an identity map of the formatted
//! output. Attribution from reflowed spans back to pre-layout source
//! (for fix-it style refactors) would require the layout pass to carry a
//! `PropGraph<AstId, AstId>` from output to input; no current consumer
//! needs that.

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
