/// String rendering and escape handling for Fink string values.
///
/// TODO: Review supported escape sequences — `\v` and `\b` are archaic;
/// consider whether Fink should support them or trim the set down.
///
/// `LitStr` nodes in the AST hold raw source bytes — escape sequences are
/// not yet processed. Functions here convert raw source to actual string
/// values at the appropriate boundary (codegen, eval, test infrastructure).
/// Render a `LitStr`'s raw source bytes into an actual string value by
/// processing escape sequences.
///
/// Escape sequences:
///   \n  → newline        \r  → CR          \t  → tab
///   \v  → vertical tab  \b  → backspace    \f  → form feed
///   \\  → backslash      \'  → single quote
///   \$  → dollar (prevents interpolation in source; renders as '$')
///   \xNN       → byte value (2 hex digits)
///   \u{NNNNNN} → unicode codepoint (1-6 hex digits, _ separators allowed)
pub fn render(raw: &str) -> String {
  let mut out = String::with_capacity(raw.len());
  let bytes = raw.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'\\' && i + 1 < bytes.len() {
      i += 1;
      match bytes[i] {
        b'n'  => out.push('\n'),
        b'r'  => out.push('\r'),
        b't'  => out.push('\t'),
        b'v'  => out.push('\x0B'),
        b'b'  => out.push('\x08'),
        b'f'  => out.push('\x0C'),
        b'\'' => out.push('\''),
        b'\\' => out.push('\\'),
        b'$'  => out.push('$'),
        b'x'  => {
          let hi = hex_digit(bytes.get(i + 1).copied().unwrap_or(0));
          let lo = hex_digit(bytes.get(i + 2).copied().unwrap_or(0));
          if let (Some(hi), Some(lo)) = (hi, lo) {
            out.push((hi << 4 | lo) as char);
            i += 2;
          } else {
            out.push_str("\\x");
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
              out.push(ch);
            }
            i = j; // points at '}', will be incremented at end of loop
          } else {
            out.push_str("\\u");
          }
        }
        b => {
          out.push('\\');
          out.push(b as char);
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
      // LitStr must contain valid UTF-8 — invalid sequences indicate a parser bug.
      let s = std::str::from_utf8(&bytes[i..i + seq_len])
        .expect("render: invalid UTF-8 sequence in LitStr — parser bug");
      out.push_str(s);
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

  #[test]
  fn plain_text() {
    assert_eq!(render_lit_str("hello"), "hello");
    // literal UTF-8 characters pass through unchanged — one example per byte width
    assert_eq!(render_lit_str("é"), "é");       // U+00E9  — 2-byte
    assert_eq!(render_lit_str("語"), "語");     // U+8A9E  — 3-byte
    assert_eq!(render_lit_str("🐣"), "🐣");   // U+1F423 — 4-byte
    // mixed
    assert_eq!(render_lit_str("héllo 語 🐣"), "héllo 語 🐣");
    assert_eq!(render_lit_str("fink 🐣 言語"), "fink 🐣 言語");
  }

  #[test]
  fn escape_chars() {
    assert_eq!(render_lit_str(r"\n"), "\n");
    assert_eq!(render_lit_str(r"\r"), "\r");
    assert_eq!(render_lit_str(r"\t"), "\t");
    assert_eq!(render_lit_str(r"\f"), "\x0C");
    assert_eq!(render_lit_str(r"\$"), "$");
    assert_eq!(render_lit_str(r"\\"), r"\");
    assert_eq!(render_lit_str(r"\'"), r"'");
    // TODO: might not want to support the following
    assert_eq!(render_lit_str(r"\v"), "\x0B");
    assert_eq!(render_lit_str(r"\b"), "\x08");

  }


  #[test]
  fn escape_hex() {
    assert_eq!(render_lit_str(r"\x41"), "A");        // 0x41 = 'A'
    assert_eq!(render_lit_str(r"\x0f"), "\x0f");     // 0x0F = form-feed-ish control char
    assert_eq!(render_lit_str(r"\x1"), r"\x1");      // only 1 digit → not valid, passed through literally
  }

  #[test]
  fn escape_unicode() {
    assert_eq!(render_lit_str(r"\u{00}"), "\u{0000}");       // U+0000 lowest
    assert_eq!(render_lit_str(r"\u{0041}"), "A");            // U+0041 = 'A'
    assert_eq!(render_lit_str(r"\u{00_41}"), "A");           // same with _ separator
    assert_eq!(render_lit_str(r"\u{1F423}"), "\u{1F423}");   // 🐣 U+1F423 hatching chick
    assert_eq!(render_lit_str(r"\u{10_FF_FF}"), "\u{10FFFF}"); // U+10FFFF highest valid codepoint
    // codepoints above U+10FFFF are invalid → char::from_u32 returns None → silently dropped
    assert_eq!(render_lit_str(r"\u{11_00_00}"), "");         // U+110000 invalid, dropped
    // bare \u without { — passed through literally
    assert_eq!(render_lit_str(r"\u0041"), "\\u0041");
  }



  #[test]
  fn dollar_brace_escape() {
    // \${ in source → literal '${' in output
    assert_eq!(render_lit_str(r"\${name}"), "${name}");
  }

  #[test]
  fn edge_cases() {
    // \\${ → literal '\' then literal '${' (not a \$ escape)
    assert_eq!(render_lit_str(r"\\${name}"), r"\${name}");
    // trailing lone backslash → passed through as '\'
    assert_eq!(render_lit_str("\\"), "\\");
    // \u with no digits → passed through as '\u'
    assert_eq!(render_lit_str(r"\u"), r"\u");
    // \x with no digits → passed through as '\x'
    assert_eq!(render_lit_str(r"\x"), r"\x");
    // bare \uFF without { — passed through literally (delimited form required)
    assert_eq!(render_lit_str(r"\uFFzz"), "\\uFFzz");
  }
}
