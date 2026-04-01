// Partial application pass — desugars `?` (Partial nodes) into `Fn` nodes.
//
// Scoping rules:
//   - `?` bubbles up to the nearest enclosing scope boundary
//   - Scope boundaries: Group (...), each segment of a Pipe, top of statement
//   - Everything else is transparent: Apply, InfixOp, UnaryOp, Member, Range,
//     Spread, LitSeq, LitRec, StrTempl, Bind (RHS only), BindRight (LHS only)
//   - All `?` in the same scope become the same single param `$`
//   - `?` in pattern position (Arm lhs, Bind lhs) is a compile error

use crate::ast::{AstId, CmpPart, Exprs, Node, NodeKind};
use crate::lexer::{Loc, Token};
use crate::ast::transform::{Transform, TransformError, TransformResult};


// --- public entry point ---

/// Apply partial desugaring. Returns the transformed node and the updated node count.
pub fn apply(node: Node<'_>, node_count: u32) -> Result<(Node<'_>, u32), TransformError> {
  let mut pass = PartialPass { next_id: node_count, synth_counter: 0 };
  let result = pass.transform_stmt(node)?;
  //TODO should return a Parseresult with node and updated ast index
  Ok((result, pass.next_id))
}

/// Allocate a fresh AstId.
fn fresh_id(next_id: &mut u32) -> AstId {
  let id = AstId(*next_id);
  *next_id += 1;
  id
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
    | NodeKind::LitStr { .. }
    | NodeKind::Ident(_)
    | NodeKind::SynthIdent(_)
    | NodeKind::Wildcard
    | NodeKind::Token(_) => false,

    // Group is a boundary — don't look inside
    NodeKind::Group { .. } => false,

    NodeKind::Module(items)
    | NodeKind::LitSeq { items, .. } | NodeKind::LitRec { items, .. } => {
      items.items.iter().any(has_partial)
    }
    NodeKind::StrTempl { children, .. } | NodeKind::StrRawTempl { children, .. } => {
      children.iter().any(has_partial)
    }
    NodeKind::UnaryOp { operand, .. } => has_partial(operand),
    NodeKind::InfixOp { lhs, rhs, .. } => has_partial(lhs) || has_partial(rhs),
    NodeKind::ChainedCmp(parts) => parts.iter().any(|p| match p {
      CmpPart::Operand(n) => has_partial(n),
      CmpPart::Op(_) => false,
    }),
    NodeKind::Spread { inner, .. } => inner.as_ref().is_some_and(|n| has_partial(n)),
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
      subjects.items.iter().any(has_partial) || arms.items.iter().any(has_partial)
    }
    NodeKind::Arm { lhs, body, .. } => {
      has_partial(lhs) || body.items.iter().any(has_partial)
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
fn replace_partial<'src>(node: Node<'src>, param_loc: Loc, synth_id: u32) -> Node<'src> {
  let id = node.id;
  let loc = node.loc;
  match node.kind {
    // Reuse the ?'s AstId and loc — maps back to the ? position in source.
    NodeKind::Partial => Node { id, kind: NodeKind::SynthIdent(synth_id), loc },

    // Leaf — return as-is
    NodeKind::LitBool(_)
    | NodeKind::LitInt(_)
    | NodeKind::LitFloat(_)
    | NodeKind::LitDecimal(_)
    | NodeKind::LitStr { .. }
    | NodeKind::Ident(_)
    | NodeKind::Wildcard => node,

    // Group is a boundary — don't replace inside, leave for transform_group
    NodeKind::Group { .. } => node,

    NodeKind::LitSeq { open, close, items } => {
      Node { id, kind: NodeKind::LitSeq { open, close, items: replace_exprs(items, param_loc, synth_id) }, loc }
    }
    NodeKind::LitRec { open, close, items } => {
      Node { id, kind: NodeKind::LitRec { open, close, items: replace_exprs(items, param_loc, synth_id) }, loc }
    }
    NodeKind::StrTempl { open, close, children } => {
      Node { id, kind: NodeKind::StrTempl { open, close, children: replace_vec(children, param_loc, synth_id) }, loc }
    }
    NodeKind::StrRawTempl { open, close, children } => {
      Node { id, kind: NodeKind::StrRawTempl { open, close, children: replace_vec(children, param_loc, synth_id) }, loc }
    }
    NodeKind::UnaryOp { op, operand } => {
      let operand = replace_partial(*operand, param_loc, synth_id);
      Node { id, kind: NodeKind::UnaryOp { op, operand: Box::new(operand) }, loc }
    }
    NodeKind::InfixOp { op, lhs, rhs } => {
      let lhs = replace_partial(*lhs, param_loc, synth_id);
      let rhs = replace_partial(*rhs, param_loc, synth_id);
      Node { id, kind: NodeKind::InfixOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc }
    }
    NodeKind::ChainedCmp(parts) => {
      let parts = parts.into_iter().map(|p| match p {
        CmpPart::Operand(n) => CmpPart::Operand(replace_partial(n, param_loc, synth_id)),
        CmpPart::Op(op) => CmpPart::Op(op),
      }).collect();
      Node { id, kind: NodeKind::ChainedCmp(parts), loc }
    }
    NodeKind::Spread { op, inner } => {
      let inner = inner.map(|n| Box::new(replace_partial(*n, param_loc, synth_id)));
      Node { id, kind: NodeKind::Spread { op, inner }, loc }
    }
    NodeKind::Member { op, lhs, rhs } => {
      let lhs = replace_partial(*lhs, param_loc, synth_id);
      let rhs_id = rhs.id;
      let rhs = match rhs.kind {
        NodeKind::Group { open, close, inner } => {
          let rhs_loc = rhs.loc;
          let inner = replace_partial(*inner, param_loc, synth_id);
          Node { id: rhs_id, kind: NodeKind::Group { open, close, inner: Box::new(inner) }, loc: rhs_loc }
        }
        _ => replace_partial(*rhs, param_loc, synth_id),
      };
      Node { id, kind: NodeKind::Member { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc }
    }
    NodeKind::Apply { func, args } => {
      let func = replace_partial(*func, param_loc, synth_id);
      let args = replace_exprs(args, param_loc, synth_id);
      Node { id, kind: NodeKind::Apply { func: Box::new(func), args }, loc }
    }
    NodeKind::Bind { op, lhs, rhs } => {
      let rhs = replace_partial(*rhs, param_loc, synth_id);
      Node { id, kind: NodeKind::Bind { op, lhs, rhs: Box::new(rhs) }, loc }
    }
    NodeKind::BindRight { op, lhs, rhs } => {
      let lhs = replace_partial(*lhs, param_loc, synth_id);
      Node { id, kind: NodeKind::BindRight { op, lhs: Box::new(lhs), rhs }, loc }
    }
    NodeKind::Arm { lhs, sep, body } => {
      let body = replace_exprs(body, param_loc, synth_id);
      Node { id, kind: NodeKind::Arm { lhs, sep, body }, loc }
    }

    other => Node { id, kind: other, loc },
  }
}

fn replace_vec<'src>(nodes: Vec<Node<'src>>, param_loc: Loc, synth_id: u32) -> Vec<Node<'src>> {
  nodes.into_iter().map(|n| replace_partial(n, param_loc, synth_id)).collect()
}

fn replace_exprs<'src>(exprs: Exprs<'src>, param_loc: Loc, synth_id: u32) -> Exprs<'src> {
  Exprs { items: replace_vec(exprs.items, param_loc, synth_id), seps: exprs.seps }
}

/// Find the loc of the first Partial node in the tree.
fn partial_loc(node: &Node) -> Loc {
  if matches!(node.kind, NodeKind::Partial) { return node.loc; }
  let mut loc = node.loc;
  crate::ast::walk(node, &mut |n| {
    if matches!(n.kind, NodeKind::Partial) { loc = n.loc; }
  });
  loc
}

/// Wrap an expression in `fn $: expr` if it contains Partial nodes.
fn wrap_if_partial<'src>(node: Node<'src>, next_id: &mut u32, synth_counter: &mut u32) -> Node<'src> {
  if !has_partial(&node) {
    return node;
  }
  let synth_id = *synth_counter;
  *synth_counter += 1;
  let param_loc = partial_loc(&node);
  let body = replace_partial(node, param_loc, synth_id);
  let body_loc = body.loc;
  let param = Node { id: fresh_id(next_id), kind: NodeKind::SynthIdent(synth_id), loc: param_loc };
  let patterns = Node { id: fresh_id(next_id), kind: NodeKind::Patterns(Exprs { items: vec![param], seps: vec![] }), loc: param_loc };
  let sep = Token { kind: crate::lexer::TokenKind::Colon, loc: param_loc, src: ":" };
  Node { id: fresh_id(next_id), kind: NodeKind::Fn { params: Box::new(patterns), sep, body: Exprs { items: vec![body], seps: vec![] } }, loc: body_loc }
}

// --- transformer ---

struct PartialPass {
  next_id: u32,
  synth_counter: u32,
}

impl<'src> PartialPass {
  /// Transform a statement — top-level scope boundary.
  /// Wraps in Fn if any Partial remains after processing inner scope boundaries.
  fn transform_stmt(&mut self, node: Node<'src>) -> TransformResult<'src> {
    let id = node.id;
    let loc = node.loc;
    match node.kind {
      // Bind: only wrap RHS, never the whole Bind
      NodeKind::Bind { op, lhs, rhs } => {
        let rhs = self.transform_stmt(*rhs)?;
        Ok(Node { id, kind: NodeKind::Bind { op, lhs, rhs: Box::new(rhs) }, loc })
      }

      // BindRight: only wrap LHS value, never the whole BindRight
      NodeKind::BindRight { op, lhs, rhs } => {
        let lhs = self.transform_stmt(*lhs)?;
        Ok(Node { id, kind: NodeKind::BindRight { op, lhs: Box::new(lhs), rhs }, loc })
      }

      // Arm: body stmts are independent scopes, lhs is pattern (skip)
      NodeKind::Arm { lhs, sep, body } => {
        let body = self.transform_body(body)?;
        Ok(Node { id, kind: NodeKind::Arm { lhs, sep, body }, loc })
      }

      // Group: explicit scope boundary — process inner as independent stmt.
      // When ? is present, the inner gets wrapped in a fn (replacing the scope),
      // so strip the Group. Otherwise preserve it for downstream passes (CPS scoping).
      NodeKind::Group { open, close, inner } => {
        if has_partial(&inner) {
          self.transform_stmt(*inner)
        } else {
          let inner = self.transform_stmt(*inner)?;
          Ok(Node { id, kind: NodeKind::Group { open, close, inner: Box::new(inner) }, loc })
        }
      }

      // Pipe: each segment is an independent scope
      NodeKind::Pipe(exprs) => {
        let mut new_items = Vec::with_capacity(exprs.items.len());
        for child in exprs.items {
          new_items.push(self.transform_stmt(child)?);
        }
        Ok(Node { id, kind: NodeKind::Pipe(Exprs { items: new_items, seps: exprs.seps }), loc })
      }

      // Fn: body stmts are independent scopes (like Module/Arm)
      NodeKind::Fn { sep, params, body } => {
        let body = self.transform_body(body)?;
        Ok(Node { id, kind: NodeKind::Fn { sep, params, body }, loc })
      }

      // Module: recurse into each expression as independent scope
      NodeKind::Module(exprs) => {
        let body = self.transform_body(exprs)?;
        Ok(Node { id, kind: NodeKind::Module(body), loc })
      }

      // Everything else: recurse into children (processing inner Group/Pipe boundaries),
      // then wrap in Fn if any Partial remains.
      // Note: self.transform() may reconstruct with AstId(0) via the default trait,
      // but wrap_if_partial → replace_partial will reconstruct with the correct ids
      // from the original tree.
      other => {
        if !has_partial(&Node { id, kind: other.clone(), loc }) {
          // No partials — return unchanged.
          return Ok(Node { id, kind: other, loc });
        }
        let node = self.transform(Node { id, kind: other, loc })?;
        Ok(wrap_if_partial(node, &mut self.next_id, &mut self.synth_counter))
      }
    }
  }

  fn transform_body(&mut self, body: Exprs<'src>) -> Result<Exprs<'src>, TransformError> {
    let items = body.items.into_iter().map(|n| self.transform_stmt(n)).collect::<Result<_, _>>()?;
    Ok(Exprs { items, seps: body.seps })
  }
}

impl<'src> Transform<'src> for PartialPass {
  // Scope boundaries (Group, Pipe, Bind, BindRight, Arm) are handled in transform_stmt.
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
    let rhs_id = rhs.id;
    let rhs = match rhs.kind {
      NodeKind::Group { open, close, inner } => {
        let rhs_loc = rhs.loc;
        let inner = self.transform(*inner)?;
        Node { id: rhs_id, kind: NodeKind::Group { open, close, inner: Box::new(inner) }, loc: rhs_loc }
      }
      _ => self.transform(rhs)?,
    };
    // id for the parent Member node is lost by the trait — use AstId(0).
    // This only runs for expressions that contain partials (which get wrapped in fn anyway).
    Ok(Node::new(NodeKind::Member { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc))
  }
}

// --- test runner ---

#[cfg(test)]
mod tests {
  fn partial(src: &str) -> String {
    use crate::ast::NodeKind;
    match crate::parser::parse(src) {
      Err(e) => format!("PARSE ERROR: {}", e.message),
      Ok(result) => {
        let before = result.root.print();
        match super::apply(result.root, result.node_count) {
          Ok((node, _new_count)) => {
            let after = node.print();
            if before == after {
              return "No Change".to_string();
            }
            if let NodeKind::Module(exprs) = &node.kind {
              if exprs.items.len() == 1 {
                return exprs.items[0].print();
              }
            }
            node.print()
          }
          Err(e) => format!("ERROR: {}", e.message),
        }
      }
    }
  }

  test_macros::include_fink_tests!("src/passes/partial/test_partial.fnk");
}
