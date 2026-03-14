// Partial application pass — desugars `?` (Partial nodes) into `Fn` nodes.
//
// Scoping rules:
//   - `?` bubbles up to the nearest enclosing scope boundary
//   - Scope boundaries: Group (...), each segment of a Pipe, top of statement
//   - Everything else is transparent: Apply, InfixOp, UnaryOp, Member, Range,
//     Spread, LitSeq, LitRec, StrTempl, Bind (RHS only), BindRight (LHS only)
//   - All `?` in the same scope become the same single param `$`
//   - `?` in pattern position (Arm lhs, Bind lhs) is a compile error

use crate::ast::{CmpPart, Exprs, Node, NodeKind};
use crate::lexer::{Loc, Pos, Token, TokenKind};
use crate::ast::transform::{Transform, TransformError, TransformResult};

const PARAM: &str = "$";

// --- public entry point ---

pub fn apply(node: Node<'_>) -> Result<Node<'_>, TransformError> {
  let mut pass = PartialPass;
  pass.transform_stmt(node)
}

// --- helpers ---

/// Returns true if the node tree contains any Partial node.
fn has_partial(node: &Node) -> bool {
  match &node.kind {
    NodeKind::Partial => true,
    NodeKind::LitBool(_)
    | NodeKind::LitInt(_)
    | NodeKind::LitFloat(_)
    | NodeKind::LitDecimal(_)
    | NodeKind::LitStr(_)
    | NodeKind::Ident(_)
    | NodeKind::Wildcard => false,

    // Group is a boundary — don't look inside
    NodeKind::Group { .. } => false,

    NodeKind::LitSeq { items, .. } | NodeKind::LitRec { items, .. } => {
      items.items.iter().any(has_partial)
    }
    NodeKind::StrTempl(children) | NodeKind::StrRawTempl(children) => {
      children.iter().any(has_partial)
    }
    NodeKind::UnaryOp { operand, .. } => has_partial(operand),
    NodeKind::InfixOp { lhs, rhs, .. } => has_partial(lhs) || has_partial(rhs),
    NodeKind::ChainedCmp(parts) => parts.iter().any(|p| match p {
      CmpPart::Operand(n) => has_partial(n),
      CmpPart::Op(_) => false,
    }),
    NodeKind::Spread { inner, .. } => inner.as_ref().map_or(false, |n| has_partial(n)),
    NodeKind::Member { lhs, rhs, .. } => {
      // Member rhs may be Group (computed key) — look through it; it's not a scope boundary here
      let rhs_inner = match &rhs.kind {
        NodeKind::Group { inner, .. } => inner.as_ref(),
        _ => rhs.as_ref(),
      };
      has_partial(lhs) || has_partial(rhs_inner)
    }
    NodeKind::Bind { rhs, .. } => has_partial(rhs),      // lhs is pattern — skip
    NodeKind::BindRight { lhs, .. } => has_partial(lhs), // rhs is pattern — skip
    NodeKind::Apply { func, args } => {
      has_partial(func) || args.items.iter().any(has_partial)
    }
    NodeKind::Pipe(_) => false, // Pipe children are independent segments
    NodeKind::Fn { params, body, .. } => {
      has_partial(params) || body.items.iter().any(has_partial)
    }
    NodeKind::Patterns(children) => children.items.iter().any(has_partial),
    NodeKind::Match { subjects, arms, .. } => {
      has_partial(subjects) || arms.items.iter().any(has_partial)
    }
    NodeKind::Arm { lhs, body, .. } => {
      lhs.items.iter().any(has_partial) || body.items.iter().any(has_partial)
    }
    NodeKind::Block { name, params, body, .. } => {
      has_partial(name) || has_partial(params) || body.items.iter().any(has_partial)
    }
    NodeKind::Try(inner) => has_partial(inner),
    NodeKind::Yield(inner) => has_partial(inner),
  }
}

/// Replace all Partial nodes in the tree with Ident("$").
/// Does NOT descend into Group boundaries (those are handled by transform_group).
fn replace_partial<'src>(node: Node<'src>, param_loc: Loc) -> Node<'src> {
  let loc = node.loc;
  match node.kind {
    NodeKind::Partial => Node::new(NodeKind::Ident(PARAM), param_loc),

    // Leaf — return as-is
    NodeKind::LitBool(_)
    | NodeKind::LitInt(_)
    | NodeKind::LitFloat(_)
    | NodeKind::LitDecimal(_)
    | NodeKind::LitStr(_)
    | NodeKind::Ident(_)
    | NodeKind::Wildcard => node,

    // Group is a boundary — don't replace inside, leave for transform_group
    NodeKind::Group { .. } => node,

    NodeKind::LitSeq { open, close, items } => {
      Node::new(NodeKind::LitSeq { open, close, items: replace_exprs(items, param_loc) }, loc)
    }
    NodeKind::LitRec { open, close, items } => {
      Node::new(NodeKind::LitRec { open, close, items: replace_exprs(items, param_loc) }, loc)
    }
    NodeKind::StrTempl(children) => {
      Node::new(NodeKind::StrTempl(replace_vec(children, param_loc)), loc)
    }
    NodeKind::StrRawTempl(children) => {
      Node::new(NodeKind::StrRawTempl(replace_vec(children, param_loc)), loc)
    }
    NodeKind::UnaryOp { op, operand } => {
      let operand = replace_partial(*operand, param_loc);
      Node::new(NodeKind::UnaryOp { op, operand: Box::new(operand) }, loc)
    }
    NodeKind::InfixOp { op, lhs, rhs } => {
      let lhs = replace_partial(*lhs, param_loc);
      let rhs = replace_partial(*rhs, param_loc);
      Node::new(NodeKind::InfixOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc)
    }
    NodeKind::ChainedCmp(parts) => {
      let parts = parts.into_iter().map(|p| match p {
        CmpPart::Operand(n) => CmpPart::Operand(replace_partial(n, param_loc)),
        CmpPart::Op(op) => CmpPart::Op(op),
      }).collect();
      Node::new(NodeKind::ChainedCmp(parts), loc)
    }
    NodeKind::Spread { op, inner } => {
      let inner = inner.map(|n| Box::new(replace_partial(*n, param_loc)));
      Node::new(NodeKind::Spread { op, inner }, loc)
    }
    NodeKind::Member { op, lhs, rhs } => {
      let lhs = replace_partial(*lhs, param_loc);
      // Member rhs Group (computed key) is transparent — replace inside, preserve Group wrapper
      let rhs = match rhs.kind {
        NodeKind::Group { open, close, inner } => {
          let rhs_loc = rhs.loc;
          let inner = replace_partial(*inner, param_loc);
          Node::new(NodeKind::Group { open, close, inner: Box::new(inner) }, rhs_loc)
        }
        _ => replace_partial(*rhs, param_loc),
      };
      Node::new(NodeKind::Member { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc)
    }
    NodeKind::Apply { func, args } => {
      let func = replace_partial(*func, param_loc);
      let args = replace_exprs(args, param_loc);
      Node::new(NodeKind::Apply { func: Box::new(func), args }, loc)
    }
    NodeKind::Bind { op, lhs, rhs } => {
      // lhs is pattern — don't replace; rhs is value
      let rhs = replace_partial(*rhs, param_loc);
      Node::new(NodeKind::Bind { op, lhs, rhs: Box::new(rhs) }, loc)
    }
    NodeKind::BindRight { op, lhs, rhs } => {
      // rhs is pattern — don't replace; lhs is value
      let lhs = replace_partial(*lhs, param_loc);
      Node::new(NodeKind::BindRight { op, lhs: Box::new(lhs), rhs }, loc)
    }
    NodeKind::Arm { lhs, sep, body } => {
      // In LitRec context: lhs is the key (Ident, not replaced), body has the value
      let body = replace_exprs(body, param_loc);
      Node::new(NodeKind::Arm { lhs, sep, body }, loc)
    }

    // For anything else, return as-is (Pipe, Fn, Match, Block, Try — complex)
    other => Node::new(other, loc),
  }
}

fn replace_vec<'src>(nodes: Vec<Node<'src>>, param_loc: Loc) -> Vec<Node<'src>> {
  nodes.into_iter().map(|n| replace_partial(n, param_loc)).collect()
}

fn replace_exprs<'src>(exprs: Exprs<'src>, param_loc: Loc) -> Exprs<'src> {
  Exprs { items: replace_vec(exprs.items, param_loc), seps: exprs.seps }
}

/// Wrap an expression in `fn $: expr` if it contains Partial nodes.
fn wrap_if_partial<'src>(node: Node<'src>) -> Node<'src> {
  if !has_partial(&node) {
    return node;
  }
  let param_loc = node.loc;
  let body = replace_partial(node, param_loc);
  let body_loc = body.loc;
  let param = Node::new(NodeKind::Ident(PARAM), param_loc);
  let patterns = Node::new(NodeKind::Patterns(Exprs { items: vec![param], seps: vec![] }), param_loc);
  let sep = Token { kind: crate::lexer::TokenKind::Colon, loc: param_loc, src: ":" };
  Node::new(NodeKind::Fn { params: Box::new(patterns), sep, body: Exprs { items: vec![body], seps: vec![] } }, body_loc)
}

// --- transformer ---

struct PartialPass;

impl<'src> PartialPass {
  /// Transform a statement — top-level scope boundary.
  /// Wraps in Fn if any Partial remains after processing inner scope boundaries.
  fn transform_stmt(&mut self, node: Node<'src>) -> TransformResult<'src> {
    let loc = node.loc;
    match node.kind {
      // Bind: only wrap RHS, never the whole Bind
      NodeKind::Bind { op, lhs, rhs } => {
        let rhs = self.transform_stmt(*rhs)?;
        Ok(Node::new(NodeKind::Bind { op, lhs, rhs: Box::new(rhs) }, loc))
      }

      // BindRight: only wrap LHS value, never the whole BindRight
      NodeKind::BindRight { op, lhs, rhs } => {
        let lhs = self.transform_stmt(*lhs)?;
        Ok(Node::new(NodeKind::BindRight { op, lhs: Box::new(lhs), rhs }, loc))
      }

      // Arm: body stmts are independent scopes, lhs is pattern (skip)
      NodeKind::Arm { lhs, sep, body } => {
        let body = self.transform_body(body)?;
        Ok(Node::new(NodeKind::Arm { lhs, sep, body }, loc))
      }


      // Group: explicit scope boundary — process inner as independent stmt
      NodeKind::Group { inner, .. } => {
        self.transform_stmt(*inner)
      }

      // Pipe: each segment is an independent scope
      NodeKind::Pipe(exprs) => {
        let mut new_items = Vec::with_capacity(exprs.items.len());
        for child in exprs.items {
          new_items.push(self.transform_stmt(child)?);
        }
        Ok(Node::new(NodeKind::Pipe(Exprs { items: new_items, seps: exprs.seps }), loc))
      }

      // Everything else: recurse into children (processing inner Group/Pipe boundaries),
      // then wrap in Fn if any Partial remains
      other => {
        let node = self.transform(Node::new(other, loc))?;
        Ok(wrap_if_partial(node))
      }
    }
  }

  fn transform_body(&mut self, body: Exprs<'src>) -> Result<Exprs<'src>, TransformError> {
    let items = body.items.into_iter().map(|n| self.transform_stmt(n)).collect::<Result<_, _>>()?;
    Ok(Exprs { items, seps: body.seps })
  }
}

impl<'src> Transform<'src> for PartialPass {
  // The default walker recurses into all children.
  // Scope boundaries (Group, Pipe, Bind, BindRight, Arm) are handled in transform_stmt.
  // When the default walker hits a Group, it calls transform_group below.
  fn transform_group(&mut self, _open: Token<'src>, _close: Token<'src>, inner: Node<'src>, _loc: Loc) -> TransformResult<'src> {
    self.transform_stmt(inner)
  }

  // Member rhs Group (computed key) is transparent — don't create a scope boundary for it.
  fn transform_member(
    &mut self,
    op: Token<'src>,
    lhs: Node<'src>,
    rhs: Node<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let lhs = self.transform(lhs)?;
    // If rhs is a Group (computed key), transform its inner directly — not as a scope boundary
    let rhs = match rhs.kind {
      NodeKind::Group { open, close, inner } => {
        let rhs_loc = rhs.loc;
        let inner = self.transform(*inner)?;
        Node::new(NodeKind::Group { open, close, inner: Box::new(inner) }, rhs_loc)
      }
      _ => self.transform(rhs)?,
    };
    Ok(Node::new(NodeKind::Member { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc))
  }
}

// --- test runner ---

#[cfg(test)]
mod tests {
  fn partial(src: &str) -> String {
    match crate::parser::parse(src) {
      Err(e) => format!("PARSE ERROR: {}", e.message),
      Ok(result) => match super::apply(result.root) {
        Ok(node) => node.print(),
        Err(e) => format!("ERROR: {}", e.message),
      },
    }
  }

  test_macros::include_fink_tests!("src/passes/partial/test_partial.fnk");
}
