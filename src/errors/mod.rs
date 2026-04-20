//! Diagnostic formatting for user-facing compile errors.
//!
//! A `Diagnostic` carries a message, a source location, and an optional
//! hint; `format` renders it with a slice of source context (lines
//! before/after, a caret column) for CLI display.

use crate::lexer::Loc;

pub struct Diagnostic {
  pub message: String,
  pub loc: Loc,
  pub hint: Option<String>,
}

pub struct FormatOptions<'a> {
  pub lines_before: usize,
  pub lines_after: usize,
  pub path: Option<&'a str>,
}

impl Default for FormatOptions<'_> {
  fn default() -> Self {
    FormatOptions { lines_before: 1, lines_after: 0, path: None }
  }
}

pub fn format_diagnostic(src: &str, diag: &Diagnostic, opts: &FormatOptions) -> String {
  let lines: Vec<&str> = src.split('\n').collect();
  let error_line_idx = diag.loc.start.line as usize - 1;

  let col_start = diag.loc.start.col as usize;
  let col_end = if diag.loc.end.line == diag.loc.start.line {
    diag.loc.end.col as usize
  } else {
    // Multi-line span: clamp to end of start line
    lines.get(error_line_idx).map_or(col_start, |l| l.len())
  };
  let span = col_end.saturating_sub(col_start).max(1);

  let mut out = String::new();

  // Context lines before
  let before_start = error_line_idx.saturating_sub(opts.lines_before);
  for line in &lines[before_start..error_line_idx] {
    out.push_str(line);
    out.push('\n');
  }

  // Error line
  if let Some(line) = lines.get(error_line_idx) {
    out.push_str(line);
    out.push('\n');
  }

  // Caret line
  out.push_str(&" ".repeat(col_start));
  out.push_str(&"^".repeat(span));
  out.push('\n');

  // Message
  out.push_str(&diag.message);

  // Hint
  if let Some(hint) = &diag.hint {
    out.push('\n');
    out.push_str(hint);
  }

  // Location reference
  if let Some(path) = opts.path {
    out.push('\n');
    // col is 0-indexed in Pos, display as 1-indexed
    out.push_str(&format!("{}:{}:{}", path, diag.loc.start.line, col_start + 1));
  }

  // Context lines after
  let after_end = (error_line_idx + 1 + opts.lines_after).min(lines.len());
  for line in &lines[error_line_idx + 1..after_end] {
    out.push('\n');
    out.push_str(line);
  }

  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::lexer::{Loc, Pos};

  fn pos(idx: u32, line: u32, col: u32) -> Pos {
    Pos { idx, line, col }
  }

  fn loc(start: Pos, end: Pos) -> Loc {
    Loc { start, end }
  }

  fn diag(message: &str, loc: Loc) -> Diagnostic {
    Diagnostic { message: message.to_string(), loc, hint: None }
  }

  #[test]
  fn single_token_no_context() {
    let src = "[ 1, 2, }, 3]";
    let d = diag("Unexpected token `}`", loc(pos(8, 1, 8), pos(9, 1, 9)));
    let opts = FormatOptions { lines_before: 0, lines_after: 0, path: None };
    assert_eq!(format_diagnostic(src, &d, &opts), "\
[ 1, 2, }, 3]
        ^
Unexpected token `}`");
  }

  #[test]
  fn multi_char_span() {
    let src = "foo bar spam";
    let d = diag("Unknown identifier", loc(pos(4, 1, 4), pos(7, 1, 7)));
    let opts = FormatOptions { lines_before: 0, lines_after: 0, path: None };
    assert_eq!(format_diagnostic(src, &d, &opts), "\
foo bar spam
    ^^^
Unknown identifier");
  }

  #[test]
  fn one_line_before() {
    let src = "foo bar spam\n[ 1, 2, }, 3]";
    let d = diag("Unexpected token `}`", loc(pos(21, 2, 8), pos(22, 2, 9)));
    let opts = FormatOptions { lines_before: 1, lines_after: 0, path: None };
    assert_eq!(format_diagnostic(src, &d, &opts), "\
foo bar spam
[ 1, 2, }, 3]
        ^
Unexpected token `}`");
  }

  #[test]
  fn with_path() {
    let src = "[ 1, 2, }, 3]";
    let d = diag("Unexpected token `}`", loc(pos(8, 1, 8), pos(9, 1, 9)));
    let opts = FormatOptions { lines_before: 0, lines_after: 0, path: Some("path/to/module.fnk") };
    assert_eq!(format_diagnostic(src, &d, &opts), "\
[ 1, 2, }, 3]
        ^
Unexpected token `}`
path/to/module.fnk:1:9");
  }

  #[test]
  fn zero_width_eof() {
    let src = "foo";
    let d = diag("Unexpected end of file", loc(pos(3, 1, 3), pos(3, 1, 3)));
    let opts = FormatOptions { lines_before: 0, lines_after: 0, path: None };
    // Zero-width span gets a single ^
    assert_eq!(format_diagnostic(src, &d, &opts), "\
foo
   ^
Unexpected end of file");
  }

  #[test]
  fn token_at_col_zero() {
    let src = "}foo";
    let d = diag("Unexpected `}`", loc(pos(0, 1, 0), pos(1, 1, 1)));
    let opts = FormatOptions { lines_before: 0, lines_after: 0, path: None };
    assert_eq!(format_diagnostic(src, &d, &opts), "\
}foo
^
Unexpected `}`");
  }

  #[test]
  fn with_hint() {
    let src = "[ 1, 2, }, 3]";
    let d = Diagnostic {
      message: "Unexpected token `}`".to_string(),
      loc: loc(pos(8, 1, 8), pos(9, 1, 9)),
      hint: Some("hint: did you mean `]`?".to_string()),
    };
    let opts = FormatOptions { lines_before: 0, lines_after: 0, path: None };
    assert_eq!(format_diagnostic(src, &d, &opts), "\
[ 1, 2, }, 3]
        ^
Unexpected token `}`
hint: did you mean `]`?");
  }

  #[test]
  fn lines_after() {
    let src = "[ 1, 2, }, 3]\nnext line";
    let d = diag("Unexpected token `}`", loc(pos(8, 1, 8), pos(9, 1, 9)));
    let opts = FormatOptions { lines_before: 0, lines_after: 1, path: None };
    assert_eq!(format_diagnostic(src, &d, &opts), "\
[ 1, 2, }, 3]
        ^
Unexpected token `}`
next line");
  }
}
