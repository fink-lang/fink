// Sourcemap infrastructure.
//
// Two parallel representations:
// - `native` — byte-offset mappings for Fink-text passes + test blobs.
//   The canonical in-tree form. Opaque base64url codec for embedding.
// - `smv3` — Source Map v3 JSON format. Retained for the WAT emitter;
//   everything else uses native.
//
// `MappedWriter` is the shared output-tracking writer. Each `mark()`
// feeds both streams; callers pick which to serialize via `finish*`.

pub mod native;
pub mod smv3;

// Backcompat re-exports so existing `use crate::sourcemap::{...}` sites
// continue to work. New code should prefer `sourcemap::native::*` and
// `sourcemap::smv3::*` explicitly.
pub use smv3::{SourceMap, base64_encode};

use crate::lexer::Loc;
use native::ByteRange;

/// Tracks output position and collects source mappings as text is written.
///
/// Collects two parallel mapping streams — the classic SMv3 line/col stream
/// and a native-format byte-offset stream. Callers decide which to
/// serialize by calling `finish` (SMv3) or `finish_native`.
pub struct MappedWriter {
  out: String,
  line: u32,
  col: u32,
  byte_pos: u32,
  mappings: Vec<smv3::Mapping>,
  native_mappings: Vec<native::Mapping>,
}

impl Default for MappedWriter {
  fn default() -> Self {
    Self::new()
  }
}

impl MappedWriter {
  pub fn new() -> Self {
    Self {
      out: String::new(),
      line: 0,
      col: 0,
      byte_pos: 0,
      mappings: Vec::new(),
      native_mappings: Vec::new(),
    }
  }

  /// Current output line (0-indexed).
  pub fn line(&self) -> u32 { self.line }

  /// Current output column (0-indexed, UTF-16 code units).
  pub fn col(&self) -> u32 { self.col }

  /// Record a mapping from the current output position to the given source location.
  /// Line 0 is a sentinel meaning "no source origin" — emits an unmapped segment
  /// that stops the previous mapping from bleeding into structural text.
  ///
  /// Updates both the SMv3 and native mapping streams.
  pub fn mark(&mut self, loc: Loc) {
    if loc.start.line == 0 {
      self.mappings.push(smv3::Mapping { out_line: self.line, out_col: self.col, src: None });
      self.native_mappings.push(native::Mapping { out: self.byte_pos, src: None });
      return;
    }
    self.mappings.push(smv3::Mapping {
      out_line: self.line,
      out_col: self.col,
      // Loc uses 1-indexed lines, sourcemaps use 0-indexed.
      src: Some((loc.start.line.saturating_sub(1), loc.start.col)),
    });
    self.native_mappings.push(native::Mapping {
      out: self.byte_pos,
      src: Some(ByteRange::new(loc.start.idx, loc.end.idx)),
    });
  }

  /// Write a string to the output, updating line/col and byte tracking.
  pub fn push_str(&mut self, s: &str) {
    for ch in s.chars() {
      if ch == '\n' {
        self.line += 1;
        self.col = 0;
      } else {
        self.col += ch.len_utf16() as u32;
      }
    }
    self.byte_pos += s.len() as u32;
    self.out.push_str(s);
  }

  /// Write a single character to the output, updating line/col and byte tracking.
  pub fn push(&mut self, ch: char) {
    if ch == '\n' {
      self.line += 1;
      self.col = 0;
    } else {
      self.col += ch.len_utf16() as u32;
    }
    self.byte_pos += ch.len_utf8() as u32;
    self.out.push(ch);
  }

  /// Consume the writer and produce the output string and SMv3 source map.
  pub fn finish(self, source: &str) -> (String, SourceMap) {
    let srcmap = SourceMap::new(source.to_string(), None, self.mappings);
    (self.out, srcmap)
  }

  /// Consume the writer and produce the output string and SMv3 source map
  /// with the original source content embedded.
  pub fn finish_with_content(self, source: &str, content: &str) -> (String, SourceMap) {
    let srcmap = SourceMap::new(source.to_string(), Some(content.to_string()), self.mappings);
    (self.out, srcmap)
  }

  /// Consume the writer and return only the output string, discarding mappings.
  pub fn finish_string(self) -> String {
    self.out
  }

  /// Consume the writer and produce the output string plus the native-form
  /// source map. Parallel to `finish` but emits byte-offset mappings,
  /// not SMv3 line/col.
  pub fn finish_native(self) -> (String, native::SourceMap) {
    let sm = native::SourceMap { mappings: self.native_mappings };
    (self.out, sm)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn mapped_writer_tracks_position() {
    let mut w = MappedWriter::new();
    w.push_str("ab");
    assert_eq!(w.line, 0);
    assert_eq!(w.col, 2);
    w.push('\n');
    assert_eq!(w.line, 1);
    assert_eq!(w.col, 0);
    w.push_str("cd");
    assert_eq!(w.line, 1);
    assert_eq!(w.col, 2);
  }
}
