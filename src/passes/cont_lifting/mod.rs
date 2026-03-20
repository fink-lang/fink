// Continuation lifting pass.
//
// Hoists all inline continuation bodies (Cont::Expr) into LetFn nodes so that
// every continuation is a named function by the time closure_lifting runs.
// closure_lifting requires named fns — it cannot hoist anonymous inline closures.
//
// Input:  CpsResult (after CPS transform)
// Output: CpsResult (all Cont::Expr bodies replaced by LetFn + Cont::Ref)
//
// Rewrite for each node with cont: Cont::Expr { arg, body }:
//
//   Before:
//     ·apply func, args, fn arg: <body>
//
//   After:
//     ·fn fn arg: <body>     ← new LetFn (Bind::Cont)
//       fn ·ƒ_N:
//         ·apply func, args, ·ƒ_N   ← Cont::Ref(·ƒ_N)
//
// All Cont::Expr bodies are hoisted unconditionally — including trivial ones and
// multi-arg conts (MatchNext/MatchField). closure_lifting needs every cont to be
// a named LetFn to thread captures.
//
// CPS transform contract:
//   1. Every new node gets a CpsId via the id allocator + origin entry.
//   2. Synthesized nodes carry None as AstId origin.
//   3. The output CpsResult.origin must be dense.
//   4. Produce a fresh tree — never mutate input in place.

use crate::ast::AstId;
use crate::passes::cps::ir::{
  Arg, Bind, BindNode, Cont, CpsId, CpsResult, Expr, ExprKind, Param,
};
use crate::propgraph::PropGraph;

// ---------------------------------------------------------------------------
// Id allocator
// ---------------------------------------------------------------------------

struct Alloc {
  origin: PropGraph<CpsId, Option<AstId>>,
}

impl Alloc {
  fn new(existing: PropGraph<CpsId, Option<AstId>>) -> Self {
    Alloc { origin: existing }
  }

  fn next(&mut self, origin: Option<AstId>) -> CpsId {
    self.origin.push(origin)
  }

  fn bind(&mut self, kind: Bind, origin: Option<AstId>) -> BindNode {
    let id = self.next(origin);
    BindNode { id, kind }
  }

  fn expr<'src>(&mut self, kind: ExprKind<'src>, origin: Option<AstId>) -> Expr<'src> {
    let id = self.next(origin);
    Expr { id, kind }
  }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Hoist all inline App continuation bodies into LetFn nodes.
/// Returns a new CpsResult with the rewritten IR.
pub fn lift<'src>(result: CpsResult<'src>) -> CpsResult<'src> {
  let mut alloc = Alloc::new(result.origin);
  let new_root = lift_expr(result.root, &mut alloc);
  CpsResult { root: new_root, origin: alloc.origin }
}

// ---------------------------------------------------------------------------
// Transform
// ---------------------------------------------------------------------------

fn lift_expr<'src>(expr: Expr<'src>, alloc: &mut Alloc) -> Expr<'src> {
  use ExprKind::*;
  match expr.kind {
    App { func, mut args } => {
      // Pop the last Arg::Cont and hoist it. The hoisted LetFn's body
      // contains the App with remaining args — lift_expr is called on it
      // inside hoist_cont, which recursively hoists the next Arg::Cont.
      // Arg::Expr entries are recursed into via lift_expr on the inner App.
      match args.iter().rposition(|a| matches!(a, Arg::Cont(_))) {
        Some(idx) => {
          let cont = match args.remove(idx) {
            Arg::Cont(c) => c,
            _ => unreachable!(),
          };
          hoist_cont(expr.id, cont, alloc, |cont| {
            args.insert(idx, Arg::Cont(cont));
            App { func, args }
          })
        }
        None => {
          // No conts — just recurse into Arg::Expr entries.
          let args = args.into_iter().map(|a| match a {
            Arg::Expr(e) => Arg::Expr(Box::new(lift_expr(*e, alloc))),
            other => other,
          }).collect();
          Expr { id: expr.id, kind: App { func, args } }
        }
      }
    }

    Yield { value, cont } =>
      hoist_cont(expr.id, cont, alloc, |cont| Yield { value, cont }),

    LetVal { name, val, body } => {
      let body = recurse_cont(body, alloc);
      Expr { id: expr.id, kind: LetVal { name, val, body } }
    }

    LetFn { name, params, cont, fn_body, body } => {
      let fn_body = lift_expr(*fn_body, alloc);
      let body    = recurse_cont(body, alloc);
      Expr { id: expr.id, kind: LetFn { name, params, cont, fn_body: Box::new(fn_body), body } }
    }


    If { cond, then, else_ } => {
      let then  = lift_expr(*then, alloc);
      let else_ = lift_expr(*else_, alloc);
      Expr { id: expr.id, kind: If { cond, then: Box::new(then), else_: Box::new(else_) } }
    }

  }
}

/// Recurse into a `Cont` without hoisting — for `body:` fields on `LetVal`/`LetFn`
/// where the continuation is lexical sequencing, not a call result closure.
fn recurse_cont<'src>(cont: Cont<'src>, alloc: &mut Alloc) -> Cont<'src> {
  match cont {
    Cont::Ref(_) => cont,
    Cont::Expr { args, body } => {
      let body = lift_expr(*body, alloc);
      Cont::Expr { args, body: Box::new(body) }
    }
  }
}

/// Hoist a `Cont::Expr` body into a `LetFn`, replacing it with `Cont::Ref`.
/// `make_kind` rebuilds the parent node's kind given the (possibly rewritten) cont.
///
/// If `cont` is `Cont::Ref` — return the node unchanged.
/// If `cont` is `Cont::Expr { arg, body }` — lift body, wrap parent in a LetFn:
///
///   LetFn { name: ·ƒ_N, params: [arg], fn_body: body,
///           body: Cont::Expr { <parent node with Cont::Ref(·ƒ_N)> } }
fn hoist_cont<'src, F>(
  node_id: CpsId,
  cont: Cont<'src>,
  alloc: &mut Alloc,
  make_kind: F,
) -> Expr<'src>
where
  F: FnOnce(Cont<'src>) -> ExprKind<'src>,
{
  match cont {
    Cont::Ref(_) => Expr { id: node_id, kind: make_kind(cont) },
    Cont::Expr { args, body } => {
      let body = lift_expr(*body, alloc);
      let cont_name        = alloc.bind(Bind::Cont, None);
      let inner_cont_param = alloc.bind(Bind::Cont, None);
      let inner_kind = make_kind(Cont::Ref(cont_name.id));
      let inner = lift_expr(alloc.expr(inner_kind, None), alloc);
      let params = args.into_iter().map(Param::Name).collect();
      Expr {
        id: node_id,
        kind: ExprKind::LetFn {
          name:    cont_name,
          params,
          cont:    inner_cont_param,
          fn_body: Box::new(body),
          body:    Cont::Expr { args: vec![alloc.bind(Bind::Synth, None)], body: Box::new(inner) },
        },
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use test_macros::include_fink_tests;

  use crate::ast::build_index;
  use crate::parser::parse;
  use crate::passes::cps::fmt::{fmt_with, Ctx};
  use crate::passes::cps::transform::lower_expr;
  use super::lift;

  /// Run parse → CPS → cont_lifting and return the formatted CPS IR.
  fn cont_lift(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let cps = lower_expr(&r.root);
        let lifted = lift(cps);
        let ctx = Ctx { origin: &lifted.origin, ast_index: &ast_index, captures: None };
        fmt_with(&lifted.root, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  include_fink_tests!("src/passes/cont_lifting/test_cont_lifting.fnk");
}
