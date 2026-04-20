use super::{Ast, AstBuilder, AstId, CmpPart, Exprs, NodeKind};
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

pub type TransformResult = Result<AstId, TransformError>;

// --- transformer trait ---
//
// Append-only rewrites on a flat AST arena. A pass's read view is the
// immutable `src: &Ast<'src>` passed alongside `builder: &mut AstBuilder<'src>`
// — the two-handle rule documented in `arena-contract.md`.
//
// Default implementations recurse into children and append a fresh parent
// node via `builder.append`. Override only the methods you need — leaves
// return their own id unchanged by default (fast path).
//
// The `loc` of a node is preserved unless explicitly changed.

pub trait Transform<'src> {
  fn transform(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    id: AstId,
  ) -> TransformResult {
    // Read once, clone the kind, drop the `src` borrow — then we can
    // mutably borrow `builder` freely. Cloning NodeKind is cheap: children
    // are AstId (Copy), tokens are Copy, only LitStr's String and Module's
    // url are owned heap data, both small.
    let node = src.nodes.get(id);
    let loc = node.loc;
    let kind = node.kind.clone();
    match kind {
      NodeKind::LitBool(_)
      | NodeKind::LitInt(_)
      | NodeKind::LitFloat(_)
      | NodeKind::LitDecimal(_)
      | NodeKind::LitStr { .. }
      | NodeKind::Ident(_)
      | NodeKind::SynthIdent(_)
      | NodeKind::Partial
      | NodeKind::Wildcard
      | NodeKind::Token(_) => self.transform_leaf(builder, src, id),

      NodeKind::LitSeq { open, close, items } => self.transform_lit_seq(builder, src, open, close, items, loc),
      NodeKind::LitRec { open, close, items } => self.transform_lit_rec(builder, src, open, close, items, loc),
      NodeKind::StrTempl { open, close, children } => self.transform_str_templ(builder, src, open, close, children, loc),
      NodeKind::StrRawTempl { open, close, children } => self.transform_str_raw_templ(builder, src, open, close, children, loc),
      NodeKind::UnaryOp { op, operand } => self.transform_unary_op(builder, src, op, operand, loc),
      NodeKind::InfixOp { op, lhs, rhs } => self.transform_infix_op(builder, src, op, lhs, rhs, loc),
      NodeKind::ChainedCmp(parts) => self.transform_chained_cmp(builder, src, &parts, loc),
      NodeKind::Spread { op, inner } => self.transform_spread(builder, src, op, inner, loc),
      NodeKind::Member { op, lhs, rhs } => self.transform_member(builder, src, op, lhs, rhs, loc),
      NodeKind::Group { open, close, inner } => self.transform_group(builder, src, open, close, inner, loc),
      NodeKind::Try(inner) => self.transform_try(builder, src, inner, loc),
      NodeKind::Bind { op, lhs, rhs } => self.transform_bind(builder, src, op, lhs, rhs, loc),
      NodeKind::BindRight { op, lhs, rhs } => self.transform_bind_right(builder, src, op, lhs, rhs, loc),
      NodeKind::Apply { func, args } => self.transform_apply(builder, src, func, args, loc),
      NodeKind::Pipe(exprs) => self.transform_pipe(builder, src, exprs, loc),
      NodeKind::Module { exprs, url } => self.transform_module(builder, src, exprs, url, loc),
      NodeKind::Fn { params, sep, body } => self.transform_fn(builder, src, params, sep, body, loc),
      NodeKind::Patterns(exprs) => self.transform_patterns(builder, src, exprs, loc),
      NodeKind::Match { subjects, sep, arms } => self.transform_match(builder, src, subjects, sep, arms, loc),
      NodeKind::Arm { lhs, sep, body } => self.transform_arm(builder, src, lhs, sep, body, loc),
      NodeKind::Block { name, params, sep, body } => self.transform_block(builder, src, name, params, sep, body, loc),
    }
  }

  // --- leaf nodes (no children) ---
  //
  // Default: return the same id — no append, no allocation. This is the
  // fast path the append-only discipline relies on.

  fn transform_leaf(
    &mut self,
    _builder: &mut AstBuilder<'src>,
    _src: &Ast<'src>,
    id: AstId,
  ) -> TransformResult {
    Ok(id)
  }

  // --- composite nodes ---

  fn transform_lit_seq(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    open: Token<'src>,
    close: Token<'src>,
    items: Exprs<'src>,
    loc: Loc,
  ) -> TransformResult {
    let items = self.transform_exprs(builder, src, &items)?;
    Ok(builder.append(NodeKind::LitSeq { open, close, items }, loc))
  }

  fn transform_lit_rec(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    open: Token<'src>,
    close: Token<'src>,
    items: Exprs<'src>,
    loc: Loc,
  ) -> TransformResult {
    let items = self.transform_exprs(builder, src, &items)?;
    Ok(builder.append(NodeKind::LitRec { open, close, items }, loc))
  }

  fn transform_str_templ(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    open: Token<'src>,
    close: Token<'src>,
    children: Box<[AstId]>,
    loc: Loc,
  ) -> TransformResult {
    let children = self.transform_ids(builder, src, &children)?;
    Ok(builder.append(NodeKind::StrTempl { open, close, children }, loc))
  }

  fn transform_str_raw_templ(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    open: Token<'src>,
    close: Token<'src>,
    children: Box<[AstId]>,
    loc: Loc,
  ) -> TransformResult {
    let children = self.transform_ids(builder, src, &children)?;
    Ok(builder.append(NodeKind::StrRawTempl { open, close, children }, loc))
  }

  fn transform_unary_op(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    op: Token<'src>,
    operand: AstId,
    loc: Loc,
  ) -> TransformResult {
    let operand = self.transform(builder, src, operand)?;
    Ok(builder.append(NodeKind::UnaryOp { op, operand }, loc))
  }

  fn transform_infix_op(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    op: Token<'src>,
    lhs: AstId,
    rhs: AstId,
    loc: Loc,
  ) -> TransformResult {
    let lhs = self.transform(builder, src, lhs)?;
    let rhs = self.transform(builder, src, rhs)?;
    Ok(builder.append(NodeKind::InfixOp { op, lhs, rhs }, loc))
  }

  fn transform_chained_cmp(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    parts: &[CmpPart<'src>],
    loc: Loc,
  ) -> TransformResult {
    let mut new_parts: Vec<CmpPart<'src>> = Vec::with_capacity(parts.len());
    for part in parts.iter() {
      match part {
        CmpPart::Operand(n) => {
          let new_id = self.transform(builder, src, *n)?;
          new_parts.push(CmpPart::Operand(new_id));
        }
        CmpPart::Op(op) => new_parts.push(CmpPart::Op(*op)),
      }
    }
    Ok(builder.append(NodeKind::ChainedCmp(new_parts.into_boxed_slice()), loc))
  }

  fn transform_spread(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    op: Token<'src>,
    inner: Option<AstId>,
    loc: Loc,
  ) -> TransformResult {
    let inner = match inner {
      Some(id) => Some(self.transform(builder, src, id)?),
      None => None,
    };
    Ok(builder.append(NodeKind::Spread { op, inner }, loc))
  }

  fn transform_member(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    op: Token<'src>,
    lhs: AstId,
    rhs: AstId,
    loc: Loc,
  ) -> TransformResult {
    let lhs = self.transform(builder, src, lhs)?;
    let rhs = self.transform(builder, src, rhs)?;
    Ok(builder.append(NodeKind::Member { op, lhs, rhs }, loc))
  }

  fn transform_group(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    open: Token<'src>,
    close: Token<'src>,
    inner: AstId,
    loc: Loc,
  ) -> TransformResult {
    let inner = self.transform(builder, src, inner)?;
    Ok(builder.append(NodeKind::Group { open, close, inner }, loc))
  }

  fn transform_try(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    inner: AstId,
    loc: Loc,
  ) -> TransformResult {
    let inner = self.transform(builder, src, inner)?;
    Ok(builder.append(NodeKind::Try(inner), loc))
  }

  fn transform_bind(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    op: Token<'src>,
    lhs: AstId,
    rhs: AstId,
    loc: Loc,
  ) -> TransformResult {
    let lhs = self.transform(builder, src, lhs)?;
    let rhs = self.transform(builder, src, rhs)?;
    Ok(builder.append(NodeKind::Bind { op, lhs, rhs }, loc))
  }

  fn transform_bind_right(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    op: Token<'src>,
    lhs: AstId,
    rhs: AstId,
    loc: Loc,
  ) -> TransformResult {
    let lhs = self.transform(builder, src, lhs)?;
    let rhs = self.transform(builder, src, rhs)?;
    Ok(builder.append(NodeKind::BindRight { op, lhs, rhs }, loc))
  }

  fn transform_apply(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    func: AstId,
    args: Exprs<'src>,
    loc: Loc,
  ) -> TransformResult {
    let func = self.transform(builder, src, func)?;
    let args = self.transform_exprs(builder, src, &args)?;
    Ok(builder.append(NodeKind::Apply { func, args }, loc))
  }

  fn transform_pipe(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    exprs: Exprs<'src>,
    loc: Loc,
  ) -> TransformResult {
    let exprs = self.transform_exprs(builder, src, &exprs)?;
    Ok(builder.append(NodeKind::Pipe(exprs), loc))
  }

  fn transform_module(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    exprs: Exprs<'src>,
    url: String,
    loc: Loc,
  ) -> TransformResult {
    let exprs = self.transform_exprs(builder, src, &exprs)?;
    Ok(builder.append(NodeKind::Module { exprs, url }, loc))
  }

  fn transform_fn(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    params: AstId,
    sep: Token<'src>,
    body: Exprs<'src>,
    loc: Loc,
  ) -> TransformResult {
    let params = self.transform(builder, src, params)?;
    let body = self.transform_exprs(builder, src, &body)?;
    Ok(builder.append(NodeKind::Fn { params, sep, body }, loc))
  }

  fn transform_patterns(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    exprs: Exprs<'src>,
    loc: Loc,
  ) -> TransformResult {
    let exprs = self.transform_exprs(builder, src, &exprs)?;
    Ok(builder.append(NodeKind::Patterns(exprs), loc))
  }

  fn transform_match(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    subjects: Exprs<'src>,
    sep: Token<'src>,
    arms: Exprs<'src>,
    loc: Loc,
  ) -> TransformResult {
    let subjects = self.transform_exprs(builder, src, &subjects)?;
    let arms = self.transform_exprs(builder, src, &arms)?;
    Ok(builder.append(NodeKind::Match { subjects, sep, arms }, loc))
  }

  fn transform_arm(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    lhs: AstId,
    sep: Token<'src>,
    body: Exprs<'src>,
    loc: Loc,
  ) -> TransformResult {
    let lhs = self.transform(builder, src, lhs)?;
    let body = self.transform_exprs(builder, src, &body)?;
    Ok(builder.append(NodeKind::Arm { lhs, sep, body }, loc))
  }

  #[allow(clippy::too_many_arguments)]
  fn transform_block(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    name: AstId,
    params: AstId,
    sep: Token<'src>,
    body: Exprs<'src>,
    loc: Loc,
  ) -> TransformResult {
    let name = self.transform(builder, src, name)?;
    let params = self.transform(builder, src, params)?;
    let body = self.transform_exprs(builder, src, &body)?;
    Ok(builder.append(NodeKind::Block { name, params, sep, body }, loc))
  }

  // --- helpers ---

  fn transform_ids(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    ids: &[AstId],
  ) -> Result<Box<[AstId]>, TransformError> {
    let mut out = Vec::with_capacity(ids.len());
    for &id in ids {
      out.push(self.transform(builder, src, id)?);
    }
    Ok(out.into_boxed_slice())
  }

  fn transform_exprs(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    exprs: &Exprs<'src>,
  ) -> Result<Exprs<'src>, TransformError> {
    let items = self.transform_ids(builder, src, &exprs.items)?;
    Ok(Exprs { items, seps: exprs.seps.clone() })
  }
}

// --- tests ---

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ast::{Ast, AstBuilder, Exprs, NodeKind};
  use crate::lexer::{Loc, Pos, Token, TokenKind};

  fn dummy_loc() -> Loc {
    Loc {
      start: Pos { idx: 0, line: 1, col: 0 },
      end: Pos { idx: 0, line: 1, col: 0 },
    }
  }

  fn tok(src: &str) -> Token<'_> {
    Token { kind: TokenKind::Sep, loc: dummy_loc(), src }
  }

  /// Run a transform pass against a source Ast, returning the output Ast.
  fn run<'src, T: Transform<'src>>(mut pass: T, src: Ast<'src>) -> Result<Ast<'src>, TransformError> {
    let snapshot = src.clone();
    let (mut builder, root) = AstBuilder::from_ast(src);
    let new_root = pass.transform(&mut builder, &snapshot, root)?;
    Ok(builder.finish(new_root))
  }

  // Identity transformer — just recurses, changes nothing
  struct Identity;
  impl<'src> Transform<'src> for Identity {}

  // Counts how many Ident nodes were visited (via transform_leaf override).
  struct IdentCounter(usize);
  impl<'src> Transform<'src> for IdentCounter {
    fn transform_leaf(
      &mut self,
      _builder: &mut AstBuilder<'src>,
      src: &Ast<'src>,
      id: AstId,
    ) -> TransformResult {
      if matches!(src.nodes.get(id).kind, NodeKind::Ident(_)) {
        self.0 += 1;
      }
      Ok(id)
    }
  }

  // Replaces every LitInt with LitBool(true).
  struct IntToBool;
  impl<'src> Transform<'src> for IntToBool {
    fn transform_leaf(
      &mut self,
      builder: &mut AstBuilder<'src>,
      src: &Ast<'src>,
      id: AstId,
    ) -> TransformResult {
      let n = src.nodes.get(id);
      if matches!(n.kind, NodeKind::LitInt(_)) {
        Ok(builder.append(NodeKind::LitBool(true), n.loc))
      } else {
        Ok(id)
      }
    }
  }

  #[test]
  fn identity_preserves_leaf() {
    // Build: just a LitInt("42").
    let mut b = AstBuilder::new();
    let root = b.append(NodeKind::LitInt("42"), dummy_loc());
    let src = b.finish(root);
    let before = src.clone();
    let result = run(Identity, src).unwrap();
    // Identity returns the input id unchanged — root AstId must match and
    // the old slot must still contain the same node.
    assert_eq!(result.root, before.root);
    assert!(super::super::appended_only(&before, &result).is_ok());
  }

  #[test]
  fn identity_preserves_nested_bind() {
    // foo = 1
    let mut b = AstBuilder::new();
    let foo = b.append(NodeKind::Ident("foo"), dummy_loc());
    let one = b.append(NodeKind::LitInt("1"), dummy_loc());
    let bind = b.append(NodeKind::Bind { op: tok("="), lhs: foo, rhs: one }, dummy_loc());
    let src = b.finish(bind);
    let before = src.clone();
    let result = run(Identity, src).unwrap();
    // The default composite Transform methods always append a fresh parent,
    // so the root moves forward by one slot (old Bind at #2 → new Bind at #3).
    // This is semantically Identity — the new Bind has the same leaf children
    // (fast-path returned their ids unchanged). Only composite nodes get
    // re-appended.
    assert_eq!(result.nodes.len(), 4);
    let new_bind = result.nodes.get(result.root);
    match &new_bind.kind {
      NodeKind::Bind { lhs, rhs, .. } => {
        assert_eq!(*lhs, foo);
        assert_eq!(*rhs, one);
      }
      _ => panic!("expected Bind"),
    }
    assert!(super::super::appended_only(&before, &result).is_ok());
  }

  #[test]
  fn counter_counts_idents() {
    // [a, b, c]
    let mut b = AstBuilder::new();
    let a = b.append(NodeKind::Ident("a"), dummy_loc());
    let b_id = b.append(NodeKind::Ident("b"), dummy_loc());
    let c = b.append(NodeKind::Ident("c"), dummy_loc());
    let seq = b.append(
      NodeKind::LitSeq {
        open: tok("["),
        close: tok("]"),
        items: Exprs { items: Box::new([a, b_id, c]), seps: vec![] },
      },
      dummy_loc(),
    );
    let src = b.finish(seq);

    let snapshot = src.clone();
    let (mut builder, root) = AstBuilder::from_ast(src);
    let mut counter = IdentCounter(0);
    counter.transform(&mut builder, &snapshot, root).unwrap();
    assert_eq!(counter.0, 3);
  }

  #[test]
  fn counter_counts_nested_idents() {
    // add a, b  =>  Apply(Ident(add), [Ident(a), Ident(b)])
    let mut b = AstBuilder::new();
    let add = b.append(NodeKind::Ident("add"), dummy_loc());
    let a = b.append(NodeKind::Ident("a"), dummy_loc());
    let b_id = b.append(NodeKind::Ident("b"), dummy_loc());
    let apply = b.append(
      NodeKind::Apply {
        func: add,
        args: Exprs { items: Box::new([a, b_id]), seps: vec![] },
      },
      dummy_loc(),
    );
    let src = b.finish(apply);

    let snapshot = src.clone();
    let (mut builder, root) = AstBuilder::from_ast(src);
    let mut counter = IdentCounter(0);
    counter.transform(&mut builder, &snapshot, root).unwrap();
    assert_eq!(counter.0, 3);
  }

  #[test]
  fn rewrite_int_to_bool_in_bind() {
    // foo = 1  =>  foo = true
    let mut b = AstBuilder::new();
    let foo = b.append(NodeKind::Ident("foo"), dummy_loc());
    let one = b.append(NodeKind::LitInt("1"), dummy_loc());
    let bind = b.append(NodeKind::Bind { op: tok("="), lhs: foo, rhs: one }, dummy_loc());
    let src = b.finish(bind);
    let before = src.clone();

    let result = run(IntToBool, src).unwrap();

    // Append-only preserved.
    assert!(super::super::appended_only(&before, &result).is_ok());

    // The new root is a fresh Bind whose rhs is a fresh LitBool.
    match &result.nodes.get(result.root).kind {
      NodeKind::Bind { rhs, lhs, .. } => {
        assert!(matches!(result.nodes.get(*rhs).kind, NodeKind::LitBool(true)));
        // lhs is the original foo id (unchanged leaf — fast path).
        assert_eq!(*lhs, foo);
      }
      _ => panic!("expected Bind"),
    }
  }

  #[test]
  fn rewrite_propagates_through_exprs() {
    // [1, 2, 3]  =>  [true, true, true]
    let mut b = AstBuilder::new();
    let one = b.append(NodeKind::LitInt("1"), dummy_loc());
    let two = b.append(NodeKind::LitInt("2"), dummy_loc());
    let three = b.append(NodeKind::LitInt("3"), dummy_loc());
    let seq = b.append(
      NodeKind::LitSeq {
        open: tok("["),
        close: tok("]"),
        items: Exprs { items: Box::new([one, two, three]), seps: vec![] },
      },
      dummy_loc(),
    );
    let src = b.finish(seq);
    let before = src.clone();

    let result = run(IntToBool, src).unwrap();

    assert!(super::super::appended_only(&before, &result).is_ok());
    match &result.nodes.get(result.root).kind {
      NodeKind::LitSeq { items, .. } => {
        assert_eq!(items.items.len(), 3);
        for &id in items.items.iter() {
          assert!(matches!(result.nodes.get(id).kind, NodeKind::LitBool(true)));
        }
      }
      _ => panic!("expected LitSeq"),
    }
  }

  #[test]
  fn error_propagates() {
    struct AlwaysFails;
    impl<'src> Transform<'src> for AlwaysFails {
      fn transform_leaf(
        &mut self,
        _builder: &mut AstBuilder<'src>,
        src: &Ast<'src>,
        id: AstId,
      ) -> TransformResult {
        Err(TransformError::new("nope", src.nodes.get(id).loc))
      }
    }
    let mut b = AstBuilder::new();
    let root = b.append(NodeKind::LitInt("1"), dummy_loc());
    let src = b.finish(root);
    assert!(run(AlwaysFails, src).is_err());
  }

  #[test]
  fn error_short_circuits_seq() {
    struct FailOnSecond(usize);
    impl<'src> Transform<'src> for FailOnSecond {
      fn transform_leaf(
        &mut self,
        _builder: &mut AstBuilder<'src>,
        src: &Ast<'src>,
        id: AstId,
      ) -> TransformResult {
        self.0 += 1;
        if self.0 == 2 {
          Err(TransformError::new("fail", src.nodes.get(id).loc))
        } else {
          Ok(id)
        }
      }
    }
    let mut b = AstBuilder::new();
    let one = b.append(NodeKind::LitInt("1"), dummy_loc());
    let two = b.append(NodeKind::LitInt("2"), dummy_loc());
    let three = b.append(NodeKind::LitInt("3"), dummy_loc());
    let seq = b.append(
      NodeKind::LitSeq {
        open: tok("["),
        close: tok("]"),
        items: Exprs { items: Box::new([one, two, three]), seps: vec![] },
      },
      dummy_loc(),
    );
    let src = b.finish(seq);

    let snapshot = src.clone();
    let (mut builder, root) = AstBuilder::from_ast(src);
    let mut t = FailOnSecond(0);
    assert!(t.transform(&mut builder, &snapshot, root).is_err());
    assert_eq!(t.0, 2);
  }
}
