pub mod errors;
pub mod fmt;
#[cfg(feature = "runner")]
pub mod dap;
#[cfg(feature = "compiler")]
pub mod compiler;
#[cfg(feature = "runner")]
pub mod runner;
pub mod passes;
pub mod propgraph;
pub mod sourcemap;
pub mod strings;

// Re-exports for convenience — short paths for foundational types.
pub use passes::ast;
pub use passes::ast::lexer;
pub use passes::ast::parser;
