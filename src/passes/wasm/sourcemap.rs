// WASM source map — WASM byte offset → source file location.
//
// The `WasmMapping` struct is the in-memory shape used by DAP for PC →
// source lookup at breakpoints. The SMv3 JSON / custom-section
// serialisation helpers previously in this module were removed: the
// compiler no longer writes a `sourceMappingURL` section into the WASM
// binary (DWARF is the only in-binary source-info format), and no other
// caller needed the JSON path.

/// A single mapping: WASM byte offset → source file location.
#[derive(Debug, Clone)]
pub struct WasmMapping {
  /// Byte offset into the WASM module binary.
  pub wasm_offset: u32,
  /// 0-indexed source line.
  pub src_line: u32,
  /// 0-indexed source column.
  pub src_col: u32,
}
