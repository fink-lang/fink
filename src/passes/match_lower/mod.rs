// Match lowering pass.
//
// Converts match constructs (MatchBlock, MatchArm, and all Match* primitives)
// into plain App calls with BuiltIn functions and inline Cont::Expr.
//
// After this pass, the IR contains only core CPS constructs:
//   App, LetVal, LetFn, LetRec, If, Yield, Panic
//
// This pass runs after name resolution and before cont_lifting:
//   CPS transform → name_res → match_lower → cont_lifting → closure_lifting
//
// By converting match constructs to plain App + Cont, cont_lifting can
// hoist all continuations uniformly without special-casing match nodes.

use crate::ast::AstId;
use crate::passes::cps::ir::{
  Arg, BuiltIn, Callable, Cont, CpsId, CpsResult,
  Expr, ExprKind, Lit, Val, ValKind,
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

  fn val<'src>(&mut self, kind: ValKind<'src>, origin: Option<AstId>) -> Val<'src> {
    let id = self.next(origin);
    Val { id, kind }
  }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Lower all match constructs into plain App calls with BuiltIn functions.
/// Returns a new CpsResult with the rewritten IR.
pub fn lower<'src>(result: CpsResult<'src>) -> CpsResult<'src> {
  let mut alloc = Alloc::new(result.origin);
  let new_root = lower_expr(result.root, &mut alloc);
  CpsResult { root: new_root, origin: alloc.origin }
}

// ---------------------------------------------------------------------------
// Transform
// ---------------------------------------------------------------------------

fn lower_expr<'src>(expr: Expr<'src>, alloc: &mut Alloc) -> Expr<'src> {
  use ExprKind::*;
  match expr.kind {
    // Core CPS — recurse into sub-expressions and conts.
    App { func, args } => {
      let args = args.into_iter().map(|a| match a {
        Arg::Cont(c) => Arg::Cont(lower_cont(c, alloc)),
        other => other,
      }).collect();
      Expr { id: expr.id, kind: App { func, args } }
    }

    LetVal { name, val, body } => {
      let body = lower_cont(body, alloc);
      Expr { id: expr.id, kind: LetVal { name, val, body } }
    }

    LetFn { name, params, cont, fn_body, body } => {
      let fn_body = lower_expr(*fn_body, alloc);
      let body = lower_cont(body, alloc);
      Expr { id: expr.id, kind: LetFn { name, params, cont, fn_body: Box::new(fn_body), body } }
    }

    LetRec { bindings, body } => {
      let body = lower_cont(body, alloc);
      Expr { id: expr.id, kind: LetRec { bindings, body } }
    }

    If { cond, then, else_ } => {
      let then  = lower_expr(*then, alloc);
      let else_ = lower_expr(*else_, alloc);
      Expr { id: expr.id, kind: If { cond, then: Box::new(then), else_: Box::new(else_) } }
    }

    Yield { value, cont } => {
      let cont = lower_cont(cont, alloc);
      Expr { id: expr.id, kind: Yield { value, cont } }
    }

    Panic | FailCont | FailRef(_) => expr,

    // -----------------------------------------------------------------------
    // Match primitives → App with BuiltIn
    // -----------------------------------------------------------------------

    // MatchLetVal → plain LetVal (drop fail).
    MatchLetVal { name, val, body, .. } => {
      let body = lower_cont(body, alloc);
      Expr { id: expr.id, kind: LetVal { name, val, body } }
    }

    // MatchValue { val, lit, fail, body } → App { MatchValue, [val, lit, fail, body] }
    MatchValue { val, lit, fail, body } => {
      let fail_val = fail_to_val(*fail, alloc);
      let lit_val = alloc.val(ValKind::Lit(lit), None);
      let body = lower_cont(body, alloc);
      Expr { id: expr.id, kind: App {
        func: Callable::BuiltIn(BuiltIn::MatchValue),
        args: vec![Arg::Val(*val), Arg::Val(lit_val), Arg::Val(fail_val), Arg::Cont(body)],
      }}
    }

    // MatchSeq { val, fail, body } → App { MatchSeq, [val, fail, body] }
    MatchSeq { val, fail, body } => {
      let fail_val = fail_to_val(*fail, alloc);
      let body = lower_cont(body, alloc);
      Expr { id: expr.id, kind: App {
        func: Callable::BuiltIn(BuiltIn::MatchSeq),
        args: vec![Arg::Val(*val), Arg::Val(fail_val), Arg::Cont(body)],
      }}
    }

    // MatchNext { val, fail, cont } → App { MatchNext, [val, fail, cont] }
    MatchNext { val, fail, cont } => {
      let fail_val = fail_to_val(*fail, alloc);
      let cont = lower_cont(cont, alloc);
      Expr { id: expr.id, kind: App {
        func: Callable::BuiltIn(BuiltIn::MatchNext),
        args: vec![Arg::Val(*val), Arg::Val(fail_val), Arg::Cont(cont)],
      }}
    }

    // MatchDone { val, fail, cont } → App { MatchDone, [val, fail, cont] }
    MatchDone { val, fail, cont } => {
      let fail_val = fail_to_val(*fail, alloc);
      let cont = lower_cont(cont, alloc);
      Expr { id: expr.id, kind: App {
        func: Callable::BuiltIn(BuiltIn::MatchDone),
        args: vec![Arg::Val(*val), Arg::Val(fail_val), Arg::Cont(cont)],
      }}
    }

    // MatchNotDone { val, fail, body } → App { MatchNotDone, [val, fail, body] }
    MatchNotDone { val, fail, body } => {
      let fail_val = fail_to_val(*fail, alloc);
      let body = lower_cont(body, alloc);
      Expr { id: expr.id, kind: App {
        func: Callable::BuiltIn(BuiltIn::MatchNotDone),
        args: vec![Arg::Val(*val), Arg::Val(fail_val), Arg::Cont(body)],
      }}
    }

    // MatchRest { val, fail, cont } → App { MatchRest, [val, fail, cont] }
    MatchRest { val, fail, cont } => {
      let fail_val = fail_to_val(*fail, alloc);
      let cont = lower_cont(cont, alloc);
      Expr { id: expr.id, kind: App {
        func: Callable::BuiltIn(BuiltIn::MatchRest),
        args: vec![Arg::Val(*val), Arg::Val(fail_val), Arg::Cont(cont)],
      }}
    }

    // MatchRec { val, fail, body } → App { MatchRec, [val, fail, body] }
    MatchRec { val, fail, body } => {
      let fail_val = fail_to_val(*fail, alloc);
      let body = lower_cont(body, alloc);
      Expr { id: expr.id, kind: App {
        func: Callable::BuiltIn(BuiltIn::MatchRec),
        args: vec![Arg::Val(*val), Arg::Val(fail_val), Arg::Cont(body)],
      }}
    }

    // MatchField { val, field, fail, cont } → App { MatchField, [val, field, fail, cont] }
    MatchField { val, field, fail, cont } => {
      let fail_val = fail_to_val(*fail, alloc);
      let field_val = alloc.val(ValKind::Lit(Lit::Str(field)), None);
      let cont = lower_cont(cont, alloc);
      Expr { id: expr.id, kind: App {
        func: Callable::BuiltIn(BuiltIn::MatchField),
        args: vec![Arg::Val(*val), Arg::Val(field_val), Arg::Val(fail_val), Arg::Cont(cont)],
      }}
    }

    // MatchIf { func, args, fail, body } → App { func, [...args, fail, body] }
    MatchIf { func, args, fail, body } => {
      let fail_val = fail_to_val(*fail, alloc);
      let mut new_args: Vec<Arg<'src>> = args.into_iter().map(Arg::Val).collect();
      new_args.push(Arg::Val(fail_val));
      let body = lower_cont(body, alloc);
      new_args.push(Arg::Cont(body));
      Expr { id: expr.id, kind: App {
        func,
        args: new_args,
      }}
    }

    // MatchApp { func, args, fail, cont } → App { func, [...args, fail, cont] }
    MatchApp { func, args, fail, cont } => {
      let fail_val = fail_to_val(*fail, alloc);
      let mut new_args: Vec<Arg<'src>> = args.into_iter().map(Arg::Val).collect();
      new_args.push(Arg::Val(fail_val));
      let cont = lower_cont(cont, alloc);
      new_args.push(Arg::Cont(cont));
      Expr { id: expr.id, kind: App {
        func,
        args: new_args,
      }}
    }

    // -----------------------------------------------------------------------
    // MatchArm / MatchBlock — structural match constructs
    // -----------------------------------------------------------------------

    // MatchArm { matcher, body } → App { MatchArm, [Cont(matcher), Cont(body)] }
    // Both conts become Arg::Cont entries. No result cont needed.
    MatchArm { matcher, body } => {
      let matcher = lower_cont(matcher, alloc);
      let body = lower_cont(body, alloc);
      Expr { id: expr.id, kind: App {
        func: Callable::BuiltIn(BuiltIn::MatchArm),
        args: vec![Arg::Cont(matcher), Arg::Cont(body)],
      }}
    }

    // MatchBlock { params, arm_params, arms, cont }
    // → App { MatchBlock, [..params, ..Expr(arms), cont] }
    // arm_params are dropped — they're implicit in the matcher cont params.
    MatchBlock { params, arm_params: _, arms, cont } => {
      let mut new_args: Vec<Arg<'src>> = params.into_iter().map(Arg::Val).collect();
      for arm in arms {
        let lowered = lower_expr(arm, alloc);
        new_args.push(Arg::Expr(Box::new(lowered)));
      }
      let cont = lower_cont(cont, alloc);
      new_args.push(Arg::Cont(cont));
      Expr { id: expr.id, kind: App {
        func: Callable::BuiltIn(BuiltIn::MatchBlock),
        args: new_args,
      }}
    }
  }
}

/// Recurse into a Cont, lowering any match constructs inside.
fn lower_cont<'src>(cont: Cont<'src>, alloc: &mut Alloc) -> Cont<'src> {
  match cont {
    Cont::Ref(_) => cont,
    Cont::Expr { args, body } => {
      let body = lower_expr(*body, alloc);
      Cont::Expr { args, body: Box::new(body) }
    }
  }
}

/// Convert a fail Expr (Panic/FailCont/FailRef) to a Val for use as an App arg.
fn fail_to_val<'src>(fail: Expr<'src>, alloc: &mut Alloc) -> Val<'src> {
  match fail.kind {
    ExprKind::Panic => alloc.val(ValKind::Panic, None),
    ExprKind::FailRef(id) => alloc.val(ValKind::ContRef(id), None),
    ExprKind::FailCont => {
      // FailCont should only appear inside MatchBlock arms — after lowering
      // the arm dispatch, it becomes an explicit ContRef. For now, preserve
      // as Panic (will be fixed when MatchBlock lowering is complete).
      alloc.val(ValKind::Panic, None)
    }
    _ => panic!("fail_to_val: unexpected fail expr {:?}", fail.kind),
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
  use super::lower;

  /// Run parse → CPS → match_lower and return the formatted CPS IR.
  fn match_lower(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let cps = lower_expr(&r.root);
        let lowered = lower(cps);
        let ctx = Ctx { origin: &lowered.origin, ast_index: &ast_index, captures: None };
        fmt_with(&lowered.root, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  include_fink_tests!("src/passes/match_lower/test_match_lower.fnk");
}
