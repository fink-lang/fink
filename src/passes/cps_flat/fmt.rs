// cps_flat formatter — copy of ast::fmt with adjustments for flat CPS output:
//
//   - Module items separated by blank lines (makes top-level bindings readable)
//   - Source map tracking removed (not needed for debug output)

use crate::ast::{CmpPart, Node, NodeKind};
use crate::lexer::Token;

pub fn fmt(node: &Node) -> String {
  let mut out = String::new();
  fmt_node(node, &mut out, 0);
  out
}

fn ind(out: &mut String, depth: usize) {
  for _ in 0..depth {
    out.push_str("  ");
  }
}

fn is_fn(node: &Node) -> bool {
  matches!(node.kind, NodeKind::Fn { .. })
}

fn is_multiline(node: &Node) -> bool {
  match &node.kind {
    NodeKind::LitStr { open, content, .. } => open.src == "\":" || content.contains('\n'),
    NodeKind::StrRawTempl { open, .. } => open.src == "\":",
    NodeKind::Fn { body, .. } => body.items.len() > 1 || body.items.first().is_some_and(|b| !is_inline_expr(b)),
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

fn fmt_node(node: &Node, out: &mut String, depth: usize) {
  match &node.kind {
    NodeKind::LitBool(v) => out.push_str(if *v { "true" } else { "false" }),
    NodeKind::LitInt(s) => out.push_str(s),
    NodeKind::LitFloat(s) => out.push_str(s),
    NodeKind::LitDecimal(s) => out.push_str(s),
    NodeKind::LitStr { open, close: _, content: s, .. } => {
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
        if s.contains('\n') {
          for (i, line) in s.split('\n').enumerate() {
            if i > 0 { out.push('\n'); ind(out, depth + 1); }
            out.push_str(line);
          }
        } else {
          out.push_str(s);
        }
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
    NodeKind::StrRawTempl { .. } | NodeKind::StrTempl { .. } => {
      // Not produced by cps_flat — emit placeholder
      out.push_str("<templ>");
    }
    NodeKind::Ident(s) => out.push_str(s),
    NodeKind::Spread { inner, .. } => {
      out.push_str("..");
      if let Some(n) = inner { fmt_node(n, out, depth); }
    }
    NodeKind::Bind { lhs, rhs, .. } => {
      fmt_node(lhs, out, depth);
      out.push_str(" = ");
      fmt_node(rhs, out, depth);
    }
    NodeKind::Apply { func, args } => fmt_apply(func, &args.items, out, depth),
    NodeKind::Module(exprs) => {
      for (i, child) in exprs.items.iter().enumerate() {
        if i > 0 { out.push_str("\n\n"); ind(out, depth); }
        fmt_node(child, out, depth);
      }
    }
    NodeKind::Fn { params, sep, body } => fmt_fn(params, sep, &body.items, out, depth),
    NodeKind::Patterns(exprs) => {
      for (i, child) in exprs.items.iter().enumerate() {
        if i == 0 { out.push(' '); } else { out.push_str(", "); }
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
      for part in parts.iter() {
        match part {
          CmpPart::Operand(n) => fmt_node(n, out, depth),
          CmpPart::Op(tok) => { out.push(' '); out.push_str(tok.src); out.push(' '); }
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
          if multiline { out.push('\n'); ind(out, depth); out.push_str("| "); }
          else { out.push_str(" | "); }
        }
        fmt_node(child, out, depth);
      }
    }
    NodeKind::Match { subjects, arms, .. } => {
      out.push_str("match ");
      for (i, subj) in subjects.items.iter().enumerate() {
        if i > 0 { out.push_str(", "); }
        fmt_node(subj, out, depth);
      }
      out.push(':');
      for arm in &arms.items {
        out.push('\n'); ind(out, depth + 1);
        fmt_node(arm, out, depth + 1);
      }
    }
    NodeKind::Arm { lhs, body, .. } => {
      fmt_node(lhs, out, depth);
      out.push(':');
      fmt_body(&body.items, out, depth, true);
    }
    NodeKind::Try(inner) => { out.push_str("try "); fmt_node(inner, out, depth); }
    NodeKind::Yield(inner) => { out.push_str("yield "); fmt_node(inner, out, depth); }
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
  match &node.kind {
    NodeKind::Apply { args, .. } => args.items.iter().any(is_fn),
    _ => false,
  }
}

fn fmt_apply(func: &Node, args: &[Node], out: &mut String, depth: usize) {
  fmt_node(func, out, depth);

  let trailing_start = args.iter().rposition(|a| !is_fn(a) && !is_complex_arg(a))
    .map(|i| i + 1).unwrap_or(0);
  let (plain, trailing) = args.split_at(trailing_start);

  for (i, arg) in plain.iter().enumerate() {
    if i == 0 { out.push(' '); } else { out.push_str(", "); }
    fmt_node(arg, out, depth);
  }

  if trailing.is_empty() { return; }

  if trailing.len() == 1 && is_fn(&trailing[0])
    && let NodeKind::Fn { params, sep, body } = &trailing[0].kind {
      if plain.is_empty() { out.push(' '); } else { out.push_str(", "); }
      fmt_fn_with_inline(params, sep, &body.items, out, depth, false);
      return;
  }

  if !plain.is_empty() { out.push(','); }
  for arg in trailing {
    out.push('\n'); ind(out, depth + 1);
    if let NodeKind::Fn { params, sep, body } = &arg.kind {
      fmt_fn_with_inline(params, sep, &body.items, out, depth + 1, true);
    } else {
      fmt_node(arg, out, depth + 1);
    }
  }
}

fn fmt_fn(params: &Node, sep: &Token, body: &[Node], out: &mut String, depth: usize) {
  fmt_fn_with_inline(params, sep, body, out, depth, true);
}

fn fmt_fn_with_inline(params: &Node, sep: &Token, body: &[Node], out: &mut String, depth: usize, allow_apply_inline: bool) {
  let inline = body.len() == 1 && if allow_apply_inline {
    is_inline_expr(&body[0])
  } else {
    is_inline_single_trailing(&body[0])
  };
  if inline {
    fmt_fn_params(params, out);
    out.push_str(": ");
    fmt_node(&body[0], out, depth);
  } else {
    fmt_fn_params(params, out);
    out.push(':');
    fmt_body(body, out, depth, allow_apply_inline);
  }
  let _ = sep; // sep token not needed — we always emit ":"
}

fn is_inline_expr(node: &Node) -> bool {
  if is_multiline(node) { return false; }
  match &node.kind {
    NodeKind::Apply { .. } => false,
    _ => is_atom(node),
  }
}

fn is_inline_single_trailing(node: &Node) -> bool {
  is_atom(node)
}

fn fmt_fn_params(params: &Node, out: &mut String) {
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

fn fmt_body(body: &[Node], out: &mut String, depth: usize, allow_apply_inline: bool) {
  if body.len() == 1 {
    let inline = if allow_apply_inline { is_inline_expr(&body[0]) } else { is_inline_single_trailing(&body[0]) };
    if inline {
      out.push(' ');
      fmt_node(&body[0], out, depth);
      return;
    }
  }
  for stmt in body {
    out.push('\n'); ind(out, depth + 1);
    fmt_node(stmt, out, depth + 1);
  }
}
