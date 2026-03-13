// AST → Fink source pretty-printer

use crate::ast::{Node, NodeKind};

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

fn is_atom(node: &Node) -> bool {
  matches!(
    node.kind,
    NodeKind::LitBool(_)
      | NodeKind::LitInt(_)
      | NodeKind::LitFloat(_)
      | NodeKind::LitDecimal(_)
      | NodeKind::LitStr(_)
      | NodeKind::Ident(_)
  )
}

fn fmt_node(node: &Node, out: &mut String, depth: usize) {
  match &node.kind {
    NodeKind::LitBool(v) => out.push_str(if *v { "true" } else { "false" }),
    NodeKind::LitInt(s) => out.push_str(s),
    NodeKind::LitFloat(s) => out.push_str(s),
    NodeKind::LitDecimal(s) => out.push_str(s),
    NodeKind::LitStr(s) => {
      out.push('\'');
      out.push_str(s);
      out.push('\'');
    }
    NodeKind::LitSeq(children) if children.is_empty() => out.push_str("[]"),
    NodeKind::LitSeq(children) => {
      out.push('[');
      for (i, child) in children.iter().enumerate() {
        if i > 0 { out.push_str(", "); }
        fmt_node(child, out, depth);
      }
      out.push(']');
    }
    NodeKind::LitRec(children) if children.is_empty() => out.push_str("{}"),
    NodeKind::LitRec(children) => {
      out.push('{');
      for (i, child) in children.iter().enumerate() {
        if i > 0 { out.push_str(", "); }
        fmt_node(child, out, depth);
      }
      out.push('}');
    }
    NodeKind::StrRawTempl(children) => {
      // single LitStr child → raw string content (no quotes around the template itself;
      // the tag + quotes are handled by Apply above)
      if let [child] = children.as_slice() {
        if let NodeKind::LitStr(s) = &child.kind {
          out.push('\'');
          out.push_str(s);
          out.push('\'');
          return;
        }
      }
      // fallback: print children joined
      for child in children {
        fmt_node(child, out, depth);
      }
    }
    NodeKind::Ident(s) => out.push_str(s),
    NodeKind::Spread(inner) => {
      out.push_str("..");
      if let Some(n) = inner {
        fmt_node(n, out, depth);
      }
    }
    NodeKind::Bind { lhs, rhs } => {
      fmt_node(lhs, out, depth);
      out.push_str(" = ");
      fmt_node(rhs, out, depth);
    }
    NodeKind::Apply { func, args } => fmt_apply(func, args, out, depth),
    NodeKind::Fn { params, body } => fmt_fn(params, body, out, depth),
    NodeKind::Patterns(children) => {
      for (i, child) in children.iter().enumerate() {
        if i > 0 { out.push_str(", "); }
        fmt_node(child, out, depth);
      }
    }
    NodeKind::Wildcard => out.push('_'),
    _ => out.push_str("?"),
  }
}

fn is_complex_arg(node: &Node) -> bool {
  // An arg that has fn args inside it — should go on its own indented line
  match &node.kind {
    NodeKind::Apply { args, .. } => args.iter().any(is_fn),
    _ => false,
  }
}

fn fmt_apply(func: &Node, args: &[Node], out: &mut String, depth: usize) {
  // Tagged string literal: `id'foo'`, `op'+'` — func ident + single string arg, no separator
  if let [arg] = args {
    if matches!(arg.kind, NodeKind::StrRawTempl(_) | NodeKind::LitStr(_)) {
      if matches!(func.kind, NodeKind::Ident(_)) {
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
    if let NodeKind::Fn { params, body } = &trailing[0].kind {
      if plain.is_empty() { out.push(' '); } else { out.push_str(", "); }
      fmt_fn_with_inline(params, body, out, depth, false);
      return;
    }
  }

  // Multiple trailing fns/complex args → each on its own indented line
  if !plain.is_empty() { out.push(','); }
  for arg in trailing {
    out.push('\n');
    ind(out, depth + 1);
    if let NodeKind::Fn { params, body } = &arg.kind {
      fmt_fn_with_inline(params, body, out, depth + 1, true);
    } else {
      fmt_node(arg, out, depth + 1);
    }
  }
}

fn fmt_fn(params: &Node, body: &[Node], out: &mut String, depth: usize) {
  fmt_fn_with_inline(params, body, out, depth, true);
}

fn fmt_fn_with_inline(params: &Node, body: &[Node], out: &mut String, depth: usize, allow_apply_inline: bool) {
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
    NodeKind::Apply { args, .. } => !args.iter().any(is_fn),
    _ => is_atom(node),
  }
}

/// Inline after `fn params: ` when it's the single trailing fn in an apply call
fn is_inline_single_trailing(node: &Node) -> bool {
  is_atom(node)
}

fn fmt_fn_inline(params: &Node, expr: &Node, out: &mut String, depth: usize) {
  fmt_fn_params(params, out);
  out.push_str(": ");
  fmt_node(expr, out, depth);
}

fn fmt_fn_params(params: &Node, out: &mut String) {
  out.push_str("fn");
  if let NodeKind::Patterns(children) = &params.kind {
    for (i, child) in children.iter().enumerate() {
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
