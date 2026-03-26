// Compiler passes — each sub-module is one stage of the pipeline.
//
// Passes that take and produce CpsResult must uphold the CPS transform
// contract. See docs/cps-transform-contract.md.

pub mod ast;
pub mod cps;
pub mod lifting;
pub mod name_res;
pub mod partial;
pub mod scopes;
pub mod wat;
#[cfg(feature = "runner")]
pub mod wasm;
