//! Partial application pass — desugars `?` (Partial nodes) into `Fn` nodes.
//!
//! Scoping rules:
//!
//! - `?` bubbles up to the nearest enclosing scope boundary.
//! - Scope boundaries that stop the bubble:
//!   - `Group (...)`
//!   - each segment of a `Pipe`
//!   - the RHS of a `Bind` (`lhs = rhs`) — the bubble stops at `rhs`; the whole `Bind`
//!     is never wrapped
//!   - the LHS of a `BindRight` (`lhs |= rhs`) — symmetric to `Bind`
//!   - a standalone top-level expression
//! - Transparent nodes (pass through): `Apply`, `InfixOp`, `UnaryOp`, `Member`, `Range`,
//!   `Spread`, `LitSeq`, `LitRec`, `StrTempl`.
//! - All `?` in the same scope become the same single param `$`.
//! - `?` in pattern position (`Arm` lhs, `Bind` lhs) is a compile error.
//!
//! Append-only implementation: the flat AST arena grows as partial wraps
//! subtrees in synthetic `Fn` nodes. `has_partial` is a pure reader.
//! `replace_partial` produces fresh node ids for any subtree that changes;
//! unchanged subtrees return their own id (fast path).

use crate::ast::{Ast, AstBuilder, AstId, CmpPart, Exprs, NodeKind};
use crate::lexer::{Loc, Token};
use crate::ast::transform::{Transform, TransformError, TransformResult};


// --- public entry point ---

/// Apply partial desugaring. Takes ownership of the input Ast (reopens it
/// as a builder) and returns a new Ast whose root is the transformed
/// Module. All old slots remain reachable at their original ids; any
/// rewrites appear as fresh appended nodes.
pub fn apply(ast: Ast<'_>) -> Result<Ast<'_>, TransformError> {
  // Pre-pass: rewrite `Apply(f, [Wildcard])` -> `Apply(f, [])`.
  let src0 = ast.clone();
  let (mut builder0, root0) = AstBuilder::from_ast(ast);
  let pre_root = rewrite_wildcard_call(&mut builder0, &src0, root0);
  let pre_ast = builder0.finish(pre_root);

  // Main partial-application desugaring.
  let src = pre_ast.clone();
  let (mut builder, root) = AstBuilder::from_ast(pre_ast);
  let mut pass = PartialPass { synth_counter: 0 };
  let new_root = pass.transform_stmt(&mut builder, &src, root)?;
  Ok(builder.finish(new_root))
}

/// Pre-pass: rewrite `Apply(f, [Wildcard])` -> `Apply(f, [])`.
/// `_` as the sole argument means "call with no args".
fn rewrite_wildcard_call<'src>(
  builder: &mut AstBuilder<'src>,
  src: &Ast<'src>,
  id: AstId,
) -> AstId {
  let node = src.nodes.get(id);
  let loc = node.loc;
  let kind = node.kind.clone();
  match kind {
    NodeKind::Apply { func, args } => {
      let new_func = rewrite_wildcard_call(builder, src, func);
      let is_sole_wildcard = args.items.len() == 1
        && matches!(src.nodes.get(args.items[0]).kind, NodeKind::Wildcard);
      let new_args = if is_sole_wildcard {
        Exprs::empty()
      } else {
        let new_items: Vec<AstId> = args.items.iter()
          .map(|&i| rewrite_wildcard_call(builder, src, i))
          .collect();
        if new_func == func && new_items.iter().zip(args.items.iter()).all(|(a, b)| a == b) {
          return id;
        }
        Exprs { items: new_items.into_boxed_slice(), seps: args.seps.clone() }
      };
      builder.append(NodeKind::Apply { func: new_func, args: new_args }, loc)
    }
    NodeKind::Module { exprs, url } => {
      let new_items: Vec<AstId> = exprs.items.iter()
        .map(|&i| rewrite_wildcard_call(builder, src, i))
        .collect();
      if new_items.iter().zip(exprs.items.iter()).all(|(a, b)| a == b) {
        return id;
      }
      builder.append(
        NodeKind::Module {
          exprs: Exprs { items: new_items.into_boxed_slice(), seps: exprs.seps.clone() },
          url,
        },
        loc,
      )
    }
    NodeKind::Bind { op, lhs, rhs } => {
      let new_rhs = rewrite_wildcard_call(builder, src, rhs);
      if new_rhs == rhs { id } else {
        builder.append(NodeKind::Bind { op, lhs, rhs: new_rhs }, loc)
      }
    }
    NodeKind::BindRight { op, lhs, rhs } => {
      let new_lhs = rewrite_wildcard_call(builder, src, lhs);
      if new_lhs == lhs { id } else {
        builder.append(NodeKind::BindRight { op, lhs: new_lhs, rhs }, loc)
      }
    }
    NodeKind::Group { open, close, inner } => {
      let new_inner = rewrite_wildcard_call(builder, src, inner);
      if new_inner == inner { id } else {
        builder.append(NodeKind::Group { open, close, inner: new_inner }, loc)
      }
    }
    NodeKind::Pipe(exprs) => {
      let new_items: Vec<AstId> = exprs.items.iter()
        .map(|&i| rewrite_wildcard_call(builder, src, i))
        .collect();
      if new_items.iter().zip(exprs.items.iter()).all(|(a, b)| a == b) {
        return id;
      }
      builder.append(
        NodeKind::Pipe(Exprs {
          items: new_items.into_boxed_slice(),
          seps: exprs.seps.clone(),
        }),
        loc,
      )
    }
    NodeKind::Fn { sep, params, body } => {
      let new_items: Vec<AstId> = body.items.iter()
        .map(|&i| rewrite_wildcard_call(builder, src, i))
        .collect();
      if new_items.iter().zip(body.items.iter()).all(|(a, b)| a == b) {
        return id;
      }
      builder.append(
        NodeKind::Fn {
          sep,
          params,
          body: Exprs { items: new_items.into_boxed_slice(), seps: body.seps.clone() },
        },
        loc,
      )
    }
    NodeKind::Arm { lhs, sep, body } => {
      let new_items: Vec<AstId> = body.items.iter()
        .map(|&i| rewrite_wildcard_call(builder, src, i))
        .collect();
      if new_items.iter().zip(body.items.iter()).all(|(a, b)| a == b) {
        return id;
      }
      builder.append(
        NodeKind::Arm {
          lhs,
          sep,
          body: Exprs { items: new_items.into_boxed_slice(), seps: body.seps.clone() },
        },
        loc,
      )
    }
    NodeKind::InfixOp { op, lhs, rhs } => {
      let new_lhs = rewrite_wildcard_call(builder, src, lhs);
      let new_rhs = rewrite_wildcard_call(builder, src, rhs);
      if new_lhs == lhs && new_rhs == rhs { id } else {
        builder.append(NodeKind::InfixOp { op, lhs: new_lhs, rhs: new_rhs }, loc)
      }
    }
    NodeKind::UnaryOp { op, operand } => {
      let new_operand = rewrite_wildcard_call(builder, src, operand);
      if new_operand == operand { id } else {
        builder.append(NodeKind::UnaryOp { op, operand: new_operand }, loc)
      }
    }
    NodeKind::Member { op, lhs, rhs } => {
      let new_lhs = rewrite_wildcard_call(builder, src, lhs);
      let new_rhs = rewrite_wildcard_call(builder, src, rhs);
      if new_lhs == lhs && new_rhs == rhs { id } else {
        builder.append(NodeKind::Member { op, lhs: new_lhs, rhs: new_rhs }, loc)
      }
    }
    NodeKind::LitSeq { open, close, items } => {
      let new_items: Vec<AstId> = items.items.iter()
        .map(|&i| rewrite_wildcard_call(builder, src, i))
        .collect();
      if new_items.iter().zip(items.items.iter()).all(|(a, b)| a == b) {
        return id;
      }
      builder.append(
        NodeKind::LitSeq {
          open,
          close,
          items: Exprs { items: new_items.into_boxed_slice(), seps: items.seps.clone() },
        },
        loc,
      )
    }
    NodeKind::LitRec { open, close, items } => {
      let new_items: Vec<AstId> = items.items.iter()
        .map(|&i| rewrite_wildcard_call(builder, src, i))
        .collect();
      if new_items.iter().zip(items.items.iter()).all(|(a, b)| a == b) {
        return id;
      }
      builder.append(
        NodeKind::LitRec {
          open,
          close,
          items: Exprs { items: new_items.into_boxed_slice(), seps: items.seps.clone() },
        },
        loc,
      )
    }
    NodeKind::StrTempl { open, close, children } => {
      let new_children: Vec<AstId> = children.iter()
        .map(|&i| rewrite_wildcard_call(builder, src, i))
        .collect();
      if new_children.iter().zip(children.iter()).all(|(a, b)| a == b) {
        return id;
      }
      builder.append(
        NodeKind::StrTempl { open, close, children: new_children.into_boxed_slice() },
        loc,
      )
    }
    NodeKind::StrRawTempl { open, close, children } => {
      let new_children: Vec<AstId> = children.iter()
        .map(|&i| rewrite_wildcard_call(builder, src, i))
        .collect();
      if new_children.iter().zip(children.iter()).all(|(a, b)| a == b) {
        return id;
      }
      builder.append(
        NodeKind::StrRawTempl { open, close, children: new_children.into_boxed_slice() },
        loc,
      )
    }
    NodeKind::Spread { op, inner } => {
      let new_inner = inner.map(|i| rewrite_wildcard_call(builder, src, i));
      if new_inner == inner { id } else {
        builder.append(NodeKind::Spread { op, inner: new_inner }, loc)
      }
    }
    NodeKind::Match { subjects, sep, arms } => {
      let new_subjects: Vec<AstId> = subjects.items.iter()
        .map(|&i| rewrite_wildcard_call(builder, src, i))
        .collect();
      let new_arms: Vec<AstId> = arms.items.iter()
        .map(|&i| rewrite_wildcard_call(builder, src, i))
        .collect();
      let subjects_unchanged = new_subjects.iter().zip(subjects.items.iter()).all(|(a, b)| a == b);
      let arms_unchanged = new_arms.iter().zip(arms.items.iter()).all(|(a, b)| a == b);
      if subjects_unchanged && arms_unchanged { return id; }
      builder.append(
        NodeKind::Match {
          subjects: Exprs { items: new_subjects.into_boxed_slice(), seps: subjects.seps.clone() },
          sep,
          arms: Exprs { items: new_arms.into_boxed_slice(), seps: arms.seps.clone() },
        },
        loc,
      )
    }
    NodeKind::Try(inner) => {
      let new_inner = rewrite_wildcard_call(builder, src, inner);
      if new_inner == inner { id } else {
        builder.append(NodeKind::Try(new_inner), loc)
      }
    }
    NodeKind::Block { name, params, sep, body } => {
      let new_items: Vec<AstId> = body.items.iter()
        .map(|&i| rewrite_wildcard_call(builder, src, i))
        .collect();
      if new_items.iter().zip(body.items.iter()).all(|(a, b)| a == b) {
        return id;
      }
      builder.append(
        NodeKind::Block {
          name,
          params,
          sep,
          body: Exprs { items: new_items.into_boxed_slice(), seps: body.seps.clone() },
        },
        loc,
      )
    }
    NodeKind::ChainedCmp(parts) => {
      let mut any_changed = false;
      let new_parts: Vec<CmpPart<'src>> = parts.iter().map(|p| match p {
        CmpPart::Operand(n) => {
          let new_n = rewrite_wildcard_call(builder, src, *n);
          if new_n != *n { any_changed = true; }
          CmpPart::Operand(new_n)
        }
        CmpPart::Op(op) => CmpPart::Op(*op),
      }).collect();
      if !any_changed { id } else {
        builder.append(NodeKind::ChainedCmp(new_parts.into_boxed_slice()), loc)
      }
    }
    _ => id,
  }
}

// --- helpers ---

/// Returns true if the node tree rooted at `id` contains any Partial node
/// (not crossing `Group` scope boundaries).
fn has_partial(ast: &Ast<'_>, id: AstId) -> bool {
  match &ast.nodes.get(id).kind {
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

    NodeKind::Module { exprs: items, .. }
    | NodeKind::LitSeq { items, .. } | NodeKind::LitRec { items, .. } => {
      items.items.iter().any(|&id| has_partial(ast, id))
    }
    NodeKind::StrTempl { children, .. } | NodeKind::StrRawTempl { children, .. } => {
      children.iter().any(|&id| has_partial(ast, id))
    }
    NodeKind::UnaryOp { operand, .. } => has_partial(ast, *operand),
    NodeKind::InfixOp { lhs, rhs, .. } => has_partial(ast, *lhs) || has_partial(ast, *rhs),
    NodeKind::ChainedCmp(parts) => parts.iter().any(|p| match p {
      CmpPart::Operand(n) => has_partial(ast, *n),
      CmpPart::Op(_) => false,
    }),
    NodeKind::Spread { inner, .. } => inner.is_some_and(|id| has_partial(ast, id)),
    NodeKind::Member { lhs, rhs, .. } => {
      // Member rhs may be Group (computed key) — look through it; it's not a scope boundary here
      let rhs_inner = match &ast.nodes.get(*rhs).kind {
        NodeKind::Group { inner, .. } => *inner,
        _ => *rhs,
      };
      has_partial(ast, *lhs) || has_partial(ast, rhs_inner)
    }
    NodeKind::Bind { rhs, .. } => has_partial(ast, *rhs),      // lhs is pattern — skip
    NodeKind::BindRight { lhs, .. } => has_partial(ast, *lhs), // rhs is pattern — skip
    NodeKind::Apply { func, args } => {
      has_partial(ast, *func) || args.items.iter().any(|&id| has_partial(ast, id))
    }
    NodeKind::Pipe(_) => false, // Pipe children are independent segments
    NodeKind::Fn { params, body, .. } => {
      has_partial(ast, *params) || body.items.iter().any(|&id| has_partial(ast, id))
    }
    NodeKind::Patterns(children) => children.items.iter().any(|&id| has_partial(ast, id)),
    NodeKind::Match { subjects, arms, .. } => {
      subjects.items.iter().any(|&id| has_partial(ast, id))
        || arms.items.iter().any(|&id| has_partial(ast, id))
    }
    NodeKind::Arm { lhs, body, .. } => {
      has_partial(ast, *lhs) || body.items.iter().any(|&id| has_partial(ast, id))
    }
    NodeKind::Block { name, params, body, .. } => {
      has_partial(ast, *name) || has_partial(ast, *params)
        || body.items.iter().any(|&id| has_partial(ast, id))
    }
    NodeKind::Try(inner) => has_partial(ast, *inner),
  }
}

/// Replace all Partial nodes in the tree at `id` with `SynthIdent(synth_id)`.
/// Returns a fresh AstId for every subtree that contained at least one
/// Partial; returns the input id unchanged for subtrees with none.
///
/// Does NOT descend into Group boundaries (those are handled by the
/// caller in `transform_stmt`).
fn replace_partial<'src>(
  builder: &mut AstBuilder<'src>,
  src: &Ast<'src>,
  id: AstId,
  synth_id: u32,
) -> AstId {
  // Fast path: nothing to replace — return the id untouched.
  if !has_partial(src, id) {
    return id;
  }
  let node = src.nodes.get(id);
  let loc = node.loc;
  let kind = node.kind.clone();
  let new_kind: NodeKind<'src> = match kind {
    // The ?'s id is the original — we append a fresh SynthIdent at a new slot.
    NodeKind::Partial => NodeKind::SynthIdent(synth_id),

    // Leaves can't contain Partial, so has_partial short-circuits above.
    // Group is a boundary — skipped by has_partial.

    NodeKind::LitSeq { open, close, items } => {
      NodeKind::LitSeq { open, close, items: replace_exprs(builder, src, &items, synth_id) }
    }
    NodeKind::LitRec { open, close, items } => {
      NodeKind::LitRec { open, close, items: replace_exprs(builder, src, &items, synth_id) }
    }
    NodeKind::StrTempl { open, close, children } => {
      NodeKind::StrTempl { open, close, children: replace_ids(builder, src, &children, synth_id) }
    }
    NodeKind::StrRawTempl { open, close, children } => {
      NodeKind::StrRawTempl { open, close, children: replace_ids(builder, src, &children, synth_id) }
    }
    NodeKind::UnaryOp { op, operand } => {
      NodeKind::UnaryOp { op, operand: replace_partial(builder, src, operand, synth_id) }
    }
    NodeKind::InfixOp { op, lhs, rhs } => {
      NodeKind::InfixOp {
        op,
        lhs: replace_partial(builder, src, lhs, synth_id),
        rhs: replace_partial(builder, src, rhs, synth_id),
      }
    }
    NodeKind::ChainedCmp(parts) => {
      let new_parts: Box<[CmpPart<'src>]> = parts.iter().map(|p| match p {
        CmpPart::Operand(n) => CmpPart::Operand(replace_partial(builder, src, *n, synth_id)),
        CmpPart::Op(op) => CmpPart::Op(*op),
      }).collect::<Vec<_>>().into_boxed_slice();
      NodeKind::ChainedCmp(new_parts)
    }
    NodeKind::Spread { op, inner } => {
      NodeKind::Spread {
        op,
        inner: inner.map(|id| replace_partial(builder, src, id, synth_id)),
      }
    }
    NodeKind::Member { op, lhs, rhs } => {
      let new_lhs = replace_partial(builder, src, lhs, synth_id);
      // Look through Group for computed keys — it's not a boundary here.
      let new_rhs = match &src.nodes.get(rhs).kind {
        NodeKind::Group { open, close, inner } => {
          let open = *open;
          let close = *close;
          let inner = *inner;
          let rhs_loc = src.nodes.get(rhs).loc;
          let new_inner = replace_partial(builder, src, inner, synth_id);
          if new_inner == inner {
            rhs  // unchanged — reuse the original Group id
          } else {
            builder.append(NodeKind::Group { open, close, inner: new_inner }, rhs_loc)
          }
        }
        _ => replace_partial(builder, src, rhs, synth_id),
      };
      NodeKind::Member { op, lhs: new_lhs, rhs: new_rhs }
    }
    NodeKind::Apply { func, args } => {
      NodeKind::Apply {
        func: replace_partial(builder, src, func, synth_id),
        args: replace_exprs(builder, src, &args, synth_id),
      }
    }
    NodeKind::Bind { op, lhs, rhs } => {
      // lhs is a pattern — skip
      NodeKind::Bind { op, lhs, rhs: replace_partial(builder, src, rhs, synth_id) }
    }
    NodeKind::BindRight { op, lhs, rhs } => {
      // rhs is a pattern — skip
      NodeKind::BindRight { op, lhs: replace_partial(builder, src, lhs, synth_id), rhs }
    }
    NodeKind::Arm { lhs, sep, body } => {
      NodeKind::Arm { lhs, sep, body: replace_exprs(builder, src, &body, synth_id) }
    }

    // Anything else: original kind, original id (shouldn't reach here
    // for subtrees with Partial thanks to has_partial's coverage).
    other => other,
  };
  builder.append(new_kind, loc)
}

fn replace_ids<'src>(
  builder: &mut AstBuilder<'src>,
  src: &Ast<'src>,
  ids: &[AstId],
  synth_id: u32,
) -> Box<[AstId]> {
  ids.iter().map(|&id| replace_partial(builder, src, id, synth_id)).collect::<Vec<_>>().into_boxed_slice()
}

fn replace_exprs<'src>(
  builder: &mut AstBuilder<'src>,
  src: &Ast<'src>,
  exprs: &Exprs<'src>,
  synth_id: u32,
) -> Exprs<'src> {
  Exprs {
    items: replace_ids(builder, src, &exprs.items, synth_id),
    seps: exprs.seps.clone(),
  }
}

/// Find the loc of the first Partial node in the tree rooted at `id`.
fn partial_loc(ast: &Ast<'_>, id: AstId) -> Loc {
  let node = ast.nodes.get(id);
  if matches!(node.kind, NodeKind::Partial) { return node.loc; }
  let mut loc = node.loc;
  crate::ast::walk(ast, id, &mut |_cid, n| {
    if matches!(n.kind, NodeKind::Partial) { loc = n.loc; }
  });
  loc
}

/// Wrap an expression in `fn $: expr` if it contains Partial nodes.
/// Returns the (possibly new) AstId.
fn wrap_if_partial<'src>(
  builder: &mut AstBuilder<'src>,
  src: &Ast<'src>,
  id: AstId,
  synth_counter: &mut u32,
) -> AstId {
  if !has_partial(src, id) {
    return id;
  }
  let synth_id = *synth_counter;
  *synth_counter += 1;
  let param_loc = partial_loc(src, id);
  let body_id = replace_partial(builder, src, id, synth_id);
  // The body's loc: for a fresh replacement, read it from the builder;
  // for an untouched body (replace_partial returned the input id), read
  // it from src.
  let body_loc = if body_id == id {
    src.nodes.get(id).loc
  } else {
    builder.read(body_id).loc
  };

  let param = builder.append(NodeKind::SynthIdent(synth_id), param_loc);
  let patterns = builder.append(
    NodeKind::Patterns(Exprs { items: Box::new([param]), seps: vec![] }),
    param_loc,
  );
  let sep = Token { kind: crate::lexer::TokenKind::Colon, loc: param_loc, src: ":" };
  builder.append(
    NodeKind::Fn {
      params: patterns,
      sep,
      body: Exprs { items: Box::new([body_id]), seps: vec![] },
    },
    body_loc,
  )
}

/// True if both Exprs have identical item id sequences. Used by the
/// transform short-circuit to avoid appending a fresh parent when no
/// child changed.
fn exprs_unchanged<'src>(a: &Exprs<'src>, b: &Exprs<'src>) -> bool {
  a.items.len() == b.items.len()
    && a.items.iter().zip(b.items.iter()).all(|(x, y)| x == y)
}

// --- transformer ---

struct PartialPass {
  synth_counter: u32,
}

impl PartialPass {
  /// Transform a statement — top-level scope boundary.
  /// Wraps in Fn if any Partial remains after processing inner scope boundaries.
  fn transform_stmt<'src>(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    id: AstId,
  ) -> TransformResult {
    let node = src.nodes.get(id);
    let loc = node.loc;
    let kind = node.kind.clone();
    match kind {
      // Bind: only wrap RHS, never the whole Bind
      NodeKind::Bind { op, lhs, rhs } => {
        let new_rhs = self.transform_stmt(builder, src, rhs)?;
        if new_rhs == rhs {
          Ok(id)
        } else {
          Ok(builder.append(NodeKind::Bind { op, lhs, rhs: new_rhs }, loc))
        }
      }

      // BindRight: only wrap LHS value, never the whole BindRight
      NodeKind::BindRight { op, lhs, rhs } => {
        let new_lhs = self.transform_stmt(builder, src, lhs)?;
        if new_lhs == lhs {
          Ok(id)
        } else {
          Ok(builder.append(NodeKind::BindRight { op, lhs: new_lhs, rhs }, loc))
        }
      }

      // Arm: body stmts are independent scopes, lhs is pattern (skip)
      NodeKind::Arm { lhs, sep, body } => {
        let new_body = self.transform_body(builder, src, &body)?;
        // Fast path: if every body item id is unchanged, the Arm is unchanged.
        if exprs_unchanged(&body, &new_body) {
          Ok(id)
        } else {
          Ok(builder.append(NodeKind::Arm { lhs, sep, body: new_body }, loc))
        }
      }

      // Group: explicit scope boundary — process inner as independent stmt.
      // When ? is present, the inner gets wrapped in a fn (replacing the scope),
      // so strip the Group. Otherwise preserve it for downstream passes (CPS scoping).
      NodeKind::Group { open, close, inner } => {
        if has_partial(src, inner) {
          self.transform_stmt(builder, src, inner)
        } else {
          let new_inner = self.transform_stmt(builder, src, inner)?;
          if new_inner == inner {
            Ok(id)
          } else {
            Ok(builder.append(NodeKind::Group { open, close, inner: new_inner }, loc))
          }
        }
      }

      // Pipe: each segment is an independent scope
      NodeKind::Pipe(exprs) => {
        let mut new_items: Vec<AstId> = Vec::with_capacity(exprs.items.len());
        let mut any_changed = false;
        for &child_id in exprs.items.iter() {
          let new_child = self.transform_stmt(builder, src, child_id)?;
          if new_child != child_id { any_changed = true; }
          new_items.push(new_child);
        }
        if !any_changed {
          Ok(id)
        } else {
          Ok(builder.append(
            NodeKind::Pipe(Exprs {
              items: new_items.into_boxed_slice(),
              seps: exprs.seps.clone(),
            }),
            loc,
          ))
        }
      }

      // Fn: body stmts are independent scopes (like Module/Arm)
      NodeKind::Fn { sep, params, body } => {
        let new_body = self.transform_body(builder, src, &body)?;
        if exprs_unchanged(&body, &new_body) {
          Ok(id)
        } else {
          Ok(builder.append(NodeKind::Fn { sep, params, body: new_body }, loc))
        }
      }

      // Module: recurse into each expression as independent scope
      NodeKind::Module { exprs, url } => {
        let new_body = self.transform_body(builder, src, &exprs)?;
        if exprs_unchanged(&exprs, &new_body) {
          Ok(id)
        } else {
          Ok(builder.append(NodeKind::Module { exprs: new_body, url }, loc))
        }
      }

      // Everything else: recurse into children (processing inner Group/Pipe boundaries),
      // then wrap in Fn if any Partial remains.
      _ => {
        if !has_partial(src, id) {
          // No partials — return unchanged.
          return Ok(id);
        }
        let rewritten = self.transform(builder, src, id)?;
        // After the generic transform, the result may still contain Partial
        // sites that need wrapping in a synthetic Fn at this scope boundary.
        // But the Transform trait doesn't know about scope boundaries — our
        // transform_group override handles inner Groups, and everything else
        // flows up to here where we wrap if needed.
        //
        // We pass `src` with the original ids; `wrap_if_partial` walks the
        // CURRENT view (which may be the rewritten subtree if transform
        // appended new nodes, or the original subtree). For correctness we
        // want to wrap the rewritten version. Since the builder has grown,
        // we need a way to look up the rewritten node. Easiest: take a
        // snapshot of the current builder state as a view.
        //
        // Simpler: the rewritten id is either `id` (no change) or a fresh
        // appended id. Wrapping that id works against the current builder's
        // internal arena, which for reads must go through the read method.
        Ok(wrap_if_partial_on_builder(builder, src, rewritten, &mut self.synth_counter))
      }
    }
  }

  fn transform_body<'src>(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    body: &Exprs<'src>,
  ) -> Result<Exprs<'src>, TransformError> {
    let mut items: Vec<AstId> = Vec::with_capacity(body.items.len());
    for &id in body.items.iter() {
      items.push(self.transform_stmt(builder, src, id)?);
    }
    Ok(Exprs { items: items.into_boxed_slice(), seps: body.seps.clone() })
  }
}

/// Variant of `wrap_if_partial` that reads through the builder instead of
/// the immutable `src` snapshot. Used when the rewritten expression is
/// freshly appended and only visible in the builder.
fn wrap_if_partial_on_builder<'src>(
  builder: &mut AstBuilder<'src>,
  src: &Ast<'src>,
  id: AstId,
  synth_counter: &mut u32,
) -> AstId {
  // If the id came from src (no change), we can use the cheap src-based path.
  if (usize::from(id)) < src.nodes.len() {
    return wrap_if_partial(builder, src, id, synth_counter);
  }
  // Otherwise the id was freshly appended — we need to look at the
  // builder. Since builder is append-only, we can read it directly.
  // For correctness we'll just skip the partial check here: if
  // transform_stmt chose to dispatch through the generic Transform, it's
  // because has_partial was true, and the rewrite preserved the Partial
  // sites by appending new nodes. In the common case those Partial sites
  // still need wrapping. Delegate to a small inline walker.
  if !has_partial_builder(builder, id) {
    return id;
  }
  let synth_id = *synth_counter;
  *synth_counter += 1;
  let param_loc = builder.read(id).loc;
  let body_id = replace_partial_builder(builder, id, synth_id);
  let body_loc = builder.read(body_id).loc;
  let param = builder.append(NodeKind::SynthIdent(synth_id), param_loc);
  let patterns = builder.append(
    NodeKind::Patterns(Exprs { items: Box::new([param]), seps: vec![] }),
    param_loc,
  );
  let sep = Token { kind: crate::lexer::TokenKind::Colon, loc: param_loc, src: ":" };
  builder.append(
    NodeKind::Fn {
      params: patterns,
      sep,
      body: Exprs { items: Box::new([body_id]), seps: vec![] },
    },
    body_loc,
  )
}

/// Builder-backed version of `has_partial`. Reads through the current
/// arena state (which includes freshly appended nodes).
fn has_partial_builder(builder: &AstBuilder<'_>, id: AstId) -> bool {
  let node = builder.read(id);
  match &node.kind {
    NodeKind::Partial => true,
    NodeKind::LitBool(_) | NodeKind::LitInt(_) | NodeKind::LitFloat(_)
    | NodeKind::LitDecimal(_) | NodeKind::LitStr { .. } | NodeKind::Ident(_)
    | NodeKind::SynthIdent(_) | NodeKind::Wildcard | NodeKind::Token(_) => false,
    NodeKind::Group { .. } => false,
    NodeKind::Module { exprs: items, .. }
    | NodeKind::LitSeq { items, .. } | NodeKind::LitRec { items, .. } => {
      items.items.iter().any(|&id| has_partial_builder(builder, id))
    }
    NodeKind::StrTempl { children, .. } | NodeKind::StrRawTempl { children, .. } => {
      children.iter().any(|&id| has_partial_builder(builder, id))
    }
    NodeKind::UnaryOp { operand, .. } => has_partial_builder(builder, *operand),
    NodeKind::InfixOp { lhs, rhs, .. } =>
      has_partial_builder(builder, *lhs) || has_partial_builder(builder, *rhs),
    NodeKind::ChainedCmp(parts) => parts.iter().any(|p| match p {
      CmpPart::Operand(n) => has_partial_builder(builder, *n),
      CmpPart::Op(_) => false,
    }),
    NodeKind::Spread { inner, .. } => inner.is_some_and(|id| has_partial_builder(builder, id)),
    NodeKind::Member { lhs, rhs, .. } => {
      let rhs_inner = match &builder.read(*rhs).kind {
        NodeKind::Group { inner, .. } => *inner,
        _ => *rhs,
      };
      has_partial_builder(builder, *lhs) || has_partial_builder(builder, rhs_inner)
    }
    NodeKind::Bind { rhs, .. } => has_partial_builder(builder, *rhs),
    NodeKind::BindRight { lhs, .. } => has_partial_builder(builder, *lhs),
    NodeKind::Apply { func, args } => {
      has_partial_builder(builder, *func) || args.items.iter().any(|&id| has_partial_builder(builder, id))
    }
    NodeKind::Pipe(_) => false,
    NodeKind::Fn { params, body, .. } => {
      has_partial_builder(builder, *params) || body.items.iter().any(|&id| has_partial_builder(builder, id))
    }
    NodeKind::Patterns(children) => children.items.iter().any(|&id| has_partial_builder(builder, id)),
    NodeKind::Match { subjects, arms, .. } => {
      subjects.items.iter().any(|&id| has_partial_builder(builder, id))
        || arms.items.iter().any(|&id| has_partial_builder(builder, id))
    }
    NodeKind::Arm { lhs, body, .. } => {
      has_partial_builder(builder, *lhs) || body.items.iter().any(|&id| has_partial_builder(builder, id))
    }
    NodeKind::Block { name, params, body, .. } => {
      has_partial_builder(builder, *name) || has_partial_builder(builder, *params)
        || body.items.iter().any(|&id| has_partial_builder(builder, id))
    }
    NodeKind::Try(inner) => has_partial_builder(builder, *inner),
  }
}

/// Builder-backed version of `replace_partial`. Simpler than the src
/// variant because we can always append — no snapshot needed.
fn replace_partial_builder<'src>(
  builder: &mut AstBuilder<'src>,
  id: AstId,
  synth_id: u32,
) -> AstId {
  if !has_partial_builder(builder, id) {
    return id;
  }
  let node = builder.read(id);
  let loc = node.loc;
  let kind = node.kind.clone();
  let new_kind: NodeKind<'src> = match kind {
    NodeKind::Partial => NodeKind::SynthIdent(synth_id),
    NodeKind::LitSeq { open, close, items } => {
      NodeKind::LitSeq { open, close, items: replace_exprs_builder(builder, &items, synth_id) }
    }
    NodeKind::LitRec { open, close, items } => {
      NodeKind::LitRec { open, close, items: replace_exprs_builder(builder, &items, synth_id) }
    }
    NodeKind::StrTempl { open, close, children } => {
      NodeKind::StrTempl { open, close, children: replace_ids_builder(builder, &children, synth_id) }
    }
    NodeKind::StrRawTempl { open, close, children } => {
      NodeKind::StrRawTempl { open, close, children: replace_ids_builder(builder, &children, synth_id) }
    }
    NodeKind::UnaryOp { op, operand } => {
      NodeKind::UnaryOp { op, operand: replace_partial_builder(builder, operand, synth_id) }
    }
    NodeKind::InfixOp { op, lhs, rhs } => {
      NodeKind::InfixOp {
        op,
        lhs: replace_partial_builder(builder, lhs, synth_id),
        rhs: replace_partial_builder(builder, rhs, synth_id),
      }
    }
    NodeKind::ChainedCmp(parts) => {
      let new_parts: Box<[CmpPart<'src>]> = parts.iter().map(|p| match p {
        CmpPart::Operand(n) => CmpPart::Operand(replace_partial_builder(builder, *n, synth_id)),
        CmpPart::Op(op) => CmpPart::Op(*op),
      }).collect::<Vec<_>>().into_boxed_slice();
      NodeKind::ChainedCmp(new_parts)
    }
    NodeKind::Spread { op, inner } => {
      NodeKind::Spread {
        op,
        inner: inner.map(|id| replace_partial_builder(builder, id, synth_id)),
      }
    }
    NodeKind::Member { op, lhs, rhs } => {
      let new_lhs = replace_partial_builder(builder, lhs, synth_id);
      let new_rhs = match &builder.read(rhs).kind {
        NodeKind::Group { open, close, inner } => {
          let open = *open;
          let close = *close;
          let inner = *inner;
          let rhs_loc = builder.read(rhs).loc;
          let new_inner = replace_partial_builder(builder, inner, synth_id);
          if new_inner == inner {
            rhs
          } else {
            builder.append(NodeKind::Group { open, close, inner: new_inner }, rhs_loc)
          }
        }
        _ => replace_partial_builder(builder, rhs, synth_id),
      };
      NodeKind::Member { op, lhs: new_lhs, rhs: new_rhs }
    }
    NodeKind::Apply { func, args } => {
      NodeKind::Apply {
        func: replace_partial_builder(builder, func, synth_id),
        args: replace_exprs_builder(builder, &args, synth_id),
      }
    }
    NodeKind::Bind { op, lhs, rhs } => {
      NodeKind::Bind { op, lhs, rhs: replace_partial_builder(builder, rhs, synth_id) }
    }
    NodeKind::BindRight { op, lhs, rhs } => {
      NodeKind::BindRight { op, lhs: replace_partial_builder(builder, lhs, synth_id), rhs }
    }
    NodeKind::Arm { lhs, sep, body } => {
      NodeKind::Arm { lhs, sep, body: replace_exprs_builder(builder, &body, synth_id) }
    }
    other => other,
  };
  builder.append(new_kind, loc)
}

fn replace_ids_builder<'src>(
  builder: &mut AstBuilder<'src>,
  ids: &[AstId],
  synth_id: u32,
) -> Box<[AstId]> {
  ids.iter().map(|&id| replace_partial_builder(builder, id, synth_id))
    .collect::<Vec<_>>().into_boxed_slice()
}

fn replace_exprs_builder<'src>(
  builder: &mut AstBuilder<'src>,
  exprs: &Exprs<'src>,
  synth_id: u32,
) -> Exprs<'src> {
  Exprs {
    items: replace_ids_builder(builder, &exprs.items, synth_id),
    seps: exprs.seps.clone(),
  }
}

// --- Transform trait impl ---

impl<'src> Transform<'src> for PartialPass {
  // Scope boundaries (Group, Pipe, Bind, BindRight, Arm) are handled in transform_stmt.
  // The default `transform` method dispatches Group to this method, so we need to
  // short-circuit back into transform_stmt for Group's inner.
  fn transform_group(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    _open: Token<'src>,
    _close: Token<'src>,
    inner: AstId,
    _loc: Loc,
  ) -> TransformResult {
    self.transform_stmt(builder, src, inner)
  }

  // Member rhs Group (computed key) is transparent — don't create a scope boundary for it.
  fn transform_member(
    &mut self,
    builder: &mut AstBuilder<'src>,
    src: &Ast<'src>,
    op: Token<'src>,
    lhs: AstId,
    rhs: AstId,
    loc: Loc,
  ) -> TransformResult {
    let new_lhs = self.transform(builder, src, lhs)?;
    let new_rhs = match &src.nodes.get(rhs).kind {
      NodeKind::Group { open, close, inner } => {
        let open = *open;
        let close = *close;
        let inner = *inner;
        let rhs_loc = src.nodes.get(rhs).loc;
        let new_inner = self.transform(builder, src, inner)?;
        builder.append(NodeKind::Group { open, close, inner: new_inner }, rhs_loc)
      }
      _ => self.transform(builder, src, rhs)?,
    };
    Ok(builder.append(NodeKind::Member { op, lhs: new_lhs, rhs: new_rhs }, loc))
  }
}

// --- test runner ---

#[cfg(test)]
mod tests {
  fn partial(src: &str) -> String {
    use crate::ast::NodeKind;
    match crate::parser::parse(src, "test") {
      Err(e) => format!("PARSE ERROR: {}", e.message),
      Ok(ast) => {
        let before = ast.print();
        match super::apply(ast) {
          Ok(new_ast) => {
            let after = new_ast.print();
            if before == after {
              return "No Change".to_string();
            }
            // For a single-stmt module, print just the stmt to match the
            // old pre-flatten test expectations (which didn't include
            // the Module wrapper).
            let root = new_ast.nodes.get(new_ast.root);
            if let NodeKind::Module { exprs, .. } = &root.kind
              && exprs.items.len() == 1 {
                return new_ast.print_subtree(exprs.items[0]);
              }
            after
          }
          Err(e) => format!("ERROR: {}", e.message),
        }
      }
    }
  }

  test_macros::include_fink_tests!("src/passes/partial/test_partial.fnk");
}
