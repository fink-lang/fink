#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pos {
  pub idx: u32,
  pub line: u32,
  pub col: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loc {
  pub start: Pos,
  pub end: Pos,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
  Ident,
  Int,
  Float,
  Decimal,
  Sep,
  Comma,
  Semicolon,
  Colon,
  Partial,
  BlockStart,
  BlockCont,
  BlockEnd,
  BracketOpen,
  BracketClose,
  StrStart,
  StrText,
  StrExprStart,
  StrExprEnd,
  StrEnd,
  Comment,
  CommentStart,
  CommentText,
  CommentEnd,
  EOF,
  Err,
}

#[derive(Clone, PartialEq)]
pub struct Token<'src> {
  pub kind: TokenKind,
  pub loc: Loc,
  pub src: &'src str,
}

fn escape_src(s: &str) -> String {
  s.replace('\\', "\\\\")
    .replace('\'', "\\'")
    .replace("${", r"\${")
    .replace('\n', r"\n")
}

impl std::fmt::Display for Pos {
  fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
    write!(f, "[{}, {}, {}]", self.idx, self.line, self.col)
  }
}

impl<'src> std::fmt::Display for Token<'src> {
  fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
    use TokenKind::*;
    let start = self.loc.start;
    let end = self.loc.end;
    match &self.kind {
      Ident => write!(f, "Ident '{}', loc {start}, {end}", escape_src(self.src)),
      Int => write!(f, "Int '{}', loc {start}, {end}", escape_src(self.src)),
      Float => write!(f, "Float '{}', loc {start}, {end}", escape_src(self.src)),
      Decimal => write!(f, "Decimal '{}', loc {start}, {end}", escape_src(self.src)),
      Sep => write!(f, "Op '{}', loc {start}, {end}", escape_src(self.src)),
      Comma => write!(f, "Comma ',', loc {start}, {end}"),
      Semicolon => write!(f, "Semicolon ';', loc {start}, {end}"),
      Colon => write!(f, "Colon ':', loc {start}, {end}"),
      Partial => write!(f, "Partial '?', loc {start}, {end}"),
      BracketOpen => write!(f, "BracketOpen '{}', loc {start}, {end}", escape_src(self.src)),
      BracketClose => write!(f, "BracketClose '{}', loc {start}, {end}", escape_src(self.src)),
      StrStart => write!(f, "StrStart '{}', loc {start}, {end}", escape_src(self.src)),
      StrText => write!(f, "StrText '{}', loc {start}, {end}", escape_src(self.src)),
      StrExprStart => write!(f, r"StrExprStart '\${{', loc {start}, {end}"),
      StrExprEnd => write!(f, "StrExprEnd '}}', loc {start}, {end}"),
      StrEnd => write!(f, "StrEnd '{}', loc {start}, {end}", escape_src(self.src)),
      Comment => write!(f, "Comment '{}', loc {start}, {end}", escape_src(self.src)),
      CommentStart => write!(f, "CommentStart '{}', loc {start}, {end}", escape_src(self.src)),
      CommentText => write!(f, "CommentText '{}', loc {start}, {end}", escape_src(self.src)),
      CommentEnd => write!(f, "CommentEnd '{}', loc {start}, {end}", escape_src(self.src)),
      BlockStart => write!(f, "BlockStart loc {start}, {end}"),
      BlockCont => write!(f, "BlockCont loc {start}, {end}"),
      BlockEnd => write!(f, "BlockEnd loc {start}, {end}"),
      EOF => write!(f, "EOF loc {start}, {end}"),
      Err => write!(f, "Err '{}', loc {start}, {end}", escape_src(self.src)),
    }
  }
}

enum LexMode {
  Block,
  Bracket(u8, usize), // opening byte + ind.len() snapshot at open time
  StrText,
  StrExpr,
  RawBlock(usize), // raw: block — col of `raw:` keyword; content must be strictly deeper
}

pub struct Lexer<'src> {
  src: &'src str,
  pos: Pos,
  mode: Vec<LexMode>,
  ind: Vec<usize>,
  seps: Vec<Vec<u8>>,
  emitted_start: bool,
  pending: Vec<Token<'src>>, // buffered tokens drained front-to-back
}

impl<'src> Lexer<'src> {
  pub fn new(src: &'src str) -> Self {
    Lexer {
      src,
      pos: Pos { idx: 0, line: 1, col: 0 },
      mode: vec![LexMode::Block],
      ind: vec![0, 0],
      seps: vec![],
      emitted_start: false,
      pending: vec![],
    }
  }

  pub fn register_separator(&mut self, sep: &[u8]) {
    // TODO: we iter through the already sorted seps, so we should
    // either find the sep already registeresd or insert it
    // just before the next shorter one. All in one loop, rather
    // than re-sorting. maybe premetrue optimization.
    if !self.seps.iter().any(|existing| existing == sep) {
      self.seps.push(sep.to_vec());
      // Sort longest-first for greedy matching
      self.seps.sort_by(|lhs, rhs| rhs.len().cmp(&lhs.len()));
    }
  }

  fn peek_bytes(&self) -> &[u8] {
    &self.src.as_bytes()[self.pos.idx as usize..]
  }

  fn advance(&mut self, num_bytes: u32) {
    self.pos.idx += num_bytes;
    self.pos.col += num_bytes;
  }

  fn advance_line(&mut self) {
    let new_idx = self.pos.idx + 1;
    if new_idx <= self.src.len() as u32 {
      self.pos.idx = new_idx;
      self.pos.line += 1;
      self.pos.col = 0;
    }
  }

  fn make_token(&self, kind: TokenKind, start: Pos) -> Token<'src> {
    let end = self.pos;
    Token {
      kind,
      loc: Loc { start, end },
      src: &self.src[start.idx as usize..end.idx as usize],
    }
  }

  fn consume(&mut self, num_bytes: u32, kind: TokenKind) -> Token<'src> {
    let start = self.pos;
    self.advance(num_bytes);
    self.make_token(kind, start)
  }

  // Called when positioned at `\n` outside brackets.
  // Skips blank lines, then emits BlockStart / BlockCont / BlockEnd.
  // For multi-level dedent: emits one BlockEnd WITHOUT advancing, so the
  // next next_token() call re-enters here and emits the next one.
  fn consume_newline(&mut self) -> Token<'src> {
    let start = self.pos;

    // Skip blank lines (only spaces then another newline or EOF)
    loop {
      match self.peek_bytes() {
        [b'\n', rest @ ..] => {
          let spaces = rest.iter().take_while(|&&byte| byte == b' ').count();
          match rest.get(spaces) {
            Some(&b'\n') | None => {
              self.advance_line();
              self.advance(spaces as u32);
            }
            _ => break,
          }
        }
        _ => break,
      }
    }

    // Indentation of the upcoming real line
    let ind = match self.peek_bytes() {
      [b'\n', rest @ ..] => rest.iter().take_while(|&&byte| byte == b' ').count(),
      _ => 0,
    };

    let curr_ind = *self.ind.last().unwrap();

    if ind > curr_ind {
      // Deeper → BlockStart; advance past \n + indent
      self.ind.push(ind);
      self.advance_line();
      self.advance(ind as u32);
      self.make_token(TokenKind::BlockStart, start)
    } else if ind < curr_ind {
      // Shallower → one BlockEnd, zero-width, do NOT advance.
      // Next call re-enters here; curr_ind will be smaller until we land.
      self.ind.pop();
      let curr_ind_after = *self.ind.last().unwrap();
      if curr_ind_after < ind {
        // Overshot — doesn't land on a known level.
        // Advance so we don't loop forever, then error.
        // Push ind back so EOF can still emit the closing BlockEnd for this level.
        self.ind.push(ind);
        self.advance_line();
        self.advance(ind as u32);
        return Token {
          kind: TokenKind::Err,
          loc: Loc { start, end: self.pos },
          src: "unexpected dedent",
        };
      }
      Token { kind: TokenKind::BlockEnd, loc: Loc { start, end: start }, src: "" }
    } else {
      // Same level → BlockCont; advance past \n + indent
      self.advance_line();
      self.advance(ind as u32);
      self.make_token(TokenKind::BlockCont, start)
    }
  }

  fn consume_ident(&mut self) -> Token<'src> {
    let start = self.pos;
    loop {
      match self.peek_bytes() {
        [b'$' | b'_' | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | 0x80..=0xFF, ..] => self.advance(1),
        // `-` only if immediately followed by an ident-start byte (no spaces, no structural chars)
        [b'-', b'$' | b'_' | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | 0x80..=0xFF, ..] => self.advance(1),
        // `-` not followed by an ident-start byte: unterminated identifier error
        [b'-', ..] | [b'-'] => {
          let ident = self.make_token(TokenKind::Ident, start);
          let err_start = self.pos;
          self.advance(1);
          self.pending.push(self.make_token(TokenKind::Err, err_start));
          // Overwrite src of the pending Err token with the error message
          let err_idx = self.pending.len() - 1;
          self.pending[err_idx].src = "unterminated identifier";
          return ident;
        }
        _ => return self.make_token(TokenKind::Ident, start),
      }
    }
  }

  fn consume_hex(&mut self) -> Token<'src> {
    let start = self.pos;
    self.advance(2); // consume 0x
    loop {
      match self.peek_bytes() {
        [b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F' | b'_', ..] => self.advance(1),
        _ => return self.make_token(TokenKind::Int, start),
      }
    }
  }

  fn consume_bin(&mut self) -> Token<'src> {
    let start = self.pos;
    self.advance(2); // consume 0b
    loop {
      match self.peek_bytes() {
        [b'0' | b'1' | b'_', ..] => self.advance(1),
        _ => return self.make_token(TokenKind::Int, start),
      }
    }
  }

  fn consume_oct(&mut self) -> Token<'src> {
    let start = self.pos;
    self.advance(2); // consume 0o
    loop {
      match self.peek_bytes() {
        [b'0'..=b'7' | b'_', ..] => self.advance(1),
        _ => return self.make_token(TokenKind::Int, start),
      }
    }
  }

  fn consume_number(&mut self) -> Token<'src> {
    let start = self.pos;
    loop {
      match self.peek_bytes() {
        [b'0'..=b'9' | b'_', ..] => self.advance(1),
        [b'.', b'0'..=b'9', ..] => return self.consume_float_frac(start),
        [b'd', ..] => return self.consume_decimal_suffix(start),
        _ => return self.make_token(TokenKind::Int, start),
      }
    }
  }

  fn consume_float_frac(&mut self, start: Pos) -> Token<'src> {
    self.advance(1); // consume '.'
    loop {
      match self.peek_bytes() {
        [b'0'..=b'9' | b'_', ..] => self.advance(1),
        [b'e', b'+' | b'-', b'0'..=b'9', ..] => {
          self.advance(2);
          return self.consume_float_exp(start);
        }
        [b'e', b'0'..=b'9', ..] => {
          self.advance(1);
          return self.consume_float_exp(start);
        }
        [b'd', ..] => return self.consume_decimal_suffix(start),
        _ => return self.make_token(TokenKind::Float, start),
      }
    }
  }

  fn consume_float_exp(&mut self, start: Pos) -> Token<'src> {
    loop {
      match self.peek_bytes() {
        [b'0'..=b'9' | b'_', ..] => self.advance(1),
        _ => return self.make_token(TokenKind::Float, start),
      }
    }
  }

  fn consume_decimal_suffix(&mut self, start: Pos) -> Token<'src> {
    self.advance(1); // consume 'd'
    // Optional exponent: d-NNN or d+NNN
    match self.peek_bytes() {
      [b'-' | b'+', b'0'..=b'9', ..] => {
        self.advance(1);
        loop {
          match self.peek_bytes() {
            [b'0'..=b'9' | b'_', ..] => self.advance(1),
            _ => return self.make_token(TokenKind::Decimal, start),
          }
        }
      }
      _ => self.make_token(TokenKind::Decimal, start),
    }
  }

  // Push BlockEnd tokens into self.pending for each ind level that is deeper
  // than `col`. Used after string/comment error recovery to drain the block stack
  // before the Err token.
  fn push_block_ends(&mut self, col: usize, at: Pos) {
    while self.ind.len() > 1 && *self.ind.last().unwrap() > col {
      self.ind.pop();
      self.pending.push(Token { kind: TokenKind::BlockEnd, loc: Loc { start: at, end: at }, src: "" });
    }
  }

  // Scan all lines of the current string segment (from self.pos to closing `'`,
  // `${`, or EOF). Fills self.pending with StrText tokens (one per line, stripped),
  // plus an Err token at the end if an indent violation or EOF is hit. Advances
  // self.pos past all scanned content. Returns the first pending token.
  fn consume_str_text(&mut self) -> Token<'src> {
    let ind_floor = *self.ind.last().unwrap();
    let bytes = self.src.as_bytes();

    // --- Pass 1: collect raw lines ---
    // Each entry: (line_start: Pos, end: Pos, has_nl: bool, only_spaces: bool, is_closing_only: bool)
    // line_start.col is the col at the start of this segment (0 for continuations after \n).
    // end is after the last byte of this segment including \n if has_nl.
    // only_spaces: all bytes in [line_start.idx..end.idx - has_nl] are spaces.
    // is_closing_only: only_spaces && terminated by ' (not \n or ${).
    struct RawLine {
      start: Pos,
      end: Pos,
      only_spaces: bool,
      is_closing_only: bool,
      indent: usize, // number of leading spaces before content (or before closing ')
    }

    let mut raw: Vec<RawLine> = vec![];
    let mut p = self.pos;
    let mut eof_err: Option<Token<'src>> = None;

    'outer: loop {
      let seg_start = p;
      let mut i = p.idx as usize;
      let mut only_spaces = true;
      // Count leading spaces for indent (used in pass 2 for strip_level / error check)
      let leading_spaces = bytes[i..].iter().take_while(|&&b| b == b' ').count();

      loop {
        if i >= bytes.len() {
          let ep = Pos { idx: i as u32, line: p.line, col: p.col + (i as u32 - p.idx) };
          eof_err = Some(Token { kind: TokenKind::Err, loc: Loc { start: ep, end: ep }, src: "unterminated string" });
          raw.push(RawLine { start: seg_start, end: ep, only_spaces, is_closing_only: false, indent: leading_spaces });
          p = ep;
          break 'outer;
        }
        match bytes[i] {
          b'\n' => {
            let end = Pos { idx: i as u32 + 1, line: p.line + 1, col: 0 };
            raw.push(RawLine { start: seg_start, end, only_spaces, is_closing_only: false, indent: leading_spaces });
            p = end;
            break;
          }
          b'\'' => {
            let end = Pos { idx: i as u32, line: p.line, col: p.col + (i as u32 - p.idx) };
            raw.push(RawLine { start: seg_start, end, only_spaces, is_closing_only: only_spaces, indent: leading_spaces });
            p = end;
            break 'outer;
          }
          b'$' if i + 1 < bytes.len() && bytes[i + 1] == b'{' => {
            let end = Pos { idx: i as u32, line: p.line, col: p.col + (i as u32 - p.idx) };
            raw.push(RawLine { start: seg_start, end, only_spaces, is_closing_only: false, indent: leading_spaces });
            p = end;
            break 'outer;
          }
          b'\\' => { only_spaces = false; i += 2; }
          b' '  => { i += 1; }
          _     => { only_spaces = false; i += 1; }
        }
      }
    }

    // --- Pass 2: compute strip_level, find first indent error ---
    // Index 0 is exempt (same line as opening '). Blank lines exempt from both.
    // Closing-only lines participate in strip_level but never trigger errors.
    let mut strip_level: usize = 0;
    let mut strip_set = false;
    let mut error_at: Option<usize> = None;

    for (idx, line) in raw.iter().enumerate() {
      if idx == 0 { continue; }
      if line.only_spaces && !line.is_closing_only { continue; } // blank continuation
      let col = line.indent;
      if col < ind_floor {
        error_at = Some(idx);
        break;
      }
      strip_level = if strip_set { strip_level.min(col) } else { col };
      strip_set = true;
    }

    // --- Pass 3: emit StrText tokens into self.pending ---
    let emit_count = error_at.unwrap_or(raw.len());
    for (idx, line) in raw.iter().take(emit_count).enumerate() {
      if line.is_closing_only { continue; } // no content to emit
      // Skip strip_level leading spaces for continuation lines (idx > 0).
      let skip = if idx == 0 || line.only_spaces { 0usize } else { strip_level.min(line.indent) };
      let content_idx = line.start.idx + skip as u32;
      let content_col = if idx == 0 { line.start.col } else { skip as u32 };
      let start = Pos { idx: content_idx, line: line.start.line, col: content_col };
      let src = &self.src[content_idx as usize..line.end.idx as usize];
      if src.is_empty() { continue; }
      self.pending.push(Token { kind: TokenKind::StrText, loc: Loc { start, end: line.end }, src });
    }

    // Append error token if needed, and set self.pos
    if let Some(ei) = error_at {
      let ep = raw[ei].start;
      self.pos = ep; // stop at the offending line, don't consume it
      self.mode.pop();
      self.push_block_ends(ep.col as usize, ep);
      self.pending.push(Token { kind: TokenKind::Err, loc: Loc { start: ep, end: ep }, src: "unterminated string - unexpected dedent" });
    } else {
      self.pos = p; // advance past everything scanned
      if let Some(_) = eof_err {
        self.mode.pop();
        if p.col < ind_floor as u32 {
          // EOF at a lower indent than the string's context — treat as unexpected dedent
          self.push_block_ends(p.col as usize, p);
          self.pending.push(Token { kind: TokenKind::Err, loc: Loc { start: p, end: p }, src: "unterminated string - unexpected dedent" });
        } else {
          self.pending.push(Token { kind: TokenKind::Err, loc: Loc { start: p, end: p }, src: "unterminated string" });
        }
      }
    }

    // Return first buffered token
    if self.pending.is_empty() {
      // Shouldn't happen — but avoid infinite loop
      Token { kind: TokenKind::StrText, loc: Loc { start: self.pos, end: self.pos }, src: "" }
    } else {
      self.pending.remove(0)
    }
  }

  // Like consume_str_text but for `raw:` blocks: no `'` or `${}` terminators,
  // no escape processing. Dedent below ind_floor is normal block end (not an error).
  // Pushes StrText tokens to pending and returns the first. Caller emits StrEnd '' after.
  fn consume_raw_text(&mut self, raw_col: usize) -> Token<'src> {
    // Content must be strictly deeper than `raw:` col
    let content_floor = raw_col + 1;
    let bytes = self.src.as_bytes();

    struct RawLine { start: Pos, end: Pos, only_spaces: bool, indent: usize }

    let mut raw: Vec<RawLine> = vec![];
    // Skip leading newline after `raw:` — the first line is the `raw:` line itself
    let mut p = self.pos;
    if let [b'\n', ..] = &bytes[p.idx as usize..] {
      p = Pos { idx: p.idx + 1, line: p.line + 1, col: 0 };
    }
    let mut done = false; // true when next line dedents below ind_floor

    loop {
      let seg_start = p;
      let mut i = p.idx as usize;
      let leading_spaces = bytes[i..].iter().take_while(|&&b| b == b' ').count();
      let mut only_spaces = true;

      // Content must be strictly deeper than `raw:` col; dedent ends the block
      if leading_spaces < content_floor {
        done = true;
        break;
      }

      loop {
        if i >= bytes.len() {
          let ep = Pos { idx: i as u32, line: p.line, col: p.col + (i as u32 - p.idx) };
          raw.push(RawLine { start: seg_start, end: ep, only_spaces, indent: leading_spaces });
          p = ep;
          done = true;
          break;
        }
        match bytes[i] {
          b'\n' => {
            let end = Pos { idx: i as u32 + 1, line: p.line + 1, col: 0 };
            raw.push(RawLine { start: seg_start, end, only_spaces, indent: leading_spaces });
            p = end;
            break;
          }
          b' ' => { i += 1; }
          _    => { only_spaces = false; i += 1; }
        }
      }
      if done { break; }
    }

    // Compute strip level from all non-blank content lines
    let mut strip_level: usize = 0;
    let mut strip_set = false;
    for line in raw.iter() {
      if line.only_spaces { continue; }
      strip_level = if strip_set { strip_level.min(line.indent) } else { line.indent };
      strip_set = true;
    }

    // Emit StrText tokens — all lines are content lines (leading \n already skipped)
    for (_idx, line) in raw.iter().enumerate() {
      let skip = if line.only_spaces { 0usize } else { strip_level.min(line.indent) };
      let content_idx = line.start.idx + skip as u32;
      let start = Pos { idx: content_idx, line: line.start.line, col: line.start.col + skip as u32 };
      let src = &self.src[content_idx as usize..line.end.idx as usize];
      if src.is_empty() { continue; }
      self.pending.push(Token { kind: TokenKind::StrText, loc: Loc { start, end: line.end }, src });
    }

    self.pos = p;

    // Emit StrEnd '' — block ended by dedent (normal termination)
    if done {
      self.mode.pop();
      self.pending.push(Token { kind: TokenKind::StrEnd, loc: Loc { start: p, end: p }, src: "" });
    }

    if self.pending.is_empty() {
      Token { kind: TokenKind::StrEnd, loc: Loc { start: self.pos, end: self.pos }, src: "" }
    } else {
      self.pending.remove(0)
    }
  }

  fn consume_comment(&mut self) -> Token<'src> {
    let start = self.pos;
    loop {
      match self.peek_bytes() {
        [] | [b'\n', ..] => return self.make_token(TokenKind::Comment, start),
        _ => self.advance(1),
      }
    }
  }

  fn consume_block_comment(&mut self) -> Token<'src> {
    let ind_floor = *self.ind.last().unwrap();
    let bytes = self.src.as_bytes();

    // Emit CommentStart for opening `---`
    let comment_start = self.pos;
    self.advance(3);
    let start_tok = self.make_token(TokenKind::CommentStart, comment_start);

    // Check for single-line `---content---`
    // After advancing past opening ---, we may have content followed by ---
    // We'll handle this in pass 1 by treating --- as a closing marker on the first segment.

    // --- Pass 1: collect raw lines of comment content ---
    struct RawLine {
      start: Pos,
      end: Pos,
      only_spaces: bool,
      is_closing_only: bool, // only_spaces && terminated by ---
      indent: usize,
    }

    let mut raw: Vec<RawLine> = vec![];
    let mut p = self.pos;
    let mut eof_err: Option<Token<'src>> = None;

    'outer: loop {
      let seg_start = p;
      let mut i = p.idx as usize;
      let mut only_spaces = true;
      let leading_spaces = bytes[i..].iter().take_while(|&&b| b == b' ').count();

      loop {
        if i >= bytes.len() {
          let ep = Pos { idx: i as u32, line: p.line, col: p.col + (i as u32 - p.idx) };
          eof_err = Some(Token { kind: TokenKind::Err, loc: Loc { start: ep, end: ep }, src: "unterminated block comment" });
          raw.push(RawLine { start: seg_start, end: ep, only_spaces, is_closing_only: false, indent: leading_spaces });
          p = ep;
          break 'outer;
        }
        match bytes[i] {
          b'\n' => {
            let end = Pos { idx: i as u32 + 1, line: p.line + 1, col: 0 };
            raw.push(RawLine { start: seg_start, end, only_spaces, is_closing_only: false, indent: leading_spaces });
            p = end;
            break;
          }
          b'-' if i + 2 < bytes.len() && bytes[i + 1] == b'-' && bytes[i + 2] == b'-' => {
            // Closing ---
            let end = Pos { idx: i as u32, line: p.line, col: p.col + (i as u32 - p.idx) };
            raw.push(RawLine { start: seg_start, end, only_spaces, is_closing_only: only_spaces, indent: leading_spaces });
            p = end;
            break 'outer;
          }
          b' ' => { i += 1; }
          _    => { only_spaces = false; i += 1; }
        }
      }
    }

    // --- Pass 2: compute strip_level, find first indent error ---
    let mut strip_level: usize = 0;
    let mut strip_set = false;
    let mut error_at: Option<usize> = None;

    for (idx, line) in raw.iter().enumerate() {
      if idx == 0 { continue; }
      if line.only_spaces && !line.is_closing_only { continue; } // blank continuation
      let col = line.indent;
      if col < ind_floor {
        error_at = Some(idx);
        break;
      }
      strip_level = if strip_set { strip_level.min(col) } else { col };
      strip_set = true;
    }

    // --- Pass 3: emit CommentText tokens into self.pending ---
    let emit_count = error_at.unwrap_or(raw.len());
    for (idx, line) in raw.iter().take(emit_count).enumerate() {
      if line.is_closing_only { continue; }
      let skip = if idx == 0 || line.only_spaces { 0usize } else { strip_level.min(line.indent) };
      let content_idx = line.start.idx + skip as u32;
      let content_col = if idx == 0 { line.start.col } else { skip as u32 };
      let start = Pos { idx: content_idx, line: line.start.line, col: content_col };
      let src = &self.src[content_idx as usize..line.end.idx as usize];
      if src.is_empty() { continue; }
      self.pending.push(Token { kind: TokenKind::CommentText, loc: Loc { start, end: line.end }, src });
    }

    // Append closing token or error
    if let Some(ei) = error_at {
      let ep = raw[ei].start;
      self.pos = ep;
      self.push_block_ends(ep.col as usize, ep);
      self.pending.push(Token { kind: TokenKind::Err, loc: Loc { start: ep, end: ep }, src: "unterminated block comment - unexpected dedent" });
    } else if let Some(e) = eof_err {
      self.pos = p;
      self.pending.push(e);
    } else {
      // Emit CommentEnd for closing `---`
      self.pos = p;
      let close_start = self.pos;
      self.advance(3);
      self.pending.push(self.make_token(TokenKind::CommentEnd, close_start));
    }

    start_tok
  }

  fn consume_op(&mut self) -> Option<Token<'src>> {
    let remaining = self.peek_bytes();
    // Try longest match first (seps sorted longest-first)
    for sep in &self.seps {
      if remaining.starts_with(sep.as_slice()) {
        let num_bytes = sep.len() as u32;
        return Some(self.consume(num_bytes, TokenKind::Sep));
      }
    }
    None
  }

  pub fn next_token(&mut self) -> Token<'src> {
    // Drain any buffered tokens (e.g. from multiline string scanning)
    if !self.pending.is_empty() {
      return self.pending.remove(0);
    }

    // Emit implicit BlockStart at the beginning of every source
    if !self.emitted_start {
      self.emitted_start = true;
      return self.make_token(TokenKind::BlockStart, self.pos);
    }

    // String mode
    if matches!(self.mode.last(), Some(LexMode::StrText)) {
      return match self.peek_bytes() {
        [] => {
          self.mode.pop();
          let pos = self.pos;
          Token { kind: TokenKind::Err, loc: Loc { start: pos, end: pos }, src: "unterminated string" }
        }
        [b'\'', ..] => {
          self.mode.pop();
          self.consume(1, TokenKind::StrEnd)
        }
        [b'$', b'{', ..] => {
          self.mode.push(LexMode::StrExpr);
          self.consume(2, TokenKind::StrExprStart)
        }
        _ => self.consume_str_text(),
      };
    }

    // Raw block mode — verbatim text until dedent
    if let Some(&LexMode::RawBlock(raw_col)) = self.mode.last() {
      return self.consume_raw_text(raw_col);
    }

    // StrExpr close
    if matches!(self.mode.last(), Some(LexMode::StrExpr)) {
      if let [b'}', ..] = self.peek_bytes() {
        self.mode.pop();
        return self.consume(1, TokenKind::StrExprEnd);
      }
    }

    match self.peek_bytes() {
      [] => {
        let pos = self.pos;
        // Re-entry: drain unclosed modes/blocks before emitting EOF
        match self.mode.last() {
          Some(LexMode::StrExpr) => {
            self.mode.pop();
            return Token { kind: TokenKind::Err, loc: Loc { start: pos, end: pos }, src: "unterminated string" };
          }
          Some(LexMode::Bracket(_, _)) => {
            self.mode.pop();
            return Token { kind: TokenKind::Err, loc: Loc { start: pos, end: pos }, src: "unclosed bracket" };
          }
          _ => {}
        }
        if self.ind.len() > 1 {
          self.ind.pop();
          return self.make_token(TokenKind::BlockEnd, pos);
        }
        self.consume(0, TokenKind::EOF)
      }

      [b' ', ..] => {
        // Skip spaces outside strings
        self.advance(1);
        self.next_token()
      }

      [b'\t', ..] => {
        let tkn = self.consume(1, TokenKind::Err);
        // Return error token for tab
        Token {
          src: "tab character not allowed",
          ..tkn
        }
      }

      [b'\n', ..] if matches!(self.mode.last(), Some(LexMode::Bracket(_, _))) => {
        let saved_depth = match self.mode.last() {
          Some(LexMode::Bracket(_, d)) => *d,
          _ => unreachable!(),
        };
        let tok = self.consume_newline();
        // BlockCont at the bracket floor is not meaningful — skip it
        if tok.kind == TokenKind::BlockCont && self.ind.len() == saved_depth {
          self.next_token()
        } else {
          tok
        }
      }

      [b'\n', ..] => self.consume_newline(),

      [b'#', ..] => self.consume_comment(),

      // Doc comment: --- at line start (col == 0 or preceded only by indent)
      [b'-', b'-', b'-', ..] if self.pos.col == 0 || {
        let prefix = &self.src[..self.pos.idx as usize];
        prefix.lines().last().map_or(true, |l| l.trim().is_empty())
      } => self.consume_block_comment(),

      [b'\'', ..] => {
        self.mode.push(LexMode::StrText);
        self.consume(1, TokenKind::StrStart)
      }

      [open_byte @ (b'(' | b'[' | b'{'), ..] => {
        self.mode.push(LexMode::Bracket(*open_byte, self.ind.len()));
        self.consume(1, TokenKind::BracketOpen)
      }

      [close_byte @ (b')' | b']' | b'}'), ..] => {
        let expected = match self.mode.last() {
          Some(LexMode::Bracket(b'(', _)) => b')',
          Some(LexMode::Bracket(b'[', _)) => b']',
          Some(LexMode::Bracket(b'{', _)) => b'}',
          _ => 0,
        };
        // Drain any open blocks before closing the bracket
        if let Some(&LexMode::Bracket(_, saved_depth)) = self.mode.last() {
          if self.ind.len() > saved_depth {
            self.ind.pop();
            let pos = self.pos;
            return Token { kind: TokenKind::BlockEnd, loc: Loc { start: pos, end: pos }, src: "" };
          }
        }
        if expected == *close_byte {
          self.mode.pop();
          self.consume(1, TokenKind::BracketClose)
        } else {
          self.consume(1, TokenKind::Err)
        }
      }

      [b',', ..] => self.consume(1, TokenKind::Comma),
      [b';', ..] => self.consume(1, TokenKind::Semicolon),
      [b':', ..] => self.consume(1, TokenKind::Colon),
      [b'?', ..] => self.consume(1, TokenKind::Partial),

      [b'0', b'x', ..] => self.consume_hex(),
      [b'0', b'b', ..] => self.consume_bin(),
      [b'0', b'o', ..] => self.consume_oct(),
      [b'0'..=b'9', ..] => self.consume_number(),

      // `raw:` block — verbatim text, no escaping, ends by dedent
      [b'r', b'a', b'w', b':', ..] => {
        let start = self.pos;
        self.advance(4); // consume `raw:`
        self.mode.push(LexMode::RawBlock(start.col as usize));
        Token { kind: TokenKind::StrStart, loc: Loc { start, end: self.pos }, src: &self.src[start.idx as usize..self.pos.idx as usize] }
      }

      [b'$' | b'_' | b'a'..=b'z' | b'A'..=b'Z' | 0x80..=0xFF, ..] => self.consume_ident(),

      _ => {
        // Try registered operators
        if let Some(tok) = self.consume_op() {
          return tok;
        }
        // Unknown character
        let start = self.pos;
        self.advance(1);
        Token {
          kind: TokenKind::Err,
          loc: Loc { start, end: self.pos },
          src: &self.src[start.idx as usize..self.pos.idx as usize],
        }
      }
    }
  }
}

impl<'src> Iterator for Lexer<'src> {
  type Item = Token<'src>;

  fn next(&mut self) -> Option<Self::Item> {
    match self.next_token() {
      Token { kind: TokenKind::EOF, .. } => None,
      tok => Some(tok),
    }
  }
}

pub fn tokenize(src: &str) -> Lexer<'_> {
  Lexer::new(src)
}

pub fn tokenize_with_seps<'src>(src: &'src str, seps: &[&[u8]]) -> Lexer<'src> {
  let mut lexer = Lexer::new(src);
  for sep in seps {
    lexer.register_separator(sep);
  }
  lexer
}

#[cfg(test)]
mod tests {
  use test_macros::include_fink_tests;
  use super::tokenize_with_seps;

fn tokenize_debug(src: &str) -> String {
    let default_ops: &[&[u8]] = &[
      b"+", b"-", b"*", b"/", b"%", b"^",
      b"=", b"==", b"!=", b"<", b">", b"<=", b">=",
      b".", b"|", b"..",
    ];
    let mut lexer = tokenize_with_seps(src, default_ops);
    let mut out = String::new();
    loop {
      let tok = lexer.next_token();
      if tok.kind == super::TokenKind::EOF { break; }
      if !out.is_empty() { out.push('\n'); }
      out.push_str(&format!("{tok}"));
    }
    out
  }

  #[test]
  fn parse_test_file() {
    let src = include_str!("test_lexer.fnk");
    let result = crate::parser::parse(src);
    match result {
      Ok(_) => {}
      Err(e) => panic!("parse error in test_lexer.fnk at line {}: {}", e.loc.start.line, e.message),
    }
  }

  fn tokenize(src: &str) -> String {
    tokenize_debug(src)
  }

  include_fink_tests!("src/lexer/test_lexer.fnk");

  #[test]
  fn test_err_unexpected_dedent() {
    // Cannot express this in .fnk: input requires indent levels less than surrounding code.
    pretty_assertions::assert_eq!(
      tokenize("foo\n  bar\n    ni\n ham"),
      "BlockStart loc [0, 1, 0], [0, 1, 0]\nIdent 'foo', loc [0, 1, 0], [3, 1, 3]\nBlockStart loc [3, 1, 3], [6, 2, 2]\nIdent 'bar', loc [6, 2, 2], [9, 2, 5]\nBlockStart loc [9, 2, 5], [14, 3, 4]\nIdent 'ni', loc [14, 3, 4], [16, 3, 6]\nBlockEnd loc [16, 3, 6], [16, 3, 6]\nErr 'unexpected dedent', loc [16, 3, 6], [18, 4, 1]\nIdent 'ham', loc [18, 4, 1], [21, 4, 4]\nBlockEnd loc [21, 4, 4], [21, 4, 4]\nBlockEnd loc [21, 4, 4], [21, 4, 4]"
    );
  }

  #[test]
  fn test_tokenize_iterator() {
    use super::{tokenize, TokenKind};
    let mut lex = tokenize("foo");
    // First token is the implicit root BlockStart
    assert_eq!(lex.next().unwrap().kind, TokenKind::BlockStart);
    let tok = lex.next().unwrap();
    assert_eq!(tok.kind, TokenKind::Ident);
    assert_eq!(tok.src, "foo");
    // BlockEnd closes the root block, then iterator exhausts
    assert_eq!(lex.next().unwrap().kind, TokenKind::BlockEnd);
    assert!(lex.next().is_none());
  }

  #[test]
  fn test_register_separator_dedup() {
    use super::{tokenize_with_seps, TokenKind};
    // Registering '+' twice must not produce duplicate matches
    // Token stream: BlockStart, Sep(+), BlockEnd
    let tokens: Vec<_> = tokenize_with_seps("+", &[b"+", b"+"]).collect();
    assert_eq!(tokens.len(), 3);
    assert_eq!(tokens[0].kind, TokenKind::BlockStart);
    assert_eq!(tokens[1].kind, TokenKind::Sep);
    assert_eq!(tokens[1].src, "+");
    assert_eq!(tokens[2].kind, TokenKind::BlockEnd);
  }
}
