use super::{CmpPart, Exprs, Node, NodeKind};
use super::lexer::{Loc, Token};

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
      | NodeKind::LitStr { .. }
      | NodeKind::Ident(_)
      | NodeKind::Partial
      | NodeKind::Wildcard => self.transform_leaf(node),

      NodeKind::LitSeq { open, close, items } => self.transform_lit_seq(open, close, items, loc),
      NodeKind::LitRec { open, close, items } => self.transform_lit_rec(open, close, items, loc),
      NodeKind::StrTempl { open, close, children } => self.transform_str_templ(open, close, children, loc),
      NodeKind::StrRawTempl { open, close, children } => self.transform_str_raw_templ(open, close, children, loc),
      NodeKind::UnaryOp { op, operand } => self.transform_unary_op(op, *operand, loc),
      NodeKind::InfixOp { op, lhs, rhs } => self.transform_infix_op(op, *lhs, *rhs, loc),
      NodeKind::ChainedCmp(parts) => self.transform_chained_cmp(parts, loc),
      NodeKind::Spread { op, inner } => self.transform_spread(op, inner.map(|n| *n), loc),
      NodeKind::Member { op, lhs, rhs } => self.transform_member(op, *lhs, *rhs, loc),
      NodeKind::Group { open, close, inner } => self.transform_group(open, close, *inner, loc),
      NodeKind::Try(inner) => self.transform_try(*inner, loc),
      NodeKind::Yield(inner) => self.transform_yield(*inner, loc),
      NodeKind::Bind { op, lhs, rhs } => self.transform_bind(op, *lhs, *rhs, loc),
      NodeKind::BindRight { op, lhs, rhs } => self.transform_bind_right(op, *lhs, *rhs, loc),
      NodeKind::Apply { func, args } => self.transform_apply(*func, args, loc),
      NodeKind::Pipe(children) => self.transform_pipe(children, loc),
      NodeKind::Fn { params, sep, body } => self.transform_fn(*params, sep, body, loc),
      NodeKind::Patterns(children) => self.transform_patterns(children, loc),
      NodeKind::Match { subjects, sep, arms } => self.transform_match(*subjects, sep, arms, loc),
      NodeKind::Arm { lhs, sep, body } => self.transform_arm(lhs, sep, body, loc),
      NodeKind::Block { name, params, sep, body } => self.transform_block(*name, *params, sep, body, loc),
    }
  }

  // --- leaf nodes (no children) ---

  fn transform_leaf(&mut self, node: Node<'src>) -> TransformResult<'src> {
    Ok(node)
  }

  // --- composite nodes ---

  fn transform_lit_seq(
    &mut self,
    open: Token<'src>,
    close: Token<'src>,
    items: Exprs<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let items = self.transform_exprs(items)?;
    Ok(Node::new(NodeKind::LitSeq { open, close, items }, loc))
  }

  fn transform_lit_rec(
    &mut self,
    open: Token<'src>,
    close: Token<'src>,
    items: Exprs<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let items = self.transform_exprs(items)?;
    Ok(Node::new(NodeKind::LitRec { open, close, items }, loc))
  }

  fn transform_str_templ(
    &mut self,
    open: Token<'src>,
    close: Token<'src>,
    children: Vec<Node<'src>>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let children = self.transform_vec(children)?;
    Ok(Node::new(NodeKind::StrTempl { open, close, children }, loc))
  }

  fn transform_str_raw_templ(
    &mut self,
    open: Token<'src>,
    close: Token<'src>,
    children: Vec<Node<'src>>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let children = self.transform_vec(children)?;
    Ok(Node::new(NodeKind::StrRawTempl { open, close, children }, loc))
  }

  fn transform_unary_op(
    &mut self,
    op: Token<'src>,
    operand: Node<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let operand = self.transform(operand)?;
    Ok(Node::new(NodeKind::UnaryOp { op, operand: Box::new(operand) }, loc))
  }

  fn transform_infix_op(
    &mut self,
    op: Token<'src>,
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

  fn transform_spread(
    &mut self,
    op: Token<'src>,
    inner: Option<Node<'src>>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let inner = inner.map(|n| self.transform(n)).transpose()?;
    Ok(Node::new(NodeKind::Spread { op, inner: inner.map(Box::new) }, loc))
  }

  fn transform_member(
    &mut self,
    op: Token<'src>,
    lhs: Node<'src>,
    rhs: Node<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let lhs = self.transform(lhs)?;
    let rhs = self.transform(rhs)?;
    Ok(Node::new(NodeKind::Member { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc))
  }

  fn transform_group(&mut self, open: Token<'src>, close: Token<'src>, inner: Node<'src>, loc: Loc) -> TransformResult<'src> {
    let inner = self.transform(inner)?;
    Ok(Node::new(NodeKind::Group { open, close, inner: Box::new(inner) }, loc))
  }

  fn transform_try(&mut self, inner: Node<'src>, loc: Loc) -> TransformResult<'src> {
    let inner = self.transform(inner)?;
    Ok(Node::new(NodeKind::Try(Box::new(inner)), loc))
  }

  fn transform_yield(&mut self, inner: Node<'src>, loc: Loc) -> TransformResult<'src> {
    let inner = self.transform(inner)?;
    Ok(Node::new(NodeKind::Yield(Box::new(inner)), loc))
  }

  fn transform_bind(
    &mut self,
    op: Token<'src>,
    lhs: Node<'src>,
    rhs: Node<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let lhs = self.transform(lhs)?;
    let rhs = self.transform(rhs)?;
    Ok(Node::new(NodeKind::Bind { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc))
  }

  fn transform_bind_right(
    &mut self,
    op: Token<'src>,
    lhs: Node<'src>,
    rhs: Node<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let lhs = self.transform(lhs)?;
    let rhs = self.transform(rhs)?;
    Ok(Node::new(NodeKind::BindRight { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc))
  }

  fn transform_apply(
    &mut self,
    func: Node<'src>,
    args: Exprs<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let func = self.transform(func)?;
    let args = self.transform_exprs(args)?;
    Ok(Node::new(NodeKind::Apply { func: Box::new(func), args }, loc))
  }

  fn transform_pipe(&mut self, exprs: Exprs<'src>, loc: Loc) -> TransformResult<'src> {
    let exprs = self.transform_exprs(exprs)?;
    Ok(Node::new(NodeKind::Pipe(exprs), loc))
  }

  fn transform_fn(
    &mut self,
    params: Node<'src>,
    sep: Token<'src>,
    body: Exprs<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let params = self.transform(params)?;
    let body = self.transform_exprs(body)?;
    Ok(Node::new(NodeKind::Fn { params: Box::new(params), sep, body }, loc))
  }

  fn transform_patterns(
    &mut self,
    exprs: Exprs<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let exprs = self.transform_exprs(exprs)?;
    Ok(Node::new(NodeKind::Patterns(exprs), loc))
  }

  fn transform_match(
    &mut self,
    subjects: Node<'src>,
    sep: Token<'src>,
    arms: Exprs<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let subjects = self.transform(subjects)?;
    let arms = self.transform_exprs(arms)?;
    Ok(Node::new(NodeKind::Match { subjects: Box::new(subjects), sep, arms }, loc))
  }

  fn transform_arm(
    &mut self,
    lhs: Exprs<'src>,
    sep: Token<'src>,
    body: Exprs<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let lhs = self.transform_exprs(lhs)?;
    let body = self.transform_exprs(body)?;
    Ok(Node::new(NodeKind::Arm { lhs, sep, body }, loc))
  }

  fn transform_block(
    &mut self,
    name: Node<'src>,
    params: Node<'src>,
    sep: Token<'src>,
    body: Exprs<'src>,
    loc: Loc,
  ) -> TransformResult<'src> {
    let name = self.transform(name)?;
    let params = self.transform(params)?;
    let body = self.transform_exprs(body)?;
    Ok(Node::new(NodeKind::Block { name: Box::new(name), params: Box::new(params), sep, body }, loc))
  }

  // --- helpers ---

  fn transform_vec(&mut self, nodes: Vec<Node<'src>>) -> Result<Vec<Node<'src>>, TransformError> {
    nodes.into_iter().map(|n| self.transform(n)).collect()
  }

  fn transform_exprs(&mut self, exprs: Exprs<'src>) -> Result<Exprs<'src>, TransformError> {
    let items = exprs.items.into_iter().map(|n| self.transform(n)).collect::<Result<_, _>>()?;
    Ok(Exprs { items, seps: exprs.seps })
  }
}

// --- tests ---

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ast::{Exprs, NodeKind};
  use crate::lexer::{Loc, Pos, Token, TokenKind};

  fn exprs(items: Vec<Node>) -> Exprs {
    Exprs { items, seps: vec![] }
  }

  fn dummy_loc() -> Loc {
    Loc {
      start: Pos { idx: 0, line: 1, col: 0 },
      end: Pos { idx: 0, line: 1, col: 0 },
    }
  }

  fn tok(src: &str) -> Token<'_> {
    Token { kind: TokenKind::Sep, loc: dummy_loc(), src }
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
      op: tok("="),
      lhs: Box::new(node(NodeKind::Ident("foo"))),
      rhs: Box::new(node(NodeKind::LitInt("1"))),
    });
    let result = Identity.transform(n.clone()).unwrap();
    assert_eq!(result, n);
  }

  #[test]
  fn counter_counts_idents() {
    // [a, b, c]
    let n = node(NodeKind::LitSeq {
      open: tok("["), close: tok("]"),
      items: exprs(vec![
        node(NodeKind::Ident("a")),
        node(NodeKind::Ident("b")),
        node(NodeKind::Ident("c")),
      ]),
    });
    let mut counter = IdentCounter(0);
    counter.transform(n).unwrap();
    assert_eq!(counter.0, 3);
  }

  #[test]
  fn counter_counts_nested_idents() {
    // add a, b  =>  Apply(Ident(add), [Ident(a), Ident(b)])
    let n = node(NodeKind::Apply {
      func: Box::new(node(NodeKind::Ident("add"))),
      args: exprs(vec![node(NodeKind::Ident("a")), node(NodeKind::Ident("b"))]),
    });
    let mut counter = IdentCounter(0);
    counter.transform(n).unwrap();
    assert_eq!(counter.0, 3);
  }

  #[test]
  fn rewrite_int_to_bool_in_bind() {
    // foo = 1  =>  foo = true
    let n = node(NodeKind::Bind {
      op: tok("="),
      lhs: Box::new(node(NodeKind::Ident("foo"))),
      rhs: Box::new(node(NodeKind::LitInt("1"))),
    });
    let result = IntToBool.transform(n).unwrap();
    assert_eq!(
      result,
      node(NodeKind::Bind {
        op: tok("="),
        lhs: Box::new(node(NodeKind::Ident("foo"))),
        rhs: Box::new(node(NodeKind::LitBool(true))),
      })
    );
  }

  #[test]
  fn rewrite_propagates_through_vec() {
    // [1, 2, 3]  =>  [true, true, true]
    let n = node(NodeKind::LitSeq {
      open: tok("["), close: tok("]"),
      items: exprs(vec![
        node(NodeKind::LitInt("1")),
        node(NodeKind::LitInt("2")),
        node(NodeKind::LitInt("3")),
      ]),
    });
    let result = IntToBool.transform(n).unwrap();
    assert_eq!(
      result,
      node(NodeKind::LitSeq {
        open: tok("["), close: tok("]"),
        items: exprs(vec![
          node(NodeKind::LitBool(true)),
          node(NodeKind::LitBool(true)),
          node(NodeKind::LitBool(true)),
        ]),
      })
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
    let n = node(NodeKind::LitSeq {
      open: tok("["), close: tok("]"),
      items: exprs(vec![
        node(NodeKind::LitInt("1")),
        node(NodeKind::LitInt("2")),
        node(NodeKind::LitInt("3")),
      ]),
    });
    let mut t = FailOnSecond(0);
    assert!(t.transform(n).is_err());
    // Only visited 2 nodes before short-circuiting
    assert_eq!(t.0, 2);
  }
}
