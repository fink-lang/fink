//! Diagnostic formatting for user-facing errors.
//!
//! A `Diagnostic` carries the structured identity of an error: where it
//! came from (url + loc), what went wrong (message), and an optional
//! suggestion (hint). It is constructed at the failure site by whoever
//! detects the problem (parser, scope, lower, runtime trap decoder),
//! then handed to the outermost caller.
//!
//! Formatters are separate: they take a `Diagnostic` plus a
//! `SourceProvider` (which can fetch the source for any url referenced
//! by the diagnostic), and render whatever shape the caller wants.
//!
//! Two formatters today:
//! - `format_oneline` -- single line "ERROR: url:line:col: message"
//!   used by test harnesses and grep-friendly CLI output.
//! - `format_diagnostic` -- multi-line caret + source-context block
//!   for human-facing CLI output.

use crate::lexer::Loc;

/// Structured error suitable for propagation across pipeline stages.
/// Carries identity (url + loc) and content (message + optional hint)
/// but no source slice -- formatters pull source via a `SourceProvider`
/// so diagnostics stay cheap to construct.
#[derive(Debug, Clone)]
pub struct Diagnostic {
  pub url: String,
  pub message: String,
  pub loc: Loc,
  pub hint: Option<String>,
}

impl std::fmt::Display for Diagnostic {
  /// Default rendering is the one-line form. Code paths that have
  /// access to source should call `format_diagnostic` directly for
  /// the caret + context block.
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}", format_oneline(self))
  }
}

/// Pluggable source lookup for formatters. The simplest impl is
/// `SingleSource { url, src }` for stages that only ever see one
/// module; multi-module renders use a loader-backed impl.
pub trait SourceProvider {
  fn source(&self, url: &str) -> Option<&str>;
}

/// Source provider for the common single-module case.
pub struct SingleSource<'a> {
  pub url: &'a str,
  pub src: &'a str,
}

impl SourceProvider for SingleSource<'_> {
  fn source(&self, url: &str) -> Option<&str> {
    if url == self.url { Some(self.src) } else { None }
  }
}

/// One-line renderer: `ERROR: url:line:col: message`.
/// Used by test harnesses and any non-interactive consumer that wants
/// a single grep-friendly line per error. Does not consult the
/// `SourceProvider`; included here only for API parity.
pub fn format_oneline(diag: &Diagnostic) -> String {
  format!(
    "ERROR: {}:{}:{}: {}",
    diag.url, diag.loc.start.line, diag.loc.start.col, diag.message,
  )
}

pub struct FormatOptions {
  pub lines_before: usize,
  pub lines_after: usize,
}

impl Default for FormatOptions {
  fn default() -> Self {
    FormatOptions { lines_before: 1, lines_after: 0 }
  }
}

/// Multi-line renderer: source context + caret + message + optional
/// hint + `url:line:col` reference line. Pulls the source for
/// `diag.url` from the provider; if the source can't be found,
/// falls back to the one-line form.
pub fn format_diagnostic(provider: &dyn SourceProvider, diag: &Diagnostic, opts: &FormatOptions) -> String {
  let src = match provider.source(&diag.url) {
    Some(s) => s,
    None => return format_oneline(diag),
  };
  format_diagnostic_with_src(src, diag, opts)
}

fn format_diagnostic_with_src(src: &str, diag: &Diagnostic, opts: &FormatOptions) -> String {
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

  // Location reference (url is always known on a Diagnostic)
  out.push('\n');
  // col is 0-indexed in Pos, display as 1-indexed
  out.push_str(&format!("{}:{}:{}", diag.url, diag.loc.start.line, col_start + 1));

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
    Diagnostic {
      url: "test.fnk".to_string(),
      message: message.to_string(),
      loc,
      hint: None,
    }
  }

  fn render(src: &str, d: &Diagnostic, lines_before: usize, lines_after: usize) -> String {
    let provider = SingleSource { url: &d.url, src };
    format_diagnostic(&provider, d, &FormatOptions { lines_before, lines_after })
  }

  #[test]
  fn single_token_no_context() {
    let src = "[ 1, 2, }, 3]";
    let d = diag("Unexpected token `}`", loc(pos(8, 1, 8), pos(9, 1, 9)));
    assert_eq!(render(src, &d, 0, 0), "\
[ 1, 2, }, 3]
        ^
Unexpected token `}`
test.fnk:1:9");
  }

  #[test]
  fn multi_char_span() {
    let src = "foo bar spam";
    let d = diag("Unknown identifier", loc(pos(4, 1, 4), pos(7, 1, 7)));
    assert_eq!(render(src, &d, 0, 0), "\
foo bar spam
    ^^^
Unknown identifier
test.fnk:1:5");
  }

  #[test]
  fn one_line_before() {
    let src = "foo bar spam\n[ 1, 2, }, 3]";
    let d = diag("Unexpected token `}`", loc(pos(21, 2, 8), pos(22, 2, 9)));
    assert_eq!(render(src, &d, 1, 0), "\
foo bar spam
[ 1, 2, }, 3]
        ^
Unexpected token `}`
test.fnk:2:9");
  }

  #[test]
  fn oneline_format() {
    let d = diag("Unexpected token `}`", loc(pos(8, 1, 8), pos(9, 1, 9)));
    assert_eq!(
      format_oneline(&d),
      "ERROR: test.fnk:1:8: Unexpected token `}`",
    );
  }

  #[test]
  fn unknown_url_falls_back_to_oneline() {
    let src = "[ 1, 2, }, 3]";
    let d = diag("Unexpected token `}`", loc(pos(8, 1, 8), pos(9, 1, 9)));
    let provider = SingleSource { url: "different:url.fnk", src };
    let out = format_diagnostic(&provider, &d, &FormatOptions::default());
    assert_eq!(out, format_oneline(&d));
  }

  #[test]
  fn zero_width_eof() {
    let src = "foo";
    let d = diag("Unexpected end of file", loc(pos(3, 1, 3), pos(3, 1, 3)));
    assert_eq!(render(src, &d, 0, 0), "\
foo
   ^
Unexpected end of file
test.fnk:1:4");
  }

  #[test]
  fn token_at_col_zero() {
    let src = "}foo";
    let d = diag("Unexpected `}`", loc(pos(0, 1, 0), pos(1, 1, 1)));
    assert_eq!(render(src, &d, 0, 0), "\
}foo
^
Unexpected `}`
test.fnk:1:1");
  }

  #[test]
  fn with_hint() {
    let src = "[ 1, 2, }, 3]";
    let d = Diagnostic {
      url: "test.fnk".to_string(),
      message: "Unexpected token `}`".to_string(),
      loc: loc(pos(8, 1, 8), pos(9, 1, 9)),
      hint: Some("hint: did you mean `]`?".to_string()),
    };
    assert_eq!(render(src, &d, 0, 0), "\
[ 1, 2, }, 3]
        ^
Unexpected token `}`
hint: did you mean `]`?
test.fnk:1:9");
  }

  #[test]
  fn lines_after() {
    let src = "[ 1, 2, }, 3]\nnext line";
    let d = diag("Unexpected token `}`", loc(pos(8, 1, 8), pos(9, 1, 9)));
    assert_eq!(render(src, &d, 0, 1), "\
[ 1, 2, }, 3]
        ^
Unexpected token `}`
test.fnk:1:9
next line");
  }
}
