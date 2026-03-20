// WASM source map — maps WASM byte offsets to original source locations.
//
// In WASM source maps, all mappings are on "line 1" and the column is the
// byte offset into the WASM module binary. The source map is attached via
// a `sourceMappingURL` custom section appended to the WASM binary.
//
// See: https://github.com/aspect-build/aspect-cli/issues/WebAssembly/tool-conventions/blob/main/Debugging.md

use crate::sourcemap::{self, SourceMap};

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

/// Build a Source Map v3 from WASM byte offset mappings.
///
/// The source map uses line=0 for all generated positions (WASM is a single
/// "line") and column = byte offset into the module binary.
pub fn build_sourcemap(source_file: &str, mappings: &[WasmMapping]) -> SourceMap {
  SourceMap::from_raw(
    source_file,
    mappings.iter().map(|m| (0, m.wasm_offset, m.src_line, m.src_col)),
  )
}

/// Build a Source Map v3 with embedded source content.
pub fn build_sourcemap_with_content(
  source_file: &str,
  source_content: &str,
  mappings: &[WasmMapping],
) -> SourceMap {
  SourceMap::from_raw_with_content(
    source_file,
    source_content,
    mappings.iter().map(|m| (0, m.wasm_offset, m.src_line, m.src_col)),
  )
}

/// Append a `sourceMappingURL` custom section to a WASM binary.
///
/// The URL can be a file path, HTTP URL, or inline data URL.
pub fn append_sourcemap_url(wasm: &mut Vec<u8>, url: &str) {
  let section_name = b"sourceMappingURL";
  let url_bytes = url.as_bytes();

  // Custom section format:
  //   section_id (0x00) | section_size (leb128) | name_len (leb128) | name | payload
  let name_len = leb128_size(section_name.len() as u32) + section_name.len();
  let payload_len = leb128_size(url_bytes.len() as u32) + url_bytes.len();
  let section_size = name_len + payload_len;

  wasm.push(0x00); // custom section id
  leb128_encode(wasm, section_size as u32);
  leb128_encode(wasm, section_name.len() as u32);
  wasm.extend_from_slice(section_name);
  leb128_encode(wasm, url_bytes.len() as u32);
  wasm.extend_from_slice(url_bytes);
}

/// Append an inline source map as a data URL in the `sourceMappingURL` section.
pub fn append_inline_sourcemap(wasm: &mut Vec<u8>, srcmap: &SourceMap) {
  let json = srcmap.to_json();
  let b64 = sourcemap::base64_encode(json.as_bytes());
  let url = format!("data:application/json;base64,{b64}");
  append_sourcemap_url(wasm, &url);
}

fn leb128_encode(out: &mut Vec<u8>, mut value: u32) {
  loop {
    let mut byte = (value & 0x7f) as u8;
    value >>= 7;
    if value > 0 {
      byte |= 0x80;
    }
    out.push(byte);
    if value == 0 {
      break;
    }
  }
}

fn leb128_size(value: u32) -> usize {
  let mut v = value;
  let mut size = 0;
  loop {
    v >>= 7;
    size += 1;
    if v == 0 {
      break;
    }
  }
  size
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn append_sourcemap_url_section() {
    let wat = "(module)";
    let mut wasm = wat::parse_str(wat).unwrap();
    let orig_len = wasm.len();
    append_sourcemap_url(&mut wasm, "add.fnk.map");
    assert!(wasm.len() > orig_len);
    // The custom section should contain the URL string.
    let tail = String::from_utf8_lossy(&wasm[orig_len..]);
    assert!(tail.contains("sourceMappingURL"));
    assert!(tail.contains("add.fnk.map"));
  }

  #[test]
  fn inline_sourcemap_roundtrip() {
    let mappings = vec![
      WasmMapping { wasm_offset: 0x46, src_line: 1, src_col: 2 },
      WasmMapping { wasm_offset: 0x48, src_line: 1, src_col: 6 },
    ];
    let srcmap = build_sourcemap("add.fnk", &mappings);
    let json = srcmap.to_json();
    assert!(json.contains("\"sources\": [\"add.fnk\"]"));
    assert!(json.contains("\"version\": 3"));

    let mut wasm = wat::parse_str("(module)").unwrap();
    append_inline_sourcemap(&mut wasm, &srcmap);
    let tail = String::from_utf8_lossy(&wasm);
    assert!(tail.contains("sourceMappingURL"));
  }

  #[test]
  fn sourcemap_with_content() {
    let mappings = vec![
      WasmMapping { wasm_offset: 0x46, src_line: 0, src_col: 0 },
    ];
    let srcmap = build_sourcemap_with_content("add.fnk", "add = fn a, b:\n  a + b\n", &mappings);
    let json = srcmap.to_json();
    assert!(json.contains("\"sourcesContent\""));
    assert!(json.contains("add = fn a, b:"));
  }
}
