// Continuation lifting pass.
//
// Hoists inline App continuation bodies into top-level LetFn nodes so that
// every continuation is a named function by the time closure_lifting runs.
//
// Input:  CpsResult (after CPS transform)
// Output: CpsResult (inline App bodies replaced by LetFn + Cont::Ref)
//
// Rewrite for each App { func, args, cont: Cont::Expr { arg, body } } where body is non-trivial:
//
//   Before:
//     ·apply func, args, fn arg: <non-trivial body>
//
//   After:
//     ·fn fn arg: <non-trivial body>     ← new LetFn (Bind::Cont)
//       fn ·ƒ_N:
//         ·apply func, args, ·ƒ_N        ← App with Cont::Ref(·ƒ_N)
//
// The new LetFn is left in place (not hoisted here). closure_lifting then treats it
// as a closure if its body captures anything, and hoists it to module top.
//
// Only App is handled for now — other continuation-carrying nodes (Yield, Match*)
// will be added when needed.
//
// CPS transform contract:
//   1. Every new node gets a CpsId via the id allocator + origin entry.
//   2. Synthesized nodes carry None as AstId origin.
//   3. The output CpsResult.origin must be dense.
//   4. Produce a fresh tree — never mutate input in place.

use crate::ast::AstId;
use crate::passes::cps::ir::{
  Bind, BindNode, Cont, CpsId, CpsResult, Expr, ExprKind, Param,
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
    // App — hoist non-trivial cont body into a LetFn.
    App { func, args, cont } => {
      match cont {
        Cont::Ref(_) => Expr { id: expr.id, kind: App { func, args, cont } },
        Cont::Expr { arg, body } => {
          let body = lift_expr(*body, alloc);
          if is_trivial_body(&body) {
            Expr { id: expr.id, kind: App { func, args, cont: Cont::Expr { arg, body: Box::new(body) } } }
          } else {
            // Hoist: wrap the body in a LetFn, replace App cont with Cont::Ref.
            let cont_name = alloc.bind(Bind::Cont, None);
            let inner_cont_param = alloc.bind(Bind::Cont, None);
            let inner_app = alloc.expr(
              App { func, args, cont: Cont::Ref(cont_name.id) },
              None,
            );
            Expr {
              id: expr.id,
              kind: LetFn {
                name:    cont_name,
                params:  vec![Param::Name(arg)],
                cont:    inner_cont_param,
                fn_body: Box::new(body),
                body:    Cont::Expr {
                  arg:  alloc.bind(Bind::Synth, None),
                  body: Box::new(inner_app),
                },
              },
            }
          }
        }
      }
    }

    // Pass-through for all other nodes — recurse into sub-expressions.
    LetVal { name, val, body } => {
      let body = lift_cont(body, alloc);
      Expr { id: expr.id, kind: LetVal { name, val, body } }
    }

    LetFn { name, params, cont, fn_body, body } => {
      let fn_body = lift_expr(*fn_body, alloc);
      let body    = lift_cont(body, alloc);
      Expr { id: expr.id, kind: LetFn { name, params, cont, fn_body: Box::new(fn_body), body } }
    }

    LetRec { bindings, body } => {
      let body = lift_cont(body, alloc);
      Expr { id: expr.id, kind: LetRec { bindings, body } }
    }

    If { cond, then, else_ } => {
      let then  = lift_expr(*then, alloc);
      let else_ = lift_expr(*else_, alloc);
      Expr { id: expr.id, kind: If { cond, then: Box::new(then), else_: Box::new(else_) } }
    }

    Yield { value, cont } => {
      let cont = lift_cont(cont, alloc);
      Expr { id: expr.id, kind: Yield { value, cont } }
    }

    // Terminals — no sub-expressions.
    Panic | FailCont => expr,

    MatchLetVal { name, val, fail, body } => {
      let body = lift_cont(body, alloc);
      Expr { id: expr.id, kind: MatchLetVal { name, val, fail, body } }
    }
    MatchApp { func, args, fail, cont } => {
      let cont = lift_cont(cont, alloc);
      Expr { id: expr.id, kind: MatchApp { func, args, fail, cont } }
    }
    MatchIf { func, args, fail, body } => {
      let body = lift_cont(body, alloc);
      Expr { id: expr.id, kind: MatchIf { func, args, fail, body } }
    }
    MatchValue { val, lit, fail, body } => {
      let body = lift_cont(body, alloc);
      Expr { id: expr.id, kind: MatchValue { val, lit, fail, body } }
    }
    MatchSeq { val, cursor, fail, body } => {
      let body = lift_cont(body, alloc);
      Expr { id: expr.id, kind: MatchSeq { val, cursor, fail, body } }
    }
    MatchNext { val, cursor, next_cursor, fail, cont } => {
      let cont = lift_cont(cont, alloc);
      Expr { id: expr.id, kind: MatchNext { val, cursor, next_cursor, fail, cont } }
    }
    MatchDone { val, cursor, fail, cont } => {
      let cont = lift_cont(cont, alloc);
      Expr { id: expr.id, kind: MatchDone { val, cursor, fail, cont } }
    }
    MatchNotDone { val, cursor, fail, body } => {
      let body = lift_cont(body, alloc);
      Expr { id: expr.id, kind: MatchNotDone { val, cursor, fail, body } }
    }
    MatchRest { val, cursor, fail, cont } => {
      let cont = lift_cont(cont, alloc);
      Expr { id: expr.id, kind: MatchRest { val, cursor, fail, cont } }
    }
    MatchRec { val, cursor, fail, body } => {
      let body = lift_cont(body, alloc);
      Expr { id: expr.id, kind: MatchRec { val, cursor, fail, body } }
    }
    MatchField { val, cursor, next_cursor, field, fail, cont } => {
      let cont = lift_cont(cont, alloc);
      Expr { id: expr.id, kind: MatchField { val, cursor, next_cursor, field, fail, cont } }
    }
    MatchBlock { params, fail, arm_params, arms, cont } => {
      let cont = lift_cont(cont, alloc);
      Expr { id: expr.id, kind: MatchBlock { params, fail, arm_params, arms, cont } }
    }
  }
}

/// Recurse into a `Cont`, lifting any inline body.
fn lift_cont<'src>(cont: Cont<'src>, alloc: &mut Alloc) -> Cont<'src> {
  match cont {
    Cont::Ref(_) => cont,
    Cont::Expr { arg, body } => {
      let body = lift_expr(*body, alloc);
      Cont::Expr { arg, body: Box::new(body) }
    }
  }
}

/// Returns true if `expr` is a trivial App continuation body —
/// one that codegen can handle without hoisting. A body is trivial if it
/// has no further App with a non-trivial cont (i.e. no chained calls).
fn is_trivial_body(expr: &Expr<'_>) -> bool {
  match &expr.kind {
    ExprKind::App { cont, .. } => matches!(cont, Cont::Ref(_)),
    ExprKind::LetVal { body, .. } | ExprKind::MatchLetVal { body, .. } => {
      matches!(body, Cont::Ref(_))
    }
    ExprKind::Panic | ExprKind::FailCont => true,
    _ => false,
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
