// Free variable analysis pass.
//
// Annotates each `CpsFn` that is a closure's `func` with the names it reads
// directly from the enclosing env.  Inner closures are processed first (bottom
// up via the CpsPass default recursion) so that by the time we annotate a fn
// its nested closures are already annotated — though we stop at their
// boundaries intentionally (direct captures only, no transitive propagation).
//
// # What counts as a capture
//
//   - `Load { key: Id(name) }` or `Load { key: Op(name) }` found in the body
//   - …minus names bound by `Store` in the *same* fn body
//   - …minus the fn's own params (they arrive as arguments, not from env)
//   - Nested `Closure` bodies are *not* scanned (their captures are their own)
//
// # Output
//
// `CpsFn.captures` is populated with names in first-encountered order,
// deduplicated.  The order matches the order `Load`s appear while walking
// left-to-right through the body.

use std::collections::HashSet;
use super::cps::{CpsExpr, CpsFn, CpsKey, CpsParam, CpsVal};
use super::cps_pass::{CpsPass, CpsPassError, CpsPassResult};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn apply(expr: CpsExpr<'_>) -> Result<CpsExpr<'_>, CpsPassError> {
  FreeVarsPass.transform_expr(expr)
}

// ---------------------------------------------------------------------------
// Pass
// ---------------------------------------------------------------------------

struct FreeVarsPass;

impl<'src> CpsPass<'src> for FreeVarsPass {
  /// Override closure so we can annotate `func` with its captures.
  /// The default recursion already processes `func.body` first, so inner
  /// closures are annotated before we look at the outer one.
  fn transform_closure(
    &mut self,
    env: &'src str,
    func: CpsFn<'src>,
    cont: CpsFn<'src>,
  ) -> CpsPassResult<'src> {
    // Recurse into func and cont (processes nested closures bottom-up).
    let mut func = self.transform_fn(func)?;
    let cont = self.transform_fn(cont)?;

    // Collect names this fn reads from env — after inner closures are done,
    // so their Load nodes are already replaced by their own capture sets.
    // We walk the body and collect Load keys, stopping at nested Closure
    // boundaries.  Names that are Store-bound *within* this body are excluded
    // (they came from env of the parent scope that called store, not ours).
    let param_names: HashSet<&str> = func.params.iter().filter_map(|p| match p {
      CpsParam::Ident(s) => Some(*s),
      CpsParam::Spread(s) => Some(*s),
      CpsParam::Wildcard => None,
    }).collect();

    let mut seen: HashSet<&str> = HashSet::new();
    let mut captures: Vec<&str> = Vec::new();
    collect_loads(&func.body, &param_names, &mut seen, &mut captures);

    func.captures = captures;
    Ok(CpsExpr::Closure { env, func, cont })
  }
}

// ---------------------------------------------------------------------------
// Load collector — walks a body, stops at Closure boundaries
// ---------------------------------------------------------------------------

/// Walk `expr`, collecting Load key names into `out` (deduplicated via `seen`),
/// excluding names in `bound` (own params + store-settled names in this scope).
/// Does NOT descend into nested `Closure.func` bodies.
fn collect_loads<'src>(
  expr: &CpsExpr<'src>,
  bound: &HashSet<&str>,
  seen: &mut HashSet<&'src str>,
  out: &mut Vec<&'src str>,
) {
  match expr {
    CpsExpr::Load { cont, .. } => {
      // The name that this load binds is the continuation's first param.
      // For user vars this is `·foo`; for operators it's `op_plus` etc.
      let name = match cont.params.first() {
        Some(CpsParam::Ident(s)) => *s,
        _ => "",
      };
      // Only capture if not already bound by a param or an in-scope Store.
      if !name.is_empty() && !bound.contains(name) && seen.insert(name) {
        out.push(name);
      }
      // Continue into the continuation (same scope).
      collect_loads(&cont.body, bound, seen, out);
    }

    CpsExpr::Store { key, cont, .. } => {
      // `key` is now settled in this scope — add it to the bound set for the
      // continuation body.  The Store val itself might reference loaded names,
      // but val is a CpsVal (not an expr), so no loads there.
      let mut inner_bound = bound.clone();
      inner_bound.insert(cont.params.iter().find_map(|p| match p {
        CpsParam::Ident(s) => Some(*s),
        _ => None,
      }).unwrap_or(key));
      collect_loads(&cont.body, &inner_bound, seen, out);
    }

    // Closure boundary — do NOT descend into func.body (its captures are its own).
    // Do descend into cont (same outer scope).
    CpsExpr::Closure { cont, .. } => {
      collect_loads(&cont.body, bound, seen, out);
    }

    // Scope boundary — same as Closure: don't enter inner, do enter cont.
    CpsExpr::Scope { cont, .. } => {
      collect_loads(&cont.body, bound, seen, out);
    }

    // Apply: no CpsExpr children other than cont (which is a CpsVal here).
    // Recurse into inline fn continuations.
    CpsExpr::Apply { cont, .. } => {
      collect_loads_val(cont, bound, seen, out);
    }

    // MatchBlock: branches are CpsExprs (MatchBranch nodes), plus fail/cont fns.
    CpsExpr::MatchBlock { branches, fail, cont, .. } => {
      for b in branches { collect_loads(b, bound, seen, out); }
      collect_loads(&fail.body, bound, seen, out);
      collect_loads(&cont.body, bound, seen, out);
    }

    CpsExpr::MatchBranch { arm, .. } => {
      // arm body is a new scope — but still same outer env handle.
      collect_loads(&arm.body, bound, seen, out);
    }

    CpsExpr::MatchBind { arm, fail, cont, .. } => {
      collect_loads(&arm.body, bound, seen, out);
      collect_loads(&fail.body, bound, seen, out);
      collect_loads(&cont.body, bound, seen, out);
    }

    CpsExpr::SeqMatcher { cont, fail, .. } => {
      collect_loads(&cont.body, bound, seen, out);
      collect_loads(&fail.body, bound, seen, out);
    }

    CpsExpr::RecMatcher { cont, fail, .. } => {
      collect_loads(&cont.body, bound, seen, out);
      collect_loads(&fail.body, bound, seen, out);
    }

    CpsExpr::MatchPopAt { cont, fail, .. } => {
      collect_loads(&cont.body, bound, seen, out);
      collect_loads(&fail.body, bound, seen, out);
    }

    CpsExpr::MatchPopField { cont, fail, .. } => {
      collect_loads(&cont.body, bound, seen, out);
      collect_loads(&fail.body, bound, seen, out);
    }

    CpsExpr::MatchDone { non_empty, empty, .. } => {
      collect_loads(&non_empty.body, bound, seen, out);
      collect_loads(&empty.body, bound, seen, out);
    }

    CpsExpr::MatchRest { cont, .. } => {
      collect_loads(&cont.body, bound, seen, out);
    }

    CpsExpr::MatchLen { ok, fail, .. } => {
      collect_loads(&ok.body, bound, seen, out);
      collect_loads(&fail.body, bound, seen, out);
    }

    CpsExpr::SeqAppend { cont, .. } => collect_loads(&cont.body, bound, seen, out),
    CpsExpr::SeqConcat { cont, .. } => collect_loads(&cont.body, bound, seen, out),
    CpsExpr::RecPut { cont, .. }    => collect_loads(&cont.body, bound, seen, out),
    CpsExpr::RecMerge { cont, .. }  => collect_loads(&cont.body, bound, seen, out),
    CpsExpr::RangeExcl { cont, .. } => collect_loads(&cont.body, bound, seen, out),
    CpsExpr::RangeIncl { cont, .. } => collect_loads(&cont.body, bound, seen, out),

    CpsExpr::Err { err_cont, ok_cont, .. } => {
      collect_loads(&err_cont.body, bound, seen, out);
      collect_loads(&ok_cont.body, bound, seen, out);
    }

    CpsExpr::If { then_cont, else_cont, .. } => {
      collect_loads(&then_cont.body, bound, seen, out);
      collect_loads(&else_cont.body, bound, seen, out);
    }

    // Terminals — no children to recurse into.
    CpsExpr::TailCall { .. } | CpsExpr::Panic { .. } => {}
  }
}

/// Walk a `CpsVal` for inline `Fn` continuations that might contain loads.
fn collect_loads_val<'src>(
  val: &CpsVal<'src>,
  bound: &HashSet<&str>,
  seen: &mut HashSet<&'src str>,
  out: &mut Vec<&'src str>,
) {
  if let CpsVal::Fn(f) = val {
    collect_loads(&f.body, bound, seen, out);
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use test_macros::test_template;
  use pretty_assertions::assert_eq;
  use super::super::{cps, cps_fmt};
  use crate::parser::parse;
  use crate::transform::partial;

  fn dedent(s: &str) -> String {
    s.lines()
      .map(|line| line.strip_prefix("    ").unwrap_or(line))
      .collect::<Vec<_>>()
      .join("\n")
  }

  fn free_vars_debug(src: &str) -> String {
    let node = match parse(src) {
      Err(e) => return format!("PARSE ERROR: {}", e.message),
      Ok(n) => n,
    };
    let node = match partial::apply(node) {
      Err(e) => return format!("PARTIAL ERROR: {}", e.message),
      Ok(n) => n,
    };
    let cps_node = cps::transform(node);
    let annotated = match super::apply(cps_node.expr) {
      Err(e) => return format!("FREE VARS ERROR: {}", e.message),
      Ok(e) => e,
    };
    let wrapped = cps::CpsNode::new(annotated, cps_node.loc);
    cps_fmt::fmt_annotated(&wrapped)
  }

  #[test_template(
    "src/transform", "./test_free_vars.fnk",
    r"(?ms)^test '(?P<name>[^']+)', fn:\n  expect \S+ fn:\n(?P<src>[\s\S]+?)\n\n?  [|,] equals_free_vars fn:\n(?P<exp>[\s\S]+?)(?=\n\n\n|\n\n---|\n\ntest |\z)"
  )]
  fn test_free_vars(src: &str, exp: &str, path: &str) {
    assert_eq!(
      free_vars_debug(&dedent(src).trim().to_string()),
      dedent(exp).trim().to_string(),
      "{}",
      path
    );
  }
}
