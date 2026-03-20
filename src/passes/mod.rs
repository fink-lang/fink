// Compiler passes — each sub-module is one stage of the pipeline.
//
// Passes that take and produce CpsResult must uphold the CPS transform
// contract. See docs/cps-transform-contract.md.

pub mod ast;
pub mod closure_capture;
pub mod closure_lifting;
pub mod cont_lifting;
pub mod cps;
pub mod name_res;
pub mod partial;
#[cfg(feature = "runner")]
pub mod wasm;
