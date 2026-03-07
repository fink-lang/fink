// CPS→CPS pass infrastructure.
//
// # Design
//
// Each compiler pass over the CPS IR implements `CpsPass` and overrides only
// the variant hooks it cares about. The default implementations recurse into
// all children and rebuild the node unchanged — so a pass only needs to
// express what it *changes*, not how to walk the rest of the tree.
//
// # Pipeline model
//
// Passes are kept strictly separate:
//   - Each pass has a clear CpsExpr → CpsExpr signature (no mutation).
//   - No pass fuses logic from another pass, even for efficiency.
//   - The pipeline is an explicit chain of `pass.transform_expr(root)` calls,
//     making it easy to inspect or serialize the IR between any two steps.
//
// # Interruptibility
//
// Individual passes are atomic (one full tree walk), but the *pipeline* is
// interruptible: because each pass produces a complete CpsExpr tree, the
// pipeline can stop after any pass and hand off the result (e.g. for
// inspection, serialization, or incremental compilation).
//
// # Error handling
//
// Passes return Result so that semantic errors (e.g. undefined variables)
// can be surfaced without panicking. Errors short-circuit via `?`.

use crate::lexer::Loc;
use super::cps::{CpsExpr, CpsFn, CpsParam, CpsVal};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct CpsPassError {
  pub message: String,
  pub loc: Loc,
}

impl CpsPassError {
  pub fn new(message: impl Into<String>, loc: Loc) -> Self {
    Self { message: message.into(), loc }
  }
}

pub type CpsPassResult<'src> = Result<CpsExpr<'src>, CpsPassError>;

// ---------------------------------------------------------------------------
// CpsPass trait
// ---------------------------------------------------------------------------

pub trait CpsPass<'src> {
  /// Entry point — dispatches to the appropriate variant hook.
  fn transform_expr(&mut self, expr: CpsExpr<'src>) -> CpsPassResult<'src> {
    match expr {
      CpsExpr::Store { env, key, val, cont } =>
        self.transform_store(env, key, val, cont),
      CpsExpr::Load { env, key, cont } =>
        self.transform_load(env, key, cont),
      CpsExpr::Apply { func, args, state, cont } =>
        self.transform_apply(func, args, state, cont),
      CpsExpr::Closure { env, func, cont } =>
        self.transform_closure(env, func, cont),
      CpsExpr::Scope { env, inner, cont } =>
        self.transform_scope(env, inner, cont),
      CpsExpr::SeqAppend { seq, val, state, cont } =>
        self.transform_seq_append(seq, val, state, cont),
      CpsExpr::SeqConcat { seq, other, state, cont } =>
        self.transform_seq_concat(seq, other, state, cont),
      CpsExpr::RecPut { rec, key, val, state, cont } =>
        self.transform_rec_put(rec, key, val, state, cont),
      CpsExpr::RecMerge { rec, other, state, cont } =>
        self.transform_rec_merge(rec, other, state, cont),
      CpsExpr::RangeExcl { start, end, state, cont } =>
        self.transform_range_excl(start, end, state, cont),
      CpsExpr::RangeIncl { start, end, state, cont } =>
        self.transform_range_incl(start, end, state, cont),
      CpsExpr::Err { res, state, err_cont, ok_cont } =>
        self.transform_err(res, state, err_cont, ok_cont),
      CpsExpr::If { cond, then_cont, else_cont } =>
        self.transform_if(cond, then_cont, else_cont),
      CpsExpr::Panic { message, state } =>
        self.transform_panic(message, state),
      CpsExpr::MatchBind { val, state, arm, fail, cont } =>
        self.transform_match_bind(val, state, arm, fail, cont),
      CpsExpr::MatchBlock { vals, state, branches, fail, cont } =>
        self.transform_match_block(vals, state, branches, fail, cont),
      CpsExpr::MatchBranch { env, arm } =>
        self.transform_match_branch(env, arm),
      CpsExpr::SeqMatcher { val, state, cont, fail } =>
        self.transform_seq_matcher(val, state, cont, fail),
      CpsExpr::RecMatcher { val, state, cont, fail } =>
        self.transform_rec_matcher(val, state, cont, fail),
      CpsExpr::MatchPopAt { matcher, index, state, cont, fail } =>
        self.transform_match_pop_at(matcher, index, state, cont, fail),
      CpsExpr::MatchPopField { matcher, key, state, cont, fail } =>
        self.transform_match_pop_field(matcher, key, state, cont, fail),
      CpsExpr::MatchDone { matcher, state, non_empty, empty } =>
        self.transform_match_done(matcher, state, non_empty, empty),
      CpsExpr::MatchRest { matcher, state, cont } =>
        self.transform_match_rest(matcher, state, cont),
      CpsExpr::MatchLen { matcher, len, state, ok, fail } =>
        self.transform_match_len(matcher, len, state, ok, fail),
      CpsExpr::TailCall { cont, args } =>
        self.transform_tail_call(cont, args),
    }
  }

  // ---------------------------------------------------------------------------
  // Variant hooks — defaults recurse into continuations and rebuild unchanged.
  // ---------------------------------------------------------------------------

  fn transform_store(
    &mut self,
    env: &'src str,
    key: &'src str,
    val: Box<CpsVal<'src>>,
    cont: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let cont = self.transform_fn(cont)?;
    Ok(CpsExpr::Store { env, key, val, cont })
  }

  fn transform_load(
    &mut self,
    env: &'src str,
    key: super::cps::CpsKey<'src>,
    cont: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let cont = self.transform_fn(cont)?;
    Ok(CpsExpr::Load { env, key, cont })
  }

  fn transform_apply(
    &mut self,
    func: Box<CpsVal<'src>>,
    args: Vec<CpsVal<'src>>,
    state: &'src str,
    cont: Box<CpsVal<'src>>,
  ) -> CpsPassResult<'src> {
    let cont = self.transform_val(*cont)?;
    Ok(CpsExpr::Apply { func, args, state, cont: Box::new(cont) })
  }

  fn transform_closure(
    &mut self,
    env: &'src str,
    func: CpsFn<'src>,
    cont: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let func = self.transform_fn(func)?;
    let cont = self.transform_fn(cont)?;
    Ok(CpsExpr::Closure { env, func, cont })
  }

  fn transform_scope(
    &mut self,
    env: &'src str,
    inner: CpsFn<'src>,
    cont: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let inner = self.transform_fn(inner)?;
    let cont = self.transform_fn(cont)?;
    Ok(CpsExpr::Scope { env, inner, cont })
  }

  fn transform_seq_append(
    &mut self,
    seq: Box<CpsVal<'src>>,
    val: Box<CpsVal<'src>>,
    state: &'src str,
    cont: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let cont = self.transform_fn(cont)?;
    Ok(CpsExpr::SeqAppend { seq, val, state, cont })
  }

  fn transform_seq_concat(
    &mut self,
    seq: Box<CpsVal<'src>>,
    other: Box<CpsVal<'src>>,
    state: &'src str,
    cont: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let cont = self.transform_fn(cont)?;
    Ok(CpsExpr::SeqConcat { seq, other, state, cont })
  }

  fn transform_rec_put(
    &mut self,
    rec: Box<CpsVal<'src>>,
    key: &'src str,
    val: Box<CpsVal<'src>>,
    state: &'src str,
    cont: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let cont = self.transform_fn(cont)?;
    Ok(CpsExpr::RecPut { rec, key, val, state, cont })
  }

  fn transform_rec_merge(
    &mut self,
    rec: Box<CpsVal<'src>>,
    other: Box<CpsVal<'src>>,
    state: &'src str,
    cont: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let cont = self.transform_fn(cont)?;
    Ok(CpsExpr::RecMerge { rec, other, state, cont })
  }

  fn transform_range_excl(
    &mut self,
    start: Box<CpsVal<'src>>,
    end: Box<CpsVal<'src>>,
    state: &'src str,
    cont: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let cont = self.transform_fn(cont)?;
    Ok(CpsExpr::RangeExcl { start, end, state, cont })
  }

  fn transform_range_incl(
    &mut self,
    start: Box<CpsVal<'src>>,
    end: Box<CpsVal<'src>>,
    state: &'src str,
    cont: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let cont = self.transform_fn(cont)?;
    Ok(CpsExpr::RangeIncl { start, end, state, cont })
  }

  fn transform_err(
    &mut self,
    res: Box<CpsVal<'src>>,
    state: &'src str,
    err_cont: CpsFn<'src>,
    ok_cont: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let err_cont = self.transform_fn(err_cont)?;
    let ok_cont = self.transform_fn(ok_cont)?;
    Ok(CpsExpr::Err { res, state, err_cont, ok_cont })
  }

  fn transform_if(
    &mut self,
    cond: Box<CpsVal<'src>>,
    then_cont: CpsFn<'src>,
    else_cont: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let then_cont = self.transform_fn(then_cont)?;
    let else_cont = self.transform_fn(else_cont)?;
    Ok(CpsExpr::If { cond, then_cont, else_cont })
  }

  fn transform_panic(
    &mut self,
    message: Box<CpsVal<'src>>,
    state: &'src str,
  ) -> CpsPassResult<'src> {
    Ok(CpsExpr::Panic { message, state })
  }

  fn transform_match_bind(
    &mut self,
    val: Box<CpsVal<'src>>,
    state: &'src str,
    arm: CpsFn<'src>,
    fail: CpsFn<'src>,
    cont: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let arm = self.transform_fn(arm)?;
    let fail = self.transform_fn(fail)?;
    let cont = self.transform_fn(cont)?;
    Ok(CpsExpr::MatchBind { val, state, arm, fail, cont })
  }

  fn transform_match_block(
    &mut self,
    vals: Vec<CpsVal<'src>>,
    state: &'src str,
    branches: Vec<CpsExpr<'src>>,
    fail: CpsFn<'src>,
    cont: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let branches = branches.into_iter()
      .map(|b| self.transform_expr(b))
      .collect::<Result<Vec<_>, _>>()?;
    let fail = self.transform_fn(fail)?;
    let cont = self.transform_fn(cont)?;
    Ok(CpsExpr::MatchBlock { vals, state, branches, fail, cont })
  }

  fn transform_match_branch(
    &mut self,
    env: &'src str,
    arm: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let arm = self.transform_fn(arm)?;
    Ok(CpsExpr::MatchBranch { env, arm })
  }

  fn transform_seq_matcher(
    &mut self,
    val: Box<CpsVal<'src>>,
    state: &'src str,
    cont: CpsFn<'src>,
    fail: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let cont = self.transform_fn(cont)?;
    let fail = self.transform_fn(fail)?;
    Ok(CpsExpr::SeqMatcher { val, state, cont, fail })
  }

  fn transform_rec_matcher(
    &mut self,
    val: Box<CpsVal<'src>>,
    state: &'src str,
    cont: CpsFn<'src>,
    fail: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let cont = self.transform_fn(cont)?;
    let fail = self.transform_fn(fail)?;
    Ok(CpsExpr::RecMatcher { val, state, cont, fail })
  }

  fn transform_match_pop_at(
    &mut self,
    matcher: Box<CpsVal<'src>>,
    index: usize,
    state: &'src str,
    cont: CpsFn<'src>,
    fail: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let cont = self.transform_fn(cont)?;
    let fail = self.transform_fn(fail)?;
    Ok(CpsExpr::MatchPopAt { matcher, index, state, cont, fail })
  }

  fn transform_match_pop_field(
    &mut self,
    matcher: Box<CpsVal<'src>>,
    key: &'src str,
    state: &'src str,
    cont: CpsFn<'src>,
    fail: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let cont = self.transform_fn(cont)?;
    let fail = self.transform_fn(fail)?;
    Ok(CpsExpr::MatchPopField { matcher, key, state, cont, fail })
  }

  fn transform_match_done(
    &mut self,
    matcher: Box<CpsVal<'src>>,
    state: &'src str,
    non_empty: CpsFn<'src>,
    empty: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let non_empty = self.transform_fn(non_empty)?;
    let empty = self.transform_fn(empty)?;
    Ok(CpsExpr::MatchDone { matcher, state, non_empty, empty })
  }

  fn transform_match_rest(
    &mut self,
    matcher: Box<CpsVal<'src>>,
    state: &'src str,
    cont: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let cont = self.transform_fn(cont)?;
    Ok(CpsExpr::MatchRest { matcher, state, cont })
  }

  fn transform_match_len(
    &mut self,
    matcher: Box<CpsVal<'src>>,
    len: usize,
    state: &'src str,
    ok: CpsFn<'src>,
    fail: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    let ok = self.transform_fn(ok)?;
    let fail = self.transform_fn(fail)?;
    Ok(CpsExpr::MatchLen { matcher, len, state, ok, fail })
  }

  fn transform_tail_call(
    &mut self,
    cont: Box<CpsVal<'src>>,
    args: Vec<CpsVal<'src>>,
  ) -> CpsPassResult<'src> {
    Ok(CpsExpr::TailCall { cont, args })
  }

  // ---------------------------------------------------------------------------
  // Helpers — recurse into CpsFn bodies and inline CpsVal::Fn continuations.
  // ---------------------------------------------------------------------------

  fn transform_fn(&mut self, f: CpsFn<'src>) -> Result<CpsFn<'src>, CpsPassError> {
    let body = self.transform_expr(*f.body)?;
    Ok(CpsFn { params: f.params, body: Box::new(body), captures: f.captures })
  }

  /// Recurse into inline `CpsVal::Fn` continuations; leave other vals unchanged.
  fn transform_val(&mut self, val: CpsVal<'src>) -> Result<CpsVal<'src>, CpsPassError> {
    match val {
      CpsVal::Fn(f) => Ok(CpsVal::Fn(self.transform_fn(f)?)),
      other => Ok(other),
    }
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;
  use crate::transform::cps::{CpsKey, CpsParam};

  // Identity pass — recurses, changes nothing.
  struct Identity;
  impl<'src> CpsPass<'src> for Identity {}

  // Counts Store nodes visited.
  struct StoreCounter(usize);
  impl<'src> CpsPass<'src> for StoreCounter {
    fn transform_store(
      &mut self,
      env: &'src str,
      key: &'src str,
      val: Box<CpsVal<'src>>,
      cont: CpsFn<'src>,
    ) -> CpsPassResult<'src> {
      self.0 += 1;
      let cont = self.transform_fn(cont)?;
      Ok(CpsExpr::Store { env, key, val, cont })
    }
  }

  fn tail(name: &'static str) -> CpsExpr<'static> {
    CpsExpr::TailCall {
      cont: Box::new(CpsVal::Ident(name)),
      args: vec![],
    }
  }

  fn store_expr(key: &'static str, inner: CpsExpr<'static>) -> CpsExpr<'static> {
    CpsExpr::Store {
      env: "env",
      key,
      val: Box::new(CpsVal::Ident("v")),
      cont: CpsFn {
        params: vec![CpsParam::Ident("x"), CpsParam::Ident("env")],
        body: Box::new(inner),
        captures: vec![],
      },
    }
  }

  #[test]
  fn identity_preserves_tail_call() {
    let expr = tail("ƒ_cont");
    let result = Identity.transform_expr(expr.clone()).unwrap();
    assert_eq!(result, expr);
  }

  #[test]
  fn identity_preserves_nested() {
    let expr = store_expr("x", store_expr("y", tail("ƒ_cont")));
    let result = Identity.transform_expr(expr.clone()).unwrap();
    assert_eq!(result, expr);
  }

  #[test]
  fn counter_counts_stores() {
    let expr = store_expr("x", store_expr("y", tail("ƒ_cont")));
    let mut counter = StoreCounter(0);
    counter.transform_expr(expr).unwrap();
    assert_eq!(counter.0, 2);
  }

  #[test]
  fn identity_preserves_load() {
    let expr = CpsExpr::Load {
      env: "env",
      key: CpsKey::Id("foo"),
      cont: CpsFn {
        params: vec![CpsParam::Ident("·foo"), CpsParam::Ident("env")],
        body: Box::new(tail("ƒ_cont")),
        captures: vec![],
      },
    };
    let result = Identity.transform_expr(expr.clone()).unwrap();
    assert_eq!(result, expr);
  }

  #[test]
  fn identity_recurses_into_closure() {
    let expr = CpsExpr::Closure {
      env: "env",
      func: CpsFn {
        params: vec![CpsParam::Ident("·x"), CpsParam::Ident("env"),
                     CpsParam::Ident("state"), CpsParam::Ident("ƒ_cont")],
        body: Box::new(tail("ƒ_cont")),
        captures: vec![],
      },
      cont: CpsFn {
        params: vec![CpsParam::Ident("·f"), CpsParam::Wildcard],
        body: Box::new(tail("ƒ_cont")),
        captures: vec![],
      },
    };
    let result = Identity.transform_expr(expr.clone()).unwrap();
    assert_eq!(result, expr);
  }
}
