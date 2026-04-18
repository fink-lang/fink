// Source Map v3 — standard format emission.
//
// https://sourcemaps.info/spec.html
//
// Retained as an output format for the WAT emitter; for Fink-text passes,
// the native form in `super::native` is the canonical in-tree representation.

/// A single mapping entry: output position → optional source position.
/// `src` is None for unmapped segments (synthetic text with no source origin).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Mapping {
  pub(super) out_line: u32,
  pub(super) out_col: u32,
  pub(super) src: Option<(u32, u32)>, // (src_line, src_col) or None for unmapped
}

/// A Source Map v3 structure, ready to serialize to JSON.
#[derive(Debug)]
pub struct SourceMap {
  source: String,
  sources_content: Option<String>,
  pub(super) mappings: Vec<Mapping>,
}

impl SourceMap {
  pub(super) fn new(source: String, sources_content: Option<String>, mappings: Vec<Mapping>) -> Self {
    Self { source, sources_content, mappings }
  }

  /// Build a source map from raw (out_line, out_col, src_line, src_col) tuples.
  /// All values are 0-indexed.
  pub fn from_raw(
    source: &str,
    mappings: impl Iterator<Item = (u32, u32, u32, u32)>,
  ) -> Self {
    Self {
      source: source.to_string(),
      sources_content: None,
      mappings: mappings
        .map(|(out_line, out_col, src_line, src_col)| Mapping { out_line, out_col, src: Some((src_line, src_col)) })
        .collect(),
    }
  }

  /// Build a source map with embedded source content from raw tuples.
  pub fn from_raw_with_content(
    source: &str,
    content: &str,
    mappings: impl Iterator<Item = (u32, u32, u32, u32)>,
  ) -> Self {
    Self {
      source: source.to_string(),
      sources_content: Some(content.to_string()),
      mappings: mappings
        .map(|(out_line, out_col, src_line, src_col)| Mapping { out_line, out_col, src: Some((src_line, src_col)) })
        .collect(),
    }
  }

  /// Number of mapping entries.
  pub fn len(&self) -> usize {
    self.mappings.len()
  }

  /// Whether there are no mapping entries.
  pub fn is_empty(&self) -> bool {
    self.mappings.is_empty()
  }

  /// Iterator over mapped entries as (out_line, out_col, src_line, src_col) tuples.
  /// Skips unmapped segments. All values are 0-indexed.
  pub fn iter(&self) -> impl Iterator<Item = (u32, u32, u32, u32)> + '_ {
    self.mappings.iter().filter_map(|m| {
      let (src_line, src_col) = m.src?;
      Some((m.out_line, m.out_col, src_line, src_col))
    })
  }

  /// Encode as a Source Map v3 JSON string.
  pub fn to_json(&self) -> String {
    let mappings = encode_mappings(&self.mappings);
    let source = json_escape(&self.source);
    let mut out = format!(
      "{{\n  \"version\": 3,\n  \"sources\": [\"{source}\"],\n"
    );
    if let Some(content) = &self.sources_content {
      out.push_str(&format!(
        "  \"sourcesContent\": [\"{}\"],\n",
        json_escape(content)
      ));
    }
    out.push_str(&format!("  \"mappings\": \"{mappings}\"\n}}"));
    out
  }
}

/// Encode all mappings into the v3 "mappings" string.
///
/// Each output line is separated by `;`. Within a line, segments are
/// separated by `,`. Each segment is a sequence of VLQ-encoded fields:
///   [0] output column (relative to prev segment on same line)
///   [1] source index  (relative, always 0 for single-source)
///   [2] source line   (relative to prev segment's source line)
///   [3] source column (relative to prev segment's source col)
fn encode_mappings(mappings: &[Mapping]) -> String {
  let mut out = String::new();

  let mut prev_out_col: i64 = 0;
  let mut prev_src_line: i64 = 0;
  let mut prev_src_col: i64 = 0;
  let mut prev_out_line: u32 = 0;
  let mut first_on_line = true;

  for m in mappings {
    // Emit `;` for each output line we've moved past.
    while prev_out_line < m.out_line {
      out.push(';');
      prev_out_line += 1;
      prev_out_col = 0;
      first_on_line = true;
    }

    if !first_on_line {
      out.push(',');
    }
    first_on_line = false;

    let out_col = m.out_col as i64;

    // Field 0: output column delta
    vlq_encode(&mut out, out_col - prev_out_col);
    prev_out_col = out_col;

    if let Some((src_line, src_col)) = m.src {
      let src_line = src_line as i64;
      let src_col = src_col as i64;
      // Field 1: source index delta (always 0 — single source)
      vlq_encode(&mut out, 0);
      // Field 2: source line delta
      vlq_encode(&mut out, src_line - prev_src_line);
      // Field 3: source column delta
      vlq_encode(&mut out, src_col - prev_src_col);
      prev_src_line = src_line;
      prev_src_col = src_col;
    }
    // else: unmapped segment — only field 0 (1-field VLQ)
  }

  out
}

const VLQ_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode a single signed integer as a Base64 VLQ sequence.
fn vlq_encode(out: &mut String, value: i64) {
  // Convert signed to VLQ-signed: LSB is the sign bit.
  let mut v = if value < 0 {
    ((-value) << 1) | 1
  } else {
    value << 1
  } as u64;

  loop {
    let mut digit = (v & 0x1f) as u8; // 5 bits
    v >>= 5;
    if v > 0 {
      digit |= 0x20; // continuation bit
    }
    out.push(VLQ_CHARS[digit as usize] as char);
    if v == 0 {
      break;
    }
  }
}

const B64_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes as standard Base64.
pub fn base64_encode(data: &[u8]) -> String {
  let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
  for chunk in data.chunks(3) {
    let b0 = chunk[0] as u32;
    let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
    let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
    let triple = (b0 << 16) | (b1 << 8) | b2;

    out.push(B64_CHARS[((triple >> 18) & 0x3f) as usize] as char);
    out.push(B64_CHARS[((triple >> 12) & 0x3f) as usize] as char);
    if chunk.len() > 1 {
      out.push(B64_CHARS[((triple >> 6) & 0x3f) as usize] as char);
    } else {
      out.push('=');
    }
    if chunk.len() > 2 {
      out.push(B64_CHARS[(triple & 0x3f) as usize] as char);
    } else {
      out.push('=');
    }
  }
  out
}

/// Minimal JSON string escaping for source paths.
fn json_escape(s: &str) -> String {
  let mut out = String::with_capacity(s.len());
  for ch in s.chars() {
    match ch {
      '"' => out.push_str("\\\""),
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      _ => out.push(ch),
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn vlq_zero() {
    let mut out = String::new();
    vlq_encode(&mut out, 0);
    assert_eq!(out, "A");
  }

  #[test]
  fn vlq_positive() {
    let mut out = String::new();
    vlq_encode(&mut out, 1);
    assert_eq!(out, "C");
  }

  #[test]
  fn vlq_negative() {
    let mut out = String::new();
    vlq_encode(&mut out, -1);
    assert_eq!(out, "D");
  }

  #[test]
  fn vlq_large() {
    // 16 → VLQ signed = 32 → 5-bit chunks: [0, 1] → continuation on first
    let mut out = String::new();
    vlq_encode(&mut out, 16);
    assert_eq!(out, "gB");
  }

  #[test]
  fn single_mapping_encodes() {
    let mappings = vec![Mapping {
      out_line: 0,
      out_col: 0,
      src: Some((0, 0)),
    }];
    assert_eq!(encode_mappings(&mappings), "AAAA");
  }

  #[test]
  fn two_lines_encode() {
    let mappings = vec![
      Mapping { out_line: 0, out_col: 0, src: Some((0, 0)) },
      Mapping { out_line: 1, out_col: 2, src: Some((1, 2)) },
    ];
    // Line 0: AAAA; Line 1: EACE (col +2, src 0, srcline +1, srccol +2)
    assert_eq!(encode_mappings(&mappings), "AAAA;EACE");
  }

  #[test]
  fn sourcemap_json_valid() {
    let srcmap = SourceMap {
      source: "test.fnk".to_string(),
      sources_content: None,
      mappings: vec![Mapping {
        out_line: 0,
        out_col: 0,
        src: Some((0, 0)),
      }],
    };
    let json = srcmap.to_json();
    assert!(json.contains("\"version\": 3"));
    assert!(json.contains("\"sources\": [\"test.fnk\"]"));
    assert!(!json.contains("\"sourcesContent\""));
    assert!(json.contains("\"mappings\": \"AAAA\""));
  }
}
