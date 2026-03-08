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
use super::cps::{Arg, Arm, BindName, Expr, ExprKind, KeyKind, Name, Param, Pat, PatKind, Val, ValKind};

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
      let mut seen: HashSet<Name<'_>> = HashSet::new();
      let mut free_vars: Vec<Name<'_>> = vec![];
      collect_keys(&fn_body, &bound, &mut seen, &mut free_vars);
      LetFn { name, params, free_vars, fn_body: Box::new(fn_body), body: Box::new(body) }
    }

    LetPat { pat, val, body } => LetPat {
      pat,
      val,
      body: Box::new(transform_expr(*body)),
    },

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

    Match { scrutinee, arms, result, body } => Match {
      scrutinee,
      arms: arms.into_iter().map(|mut arm| {
        arm.fn_body = Box::new(transform_expr(*arm.fn_body));
        arm
      }).collect(),
      result,
      body: Box::new(transform_expr(*body)),
    },

    Ret(_) => expr.kind,
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
  seen: &mut HashSet<Name<'src>>,
  out: &mut Vec<Name<'src>>,
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

    LetPat { val, pat, body } => {
      collect_key_from_val(val, bound, seen, out);
      // Names bound by the pattern are settled for the continuation.
      let mut inner_bound = bound.clone();
      for bind_name in pat.bindings() {
        if let BindName::User(s) = bind_name {
          inner_bound.insert(s);
        }
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

    If { cond, then, else_ } => {
      collect_key_from_val(cond, bound, seen, out);
      collect_keys(then, bound, seen, out);
      collect_keys(else_, bound, seen, out);
    }

    Match { scrutinee, arms, body, .. } => {
      collect_key_from_val(scrutinee, bound, seen, out);
      for arm in arms {
        collect_keys_from_arm(arm, bound, seen, out);
      }
      collect_keys(body, bound, seen, out);
    }
  }
}

fn collect_key_from_val<'src>(
  val: &Val<'src>,
  bound: &HashSet<Name<'src>>,
  seen: &mut HashSet<Name<'src>>,
  out: &mut Vec<Name<'src>>,
) {
  if let ValKind::Key(key) = &val.kind {
    match &key.kind {
      KeyKind::Name(n) => {
        if *n != "_" && !bound.contains(n) && seen.insert(n) {
          out.push(n);
        }
      }
      KeyKind::Prim(_) => {
        // Prims are known builtins — not free variables, skip.
      }
      KeyKind::Op(op) => {
        // Ops are stored as their rendered local name (op_plus etc.)
        // so that sigil() can prefix them consistently.
        let rendered: &'static str = op_local_name(op);
        if !seen.contains(rendered) {
          seen.insert(rendered);
          out.push(rendered);
        }
      }
    }
  }
}

/// Map an operator symbol to its local rendered name (without · prefix).
/// Matches sigil_op() in cps_fmt.rs.
fn op_local_name(op: &str) -> &'static str {
  match op {
    "+"   => "op_plus",
    "-"   => "op_minus",
    "*"   => "op_mul",
    "/"   => "op_div",
    "%"   => "op_rem",
    "=="  => "op_eq",
    "!="  => "op_neq",
    "<"   => "op_lt",
    "<="  => "op_lte",
    ">"   => "op_gt",
    ">="  => "op_gte",
    "."   => "op_dot",
    "and" => "op_and",
    "or"  => "op_or",
    "not" => "op_not",
    _     => "op_unknown",
  }
}

/// Arms introduce their own bindings — exclude them from the outer capture set.
/// (Arm bindings are params of the arm fn, not free vars of the enclosing fn.)
fn collect_keys_from_arm<'src>(
  arm: &Arm<'src>,
  bound: &HashSet<Name<'src>>,
  seen: &mut HashSet<Name<'src>>,
  out: &mut Vec<Name<'src>>,
) {
  let mut arm_bound = bound.clone();
  for b in &arm.bindings {
    if let BindName::User(s) = b {
      arm_bound.insert(s);
    }
  }
  collect_pat_keys(&arm.pattern, bound, seen, out);
  collect_keys(&arm.fn_body, &arm_bound, seen, out);
}

fn collect_pat_keys<'src>(
  pat: &Pat<'src>,
  bound: &HashSet<Name<'src>>,
  seen: &mut HashSet<Name<'src>>,
  out: &mut Vec<Name<'src>>,
) {
  use PatKind::*;
  match &pat.kind {
    Wildcard | Bind(_) | Lit(_) => {}
    Range { start, end, .. } => {
      collect_pat_keys(start, bound, seen, out);
      collect_pat_keys(end, bound, seen, out);
    }
    Guard { pat, guard } => {
      collect_pat_keys(pat, bound, seen, out);
      collect_key_from_val(guard, bound, seen, out);
    }
    Seq { elems, .. } => {
      for elem in elems {
        use super::cps::SeqElem;
        match elem {
          SeqElem::Pat(p) => collect_pat_keys(p, bound, seen, out),
          SeqElem::Spread(_) => {}
        }
      }
    }
    Rec { fields, .. } => {
      for f in fields {
        collect_pat_keys(&f.pattern, bound, seen, out);
      }
    }
    Str(_) => {}
  }
}
