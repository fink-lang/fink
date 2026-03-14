// AST → Fink source pretty-printer
//
// All output goes through MappedWriter so every emitted token is
// associated with its source location.  The public API offers both
// `fmt` (string only) and `fmt_mapped` (string + source map).

use crate::ast::{CmpPart, Node, NodeKind};
use crate::lexer::{Loc, Pos, Token};
use crate::sourcemap::{MappedWriter, SourceMap};

/// Format an AST back to Fink source, discarding source-map info.
pub fn fmt(node: &Node) -> String {
  let mut out = MappedWriter::new();
  fmt_node(node, &mut out, 0);
  out.finish_string()
}

/// Format an AST back to Fink source, returning source + source map.
pub fn fmt_mapped(node: &Node, source_name: &str) -> (String, SourceMap) {
  let mut out = MappedWriter::new();
  fmt_node(node, &mut out, 0);
  out.finish(source_name)
}

/// Format an AST back to Fink source, returning source + source map
/// with original source content embedded.
pub fn fmt_mapped_with_content(node: &Node, source_name: &str, content: &str) -> (String, SourceMap) {
  let mut out = MappedWriter::new();
  fmt_node(node, &mut out, 0);
  out.finish_with_content(source_name, content)
}

fn ind(out: &mut MappedWriter, depth: usize) {
  for _ in 0..depth {
    out.push_str("  ");
  }
}

fn is_fn(node: &Node) -> bool {
  matches!(node.kind, NodeKind::Fn { .. })
}

/// Check if a node produces multi-line output (block strings, fn bodies, match, etc.)
fn is_multiline(node: &Node) -> bool {
  match &node.kind {
    NodeKind::LitStr { open, content, .. } => open.src == "\":" || content.contains('\n'),
    NodeKind::StrRawTempl { open, .. } => open.src == "\":",
    NodeKind::Fn { body, .. } => body.items.len() > 1 || body.items.first().map_or(false, |b| !is_inline_expr(b)),
    NodeKind::Match { .. } | NodeKind::Block { .. } => true,
    NodeKind::Apply { args, .. } => args.items.iter().any(|a| is_multiline(a) || is_fn(a)),
    NodeKind::Pipe(exprs) => exprs.items.iter().any(|e| is_multiline(e)),
    _ => false,
  }
}

fn is_atom(node: &Node) -> bool {
  match &node.kind {
    NodeKind::LitStr { content, .. } => !content.contains('\n'),
    _ => matches!(
      node.kind,
      NodeKind::LitBool(_)
        | NodeKind::LitInt(_)
        | NodeKind::LitFloat(_)
        | NodeKind::LitDecimal(_)
        | NodeKind::Ident(_)
    ),
  }
}

fn fmt_node(node: &Node, out: &mut MappedWriter, depth: usize) {
  out.mark(node.loc);
  match &node.kind {
    NodeKind::LitBool(v) => out.push_str(if *v { "true" } else { "false" }),
    NodeKind::LitInt(s) => out.push_str(s),
    NodeKind::LitFloat(s) => out.push_str(s),
    NodeKind::LitDecimal(s) => out.push_str(s),
    NodeKind::LitStr { open, close, content: s } => {
      if open.src == "\":" {
        // Block string: emit ": followed by indented content lines
        // Map each line back to its source line (content starts one line after ":")
        let content = s.trim_end_matches('\n');
        let base_line = open.loc.end.line;  // line where ": appears
        out.push_str("\":");
        for (i, line) in content.split('\n').enumerate() {
          out.push('\n');
          ind(out, depth + 1);
          let src_line = base_line + i as u32 + 1;
          let src_pos = Pos { idx: 0, line: src_line, col: 0 };
          out.mark(Loc { start: src_pos, end: src_pos });
          out.push_str(line);
        }
      } else {
        out.push('\'');
        out.mark(Loc { start: open.loc.end, end: open.loc.end });
        if s.contains('\n') {
          for (i, line) in s.split('\n').enumerate() {
            if i > 0 {
              out.push('\n');
              ind(out, depth + 1);
            }
            out.push_str(line);
          }
        } else {
          out.push_str(s);
        }
        out.mark(close.loc);
        out.push('\'');
      }
    }
    NodeKind::LitSeq { close, items, .. } if items.items.is_empty() => {
      out.push('[');
      out.mark(close.loc);
      out.push(']');
    }
    NodeKind::LitSeq { close, items, .. } => {
      out.push('[');
      for (i, child) in items.items.iter().enumerate() {
        if i > 0 { out.push_str(", "); }
        fmt_node(child, out, depth);
      }
      out.mark(close.loc);
      out.push(']');
    }
    NodeKind::LitRec { close, items, .. } if items.items.is_empty() => {
      out.push('{');
      out.mark(close.loc);
      out.push('}');
    }
    NodeKind::LitRec { close, items, .. } => {
      out.push('{');
      for (i, child) in items.items.iter().enumerate() {
        if i > 0 { out.push_str(", "); }
        fmt_node(child, out, depth);
      }
      out.mark(close.loc);
      out.push('}');
    }
    NodeKind::StrRawTempl { open, close, children } => {
      // single LitStr child → raw string content (no quotes around the template itself;
      // the tag + quotes are handled by Apply above)
      if let [child] = children.as_slice() {
        if let NodeKind::LitStr { content: s, .. } = &child.kind {
          if open.src == "\":" {
            let content = s.trim_end_matches('\n');
            let base_line = open.loc.end.line;
            out.push_str("\":");
            for (i, line) in content.split('\n').enumerate() {
              out.push('\n');
              ind(out, depth + 1);
              let src_line = base_line + i as u32 + 1;
              let src_pos = Pos { idx: 0, line: src_line, col: 0 };
              out.mark(Loc { start: src_pos, end: src_pos });
              out.push_str(line);
            }
          } else {
            out.push('\'');
            out.mark(Loc { start: open.loc.end, end: open.loc.end });
            out.push_str(s);
            out.mark(close.loc);
            out.push('\'');
          }
          return;
        }
      }
      // fallback: print children joined
      for child in children {
        fmt_node(child, out, depth);
      }
    }
    NodeKind::Ident(s) => out.push_str(s),
    NodeKind::Spread { op, inner } => {
      out.mark(op.loc);
      out.push_str("..");
      if let Some(n) = inner {
        fmt_node(n, out, depth);
      }
    }
    NodeKind::Bind { op, lhs, rhs } => {
      fmt_node(lhs, out, depth);
      out.push(' ');
      out.mark(op.loc);
      out.push_str("= ");
      fmt_node(rhs, out, depth);
    }
    NodeKind::Apply { func, args } => fmt_apply(func, &args.items, out, depth),
    NodeKind::Fn { params, sep, body } => fmt_fn(params, sep, &body.items, out, depth),
    NodeKind::Patterns(exprs) => {
      for (i, child) in exprs.items.iter().enumerate() {
        if i > 0 { out.push_str(", "); }
        fmt_node(child, out, depth);
      }
    }
    NodeKind::UnaryOp { op, operand } => {
      out.mark(op.loc);
      out.push_str(op.src);
      if !op.src.starts_with('-') { out.push(' '); }
      fmt_node(operand, out, depth);
    }
    NodeKind::InfixOp { op, lhs, rhs } => {
      fmt_node(lhs, out, depth);
      out.push(' ');
      out.mark(op.loc);
      out.push_str(op.src);
      out.push(' ');
      fmt_node(rhs, out, depth);
    }
    NodeKind::ChainedCmp(parts) => {
      for part in parts.iter() {
        match part {
          CmpPart::Operand(n) => fmt_node(n, out, depth),
          CmpPart::Op(tok) => {
            out.push(' ');
            out.mark(tok.loc);
            out.push_str(tok.src);
            out.push(' ');
          }
        }
      }
    }
    NodeKind::Member { op, lhs, rhs } => {
      fmt_node(lhs, out, depth);
      out.mark(op.loc);
      out.push('.');
      fmt_node(rhs, out, depth);
    }
    NodeKind::Group { close, inner, .. } => {
      out.push('(');
      fmt_node(inner, out, depth);
      out.mark(close.loc);
      out.push(')');
    }
    NodeKind::Partial => out.push('?'),
    NodeKind::Wildcard => out.push('_'),
    NodeKind::BindRight { op, lhs, rhs } => {
      fmt_node(lhs, out, depth);
      out.push(' ');
      out.mark(op.loc);
      out.push_str("|= ");
      fmt_node(rhs, out, depth);
    }
    NodeKind::Pipe(exprs) => {
      let multiline = exprs.items.iter().any(|e| is_multiline(e));
      for (i, child) in exprs.items.iter().enumerate() {
        if i > 0 {
          if multiline {
            out.push('\n');
            ind(out, depth);
            out.push_str("| ");
          } else {
            out.push_str(" | ");
          }
        }
        fmt_node(child, out, depth);
      }
    }
    NodeKind::Match { subjects, sep, arms } => {
      out.push_str("match ");
      fmt_node(subjects, out, depth);
      out.mark(sep.loc);
      out.push(':');
      for arm in &arms.items {
        out.push('\n');
        ind(out, depth + 1);
        fmt_node(arm, out, depth + 1);
      }
    }
    NodeKind::Arm { lhs, sep, body } => {
      for (i, pat) in lhs.items.iter().enumerate() {
        if i > 0 { out.push_str(", "); }
        fmt_node(pat, out, depth);
      }
      out.mark(sep.loc);
      out.push(':');
      fmt_body(&body.items, out, depth, true);
    }
    NodeKind::Try(inner) => {
      out.push_str("try ");
      fmt_node(inner, out, depth);
    }
    NodeKind::Yield(inner) => {
      out.push_str("yield ");
      fmt_node(inner, out, depth);
    }
    NodeKind::StrTempl { children, .. } => {
      for child in children {
        fmt_node(child, out, depth);
      }
    }
    NodeKind::Block { name, params, sep, body } => {
      fmt_node(name, out, depth);
      out.push(' ');
      fmt_node(params, out, depth);
      out.mark(sep.loc);
      out.push(':');
      fmt_body(&body.items, out, depth, true);
    }
  }
}

fn is_complex_arg(node: &Node) -> bool {
  // An arg that has fn args inside it — should go on its own indented line
  match &node.kind {
    NodeKind::Apply { args, .. } => args.items.iter().any(is_fn),
    _ => false,
  }
}

fn fmt_apply(func: &Node, args: &[Node], out: &mut MappedWriter, depth: usize) {
  // Tagged string literal: `id'foo'`, `op'+'` — func ident + single quoted string arg, no separator
  // Excludes block strings (`":`) which need a space separator
  if let [arg] = args {
    if matches!(func.kind, NodeKind::Ident(_)) && !is_multiline(arg) {
      if matches!(arg.kind, NodeKind::StrRawTempl { .. } | NodeKind::LitStr { .. }) {
        fmt_node(func, out, depth);
        fmt_node(arg, out, depth);
        return;
      }
    }
  }

  fmt_node(func, out, depth);

  // Split args into leading non-fn args and trailing fn/complex args
  // "Complex" args (applies with fn args) get treated like trailing fns — each on its own line
  let trailing_start = args.iter().rposition(|a| !is_fn(a) && !is_complex_arg(a))
    .map(|i| i + 1).unwrap_or(0);
  let (plain, trailing) = args.split_at(trailing_start);

  // First plain arg: space separator; rest: ", "
  for (i, arg) in plain.iter().enumerate() {
    if i == 0 { out.push(' '); } else { out.push_str(", "); }
    fmt_node(arg, out, depth);
  }

  if trailing.is_empty() {
    return;
  }

  // Single trailing fn (no complex args) → keep `fn params:` on same line
  if trailing.len() == 1 && is_fn(&trailing[0]) {
    if let NodeKind::Fn { params, sep, body } = &trailing[0].kind {
      if plain.is_empty() { out.push(' '); } else { out.push_str(", "); }
      fmt_fn_with_inline(params, sep, &body.items, out, depth, false);
      return;
    }
  }

  // Multiple trailing fns/complex args → each on its own indented line
  if !plain.is_empty() { out.push(','); }
  for arg in trailing {
    out.push('\n');
    ind(out, depth + 1);
    if let NodeKind::Fn { params, sep, body } = &arg.kind {
      fmt_fn_with_inline(params, sep, &body.items, out, depth + 1, true);
    } else {
      fmt_node(arg, out, depth + 1);
    }
  }
}

fn fmt_fn(params: &Node, sep: &Token, body: &[Node], out: &mut MappedWriter, depth: usize) {
  fmt_fn_with_inline(params, sep, body, out, depth, true);
}

fn fmt_fn_with_inline(params: &Node, sep: &Token, body: &[Node], out: &mut MappedWriter, depth: usize, allow_apply_inline: bool) {
  let inline = body.len() == 1 && if allow_apply_inline {
    is_inline_expr(&body[0])
  } else {
    is_inline_single_trailing(&body[0])
  };
  if inline {
    fmt_fn_inline(params, sep, &body[0], out, depth);
  } else {
    fmt_fn_params(params, out);
    out.mark(sep.loc);
    out.push(':');
    fmt_body(body, out, depth, allow_apply_inline);
  }
}

/// Inline after `fn params: ` in general (standalone fn, stacked fn args)
fn is_inline_expr(node: &Node) -> bool {
  if is_multiline(node) { return false; }
  match &node.kind {
    // apply with no trailing fn args and no multiline args → inline
    NodeKind::Apply { args, .. } => !args.items.iter().any(|a| is_fn(a) || is_multiline(a)),
    _ => is_atom(node),
  }
}

/// Inline after `fn params: ` when it's the single trailing fn in an apply call
fn is_inline_single_trailing(node: &Node) -> bool {
  is_atom(node)
}

fn fmt_fn_inline(params: &Node, sep: &Token, expr: &Node, out: &mut MappedWriter, depth: usize) {
  fmt_fn_params(params, out);
  out.mark(sep.loc);
  out.push_str(": ");
  fmt_node(expr, out, depth);
}

fn fmt_fn_params(params: &Node, out: &mut MappedWriter) {
  out.push_str("fn");
  if let NodeKind::Patterns(exprs) = &params.kind {
    for (i, child) in exprs.items.iter().enumerate() {
      if i == 0 { out.push(' '); } else { out.push_str(", "); }
      fmt_node(child, out, 0);
    }
  } else {
    out.push(' ');
    fmt_node(params, out, 0);
  }
}

fn fmt_body(body: &[Node], out: &mut MappedWriter, depth: usize, allow_apply_inline: bool) {
  if body.len() == 1 {
    let inline = if allow_apply_inline {
      is_inline_expr(&body[0])
    } else {
      is_inline_single_trailing(&body[0])
    };
    if inline {
      out.push(' ');
      fmt_node(&body[0], out, depth);
      return;
    }
  }
  // Block body: each statement on its own indented line
  for stmt in body {
    out.push('\n');
    ind(out, depth + 1);
    fmt_node(stmt, out, depth + 1);
  }
}


// --- tests ---

#[cfg(test)]
mod tests {
  use super::fmt as ast_fmt;
  use crate::parser::parse;

  fn fmt(src: &str) -> String {
    let result = parse(src).expect("parse failed");
    ast_fmt(&result.root)
  }

  test_macros::include_fink_tests!("src/passes/ast/test_fmt.fnk");

  // --- multiline string indentation tests ---

  #[test]
  fn fmt_multiline_string_at_top_level() {
    // Multiline quoted string at depth 0 — continuation lines at depth+1
    let out = fmt("'foo\nbar\nspam'");
    assert_eq!(out, "'foo\n  bar\n  spam'");
  }

  #[test]
  fn fmt_multiline_string_in_fn_body() {
    // Multiline string in apply inside fn body — continuation lines indented under apply
    let out = fmt("fn:\n  log 'foo\n  bar\n  spam'");
    assert_eq!(out, "fn:\n  log 'foo\n    bar\n    spam'");
  }

  // --- source map tests ---

  use super::fmt_mapped;

  /// Parse source, format with source map, return mappings as (out_line, out_col, src_line, src_col).
  fn mappings(src: &str) -> Vec<(u32, u32, u32, u32)> {
    let result = parse(src).expect("parse failed");
    let (_, srcmap) = fmt_mapped(&result.root, "test.fnk");
    srcmap.iter().collect()
  }

  #[test]
  fn sourcemap_ident() {
    // "foo" → single mapping at (0,0) → source (0,0)
    let m = mappings("foo");
    assert_eq!(m, vec![(0, 0, 0, 0)]);
  }

  #[test]
  fn sourcemap_string_literal() {
    // "'hello'" → mapping for node start + string content + closing quote
    let m = mappings("'hello'");
    assert_eq!(m, vec![
      (0, 0, 0, 0),  // LitStr node (opening quote)
      (0, 1, 0, 1),  // string content
      (0, 6, 0, 6),  // closing quote
    ]);
  }

  #[test]
  fn sourcemap_bind() {
    // "foo = bar" → Bind, lhs Ident, = operator, rhs Ident
    let m = mappings("foo = bar");
    assert_eq!(m, vec![
      (0, 0, 0, 0),  // Bind node
      (0, 0, 0, 0),  // Ident 'foo'
      (0, 4, 0, 4),  // = operator
      (0, 6, 0, 6),  // Ident 'bar'
    ]);
  }

  #[test]
  fn sourcemap_apply() {
    // "foo bar" → Apply, func Ident, arg Ident
    let m = mappings("foo bar");
    assert_eq!(m, vec![
      (0, 0, 0, 0),  // Apply node
      (0, 0, 0, 0),  // Ident 'foo'
      (0, 4, 0, 4),  // Ident 'bar'
    ]);
  }

  #[test]
  fn sourcemap_apply_multiple_args() {
    // "foo a, b" → Apply, func, arg, arg
    let m = mappings("foo a, b");
    assert_eq!(m, vec![
      (0, 0, 0, 0),  // Apply node
      (0, 0, 0, 0),  // Ident 'foo'
      (0, 4, 0, 4),  // Ident 'a'
      (0, 7, 0, 7),  // Ident 'b'
    ]);
  }

  #[test]
  fn sourcemap_fn_inline() {
    // "fn x:\n  foo x" → inlined to "fn x: foo x" (single apply body)
    let m = mappings("fn x:\n  foo x");
    assert_eq!(m, vec![
      (0, 0, 0, 0),   // Fn node
      (0, 3, 0, 3),   // Patterns → Ident 'x'
      (0, 4, 0, 4),   // : separator
      (0, 6, 1, 2),   // Apply 'foo x'
      (0, 6, 1, 2),   // Ident 'foo'
      (0, 10, 1, 6),  // Ident 'x'
    ]);
  }

  #[test]
  fn sourcemap_fn_multiline_body() {
    // Multi-statement body stays multi-line
    let m = mappings("fn x:\n  foo x\n  bar x");
    let lines: Vec<u32> = m.iter().map(|&(l, _, _, _)| l).collect();
    assert!(lines.contains(&0), "should have line 0 mappings");
    assert!(lines.contains(&1), "should have line 1 mappings");
    assert!(lines.contains(&2), "should have line 2 mappings");
  }

  #[test]
  fn sourcemap_infix() {
    // "a + b" → InfixOp, lhs, + operator, rhs
    let m = mappings("a + b");
    assert_eq!(m, vec![
      (0, 0, 0, 0),  // InfixOp node
      (0, 0, 0, 0),  // Ident 'a'
      (0, 2, 0, 2),  // + operator
      (0, 4, 0, 4),  // Ident 'b'
    ]);
  }

  #[test]
  fn sourcemap_mapping_count() {
    // Each node produces one mapping; operators/delimiters add their own
    let m = mappings("foo");
    assert_eq!(m.len(), 1);

    let m = mappings("foo bar");
    assert_eq!(m.len(), 3); // Apply + func + arg

    let m = mappings("foo = bar baz");
    assert_eq!(m.len(), 6); // Bind + lhs + = + Apply + func + arg
  }
}
