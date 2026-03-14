// AST → Fink source pretty-printer
//
// All output goes through MappedWriter so every emitted token is
// associated with its source location.  The public API offers both
// `fmt` (string only) and `fmt_mapped` (string + source map).

use crate::ast::{CmpPart, Exprs, Node, NodeKind};
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
    NodeKind::LitStr { open, .. } => open.src == "\":",
    NodeKind::StrRawTempl { open, .. } => open.src == "\":",
    NodeKind::Fn { body, .. } => body.items.len() > 1 || body.items.first().map_or(false, |b| !is_inline_expr(b)),
    NodeKind::Match { .. } | NodeKind::Block { .. } => true,
    NodeKind::Apply { args, .. } => args.items.iter().any(|a| is_multiline(a) || is_fn(a)),
    NodeKind::Pipe(exprs) => exprs.items.iter().any(|e| is_multiline(e)),
    _ => false,
  }
}

fn is_atom(node: &Node) -> bool {
  matches!(
    node.kind,
    NodeKind::LitBool(_)
      | NodeKind::LitInt(_)
      | NodeKind::LitFloat(_)
      | NodeKind::LitDecimal(_)
      | NodeKind::LitStr { .. }
      | NodeKind::Ident(_)
  )
}

fn fmt_node(node: &Node, out: &mut MappedWriter, depth: usize) {
  out.mark(node.loc);
  match &node.kind {
    NodeKind::LitBool(v) => out.push_str(if *v { "true" } else { "false" }),
    NodeKind::LitInt(s) => out.push_str(s),
    NodeKind::LitFloat(s) => out.push_str(s),
    NodeKind::LitDecimal(s) => out.push_str(s),
    NodeKind::LitStr { open, content: s, .. } => {
      if open.src == "\":" {
        // Block string: emit ": followed by indented content lines
        let content = s.trim_end_matches('\n');
        out.push_str("\":");
        for line in content.split('\n') {
          out.push('\n');
          ind(out, depth + 1);
          out.push_str(line);
        }
      } else {
        out.push('\'');
        out.push_str(s);
        out.push('\'');
      }
    }
    NodeKind::LitSeq { items, .. } if items.items.is_empty() => out.push_str("[]"),
    NodeKind::LitSeq { items, .. } => {
      out.push('[');
      for (i, child) in items.items.iter().enumerate() {
        if i > 0 { out.push_str(", "); }
        fmt_node(child, out, depth);
      }
      out.push(']');
    }
    NodeKind::LitRec { items, .. } if items.items.is_empty() => out.push_str("{}"),
    NodeKind::LitRec { items, .. } => {
      out.push('{');
      for (i, child) in items.items.iter().enumerate() {
        if i > 0 { out.push_str(", "); }
        fmt_node(child, out, depth);
      }
      out.push('}');
    }
    NodeKind::StrRawTempl { open, children, .. } => {
      // single LitStr child → raw string content (no quotes around the template itself;
      // the tag + quotes are handled by Apply above)
      if let [child] = children.as_slice() {
        if let NodeKind::LitStr { content: s, .. } = &child.kind {
          if open.src == "\":" {
            let content = s.trim_end_matches('\n');
            out.push_str("\":");
            for line in content.split('\n') {
              out.push('\n');
              ind(out, depth + 1);
              out.push_str(line);
            }
          } else {
            out.push('\'');
            out.push_str(s);
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
    NodeKind::Spread { inner, .. } => {
      out.push_str("..");
      if let Some(n) = inner {
        fmt_node(n, out, depth);
      }
    }
    NodeKind::Bind { lhs, rhs, .. } => {
      fmt_node(lhs, out, depth);
      out.push_str(" = ");
      fmt_node(rhs, out, depth);
    }
    NodeKind::Apply { func, args } => fmt_apply(func, &args.items, out, depth),
    NodeKind::Fn { params, body, .. } => fmt_fn(params, &body.items, out, depth),
    NodeKind::Patterns(exprs) => {
      for (i, child) in exprs.items.iter().enumerate() {
        if i > 0 { out.push_str(", "); }
        fmt_node(child, out, depth);
      }
    }
    NodeKind::UnaryOp { op, operand } => {
      out.push_str(op.src);
      if !op.src.starts_with('-') { out.push(' '); }
      fmt_node(operand, out, depth);
    }
    NodeKind::InfixOp { op, lhs, rhs } => {
      fmt_node(lhs, out, depth);
      out.push(' ');
      out.push_str(op.src);
      out.push(' ');
      fmt_node(rhs, out, depth);
    }
    NodeKind::ChainedCmp(parts) => {
      for (i, part) in parts.iter().enumerate() {
        match part {
          CmpPart::Operand(n) => fmt_node(n, out, depth),
          CmpPart::Op(tok) => {
            out.push(' ');
            out.push_str(tok.src);
            out.push(' ');
          }
        }
      }
    }
    NodeKind::Member { lhs, rhs, .. } => {
      fmt_node(lhs, out, depth);
      out.push('.');
      fmt_node(rhs, out, depth);
    }
    NodeKind::Group { inner, .. } => {
      out.push('(');
      fmt_node(inner, out, depth);
      out.push(')');
    }
    NodeKind::Partial => out.push('?'),
    NodeKind::Wildcard => out.push('_'),
    NodeKind::BindRight { lhs, rhs, .. } => {
      fmt_node(lhs, out, depth);
      out.push_str(" |= ");
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
    NodeKind::Match { subjects, arms, .. } => {
      out.push_str("match ");
      fmt_node(subjects, out, depth);
      out.push(':');
      for arm in &arms.items {
        out.push('\n');
        ind(out, depth + 1);
        fmt_node(arm, out, depth + 1);
      }
    }
    NodeKind::Arm { lhs, body, .. } => {
      for (i, pat) in lhs.items.iter().enumerate() {
        if i > 0 { out.push_str(", "); }
        fmt_node(pat, out, depth);
      }
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
    NodeKind::Block { name, params, body, .. } => {
      fmt_node(name, out, depth);
      out.push(' ');
      fmt_node(params, out, depth);
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
    if let NodeKind::Fn { params, body, .. } = &trailing[0].kind {
      if plain.is_empty() { out.push(' '); } else { out.push_str(", "); }
      fmt_fn_with_inline(params, &body.items, out, depth, false);
      return;
    }
  }

  // Multiple trailing fns/complex args → each on its own indented line
  if !plain.is_empty() { out.push(','); }
  for arg in trailing {
    out.push('\n');
    ind(out, depth + 1);
    if let NodeKind::Fn { params, body, .. } = &arg.kind {
      fmt_fn_with_inline(params, &body.items, out, depth + 1, true);
    } else {
      fmt_node(arg, out, depth + 1);
    }
  }
}

fn fmt_fn(params: &Node, body: &[Node], out: &mut MappedWriter, depth: usize) {
  fmt_fn_with_inline(params, body, out, depth, true);
}

fn fmt_fn_with_inline(params: &Node, body: &[Node], out: &mut MappedWriter, depth: usize, allow_apply_inline: bool) {
  let inline = body.len() == 1 && if allow_apply_inline {
    is_inline_expr(&body[0])
  } else {
    is_inline_single_trailing(&body[0])
  };
  if inline {
    fmt_fn_inline(params, &body[0], out, depth);
  } else {
    fmt_fn_params(params, out);
    out.push(':');
    fmt_body(body, out, depth, allow_apply_inline);
  }
}

/// Inline after `fn params: ` in general (standalone fn, stacked fn args)
fn is_inline_expr(node: &Node) -> bool {
  match &node.kind {
    // apply with no trailing fn args → inline
    NodeKind::Apply { args, .. } => !args.items.iter().any(is_fn),
    _ => is_atom(node),
  }
}

/// Inline after `fn params: ` when it's the single trailing fn in an apply call
fn is_inline_single_trailing(node: &Node) -> bool {
  is_atom(node)
}

fn fmt_fn_inline(params: &Node, expr: &Node, out: &mut MappedWriter, depth: usize) {
  fmt_fn_params(params, out);
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
}
