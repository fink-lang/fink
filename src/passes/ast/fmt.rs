// AST → Fink source pretty-printer
//
// All output goes through MappedWriter so every emitted token is
// associated with its source location.  The public API offers both
// `fmt` (string only) and `fmt_mapped` (string + source map).

use std::cell::Cell;

use crate::ast::{Ast, AstId, CmpPart, Node, NodeKind};
use crate::lexer::{Loc, Pos, Token};
use crate::sourcemap::{MappedWriter, SourceMap};

thread_local! {
  /// When true, fn bodies are never inlined — always rendered as indented blocks.
  /// Used by CPS/lifting formatters where all fn bodies should be block-style.
  static FORCE_BLOCK_FN_BODIES: Cell<bool> = const { Cell::new(false) };
}

/// Format an AST back to Fink source, discarding source-map info.
pub fn fmt(ast: &Ast<'_>) -> String {
  let mut out = MappedWriter::new();
  fmt_node(ast, ast.root, &mut out, 0);
  out.finish_string()
}

/// Format an AST back to Fink source with fn bodies always on new lines (for CPS output).
pub fn fmt_block(ast: &Ast<'_>) -> String {
  FORCE_BLOCK_FN_BODIES.with(|f| f.set(true));
  let mut out = MappedWriter::new();
  fmt_node(ast, ast.root, &mut out, 0);
  let result = out.finish_string();
  FORCE_BLOCK_FN_BODIES.with(|f| f.set(false));
  result
}

/// Format an AST back to Fink source, returning source + source map.
pub fn fmt_mapped(ast: &Ast<'_>, source_name: &str) -> (String, SourceMap) {
  let mut out = MappedWriter::new();
  fmt_node(ast, ast.root, &mut out, 0);
  out.finish(source_name)
}

/// Format an AST back to Fink source, returning source + source map
/// with original source content embedded.
pub fn fmt_mapped_with_content(ast: &Ast<'_>, source_name: &str, content: &str) -> (String, SourceMap) {
  let mut out = MappedWriter::new();
  fmt_node(ast, ast.root, &mut out, 0);
  out.finish_with_content(source_name, content)
}

/// Emit a sentinel mark (line 0) to stop the previous mapping from bleeding
/// into structural text (separators, keywords) that has no source origin.
fn stop_mark(out: &mut MappedWriter) {
  let p = Pos { idx: 0, line: 0, col: 0 };
  out.mark(Loc { start: p, end: p });
}

fn ind(out: &mut MappedWriter, depth: usize) {
  for _ in 0..depth {
    out.push_str("  ");
  }
}

fn is_fn(ast: &Ast<'_>, id: AstId) -> bool {
  matches!(ast.nodes.get(id).kind, NodeKind::Fn { .. })
}

/// Check if a node produces multi-line output (block strings, fn bodies, match, etc.)
fn is_multiline(ast: &Ast<'_>, id: AstId) -> bool {
  match &ast.nodes.get(id).kind {
    NodeKind::LitStr { open, content, .. } => open.src == "\":" || content.contains('\n'),
    NodeKind::StrRawTempl { open, .. } => open.src == "\":",
    NodeKind::Fn { body, .. } => body.items.len() > 1 || body.items.first().is_some_and(|&b| !is_inline_expr(ast, b)),
    NodeKind::Match { .. } | NodeKind::Block { .. } => true,
    NodeKind::Apply { args, .. } => args.items.iter().any(|&a| is_multiline(ast, a) || is_fn(ast, a)),
    NodeKind::Pipe(exprs) => exprs.items.iter().any(|&e| is_multiline(ast, e)),
    _ => false,
  }
}

fn is_atom(ast: &Ast<'_>, id: AstId) -> bool {
  let node = ast.nodes.get(id);
  match &node.kind {
    NodeKind::LitStr { content, .. } => !content.contains('\n'),
    _ => matches!(
      node.kind,
      NodeKind::LitBool(_)
        | NodeKind::LitInt(_)
        | NodeKind::LitFloat(_)
        | NodeKind::LitDecimal(_)
        | NodeKind::Ident(_)
        | NodeKind::SynthIdent(_)
    ),
  }
}

fn fmt_node(ast: &Ast<'_>, id: AstId, out: &mut MappedWriter, depth: usize) {
  let node: &Node<'_> = ast.nodes.get(id);
  if !matches!(node.kind, NodeKind::Module { .. }) { out.mark(node.loc); }
  // Clone kind out so we don't hold a borrow of `ast` during recursive calls.
  // NodeKind clone is cheap: children are AstId (Copy), tokens are Copy,
  // only LitStr's String and Module's url are owned heap data.
  let node_loc = node.loc;
  let kind = node.kind.clone();
  match kind {
    NodeKind::LitBool(v) => out.push_str(if v { "true" } else { "false" }),
    NodeKind::LitInt(s) => out.push_str(s),
    NodeKind::LitFloat(s) => out.push_str(s),
    NodeKind::LitDecimal(s) => out.push_str(s),
    NodeKind::LitStr { open, close, content: s, .. } => {
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
        if open.loc.start.line == 0 {
          // Synthetic string (CPS formatter): unmap the quote, map content to node loc.
          stop_mark(out);
          out.push('\'');
          out.mark(node_loc);
        } else {
          out.push('\'');
          out.mark(Loc { start: open.loc.end, end: open.loc.end });
        }
        if s.contains('\n') {
          for (i, line) in s.split('\n').enumerate() {
            if i > 0 {
              out.push('\n');
              ind(out, depth + 1);
            }
            out.push_str(line);
          }
        } else {
          out.push_str(&s);
        }
        out.mark(close.loc);
        out.push('\'');
      }
    }
    NodeKind::LitSeq { open, close, items, .. } if items.items.is_empty() => {
      out.mark(open.loc);
      out.push('[');
      out.mark(close.loc);
      out.push(']');
    }
    NodeKind::LitSeq { open, close, items, .. } => {
      out.mark(open.loc);
      out.push('[');
      for (i, &child_id) in items.items.iter().enumerate() {
        if i > 0 { stop_mark(out); out.push_str(", "); }
        fmt_node(ast, child_id, out, depth);
      }
      stop_mark(out);
      out.mark(close.loc);
      out.push(']');
    }
    NodeKind::LitRec { open, close, items, .. } if items.items.is_empty() => {
      out.mark(open.loc);
      out.push('{');
      out.mark(close.loc);
      out.push('}');
    }
    NodeKind::LitRec { open, close, items, .. } => {
      out.mark(open.loc);
      out.push('{');
      for (i, &child_id) in items.items.iter().enumerate() {
        if i > 0 { stop_mark(out); out.push_str(", "); }
        fmt_node(ast, child_id, out, depth);
      }
      stop_mark(out);
      out.mark(close.loc);
      out.push('}');
    }
    NodeKind::StrRawTempl { open, close, children } => {
      // The tag ident and opening quote are handled by Apply (fmt_apply).
      // This node emits the quoted content: 'text ${expr} text'
      // For block raw templates (":`), emit ":" + indented lines with ${expr} interpolation.
      if open.src == "\":" {
        let base_line = open.loc.end.line;
        out.push_str("\":");
        let mut at_line_start = true;
        let mut src_line_offset = 0u32;
        for &child_id in children.iter() {
          let child = ast.nodes.get(child_id);
          match &child.kind {
            NodeKind::LitStr { content: s, .. } => {
              for (i, line) in s.split('\n').enumerate() {
                if i > 0 || at_line_start {
                  out.push('\n');
                  ind(out, depth + 1);
                  let src_pos = Pos { idx: 0, line: base_line + src_line_offset + 1, col: 0 };
                  out.mark(Loc { start: src_pos, end: src_pos });
                  if i > 0 { src_line_offset += 1; }
                }
                out.push_str(line);
              }
              at_line_start = s.ends_with('\n');
            }
            _ => {
              if at_line_start {
                out.push('\n');
                ind(out, depth + 1);
                at_line_start = false;
              }
              out.push_str("${");
              fmt_node(ast, child_id, out, depth);
              out.push('}');
            }
          }
        }
      } else {
        // Quoted raw template: 'text \${expr} text'
        // Interpolation uses \${...} syntax (unlike StrTempl which uses \${...} differently)
        out.push('\'');
        for &child_id in children.iter() {
          let child = ast.nodes.get(child_id);
          match &child.kind {
            NodeKind::LitStr { content: s, .. } => out.push_str(s),
            _ => {
              out.push_str("\\${");
              fmt_node(ast, child_id, out, depth);
              out.push('}');
            }
          }
        }
        out.mark(close.loc);
        out.push('\'');
      }
    }
    NodeKind::Ident(s) => out.push_str(s),
    NodeKind::SynthIdent(n) => out.push_str(&format!("·$_{n}")),
    NodeKind::Spread { op, inner } => {
      out.mark(op.loc);
      out.push_str("..");
      if let Some(inner_id) = inner {
        fmt_node(ast, inner_id, out, depth);
      }
    }
    NodeKind::Bind { op, lhs, rhs } => {
      fmt_node(ast, lhs, out, depth);
      out.push(' ');
      out.mark(op.loc);
      out.push_str("= ");
      fmt_node(ast, rhs, out, depth);
    }
    NodeKind::Apply { func, args } => fmt_apply(ast, func, &args.items, out, depth),
    NodeKind::Module { exprs, .. } => {
      for (i, &child_id) in exprs.items.iter().enumerate() {
        if i > 0 { out.push('\n'); ind(out, depth); }
        fmt_node(ast, child_id, out, depth);
      }
    }
    NodeKind::Fn { params, sep, body } => fmt_fn(ast, params, &sep, &body.items, out, depth),
    NodeKind::Patterns(exprs) => {
      for (i, &child_id) in exprs.items.iter().enumerate() {
        if i > 0 { out.push_str(", "); }
        fmt_node(ast, child_id, out, depth);
      }
    }
    NodeKind::UnaryOp { op, operand } => {
      out.mark(op.loc);
      out.push_str(op.src);
      if !op.src.starts_with('-') { out.push(' '); }
      fmt_node(ast, operand, out, depth);
    }
    NodeKind::InfixOp { op, lhs, rhs } => {
      fmt_node(ast, lhs, out, depth);
      out.push(' ');
      out.mark(op.loc);
      out.push_str(op.src);
      out.push(' ');
      fmt_node(ast, rhs, out, depth);
    }
    NodeKind::ChainedCmp(parts) => {
      for part in parts.iter() {
        match part {
          CmpPart::Operand(n) => fmt_node(ast, *n, out, depth),
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
      fmt_node(ast, lhs, out, depth);
      out.mark(op.loc);
      out.push('.');
      fmt_node(ast, rhs, out, depth);
    }
    NodeKind::Group { close, inner, .. } => {
      out.push('(');
      fmt_node(ast, inner, out, depth);
      out.mark(close.loc);
      out.push(')');
    }
    NodeKind::Partial => out.push('?'),
    NodeKind::Wildcard => out.push('_'),
    NodeKind::Token(s) => out.push_str(s),
    NodeKind::BindRight { op, lhs, rhs } => {
      fmt_node(ast, lhs, out, depth);
      out.push(' ');
      out.mark(op.loc);
      out.push_str("|= ");
      fmt_node(ast, rhs, out, depth);
    }
    NodeKind::Pipe(exprs) => {
      let multiline = exprs.items.iter().any(|&e| is_multiline(ast, e));
      for (i, &child_id) in exprs.items.iter().enumerate() {
        if i > 0 {
          if multiline {
            out.push('\n');
            ind(out, depth);
            out.push_str("| ");
          } else {
            out.push_str(" | ");
          }
        }
        fmt_node(ast, child_id, out, depth);
      }
    }
    NodeKind::Match { subjects, sep, arms } => {
      out.push_str("match ");
      for (i, &subj_id) in subjects.items.iter().enumerate() {
        if i > 0 { out.push_str(", "); }
        fmt_node(ast, subj_id, out, depth);
      }
      out.mark(sep.loc);
      out.push(':');
      for &arm_id in arms.items.iter() {
        out.push('\n');
        ind(out, depth + 1);
        fmt_node(ast, arm_id, out, depth + 1);
      }
    }
    NodeKind::Arm { lhs, sep, body } => {
      fmt_node(ast, lhs, out, depth);
      out.mark(sep.loc);
      out.push(':');
      fmt_body(ast, &body.items, out, depth, true);
    }
    NodeKind::Try(inner) => {
      out.push_str("try ");
      fmt_node(ast, inner, out, depth);
    }
    NodeKind::StrTempl { open, close, children } => {
      if open.src == "\":" {
        // Block string with interpolation
        out.push_str("\":");
        // Track whether we're at start of a line (after \n) for indentation
        let mut at_line_start = true;
        for &child_id in children.iter() {
          let child = ast.nodes.get(child_id);
          match &child.kind {
            NodeKind::LitStr { content: s, .. } => {
              for (i, line) in s.split('\n').enumerate() {
                if i > 0 || at_line_start {
                  out.push('\n');
                  ind(out, depth + 1);
                }
                out.push_str(line);
              }
              at_line_start = s.ends_with('\n');
            }
            _ => {
              if at_line_start {
                out.push('\n');
                ind(out, depth + 1);
                at_line_start = false;
              }
              out.push_str("${");
              fmt_node(ast, child_id, out, depth);
              out.push('}');
            }
          }
        }
      } else {
        // Quoted string with interpolation
        out.push('\'');
        for &child_id in children.iter() {
          let child = ast.nodes.get(child_id);
          match &child.kind {
            NodeKind::LitStr { content: s, .. } => {
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
            }
            _ => {
              out.push_str("\\${");
              fmt_node(ast, child_id, out, depth);
              out.push('}');
            }
          }
        }
        out.mark(close.loc);
        out.push('\'');
      }
    }
    NodeKind::Block { name, params, sep, body } => {
      fmt_node(ast, name, out, depth);
      out.push(' ');
      fmt_node(ast, params, out, depth);
      out.mark(sep.loc);
      out.push(':');
      fmt_body(ast, &body.items, out, depth, true);
    }
  }
}

fn is_complex_arg(ast: &Ast<'_>, id: AstId) -> bool {
  // An arg that has fn args inside it — should go on its own indented line
  match &ast.nodes.get(id).kind {
    NodeKind::Apply { args, .. } => args.items.iter().any(|&a| is_fn(ast, a)),
    _ => false,
  }
}

fn fmt_apply(ast: &Ast<'_>, func: AstId, args: &[AstId], out: &mut MappedWriter, depth: usize) {
  // Tagged string literal: `id'foo'`, `op'+'` — func ident + single quoted string arg, no separator
  // Excludes block strings (`":`) which need a space separator.
  // Excludes CPS primitives (·-prefixed idents) which are not user-level tagged templates.
  if let [arg_id] = args {
    let arg = ast.nodes.get(*arg_id);
    let func_node = ast.nodes.get(func);
    if let NodeKind::Ident(func_name) = &func_node.kind
      && !func_name.starts_with('·') && !is_multiline(ast, *arg_id)
        && matches!(arg.kind, NodeKind::StrRawTempl { .. } | NodeKind::LitStr { .. })
      {
        fmt_node(ast, func, out, depth);
        fmt_node(ast, *arg_id, out, depth);
        return;
      }
  }

  fmt_node(ast, func, out, depth);

  // Split args into leading non-fn args and trailing fn/complex args
  // "Complex" args (applies with fn args) get treated like trailing fns — each on its own line
  let trailing_start = args.iter().rposition(|&a| !is_fn(ast, a) && !is_complex_arg(ast, a))
    .map(|i| i + 1).unwrap_or(0);
  let (plain, trailing) = args.split_at(trailing_start);

  // Bail-out: if a non-last arg in `plain` is fn or multiline, inline
  // "..., next_arg" rendering would corrupt the output — the multiline arg's
  // tail line would absorb subsequent `, next_arg` tokens. Force full block
  // mode: every arg on its own indented line.
  let has_bad_inline = plain.len() >= 2 && plain[..plain.len() - 1]
    .iter().any(|&a| is_fn(ast, a) || is_multiline(ast, a));
  if has_bad_inline {
    for &arg_id in args {
      out.push('\n');
      ind(out, depth + 1);
      let arg_node = ast.nodes.get(arg_id);
      if let NodeKind::Fn { params, sep, body } = &arg_node.kind {
        let params = *params;
        let sep = *sep;
        let body_items: Vec<AstId> = body.items.to_vec();
        fmt_fn_with_inline(ast, params, &sep, &body_items, out, depth + 1, true);
      } else {
        fmt_node(ast, arg_id, out, depth + 1);
      }
    }
    return;
  }

  // First plain arg: space separator; rest: ", "
  for (i, &arg_id) in plain.iter().enumerate() {
    if i == 0 { out.push(' '); } else { stop_mark(out); out.push_str(", "); }
    fmt_node(ast, arg_id, out, depth);
  }

  if trailing.is_empty() {
    return;
  }

  // Single trailing fn (no complex args) → keep `fn params:` on same line
  if trailing.len() == 1 && is_fn(ast, trailing[0]) {
    let trailing_node = ast.nodes.get(trailing[0]);
    if let NodeKind::Fn { params, sep, body } = &trailing_node.kind {
      let params = *params;
      let sep = *sep;
      let body_items: Vec<AstId> = body.items.to_vec();
      if plain.is_empty() { out.push(' '); } else { stop_mark(out); out.push_str(", "); }
      fmt_fn_with_inline(ast, params, &sep, &body_items, out, depth, false);
      return;
    }
  }

  // Multiple trailing fns/complex args → each on its own indented line
  if !plain.is_empty() { stop_mark(out); out.push(','); }
  for &arg_id in trailing {
    out.push('\n');
    ind(out, depth + 1);
    let arg_node = ast.nodes.get(arg_id);
    if let NodeKind::Fn { params, sep, body } = &arg_node.kind {
      let params = *params;
      let sep = *sep;
      let body_items: Vec<AstId> = body.items.to_vec();
      fmt_fn_with_inline(ast, params, &sep, &body_items, out, depth + 1, true);
    } else {
      fmt_node(ast, arg_id, out, depth + 1);
    }
  }
}

fn fmt_fn(ast: &Ast<'_>, params: AstId, sep: &Token, body: &[AstId], out: &mut MappedWriter, depth: usize) {
  fmt_fn_with_inline(ast, params, sep, body, out, depth, true);
}

fn fmt_fn_with_inline(ast: &Ast<'_>, params: AstId, sep: &Token, body: &[AstId], out: &mut MappedWriter, depth: usize, allow_apply_inline: bool) {
  let inline = body.len() == 1 && if allow_apply_inline {
    is_inline_expr(ast, body[0])
  } else {
    is_inline_single_trailing(ast, body[0])
  };
  if inline {
    fmt_fn_inline(ast, params, sep, body[0], out, depth);
  } else {
    fmt_fn_params(ast, params, out);
    out.mark(sep.loc);
    out.push(':');
    fmt_body(ast, body, out, depth, allow_apply_inline);
  }
}

/// Inline after `fn params: ` in general (standalone fn, stacked fn args)
fn is_inline_expr(ast: &Ast<'_>, id: AstId) -> bool {
  if FORCE_BLOCK_FN_BODIES.with(|f| f.get()) { return false; }
  if is_multiline(ast, id) { return false; }
  match &ast.nodes.get(id).kind {
    // apply with no trailing fn args and no multiline args → inline
    NodeKind::Apply { args, .. } => !args.items.iter().any(|&a| is_fn(ast, a) || is_multiline(ast, a)),
    _ => is_atom(ast, id),
  }
}

/// Inline after `fn params: ` when it's the single trailing fn in an apply call
fn is_inline_single_trailing(ast: &Ast<'_>, id: AstId) -> bool {
  if FORCE_BLOCK_FN_BODIES.with(|f| f.get()) { return false; }
  is_atom(ast, id)
}

fn fmt_fn_inline(ast: &Ast<'_>, params: AstId, sep: &Token, expr: AstId, out: &mut MappedWriter, depth: usize) {
  fmt_fn_params(ast, params, out);
  out.mark(sep.loc);
  out.push_str(": ");
  fmt_node(ast, expr, out, depth);
}

fn fmt_fn_params(ast: &Ast<'_>, params: AstId, out: &mut MappedWriter) {
  out.push_str("fn");
  if let NodeKind::Patterns(exprs) = &ast.nodes.get(params).kind {
    let items: Vec<AstId> = exprs.items.to_vec();
    for (i, child_id) in items.iter().enumerate() {
      if i == 0 { out.push(' '); } else { out.push_str(", "); }
      fmt_node(ast, *child_id, out, 0);
    }
  } else {
    out.push(' ');
    fmt_node(ast, params, out, 0);
  }
}

fn fmt_body(ast: &Ast<'_>, body: &[AstId], out: &mut MappedWriter, depth: usize, allow_apply_inline: bool) {
  if body.len() == 1 {
    let inline = if allow_apply_inline {
      is_inline_expr(ast, body[0])
    } else {
      is_inline_single_trailing(ast, body[0])
    };
    if inline {
      out.push(' ');
      fmt_node(ast, body[0], out, depth);
      return;
    }
  }
  // Block body: each statement on its own indented line
  for &stmt_id in body {
    out.push('\n');
    ind(out, depth + 1);
    fmt_node(ast, stmt_id, out, depth + 1);
  }
}


// --- tests ---

#[cfg(test)]
mod tests {
  use super::fmt as ast_fmt;
  use crate::parser::parse;

  fn fmt(src: &str) -> String {
    let ast = parse(src, "test").expect("parse failed");
    ast_fmt(&ast)
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

  // --- string interpolation (StrTempl) tests ---

  #[test]
  fn fmt_block_string_interpolation() {
    // Block string with interpolation: ": \n  hello ${name}
    let out = fmt("\":\n  supports templating ${bar}\n  no need to escape 'spam'");
    assert_eq!(out, "\":\n  supports templating ${bar}\n  no need to escape 'spam'");
  }

  #[test]
  fn fmt_quoted_string_interpolation() {
    // Quoted string with interpolation: 'hello ${name}'
    let out = fmt("'hello \\${name}'");
    assert_eq!(out, "'hello \\${name}'");
  }

  #[test]
  fn fmt_block_string_interpolation_in_fn_body() {
    // Block string interpolation nested in fn body
    let out = fmt("fn:\n  \":\n    hello ${name}");
    assert_eq!(out, "fn:\n  \":\n    hello ${name}");
  }

  // --- source map tests ---

  use super::fmt_mapped;

  /// Parse source, format with source map, return mappings as (out_line, out_col, src_line, src_col).
  fn mappings(src: &str) -> Vec<(u32, u32, u32, u32)> {
    let ast = parse(src, "test.fnk").expect("parse failed");
    let (_, srcmap) = fmt_mapped(&ast, "test.fnk");
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
