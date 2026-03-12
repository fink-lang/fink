// Free variable analysis pass.
//
// Annotates each `LetFn` with the names it reads directly from the enclosing
// scope — i.e. names that are `Key` references in the fn body, not covered
// by the fn's own params or by `LetVal` bindings settled within the same body.
//
// # Algorithm
//
// Walk the `LetFn.fn_body` and collect every `ValKind::Key` encountered, in
// first-encounter order, deduplicating via a seen set.  Exclude:
//   - Names bound by the fn's own `params`
//   - Names settled by `LetVal` bindings *within the same fn body*
//   - The wildcard `_`
//
// Stop at nested `LetFn` boundaries — their free vars are their own concern
// (they have already been annotated by the time we process the outer fn,
// because we recurse depth-first / bottom-up).
//
// # Ordering
//
// The first `Key` encountered during a left-to-right walk of the body is
// listed first.  This matches the order `·load` calls appear in the formatter
// output.

use std::collections::HashSet;
use super::cps::{Arg, BindName, Expr, ExprKind, FreeVar, KeyKind, Name, Param, Val, ValKind};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Annotate every `LetFn` in `expr` with its free variables.
/// Operates bottom-up: inner fns are annotated before outer ones.
pub fn annotate(expr: Expr<'_>) -> Expr<'_> {
  transform_expr(expr)
}

// ---------------------------------------------------------------------------
// Recursive transform
// ---------------------------------------------------------------------------

fn transform_expr(expr: Expr<'_>) -> Expr<'_> {
  use ExprKind::*;
  let meta = expr.meta.clone();
  let kind = match expr.kind {
    LetVal { name, val, body } => LetVal {
      name,
      val,
      body: Box::new(transform_expr(*body)),
    },

    LetFn { name, params, fn_body, body, .. } => {
      // Recurse into fn_body first (bottom-up).
      let fn_body = transform_expr(*fn_body);
      // Recurse into body (the continuation after the closure is created).
      let body = transform_expr(*body);
      // Compute free vars for this fn.
      let bound: HashSet<Name<'_>> = params.iter().filter_map(|p| match p {
        Param::Name(n) | Param::Spread(n) => match n {
          BindName::User(s) => if *s == "_" { None } else { Some(*s) },
          BindName::Gen(_) => None,  // compiler temps are never free-var references
        },
      }).collect();
      let mut seen: HashSet<FreeVar<'_>> = HashSet::new();
      let mut free_vars: Vec<FreeVar<'_>> = vec![];
      collect_keys(&fn_body, &bound, &mut seen, &mut free_vars);
      LetFn { name, params, free_vars, fn_body: Box::new(fn_body), body: Box::new(body) }
    }

    LetRec { bindings, body } => LetRec {
      bindings: bindings.into_iter().map(|mut b| {
        b.fn_body = Box::new(transform_expr(*b.fn_body));
        b
      }).collect(),
      body: Box::new(transform_expr(*body)),
    },

    App { func, args, result, body } => App {
      func, args, result,
      body: Box::new(transform_expr(*body)),
    },

    If { cond, then, else_ } => If {
      cond,
      then: Box::new(transform_expr(*then)),
      else_: Box::new(transform_expr(*else_)),
    },

    Yield { value, result, body } => Yield {
      value, result,
      body: Box::new(transform_expr(*body)),
    },

    Ret(_) | Panic | FailCont => expr.kind,

    // Pattern lowering primitives — recurse into both fail and body.
    MatchLetVal { name, val, fail, body } => MatchLetVal {
      name, val,
      fail: Box::new(transform_expr(*fail)),
      body: Box::new(transform_expr(*body)),
    },
    MatchApp { func, args, fail, result, body } => MatchApp {
      func, args, result,
      fail: Box::new(transform_expr(*fail)),
      body: Box::new(transform_expr(*body)),
    },
    MatchIf { func, args, fail, body } => MatchIf {
      func, args,
      fail: Box::new(transform_expr(*fail)),
      body: Box::new(transform_expr(*body)),
    },
    MatchValue { val, lit, fail, body } => MatchValue {
      val, lit,
      fail: Box::new(transform_expr(*fail)),
      body: Box::new(transform_expr(*body)),
    },
    MatchSeq { val, cursor, fail, body } => MatchSeq {
      val, cursor,
      fail: Box::new(transform_expr(*fail)),
      body: Box::new(transform_expr(*body)),
    },
    MatchNext { val, cursor, next_cursor, fail, elem, body } => MatchNext {
      val, cursor, next_cursor, elem,
      fail: Box::new(transform_expr(*fail)),
      body: Box::new(transform_expr(*body)),
    },
    MatchDone { val, cursor, fail, result, body } => MatchDone {
      val, cursor, result,
      fail: Box::new(transform_expr(*fail)),
      body: Box::new(transform_expr(*body)),
    },
    MatchNotDone { val, cursor, fail, body } => MatchNotDone {
      val, cursor,
      fail: Box::new(transform_expr(*fail)),
      body: Box::new(transform_expr(*body)),
    },
    MatchRest { val, cursor, fail, result, body } => MatchRest {
      val, cursor, result,
      fail: Box::new(transform_expr(*fail)),
      body: Box::new(transform_expr(*body)),
    },
    MatchRec { val, cursor, fail, body } => MatchRec {
      val, cursor,
      fail: Box::new(transform_expr(*fail)),
      body: Box::new(transform_expr(*body)),
    },
    MatchField { val, cursor, next_cursor, field, fail, elem, body } => MatchField {
      val, cursor, next_cursor, field, elem,
      fail: Box::new(transform_expr(*fail)),
      body: Box::new(transform_expr(*body)),
    },
    MatchBlock { params, arm_params, fail, arms, result, body } => MatchBlock {
      params,
      arm_params,
      result,
      fail: Box::new(transform_expr(*fail)),
      arms: arms.into_iter().map(transform_expr).collect(),
      body: Box::new(transform_expr(*body)),
    },
  };
  Expr { kind, meta }
}

// ---------------------------------------------------------------------------
// Key collector — walks an Expr, stops at LetFn boundaries
// ---------------------------------------------------------------------------

/// Collect `Key` names from `expr` into `out` (dedup via `seen`),
/// excluding names in `bound` (params + LetVal-settled names at this level).
/// Does NOT descend into `LetFn.fn_body` (its keys are its own).
fn collect_keys<'src>(
  expr: &Expr<'src>,
  bound: &HashSet<Name<'src>>,
  seen: &mut HashSet<FreeVar<'src>>,
  out: &mut Vec<FreeVar<'src>>,
) {
  use ExprKind::*;
  match &expr.kind {
    Ret(val) => collect_key_from_val(val, bound, seen, out),

    LetVal { val, body, name } => {
      collect_key_from_val(val, bound, seen, out);
      // `name` is now settled — exclude it from captures in the continuation.
      let mut inner_bound = bound.clone();
      if let BindName::User(s) = name {
        inner_bound.insert(s);
      }
      collect_keys(body, &inner_bound, seen, out);
    }

    // Stop at nested LetFn — its fn_body keys are its own.
    // Do continue into `body` (the continuation at our scope level).
    LetFn { body, .. } => {
      collect_keys(body, bound, seen, out);
    }

    LetRec { body, .. } => {
      // Same as LetFn: don't descend into binding fn_bodies.
      collect_keys(body, bound, seen, out);
    }

    App { func, args, body, .. } => {
      collect_key_from_val(func, bound, seen, out);
      for arg in args {
        let v = match arg { Arg::Val(v) | Arg::Spread(v) => v };
        collect_key_from_val(v, bound, seen, out);
      }
      collect_keys(body, bound, seen, out);
    }

    Yield { value, result, body } => {
      collect_key_from_val(value, bound, seen, out);
      let mut inner_bound = bound.clone();
      if let BindName::User(s) = result {
        inner_bound.insert(s);
      }
      collect_keys(body, &inner_bound, seen, out);
    }

    If { cond, then, else_ } => {
      collect_key_from_val(cond, bound, seen, out);
      collect_keys(then, bound, seen, out);
      collect_keys(else_, bound, seen, out);
    }

    // Pattern lowering primitives — collect keys from vals, recurse into fail and body.
    // Primitives that bind a name extend `bound` before recursing into `body`.
    MatchLetVal { name, val, fail, body } => {
      collect_key_from_val(val, bound, seen, out);
      collect_keys(fail, bound, seen, out);
      let mut inner = bound.clone();
      if let BindName::User(s) = name { inner.insert(s); }
      collect_keys(body, &inner, seen, out);
    }
    MatchApp { func, args, result, fail, body } => {
      collect_key_from_val(func, bound, seen, out);
      for v in args { collect_key_from_val(v, bound, seen, out); }
      collect_keys(fail, bound, seen, out);
      let mut inner = bound.clone();
      if let BindName::User(s) = result { inner.insert(s); }
      collect_keys(body, &inner, seen, out);
    }
    MatchIf { func, args, fail, body } => {
      collect_key_from_val(func, bound, seen, out);
      for v in args { collect_key_from_val(v, bound, seen, out); }
      collect_keys(fail, bound, seen, out);
      collect_keys(body, bound, seen, out);
    }
    MatchValue { val, fail, body, .. } => {
      collect_key_from_val(val, bound, seen, out);
      collect_keys(fail, bound, seen, out);
      collect_keys(body, bound, seen, out);
    }
    MatchSeq { val, fail, body, .. } => {
      collect_key_from_val(val, bound, seen, out);
      collect_keys(fail, bound, seen, out);
      collect_keys(body, bound, seen, out);
    }
    MatchNext { elem, fail, body, .. } => {
      collect_keys(fail, bound, seen, out);
      let mut inner = bound.clone();
      if let BindName::User(s) = elem { inner.insert(s); }
      collect_keys(body, &inner, seen, out);
    }
    MatchDone { result, fail, body, .. } => {
      collect_keys(fail, bound, seen, out);
      let mut inner = bound.clone();
      if let BindName::User(s) = result { inner.insert(s); }
      collect_keys(body, &inner, seen, out);
    }
    MatchNotDone { fail, body, .. } => {
      collect_keys(fail, bound, seen, out);
      collect_keys(body, bound, seen, out);
    }
    MatchRest { result, fail, body, .. } => {
      collect_keys(fail, bound, seen, out);
      let mut inner = bound.clone();
      if let BindName::User(s) = result { inner.insert(s); }
      collect_keys(body, &inner, seen, out);
    }
    MatchRec { val, fail, body, .. } => {
      collect_key_from_val(val, bound, seen, out);
      collect_keys(fail, bound, seen, out);
      collect_keys(body, bound, seen, out);
    }
    MatchField { elem, fail, body, .. } => {
      collect_keys(fail, bound, seen, out);
      let mut inner = bound.clone();
      if let BindName::User(s) = elem { inner.insert(s); }
      collect_keys(body, &inner, seen, out);
    }
    MatchBlock { params, fail, arms, body, .. } => {
      for s in params { collect_key_from_val(s, bound, seen, out); }
      collect_keys(fail, bound, seen, out);
      for arm in arms {
        collect_keys(arm, bound, seen, out);
      }
      collect_keys(body, bound, seen, out);
    }

    Panic | FailCont => {}
  }
}

fn collect_key_from_val<'src>(
  val: &Val<'src>,
  bound: &HashSet<Name<'src>>,
  seen: &mut HashSet<FreeVar<'src>>,
  out: &mut Vec<FreeVar<'src>>,
) {
  if let ValKind::Key(key) = &val.kind {
    match &key.kind {
      KeyKind::Name(n) => {
        if *n != "_" && !bound.contains(n) && seen.insert(FreeVar::Name(n)) {
          out.push(FreeVar::Name(n));
        }
      }
      KeyKind::Bind(_) => {
        // Gen param references — always bound in the enclosing fn, never free.
      }
      KeyKind::Prim(_) => {
        // Prims are known builtins — not free variables, skip.
      }
      KeyKind::Op(op) => {
        if seen.insert(FreeVar::Op(op)) {
          out.push(FreeVar::Op(op));
        }
      }
    }
  }
}


#[cfg(test)]
mod free_var_tests {
  use crate::parser::parse;
  use crate::transform::cps_fmt::fmt;
  use crate::transform::cps_transform::lower_expr;
  use super::annotate;

  fn cps_free_vars(src: &str) -> String {
    match parse(src) {
      Ok(node) => fmt(&annotate(lower_expr(&node))),
      Err(e)   => format!("ERROR: {}", e.message),
    }
  }

  test_macros::include_fink_tests!("src/transform/test_cps_free_vars.fnk");
}
