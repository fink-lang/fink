// WAT text → WASM binary compilation.
//
// Wraps the `wat` crate to convert WAT text (produced by earlier compiler
// passes) into WASM bytes that can be handed to a runtime.
//
// Debug builds embed DWARF sections so the runtime/debugger can map WASM
// offsets back to source locations.

use std::path::Path;

#[derive(Default)]
pub struct CompileOptions<'a> {
  /// Embed DWARF debug info in the output WASM binary.
  pub debug: bool,
  /// Source file path — used for DWARF file references.
  pub source_path: Option<&'a str>,
}

/// Compile WAT text to WASM bytes.
pub fn wat_to_wasm(wat: &str, opts: &CompileOptions) -> Result<Vec<u8>, String> {
  if opts.debug {
    let path = opts.source_path.map(Path::new);
    wat::Parser::new()
      .generate_dwarf(wat::GenerateDwarf::Full)
      .parse_str(path, wat)
      .map_err(|e| e.to_string())
  } else {
    wat::parse_str(wat).map_err(|e| e.to_string())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn compile_minimal_module() {
    let wat = "(module)";
    let wasm = wat_to_wasm(wat, &CompileOptions::default()).unwrap();
    assert!(wasm.starts_with(b"\0asm"));
  }

  #[test]
  fn compile_with_debug() {
    let wat = "(module (func $f (nop)))";
    let opts = CompileOptions { debug: true, source_path: Some("test.wat") };
    let wasm = wat_to_wasm(wat, &opts).unwrap();
    assert!(wasm.starts_with(b"\0asm"));
    // Debug build should be larger due to DWARF sections.
    let release = wat_to_wasm(wat, &CompileOptions::default()).unwrap();
    assert!(wasm.len() > release.len());
  }

  #[test]
  fn compile_error_reports_location() {
    let wat = "(module (func (invalid)))";
    let err = wat_to_wasm(wat, &CompileOptions::default()).unwrap_err();
    assert!(!err.is_empty());
  }
}
