// Native source-map representation.
//
// A flat list of (generated-output byte range, optional source byte range)
// entries, one per semantic output token a pass chooses to emit. No SMv3
// wire constraints: pass formatters emit entries directly in output order
// as they walk, and downstream tools decode the native form for
// inspection.
//
// On-disk form (used for embedding in test output + `--embed-sm` CLI):
// varint-packed, base64url-wrapped. Opaque to humans by design — use
// `decode_base64url` + `decode_bytes` to inspect, or the `fink decode-sm`
// subcommand.

/// Byte range in a text, half-open: `[start, end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
  pub start: u32,
  pub end:   u32,
}

impl ByteRange {
  pub fn new(start: u32, end: u32) -> Self { ByteRange { start, end } }
  pub fn len(self) -> u32 { self.end.saturating_sub(self.start) }
}

/// One mapping entry. Both positions are **byte offsets** in their
/// respective texts. The mapping is a point event: it runs from `out`
/// until the next mapping's `out` (or end of generated output).
/// `src = None` means "this output span has no source origin" — emitted
/// for synthetic tokens (keywords, wrappers) when the pass chose not to
/// attribute them to any source location.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapping {
  /// Byte offset in the generated output.
  pub out: u32,
  /// Byte range in the source (start/end).
  pub src: Option<ByteRange>,
}

/// A full source map: flat list of mappings in output order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceMap {
  pub mappings: Vec<Mapping>,
}

impl SourceMap {
  pub fn new() -> Self { SourceMap::default() }
  pub fn push(&mut self, m: Mapping) { self.mappings.push(m); }

  /// Encode to compact bytes (varint deltas).
  pub fn encode_bytes(&self) -> Vec<u8> {
    let mut out = Vec::with_capacity(self.mappings.len() * 4);
    write_uvarint(&mut out, self.mappings.len() as u64);

    let mut prev_out: i64 = 0;
    let mut prev_src_start: i64 = 0;

    for m in &self.mappings {
      let d_out = m.out as i64 - prev_out;
      write_svarint(&mut out, d_out);
      prev_out = m.out as i64;

      match m.src {
        None => write_uvarint(&mut out, 0),
        Some(src) => {
          write_uvarint(&mut out, 1);
          let d_src_start = src.start as i64 - prev_src_start;
          write_svarint(&mut out, d_src_start);
          write_uvarint(&mut out, src.len() as u64);
          prev_src_start = src.start as i64;
        }
      }
    }

    out
  }

  /// Decode compact bytes back into a SourceMap.
  pub fn decode_bytes(bytes: &[u8]) -> Result<Self, &'static str> {
    let mut r = Cursor { buf: bytes, pos: 0 };
    let n = read_uvarint(&mut r)?;
    let mut mappings = Vec::with_capacity(n as usize);

    let mut prev_out: i64 = 0;
    let mut prev_src_start: i64 = 0;

    for _ in 0..n {
      let d_out = read_svarint(&mut r)?;
      let out = (prev_out + d_out) as u32;
      prev_out = out as i64;

      let has_src = read_uvarint(&mut r)?;
      let src = match has_src {
        0 => None,
        1 => {
          let d_src_start = read_svarint(&mut r)?;
          let src_len = read_uvarint(&mut r)? as u32;
          let src_start = (prev_src_start + d_src_start) as u32;
          prev_src_start = src_start as i64;
          Some(ByteRange::new(src_start, src_start + src_len))
        }
        _ => return Err("invalid has_src tag"),
      };

      mappings.push(Mapping { out, src });
    }

    if r.pos != r.buf.len() {
      return Err("trailing bytes after sourcemap");
    }

    Ok(SourceMap { mappings })
  }

  /// Encode to base64url-wrapped compact form — suitable for `#sm:<...>`
  /// embedding in Fink or WAT comments.
  pub fn encode_base64url(&self) -> String {
    base64url_encode(&self.encode_bytes())
  }

  /// Decode from base64url form.
  pub fn decode_base64url(s: &str) -> Result<Self, &'static str> {
    let bytes = base64url_decode(s)?;
    Self::decode_bytes(&bytes)
  }
}

// ---------------------------------------------------------------------------
// Varint helpers — LEB128 for unsigned, zig-zag LEB128 for signed.
// ---------------------------------------------------------------------------

fn write_uvarint(out: &mut Vec<u8>, mut n: u64) {
  while n >= 0x80 {
    out.push((n as u8) | 0x80);
    n >>= 7;
  }
  out.push(n as u8);
}

fn write_svarint(out: &mut Vec<u8>, n: i64) {
  let zig = ((n << 1) ^ (n >> 63)) as u64;
  write_uvarint(out, zig);
}

struct Cursor<'a> { buf: &'a [u8], pos: usize }

fn read_uvarint(c: &mut Cursor<'_>) -> Result<u64, &'static str> {
  let mut result: u64 = 0;
  let mut shift = 0u32;
  loop {
    if c.pos >= c.buf.len() { return Err("uvarint: unexpected EOF"); }
    let b = c.buf[c.pos];
    c.pos += 1;
    result |= ((b & 0x7f) as u64) << shift;
    if b & 0x80 == 0 { return Ok(result); }
    shift += 7;
    if shift >= 64 { return Err("uvarint: overflow"); }
  }
}

fn read_svarint(c: &mut Cursor<'_>) -> Result<i64, &'static str> {
  let zig = read_uvarint(c)?;
  Ok(((zig >> 1) as i64) ^ -((zig & 1) as i64))
}

// ---------------------------------------------------------------------------
// base64url (no padding) — hand-rolled to avoid a dep.
// ---------------------------------------------------------------------------

const B64_URL_ALPHA: &[u8; 64] =
  b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn base64url_encode(bytes: &[u8]) -> String {
  let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
  let mut i = 0;
  while i + 3 <= bytes.len() {
    let v = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
    out.push(B64_URL_ALPHA[((v >> 18) & 0x3f) as usize] as char);
    out.push(B64_URL_ALPHA[((v >> 12) & 0x3f) as usize] as char);
    out.push(B64_URL_ALPHA[((v >>  6) & 0x3f) as usize] as char);
    out.push(B64_URL_ALPHA[( v        & 0x3f) as usize] as char);
    i += 3;
  }
  match bytes.len() - i {
    1 => {
      let v = (bytes[i] as u32) << 16;
      out.push(B64_URL_ALPHA[((v >> 18) & 0x3f) as usize] as char);
      out.push(B64_URL_ALPHA[((v >> 12) & 0x3f) as usize] as char);
    }
    2 => {
      let v = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
      out.push(B64_URL_ALPHA[((v >> 18) & 0x3f) as usize] as char);
      out.push(B64_URL_ALPHA[((v >> 12) & 0x3f) as usize] as char);
      out.push(B64_URL_ALPHA[((v >>  6) & 0x3f) as usize] as char);
    }
    _ => {}
  }
  out
}

pub fn base64url_decode(s: &str) -> Result<Vec<u8>, &'static str> {
  fn val(c: u8) -> Option<u32> {
    match c {
      b'A'..=b'Z' => Some((c - b'A') as u32),
      b'a'..=b'z' => Some((c - b'a' + 26) as u32),
      b'0'..=b'9' => Some((c - b'0' + 52) as u32),
      b'-' => Some(62),
      b'_' => Some(63),
      _ => None,
    }
  }
  let bytes = s.as_bytes();
  let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
  let mut i = 0;
  while i + 4 <= bytes.len() {
    let a = val(bytes[i    ]).ok_or("base64url: bad char")?;
    let b = val(bytes[i + 1]).ok_or("base64url: bad char")?;
    let c = val(bytes[i + 2]).ok_or("base64url: bad char")?;
    let d = val(bytes[i + 3]).ok_or("base64url: bad char")?;
    let v = (a << 18) | (b << 12) | (c << 6) | d;
    out.push((v >> 16) as u8);
    out.push((v >>  8) as u8);
    out.push( v        as u8);
    i += 4;
  }
  match bytes.len() - i {
    0 => {}
    1 => return Err("base64url: truncated input"),
    2 => {
      let a = val(bytes[i    ]).ok_or("base64url: bad char")?;
      let b = val(bytes[i + 1]).ok_or("base64url: bad char")?;
      let v = (a << 18) | (b << 12);
      out.push((v >> 16) as u8);
    }
    3 => {
      let a = val(bytes[i    ]).ok_or("base64url: bad char")?;
      let b = val(bytes[i + 1]).ok_or("base64url: bad char")?;
      let c = val(bytes[i + 2]).ok_or("base64url: bad char")?;
      let v = (a << 18) | (b << 12) | (c << 6);
      out.push((v >> 16) as u8);
      out.push((v >>  8) as u8);
    }
    _ => unreachable!(),
  }
  Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn empty_roundtrip() {
    let sm = SourceMap::new();
    let bytes = sm.encode_bytes();
    let back = SourceMap::decode_bytes(&bytes).unwrap();
    assert_eq!(back, sm);
  }

  #[test]
  fn single_mapping_roundtrip() {
    let mut sm = SourceMap::new();
    sm.push(Mapping {
      out: 0,
      src: Some(ByteRange::new(10, 13)),
    });
    let b64 = sm.encode_base64url();
    let back = SourceMap::decode_base64url(&b64).unwrap();
    assert_eq!(back, sm);
  }

  #[test]
  fn unmapped_roundtrip() {
    let mut sm = SourceMap::new();
    sm.push(Mapping { out: 0, src: None });
    sm.push(Mapping { out: 4, src: Some(ByteRange::new(0, 3)) });
    sm.push(Mapping { out: 7, src: None });
    let b64 = sm.encode_base64url();
    let back = SourceMap::decode_base64url(&b64).unwrap();
    assert_eq!(back, sm);
  }

  #[test]
  fn many_mappings_roundtrip() {
    let mut sm = SourceMap::new();
    for i in 0..50u32 {
      sm.push(Mapping {
        out: i * 4,
        src: if i % 3 == 0 { None } else { Some(ByteRange::new(i * 5, i * 5 + 2)) },
      });
    }
    let b64 = sm.encode_base64url();
    let back = SourceMap::decode_base64url(&b64).unwrap();
    assert_eq!(back, sm);
  }

  #[test]
  fn backwards_src_range_roundtrip() {
    // src spans can go backwards between mappings (e.g. spec listings reorder).
    let mut sm = SourceMap::new();
    sm.push(Mapping { out: 0, src: Some(ByteRange::new(100, 103)) });
    sm.push(Mapping { out: 2, src: Some(ByteRange::new(10, 12)) });
    sm.push(Mapping { out: 4, src: Some(ByteRange::new(50, 55)) });
    let b64 = sm.encode_base64url();
    let back = SourceMap::decode_base64url(&b64).unwrap();
    assert_eq!(back, sm);
  }

  #[test]
  fn base64url_basic_roundtrip() {
    let cases: &[&[u8]] = &[
      b"",
      b"a",
      b"ab",
      b"abc",
      b"abcd",
      b"hello, world!",
      &[0x00, 0x01, 0x02, 0xff, 0xfe, 0xfd],
    ];
    for input in cases {
      let enc = base64url_encode(input);
      let dec = base64url_decode(&enc).unwrap();
      assert_eq!(dec.as_slice(), *input);
    }
  }

  #[test]
  fn base64url_output_uses_urlsafe_alphabet() {
    // bytes 0xfb, 0xff, 0xfe encode as 0b111110 111111 111111 111110 = `+/+-` in std, `-_-_` (ish) in url.
    let enc = base64url_encode(&[0xfb, 0xff, 0xbe]);
    assert!(!enc.contains('+'));
    assert!(!enc.contains('/'));
    assert!(!enc.contains('='));
  }

  #[test]
  fn truncated_base64url_errors() {
    assert!(base64url_decode("A").is_err());
  }

  #[test]
  fn uvarint_edges() {
    for n in [0u64, 1, 127, 128, 16383, 16384, 1 << 40, u64::MAX] {
      let mut buf = Vec::new();
      write_uvarint(&mut buf, n);
      let mut c = Cursor { buf: &buf, pos: 0 };
      assert_eq!(read_uvarint(&mut c).unwrap(), n);
      assert_eq!(c.pos, buf.len());
    }
  }

  #[test]
  fn svarint_edges() {
    for n in [0i64, 1, -1, 63, -64, i32::MAX as i64, i32::MIN as i64, i64::MAX, i64::MIN] {
      let mut buf = Vec::new();
      write_svarint(&mut buf, n);
      let mut c = Cursor { buf: &buf, pos: 0 };
      assert_eq!(read_svarint(&mut c).unwrap(), n);
    }
  }
}
