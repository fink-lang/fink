// WASM passes — post-processing on WAT text or WASM binary.
//
// Pipeline: WAT text → compile → WASM bytes → (optimize) → WASM bytes
//
// The compiler passes (CPS → WAT codegen) emit WAT text as the readable,
// debuggable intermediate form. This module handles everything after that.

#[cfg(feature = "runner")]
pub mod compile;
pub mod sourcemap;
