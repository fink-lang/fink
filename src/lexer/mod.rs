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
  BlockComment,
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
    .replace("${", r"\${")
    .replace('\n', r"\n")
}

impl std::fmt::Display for Pos {
  fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
    write!(f, "({}, {}, {})", self.idx, self.line, self.col)
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
      StrStart => write!(f, "StrStart \"{}\", loc {start}, {end}", escape_src(self.src)),
      StrText => write!(f, "StrText \"{}\", loc {start}, {end}", escape_src(self.src)),
      StrExprStart => write!(f, r"StrExprStart '\${{', loc {start}, {end}"),
      StrExprEnd => write!(f, "StrExprEnd '}}', loc {start}, {end}"),
      StrEnd => write!(f, "StrEnd \"{}\", loc {start}, {end}", escape_src(self.src)),
      Comment => write!(f, "Comment \"{}\", loc {start}, {end}", escape_src(self.src)),
      BlockComment => write!(f, "DocComment \"{}\", loc {start}, {end}", escape_src(self.src)),
      BlockStart => write!(f, "BlockStart loc {start}, {end}"),
      BlockCont => write!(f, "BlockCont loc {start}, {end}"),
      BlockEnd => write!(f, "BlockEnd loc {start}, {end}"),
      EOF => write!(f, "EOF loc {start}, {end}"),
      Err => write!(f, "Err \"{}\", loc {start}, {end}", escape_src(self.src)),
    }
  }
}

enum LexMode {
  Block,
  Bracket(u8, usize), // opening byte + ind.len() snapshot at open time
  StrText,
  StrExpr,
}

pub struct Lexer<'src> {
  src: &'src str,
  pos: Pos,
  mode: Vec<LexMode>,
  ind: Vec<usize>,
  seps: Vec<Vec<u8>>,
}

impl<'src> Lexer<'src> {
  pub fn new(src: &'src str) -> Self {
    Lexer {
      src,
      pos: Pos { idx: 0, line: 1, col: 0 },
      mode: vec![LexMode::Block],
      ind: vec![0],
      seps: vec![],
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
    self.pos.idx += 1;
    self.pos.line += 1;
    self.pos.col = 0;
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
        // `-` only if followed by a non-whitespace, non-structural byte
        [b'-', next, ..] if !matches!(next,
          b' ' | b'\t' | b'\n' |
          b'(' | b')' | b'[' | b']' | b'{' | b'}' |
          b'\'' | b',' | b';' | b':' | b'#'
        ) => self.advance(1),
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

  fn consume_str_text(&mut self) -> Token<'src> {
    let start = self.pos;
    loop {
      match self.peek_bytes() {
        [] | [b'\'', ..] | [b'$', b'{', ..] => {
          return self.make_token(TokenKind::StrText, start);
        }
        [b'\\', _, ..] => self.advance(2),
        [b'\n', ..] => self.advance_line(),
        _ => self.advance(1),
      }
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
    let start = self.pos;
    self.advance(3); // consume opening ---
    // Consume until next `---` at line start (preceded only by spaces/newline)
    loop {
      match self.peek_bytes() {
        [] => return self.make_token(TokenKind::Err, start),
        [b'\n', ..] => self.advance_line(),
        [b'-', b'-', b'-', ..] => {
          self.advance(3);
          return self.make_token(TokenKind::BlockComment, start);
        }
        _ => self.advance(1),
      }
    }
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
  use test_macros::test_template;
  use super::tokenize_with_seps;

  fn tokenize_debug(src: &str) -> String {
    let default_ops: &[&[u8]] = &[
      b"+", b"-", b"*", b"/", b"%", b"^",
      b"=", b"==", b"!=", b"<", b">", b"<=", b">=",
      b".", b"|", b"..",
    ];
    let mut lexer = tokenize_with_seps(src, default_ops);
    let mut out = String::new();
    let mut depth: usize = 0;

    loop {
      let tok = lexer.next_token();
      if tok.kind == super::TokenKind::EOF {
        break;
      }
      use super::TokenKind::*;
      let indent = "  ".repeat(depth);
      match &tok.kind {
        BlockStart => {
          depth += 1;
          let indent = "  ".repeat(depth);
          if !out.is_empty() {
            out.push('\n');
          }
          out.push_str(&format!("{indent}{tok}"));
        }
        BlockEnd => {
          let line = format!("{indent}{tok}");
          depth = depth.saturating_sub(1);
          if !out.is_empty() {
            out.push('\n');
          }
          out.push_str(&line);
        }
        BracketOpen | StrStart | StrExprStart => {
          if !out.is_empty() {
            out.push('\n');
          }
          out.push_str(&format!("{indent}{tok}"));
          depth += 1;
        }
        BracketClose | StrEnd | StrExprEnd => {
          depth = depth.saturating_sub(1);
          let indent = "  ".repeat(depth);
          if !out.is_empty() {
            out.push('\n');
          }
          out.push_str(&format!("{indent}{tok}"));
        }
        _ => {
          if !out.is_empty() {
            out.push('\n');
          }
          out.push_str(&format!("{indent}{tok}"));
        }
      }
    }

    out
  }

  #[test_template(
    "src/lexer", "./*.fnk",
    r"(?ms)^---\n(?<name>.+?)\n.*?---\n(?<src>.+?)\n(^# expect.*?\n)(?<exp>^.+?((?=\n---\n)|\z))"
  )]
  fn test_lexer(src: &str, exp: &str, path: &str) {
    pretty_assertions::assert_eq!(
      tokenize_debug(src),
      exp.replace("\n\n", "\n").trim(),
      "{}",
      path
    );
  }

  #[test]
  fn test_tokenize_iterator() {
    use super::{tokenize, TokenKind};
    let mut lex = tokenize("foo");
    let tok = lex.next().unwrap();
    assert_eq!(tok.kind, TokenKind::Ident);
    assert_eq!(tok.src, "foo");
    assert!(lex.next().is_none());
  }

  #[test]
  fn test_register_separator_dedup() {
    use super::{tokenize_with_seps, TokenKind};
    // Registering '+' twice must not produce duplicate matches
    let tokens: Vec<_> = tokenize_with_seps("+", &[b"+", b"+"]).collect();
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].kind, TokenKind::Sep);
    assert_eq!(tokens[0].src, "+");
  }
}
