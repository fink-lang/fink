pub mod cps;
pub mod cps_fmt;
pub mod partial;

use crate::ast::{CmpPart, Node, NodeKind};
use crate::lexer::Loc;

// --- error ---

#[derive(Debug, Clone, PartialEq)]
pub struct TransformError {
  pub message: String,
  pub loc: Loc,
}

impl TransformError {
  pub fn new(message: impl Into<String>, loc: Loc) -> Self {
    Self { message: message.into(), loc }
  }
}

pub type TransformResult<'src> = Result<Node<'src>, TransformError>;

// --- transformer trait ---
//
// Default implementations recurse into children and rebuild the node.
// Override only the methods you need; everything else walks for free.
//
// Nodes are consumed and rebuilt (owned) since transforms produce new trees.
// The `loc` of a node is always preserved unless explicitly changed.

pub trait Transform<'src> {
  fn transform(&mut self, node: Node<'src>) -> TransformResult<'src> {
    let loc = node.loc;
    match node.kind {
      NodeKind::LitBool(_)
      | NodeKind::LitInt(_)
      | NodeKind::LitFloat(_)
      | NodeKind::LitDecimal(_)
      | NodeKind::LitStr(_)
      | NodeKind::Ident(_)
      | NodeKind::Partial
      | NodeKind::Wildcard => self.transform_leaf(node),

      NodeKind::LitSeq(children) => self.transform_lit_seq(children, loc),
      NodeKind::LitRec(children) => self.transform_lit_rec(children, loc),
      NodeKind::StrTempl(children) => self.transform_str_templ(children, loc),
      NodeKind::StrRawTempl(children) => self.transform_str_raw_templ(children, loc),
      NodeKind::UnaryOp { op, operand } => self.transform_unary_op(op, *operand, loc),
      NodeKind::InfixOp { op, lhs, rhs } => self.transform_infix_op(op, *lhs, *rhs, loc),
      NodeKind::ChainedCmp(parts) => self.transform_chained_cmp(parts, loc),
      NodeKind::Range { op, start, end } => self.transform_range(op, *start, *end, loc),
      NodeKind::Spread(inner) => self.transform_spread(inner.map(|n| *n), loc),
      NodeKind::Member { lhs, rhs } => self.transform_member(*lhs, *rhs, loc),
      NodeKind::Group(inner) => self.transform_group(*inner, loc),
      NodeKind::Try(inner) => self.transform_try(*inner, loc),
      NodeKind::Bind { lhs, rhs } => self.transform_bind(*lhs, *rhs, loc),
      NodeKind::BindRight { lhs, rhs } => self.transform_bind_right(*lhs, *rhs, loc),
      NodeKind::Apply { func, args } => self.transform_apply(*func, args, loc),
      NodeKind::Pipe(children) => self.transform_pipe(children, loc),
      NodeKind::Fn { params, body } => self.transform_fn(*params, body, loc),
      NodeKind::Patterns(children) => self.transform_patterns(children, loc),
      NodeKind::Match { subjects, arms } => self.transform_match(*subjects, arms, loc),
      NodeKind::Arm { lhs, body } => self.transform_arm(lhs, body, loc),
      NodeKind::Block { name, params, body } => self.transform_block(*name, *params, body, loc),
    }
  }

  // --- leaf nodes (no children) ---

  fn transform_leaf(&mut self, node: Node<'src>) -> TransformResult<'src> {
    Ok(node)
  }

  // --- composite nodes ---

  fn transform_lit_seq(
    &mut self,
    children: Vec<Node<'src>>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let children = self.transform_vec(children)?;
    Ok(Node::new(NodeKind::LitSeq(children), loc))
  }

  fn transform_lit_rec(
    &mut self,
    children: Vec<Node<'src>>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let children = self.transform_vec(children)?;
    Ok(Node::new(NodeKind::LitRec(children), loc))
  }

  fn transform_str_templ(
    &mut self,
    children: Vec<Node<'src>>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let children = self.transform_vec(children)?;
    Ok(Node::new(NodeKind::StrTempl(children), loc))
  }

  fn transform_str_raw_templ(
    &mut self,
    children: Vec<Node<'src>>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let children = self.transform_vec(children)?;
    Ok(Node::new(NodeKind::StrRawTempl(children), loc))
  }

  fn transform_unary_op(
    &mut self,
    op: &'src str,
    operand: Node<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let operand = self.transform(operand)?;
    Ok(Node::new(NodeKind::UnaryOp { op, operand: Box::new(operand) }, loc))
  }

  fn transform_infix_op(
    &mut self,
    op: &'src str,
    lhs: Node<'src>,
    rhs: Node<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let lhs = self.transform(lhs)?;
    let rhs = self.transform(rhs)?;
    Ok(Node::new(NodeKind::InfixOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc))
  }

  fn transform_chained_cmp(
    &mut self,
    parts: Vec<CmpPart<'src>>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let mut new_parts = Vec::with_capacity(parts.len());
    for part in parts {
      match part {
        CmpPart::Operand(n) => new_parts.push(CmpPart::Operand(self.transform(n)?)),
        CmpPart::Op(op) => new_parts.push(CmpPart::Op(op)),
      }
    }
    Ok(Node::new(NodeKind::ChainedCmp(new_parts), loc))
  }

  fn transform_range(
    &mut self,
    op: &'src str,
    start: Node<'src>,
    end: Node<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let start = self.transform(start)?;
    let end = self.transform(end)?;
    Ok(Node::new(NodeKind::Range { op, start: Box::new(start), end: Box::new(end) }, loc))
  }

  fn transform_spread(
    &mut self,
    inner: Option<Node<'src>>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let inner = inner.map(|n| self.transform(n)).transpose()?;
    Ok(Node::new(NodeKind::Spread(inner.map(Box::new)), loc))
  }

  fn transform_member(
    &mut self,
    lhs: Node<'src>,
    rhs: Node<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let lhs = self.transform(lhs)?;
    let rhs = self.transform(rhs)?;
    Ok(Node::new(NodeKind::Member { lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc))
  }

  fn transform_group(&mut self, inner: Node<'src>, loc: Loc) -> TransformResult<'src> {
    let inner = self.transform(inner)?;
    Ok(Node::new(NodeKind::Group(Box::new(inner)), loc))
  }

  fn transform_try(&mut self, inner: Node<'src>, loc: Loc) -> TransformResult<'src> {
    let inner = self.transform(inner)?;
    Ok(Node::new(NodeKind::Try(Box::new(inner)), loc))
  }

  fn transform_bind(
    &mut self,
    lhs: Node<'src>,
    rhs: Node<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let lhs = self.transform(lhs)?;
    let rhs = self.transform(rhs)?;
    Ok(Node::new(NodeKind::Bind { lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc))
  }

  fn transform_bind_right(
    &mut self,
    lhs: Node<'src>,
    rhs: Node<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let lhs = self.transform(lhs)?;
    let rhs = self.transform(rhs)?;
    Ok(Node::new(NodeKind::BindRight { lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc))
  }

  fn transform_apply(
    &mut self,
    func: Node<'src>,
    args: Vec<Node<'src>>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let func = self.transform(func)?;
    let args = self.transform_vec(args)?;
    Ok(Node::new(NodeKind::Apply { func: Box::new(func), args }, loc))
  }

  fn transform_pipe(&mut self, children: Vec<Node<'src>>, loc: Loc) -> TransformResult<'src> {
    let children = self.transform_vec(children)?;
    Ok(Node::new(NodeKind::Pipe(children), loc))
  }

  fn transform_fn(
    &mut self,
    params: Node<'src>,
    body: Vec<Node<'src>>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let params = self.transform(params)?;
    let body = self.transform_vec(body)?;
    Ok(Node::new(NodeKind::Fn { params: Box::new(params), body }, loc))
  }

  fn transform_patterns(
    &mut self,
    children: Vec<Node<'src>>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let children = self.transform_vec(children)?;
    Ok(Node::new(NodeKind::Patterns(children), loc))
  }

  fn transform_match(
    &mut self,
    subjects: Node<'src>,
    arms: Vec<Node<'src>>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let subjects = self.transform(subjects)?;
    let arms = self.transform_vec(arms)?;
    Ok(Node::new(NodeKind::Match { subjects: Box::new(subjects), arms }, loc))
  }

  fn transform_arm(
    &mut self,
    lhs: Vec<Node<'src>>,
    body: Vec<Node<'src>>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let lhs = self.transform_vec(lhs)?;
    let body = self.transform_vec(body)?;
    Ok(Node::new(NodeKind::Arm { lhs, body }, loc))
  }

  fn transform_block(
    &mut self,
    name: Node<'src>,
    params: Node<'src>,
    body: Vec<Node<'src>>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let name = self.transform(name)?;
    let params = self.transform(params)?;
    let body = self.transform_vec(body)?;
    Ok(Node::new(NodeKind::Block { name: Box::new(name), params: Box::new(params), body }, loc))
  }

  // --- helper ---

  fn transform_vec(&mut self, nodes: Vec<Node<'src>>) -> Result<Vec<Node<'src>>, TransformError> {
    nodes.into_iter().map(|n| self.transform(n)).collect()
  }
}

// --- tests ---

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ast::NodeKind;
  use crate::lexer::{Loc, Pos};

  fn dummy_loc() -> Loc {
    Loc {
      start: Pos { idx: 0, line: 1, col: 0 },
      end: Pos { idx: 0, line: 1, col: 0 },
    }
  }

  fn node(kind: NodeKind) -> Node {
    Node::new(kind, dummy_loc())
  }

  // Identity transformer — just recurses, changes nothing
  struct Identity;
  impl<'src> Transform<'src> for Identity {}

  // Counts how many Ident nodes were visited
  struct IdentCounter(usize);
  impl<'src> Transform<'src> for IdentCounter {
    fn transform_leaf(&mut self, n: Node<'src>) -> TransformResult<'src> {
      if matches!(n.kind, NodeKind::Ident(_)) {
        self.0 += 1;
      }
      Ok(n)
    }
  }

  // Replaces every LitInt with LitBool(true)
  struct IntToBool;
  impl<'src> Transform<'src> for IntToBool {
    fn transform_leaf(&mut self, n: Node<'src>) -> TransformResult<'src> {
      if matches!(n.kind, NodeKind::LitInt(_)) {
        Ok(Node::new(NodeKind::LitBool(true), n.loc))
      } else {
        Ok(n)
      }
    }
  }

  #[test]
  fn identity_preserves_leaf() {
    let n = node(NodeKind::LitInt("42"));
    let result = Identity.transform(n.clone()).unwrap();
    assert_eq!(result, n);
  }

  #[test]
  fn identity_preserves_nested() {
    // foo = 1
    let n = node(NodeKind::Bind {
      lhs: Box::new(node(NodeKind::Ident("foo"))),
      rhs: Box::new(node(NodeKind::LitInt("1"))),
    });
    let result = Identity.transform(n.clone()).unwrap();
    assert_eq!(result, n);
  }

  #[test]
  fn counter_counts_idents() {
    // [a, b, c]
    let n = node(NodeKind::LitSeq(vec![
      node(NodeKind::Ident("a")),
      node(NodeKind::Ident("b")),
      node(NodeKind::Ident("c")),
    ]));
    let mut counter = IdentCounter(0);
    counter.transform(n).unwrap();
    assert_eq!(counter.0, 3);
  }

  #[test]
  fn counter_counts_nested_idents() {
    // add a, b  =>  Apply(Ident(add), [Ident(a), Ident(b)])
    let n = node(NodeKind::Apply {
      func: Box::new(node(NodeKind::Ident("add"))),
      args: vec![node(NodeKind::Ident("a")), node(NodeKind::Ident("b"))],
    });
    let mut counter = IdentCounter(0);
    counter.transform(n).unwrap();
    assert_eq!(counter.0, 3);
  }

  #[test]
  fn rewrite_int_to_bool_in_bind() {
    // foo = 1  =>  foo = true
    let n = node(NodeKind::Bind {
      lhs: Box::new(node(NodeKind::Ident("foo"))),
      rhs: Box::new(node(NodeKind::LitInt("1"))),
    });
    let result = IntToBool.transform(n).unwrap();
    assert_eq!(
      result,
      node(NodeKind::Bind {
        lhs: Box::new(node(NodeKind::Ident("foo"))),
        rhs: Box::new(node(NodeKind::LitBool(true))),
      })
    );
  }

  #[test]
  fn rewrite_propagates_through_vec() {
    // [1, 2, 3]  =>  [true, true, true]
    let n = node(NodeKind::LitSeq(vec![
      node(NodeKind::LitInt("1")),
      node(NodeKind::LitInt("2")),
      node(NodeKind::LitInt("3")),
    ]));
    let result = IntToBool.transform(n).unwrap();
    assert_eq!(
      result,
      node(NodeKind::LitSeq(vec![
        node(NodeKind::LitBool(true)),
        node(NodeKind::LitBool(true)),
        node(NodeKind::LitBool(true)),
      ]))
    );
  }

  #[test]
  fn error_propagates() {
    struct AlwaysFails;
    impl<'src> Transform<'src> for AlwaysFails {
      fn transform_leaf(&mut self, n: Node<'src>) -> TransformResult<'src> {
        Err(TransformError::new("nope", n.loc))
      }
    }
    let n = node(NodeKind::LitInt("1"));
    assert!(AlwaysFails.transform(n).is_err());
  }

  #[test]
  fn error_short_circuits_vec() {
    struct FailOnSecond(usize);
    impl<'src> Transform<'src> for FailOnSecond {
      fn transform_leaf(&mut self, n: Node<'src>) -> TransformResult<'src> {
        self.0 += 1;
        if self.0 == 2 {
          Err(TransformError::new("fail", n.loc))
        } else {
          Ok(n)
        }
      }
    }
    let n = node(NodeKind::LitSeq(vec![
      node(NodeKind::LitInt("1")),
      node(NodeKind::LitInt("2")),
      node(NodeKind::LitInt("3")),
    ]));
    let mut t = FailOnSecond(0);
    assert!(t.transform(n).is_err());
    // Only visited 2 nodes before short-circuiting
    assert_eq!(t.0, 2);
  }
}
