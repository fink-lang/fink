// Fink formatter — two-stage pipeline:
//
//   raw AST  ──[layout]──►  formatted AST + origin map  ──[print]──►  String
//
// Stage 1 — layout (layout.rs):
//   Traverses the input AST and produces a new Node tree with canonical locs
//   that satisfy the formatting rules (max line width, indentation, etc.).
//   Also produces a PropGraph<FmtId, Option<AstId>> that maps every node in
//   the output tree back to the original node it was derived from (None for
//   synthesized nodes). This origin map is the hook for sourcemap generation.
//
// Stage 2 — print (print.rs):
//   Takes a Node tree whose locs are already canonical and materialises the
//   source string by placing token bytes at their loc positions. No formatting
//   decisions are made here — it is the identity observer of the loc contract.

pub mod layout;
pub mod print;

use crate::ast::AstId;
use crate::propgraph::PropGraph;
use crate::passes::ast::Node;

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

/// Opaque identifier for a node in the formatted AST output.
/// Separate from AstId to prevent accidental cross-indexing.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FmtId(pub u32);

impl std::fmt::Debug for FmtId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "fmt#{}", self.0)
    }
}

impl From<FmtId> for usize {
    fn from(id: FmtId) -> usize { id.0 as usize }
}

impl From<usize> for FmtId {
    fn from(n: usize) -> FmtId { FmtId(n as u32) }
}

/// Output of the layout pass.
pub struct FmtResult<'src> {
    /// Root of the formatted AST. All locs are canonical and self-consistent.
    pub root: Node<'src>,
    /// Total number of nodes allocated in the output tree.
    pub node_count: u32,
    /// Maps each output node back to the originating input node, if any.
    /// Synthesized or structure-only nodes map to None.
    pub origin: PropGraph<FmtId, Option<AstId>>,
}
