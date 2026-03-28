// WASM passes — collection, binary emission, and post-processing.
//
// ## Module layout
//
// collect.rs    — shared collect phase (lifted CPS → Module/CollectedFn)
// emit.rs       — wasm-encoder binary emitter + byte offset tracking
// dwarf.rs      — gimli::write DWARF line table emission
// fmt.rs        — custom WASM→WAT formatter (wasmparser + gimli::read)
// sourcemap.rs  — WasmMapping type (used by DAP)
// compile.rs    — WAT text → WASM binary (wat crate wrapper, legacy)

pub mod collect;
pub mod dwarf;
pub mod emit;
pub mod sourcemap;

#[cfg(feature = "runner")]
pub mod compile;
