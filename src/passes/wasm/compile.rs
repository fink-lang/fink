// WAT text → WASM binary compilation.
//
// Wraps the `wat` crate to convert WAT text (produced by earlier compiler
// passes) into WASM bytes that can be handed to a runtime.
//
// Debug builds can embed:
//   - DWARF sections (for native debuggers like LLDB)
//   - Source maps (for CDP debuggers like V8/Chrome DevTools)

use std::path::Path;

use super::sourcemap::{self, WasmMapping};

#[derive(Default)]
pub struct CompileOptions<'a> {
  /// Embed DWARF debug info in the output WASM binary.
  pub debug: bool,
  /// Source file path — used for DWARF file references and source map source.
  pub source_path: Option<&'a str>,
  /// WASM byte offset → source location mappings. When present, a source map
  /// is generated and embedded as an inline `sourceMappingURL` custom section.
  pub source_map: Option<SourceMapInfo<'a>>,
}

pub struct SourceMapInfo<'a> {
  pub mappings: &'a [WasmMapping],
  /// Original source content to embed in the source map (optional).
  pub source_content: Option<&'a str>,
}

/// Compile WAT text to WASM bytes.
pub fn wat_to_wasm(wat: &str, opts: &CompileOptions) -> Result<Vec<u8>, String> {
  let mut wasm = if opts.debug {
    let path = opts.source_path.map(Path::new);
    wat::Parser::new()
      .generate_dwarf(wat::GenerateDwarf::Full)
      .parse_str(path, wat)
      .map_err(|e| e.to_string())?
  } else {
    wat::parse_str(wat).map_err(|e| e.to_string())?
  };

  // Append inline source map if mappings are provided.
  if let (Some(source_path), Some(sm)) = (opts.source_path, &opts.source_map) {
    let srcmap = if let Some(content) = sm.source_content {
      sourcemap::build_sourcemap_with_content(source_path, content, sm.mappings)
    } else {
      sourcemap::build_sourcemap(source_path, sm.mappings)
    };
    sourcemap::append_inline_sourcemap(&mut wasm, &srcmap);
  }

  Ok(wasm)
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
    let opts = CompileOptions { debug: true, source_path: Some("test.wat"), ..Default::default() };
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

  #[test]
  fn compile_with_sourcemap_embeds_section() {
    let wat = "(module (func (export \"f\") (nop)))";
    let mappings = vec![WasmMapping { wasm_offset: 0x10, src_line: 0, src_col: 0 }];
    let opts = CompileOptions {
      source_path: Some("test.fnk"),
      source_map: Some(SourceMapInfo { mappings: &mappings, source_content: None }),
      ..Default::default()
    };
    let wasm = wat_to_wasm(wat, &opts).unwrap();
    // The binary should contain the sourceMappingURL custom section.
    let text = String::from_utf8_lossy(&wasm);
    assert!(text.contains("sourceMappingURL"), "missing sourceMappingURL section");
    assert!(text.contains("data:application/json;base64,"), "missing inline source map");
  }
}
