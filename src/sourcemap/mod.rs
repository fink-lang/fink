// Sourcemap infrastructure.
//
// `native` is the canonical in-tree source map representation: a flat
// list of `(output-byte-offset, source-byte-range)` entries with a
// compact base64url codec for embedding in output.
//
// `MappedWriter` is the shared output-tracking writer. `mark` records
// a `Loc` at the current output byte position; `finish_native` hands
// back the accumulated mappings. The writer also tracks line/col for
// consumers (such as `fmt::print`) that need to know where the cursor
// sits relative to line boundaries.

pub mod native;

use crate::lexer::Loc;
use native::ByteRange;

/// Tracks output position and collects native-form source mappings as
/// text is written. Also tracks line/col for formatters that care.
pub struct MappedWriter {
  out: String,
  line: u32,
  col: u32,
  byte_pos: u32,
  mappings: Vec<native::Mapping>,
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
    }
  }

  /// Current output line (0-indexed).
  pub fn line(&self) -> u32 { self.line }

  /// Current output column (0-indexed, UTF-16 code units).
  pub fn col(&self) -> u32 { self.col }

  /// Current output byte offset.
  pub fn byte_pos(&self) -> u32 { self.byte_pos }

  /// Record a mapping from the current output position to the given source location.
  /// `line: 0` is a sentinel meaning "no source origin" — emits an unmapped entry
  /// that stops the previous mapping from bleeding into structural text.
  pub fn mark(&mut self, loc: Loc) {
    if loc.start.line == 0 {
      self.mappings.push(native::Mapping { out: self.byte_pos, src: None });
      return;
    }
    self.mappings.push(native::Mapping {
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

  /// Consume the writer and return only the output string, discarding mappings.
  pub fn finish_string(self) -> String {
    self.out
  }

  /// Consume the writer and produce the output string plus the native-form
  /// source map.
  pub fn finish_native(self) -> (String, native::SourceMap) {
    let sm = native::SourceMap { mappings: self.mappings };
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
    assert_eq!(w.line(), 0);
    assert_eq!(w.col(), 2);
    assert_eq!(w.byte_pos(), 2);
    w.push('\n');
    assert_eq!(w.line(), 1);
    assert_eq!(w.col(), 0);
    assert_eq!(w.byte_pos(), 3);
    w.push_str("cd");
    assert_eq!(w.line(), 1);
    assert_eq!(w.col(), 2);
    assert_eq!(w.byte_pos(), 5);
  }
}
