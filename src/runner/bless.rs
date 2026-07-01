//! Bless: rewrite inline snapshot expectations in a `.test.fnk` source to the
//! actual value a failing test produced.
//!
//! The native test harness inlines each expected value as the 2nd argument of an
//! `equals` call. There is no external snapshot store, so "blessing" means
//! editing that argument in place. The fink test runner, in bless mode, hands the
//! host `(mid, [[cid, actual], ...])` -- one module's failures. The host resolves
//! `mid -> source` + each `cid -> the call's source span` (from the debug marks),
//! then this module does the pure source rewrite.
//!
//! FIRST SLICE: only the `wat":` block-string form of the 2nd argument is
//! handled (`... | equals wat":\n  <block>`). Tagged templates and plain string
//! literals are follow-ups -- see `find_block_span`.

use crate::passes::ast::{self, AstId, NodeKind};
use crate::passes::ast::lexer::Loc;

/// One expectation to rewrite: the source span the debug mark points at (the
/// `equals` call), and the actual value to inline.
pub struct Bless {
  /// Byte span of the marked call in the source (from the debug mark's `Loc`).
  pub call: Loc,
  /// The actual value the test produced (raw, unindented).
  pub actual: String,
}

/// Rewrite `source` so every blessed expectation carries its actual value.
///
/// Each `Bless.call` locates the `equals` call; the block-string 2nd argument on
/// that call's line is replaced with `actual`, reindented to the block's column.
/// Replacements are resolved to spans, sorted by position, and spliced
/// front-to-back with an accumulating offset -- input order is irrelevant.
///
/// `url` is the module's parse identity (only used for parse diagnostics).
/// Returns the rewritten source, or an error string if any expectation could not
/// be resolved (in which case nothing is written -- all-or-nothing per module).
pub fn apply_blesses(source: &str, url: &str, blesses: &[Bless]) -> Result<String, String> {
  let ast = ast::parser::parse(source, url)
    .map_err(|e| format!("bless: reparse failed: {e:?}"))?;

  // Resolve each bless to (span-to-replace, indent, actual).
  let mut edits: Vec<(usize, usize, u32, &str)> = Vec::with_capacity(blesses.len());
  for b in blesses {
    let (span, indent) = find_block_span(&ast, source, b.call)
      .ok_or_else(|| {
        format!(
          "bless: no `wat\":` block-string 2nd-arg found for call at line {}",
          b.call.start.line
        )
      })?;
    edits.push((span.0, span.1, indent, &b.actual));
  }

  // Sort by span start ascending -- REQUIRED: the offset accounting below is only
  // valid front-to-back by position, and input order is arbitrary.
  edits.sort_by_key(|e| e.0);

  // Splice, accumulating the byte delta so later spans stay aligned.
  let mut out = String::with_capacity(source.len());
  let mut cursor = 0usize;
  for (start, end, indent, actual) in edits {
    if start < cursor {
      return Err("bless: overlapping expectation spans".into());
    }
    out.push_str(&source[cursor..start]);
    out.push_str(&reindent_block(actual, indent));
    cursor = end;
  }
  out.push_str(&source[cursor..]);
  Ok(out)
}

/// Find the `wat":` block-string that is the 2nd argument of the `equals` call
/// the mark points at. Returns `((start, end), indent)` -- the byte span of the
/// block CONTENT (the lines after `wat":`) and the block's strip indent.
///
/// Anchoring: the mark's `call.start.line` is the `| equals wat":` line. We scan
/// the AST for a block-string `LitStr` (indent > 0) whose opening delimiter sits
/// on that line. FIRST SLICE limitation: block-string only.
fn find_block_span(
  ast: &ast::Ast<'_>,
  source: &str,
  call: Loc,
) -> Option<((usize, usize), u32)> {
  let want_line = call.start.line;
  // A block-string parses as per-line StrText tokens that MERGE into one LitStr:
  // each merge appends a NEW node (append-only arena) and leaves the shorter,
  // superseded node behind. All share the same `open` (`":`). So among block
  // LitStr nodes whose opener is on `want_line`, the FINAL merged node is the one
  // with the widest span (largest loc.end). Pick that.
  let mut best: Option<&ast::Node<'_>> = None;
  for i in 0..ast.nodes.len() {
    let node = ast.nodes.get(AstId::from(i));
    if let NodeKind::LitStr { open, indent, .. } = &node.kind
      && *indent > 0
      && open.loc.start.line == want_line
    {
      let wider = best
        .map(|b| node.loc.end.idx > b.loc.end.idx)
        .unwrap_or(true);
      if wider {
        best = Some(node);
      }
    }
  }
  let node = best?;
  let (open, indent) = match &node.kind {
    NodeKind::LitStr { open, indent, .. } => (*open, *indent),
    _ => unreachable!(),
  };
  // Replace the whole block body: from just after the `":` opener (open.end)
  // through the last StrText (node.loc.end). This region is
  // `\n<indent><line>\n<indent><line>...` -- reframe_block rebuilds it from the
  // actual, so the leading `\n` and per-line indent are regenerated, not kept.
  let start = open.loc.end.idx as usize;
  let end = (node.loc.end.idx as usize).min(source.len());
  Some(((start, end), indent))
}

/// Render `actual` as a block-string body indented to `indent` spaces: each line
/// prefixed with `indent` spaces, preserving the leading newline the block form
/// expects after `wat":`.
fn reindent_block(actual: &str, indent: u32) -> String {
  let pad = " ".repeat(indent as usize);
  let mut out = String::new();
  for line in actual.lines() {
    out.push('\n');
    if !line.is_empty() {
      out.push_str(&pad);
      out.push_str(line);
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::passes::ast::lexer::Pos;

  fn loc_on_line(line: u32) -> Loc {
    Loc { start: Pos { idx: 0, line, col: 0 }, end: Pos { idx: 0, line, col: 0 } }
  }

  // Find the source line of the `| equals wat":` opener so the test doesn't
  // hardcode it.
  fn equals_line(src: &str) -> u32 {
    (src.lines().position(|l| l.contains("equals wat\":")).unwrap() + 1) as u32
  }

  #[test]
  fn replaces_block_string_body() {
    // `wat` is a plain fn; `":` is a block-string literal (the expected value).
    // This mirrors the real snapshot tests' `... | equals wat":\n  <block>`.
    let src = "\
wat = fn src: src
x = 1
| equals wat\":
  (old line 1)
  (old line 2)
";
    let line = equals_line(src);
    let blesses = vec![Bless {
      call: loc_on_line(line),
      actual: "(new line 1)\n(new line 2)".into(),
    }];
    let out = apply_blesses(src, "test", &blesses).unwrap();
    let expected = "\
wat = fn src: src
x = 1
| equals wat\":
  (new line 1)
  (new line 2)
";
    assert_eq!(out, expected);
  }

  #[test]
  fn blessing_same_content_is_idempotent() {
    let src = "\
wat = fn src: src
x = 1
| equals wat\":
  (line a)
  (line b)
";
    let line = equals_line(src);
    let blesses = vec![Bless {
      call: loc_on_line(line),
      actual: "(line a)\n(line b)".into(),
    }];
    let out = apply_blesses(src, "test", &blesses).unwrap();
    assert_eq!(out, src, "blessing identical content changed the source");
  }

  #[test]
  fn missing_block_is_an_error() {
    let src = "x = 1\ny = 2\n";
    let blesses = vec![Bless { call: loc_on_line(1), actual: "z".into() }];
    assert!(apply_blesses(src, "test", &blesses).is_err());
  }
}
