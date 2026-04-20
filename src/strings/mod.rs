//! String rendering and escape handling for ƒink string values.
//!
//! ƒink strings are **byte sequences**, not UTF-8 validated text
//! (following the C / Go / Python 2 model). A string literal can hold
//! arbitrary bytes — `'\xFF'` is a valid 1-byte string even though 0xFF
//! is not valid UTF-8 on its own. A future `utf8` subtype will opt into
//! codepoint-aware semantics; until then, everything is bytes.
//!
//! `LitStr` nodes in the AST hold raw source bytes — escape sequences
//! are not yet processed. Functions here convert raw source to the
//! cooked byte sequence at the appropriate boundary (codegen, eval, test
//! infrastructure).

/// Render a `LitStr`'s raw source bytes into the cooked byte sequence by
/// processing escape sequences. Returns `Vec<u8>` rather than `String`
/// because the result may contain arbitrary bytes (e.g. from `\xFF`).
///
/// Escape sequences:
/// ```text
///   \n  → newline        \r  → CR          \t  → tab
///   \v  → vertical tab  \b  → backspace    \f  → form feed
///   \\  → backslash      \'  → single quote
///   \$  → dollar (prevents interpolation in source; renders as '$')
///   \xNN       → raw byte value (2 hex digits; may produce invalid UTF-8)
///   \u{NNNNNN} → unicode codepoint (1-6 hex digits, _ separators allowed;
///                emitted as its UTF-8 encoding)
/// ```
pub fn render(raw: &str) -> Vec<u8> {
  let mut out = Vec::with_capacity(raw.len());
  let bytes = raw.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'\\' && i + 1 < bytes.len() {
      i += 1;
      match bytes[i] {
        b'n'  => out.push(b'\n'),
        b'r'  => out.push(b'\r'),
        b't'  => out.push(b'\t'),
        b'v'  => out.push(0x0B),
        b'b'  => out.push(0x08),
        b'f'  => out.push(0x0C),
        b'\'' => out.push(b'\''),
        b'\\' => out.push(b'\\'),
        b'$'  => out.push(b'$'),
        b'x'  => {
          // \xNN emits a raw byte, not a Unicode codepoint.
          let hi = hex_digit(bytes.get(i + 1).copied().unwrap_or(0));
          let lo = hex_digit(bytes.get(i + 2).copied().unwrap_or(0));
          if let (Some(hi), Some(lo)) = (hi, lo) {
            out.push(hi << 4 | lo);
            i += 2;
          } else {
            out.extend_from_slice(b"\\x");
          }
        }
        b'u' if bytes.get(i + 1) == Some(&b'{') => {
          let mut codepoint: u32 = 0;
          let mut digits = 0;
          let mut j = i + 2; // skip past '{'
          while j < bytes.len() && bytes[j] != b'}' {
            match bytes[j] {
              b'_' => { j += 1; }
              b => {
                if let Some(d) = hex_digit(b) {
                  codepoint = codepoint << 4 | d as u32;
                  digits += 1;
                  j += 1;
                } else {
                  break;
                }
              }
            }
          }
          if digits > 0 && j < bytes.len() && bytes[j] == b'}' {
            if let Some(ch) = char::from_u32(codepoint) {
              let mut buf = [0u8; 4];
              let encoded = ch.encode_utf8(&mut buf);
              out.extend_from_slice(encoded.as_bytes());
            }
            i = j; // points at '}', will be incremented at end of loop
          } else {
            out.extend_from_slice(b"\\u");
          }
        }
        b => {
          out.push(b'\\');
          out.push(b);
        }
      }
    } else {
      // Copy the full UTF-8 sequence. Multi-byte sequences start with 0b11xxxxxx;
      // the number of leading 1-bits gives the byte count.
      let seq_len = match bytes[i] {
        b if b & 0b1111_0000 == 0b1111_0000 => 4,
        b if b & 0b1110_0000 == 0b1110_0000 => 3,
        b if b & 0b1100_0000 == 0b1100_0000 => 2,
        _ => 1,
      };
      out.extend_from_slice(&bytes[i..i + seq_len]);
      i += seq_len;
      continue;
    }
    i += 1;
  }
  out
}

/// Replace control characters with Unicode Control Pictures for test output.
///
/// Cooked strings contain actual control bytes (0x0A for \n, etc.).
/// For test formatting, these are replaced with visible symbols so
/// the output is unambiguous:
///   \n → ␊  \r → ␍  \t → ␉  \f → ␌  \b → ␈  \v → ␋
///   \\ → ⧵  \' → ′  \$ → ＄
///
/// Use for cooked strings (escapes already resolved). For raw strings that
/// still contain literal backslash sequences, only control chars should be
/// substituted — use `control_pics_raw` instead.
pub fn control_pics(s: &str) -> String {
  let mut out = String::with_capacity(s.len());
  for ch in s.chars() {
    match ch {
      '\n' => out.push('␊'),
      '\r' => out.push('␍'),
      '\t' => out.push('␉'),
      '\x0C' => out.push('␌'),
      '\x08' => out.push('␈'),
      '\x0B' => out.push('␋'),
      '\\' => out.push('⧵'),
      '\'' => out.push('′'),
      '$' => out.push('＄'),
      c => out.push(c),
    }
  }
  out
}

/// Byte-level variant of `control_pics` for `Vec<u8>` strings.
///
/// Cooked strings are stored as raw bytes (fink strings are byte arrays,
/// not UTF-8). This decodes valid UTF-8 sequences for display and emits
/// `\xNN` for lone high bytes so the output stays printable without
/// Rust's `String` panicking on invalid UTF-8.
pub fn control_pics_bytes(bytes: &[u8]) -> String {
  let mut out = String::with_capacity(bytes.len());
  let mut i = 0;
  while i < bytes.len() {
    let b = bytes[i];
    // Fast path: ASCII printable + the special control-picture substitutions.
    match b {
      b'\n' => { out.push('␊'); i += 1; continue; }
      b'\r' => { out.push('␍'); i += 1; continue; }
      b'\t' => { out.push('␉'); i += 1; continue; }
      0x0C  => { out.push('␌'); i += 1; continue; }
      0x08  => { out.push('␈'); i += 1; continue; }
      0x0B  => { out.push('␋'); i += 1; continue; }
      b'\\' => { out.push('⧵'); i += 1; continue; }
      b'\'' => { out.push('′'); i += 1; continue; }
      b'$'  => { out.push('＄'); i += 1; continue; }
      _ if b < 0x80 => { out.push(b as char); i += 1; continue; }
      _ => {}
    }
    // Non-ASCII: try to decode a valid UTF-8 sequence; fall back to \xNN.
    let seq_len = match b {
      b if b & 0b1111_1000 == 0b1111_0000 => 4,
      b if b & 0b1111_0000 == 0b1110_0000 => 3,
      b if b & 0b1110_0000 == 0b1100_0000 => 2,
      _ => 1,
    };
    if i + seq_len <= bytes.len()
      && let Ok(s) = std::str::from_utf8(&bytes[i..i + seq_len])
    {
      out.push_str(s);
      i += seq_len;
      continue;
    }
    // Invalid UTF-8 — emit \xNN for the single byte.
    out.push_str(&format!("\\x{:02X}", b));
    i += 1;
  }
  out
}

/// Replace only invisible control characters with Unicode Control Pictures.
/// Safe for raw strings where `\`, `'`, `$` are literal ASCII characters
/// that must be preserved (e.g. raw `\n` is two chars: `\` + `n`).
pub fn control_pics_raw(s: &str) -> String {
  let mut out = String::with_capacity(s.len());
  for ch in s.chars() {
    match ch {
      '\n' => out.push('␊'),
      '\r' => out.push('␍'),
      '\t' => out.push('␉'),
      '\x0C' => out.push('␌'),
      '\x08' => out.push('␈'),
      '\x0B' => out.push('␋'),
      c => out.push(c),
    }
  }
  out
}

fn hex_digit(b: u8) -> Option<u8> {
  match b {
    b'0'..=b'9' => Some(b - b'0'),
    b'a'..=b'f' => Some(b - b'a' + 10),
    b'A'..=b'F' => Some(b - b'A' + 10),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::render as render_lit_str;

  // Helper: compare render output against a `&str` by checking bytes.
  // Tests use this because render now returns `Vec<u8>` (fink strings are
  // byte arrays, not UTF-8 strings).
  fn r(raw: &str) -> Vec<u8> {
    render_lit_str(raw)
  }

  #[test]
  fn plain_text() {
    assert_eq!(r("hello"), b"hello");
    // literal UTF-8 characters pass through unchanged — one example per byte width
    assert_eq!(r("é"), "é".as_bytes());       // U+00E9  — 2-byte
    assert_eq!(r("語"), "語".as_bytes());     // U+8A9E  — 3-byte
    assert_eq!(r("🐣"), "🐣".as_bytes());   // U+1F423 — 4-byte
    // mixed
    assert_eq!(r("héllo 語 🐣"), "héllo 語 🐣".as_bytes());
    assert_eq!(r("fink 🐣 言語"), "fink 🐣 言語".as_bytes());
  }

  #[test]
  fn escape_chars() {
    assert_eq!(r(r"\n"), b"\n");
    assert_eq!(r(r"\r"), b"\r");
    assert_eq!(r(r"\t"), b"\t");
    assert_eq!(r(r"\f"), b"\x0C");
    assert_eq!(r(r"\$"), b"$");
    assert_eq!(r(r"\\"), br"\");
    assert_eq!(r(r"\'"), br"'");
    // TODO: might not want to support the following
    assert_eq!(r(r"\v"), b"\x0B");
    assert_eq!(r(r"\b"), b"\x08");
  }


  #[test]
  fn escape_hex() {
    assert_eq!(r(r"\x41"), b"A");        // 0x41 = 'A'
    assert_eq!(r(r"\x0f"), b"\x0f");     // 0x0F = form-feed-ish control char
    assert_eq!(r(r"\x1"), br"\x1");      // only 1 digit → not valid, passed through literally
    // lone high byte — invalid UTF-8, must round-trip as raw byte
    assert_eq!(r(r"\xFF"), &[0xFF][..]);
    assert_eq!(r(r"\x80"), &[0x80][..]);
  }

  #[test]
  fn escape_unicode() {
    assert_eq!(r(r"\u{00}"), "\u{0000}".as_bytes());       // U+0000 lowest
    assert_eq!(r(r"\u{0041}"), b"A");                        // U+0041 = 'A'
    assert_eq!(r(r"\u{00_41}"), b"A");                       // same with _ separator
    assert_eq!(r(r"\u{1F423}"), "\u{1F423}".as_bytes());   // 🐣 U+1F423 hatching chick
    assert_eq!(r(r"\u{10_FF_FF}"), "\u{10FFFF}".as_bytes()); // U+10FFFF highest valid codepoint
    // codepoints above U+10FFFF are invalid → char::from_u32 returns None → silently dropped
    assert_eq!(r(r"\u{11_00_00}"), b"");                    // U+110000 invalid, dropped
    // bare \u without { — passed through literally
    assert_eq!(r(r"\u0041"), b"\\u0041");
  }



  #[test]
  fn dollar_brace_escape() {
    // \${ in source → literal '${' in output
    assert_eq!(r(r"\${name}"), b"${name}");
  }

  #[test]
  fn edge_cases() {
    // \\${ → literal '\' then literal '${' (not a \$ escape)
    assert_eq!(r(r"\\${name}"), br"\${name}");
    // trailing lone backslash → passed through as '\'
    assert_eq!(r("\\"), b"\\");
    // \u with no digits → passed through as '\u'
    assert_eq!(r(r"\u"), br"\u");
    // \x with no digits → passed through as '\x'
    assert_eq!(r(r"\x"), br"\x");
    // bare \uFF without { — passed through literally (delimited form required)
    assert_eq!(r(r"\uFFzz"), b"\\uFFzz");
  }
}
