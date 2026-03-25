// Unified closure/continuation lifting pass.
//
// Replaces the separate cont_lifting and closure_lifting passes with a single
// iterative pass that lifts nested fns one level at a time, threading captured
// bindings as explicit params.
//
// ## Why a unified pass?
//
// The old pipeline (cont_lifting → closure_lifting) has fundamental ordering
// bugs: hoisting a fn to an outer scope can create forward references to binds
// that are only defined deeper in a sibling continuation chain. These are not
// mutual recursion forward refs — they are dangling references caused by
// lifting past the bind's definition site.
//
// ## Core invariant
//
// Before lifting a fn, answer: "if I move this fn one level up, which of its
// free variables would become out of scope?"
//
// Only variables bound by the immediate enclosing scope (siblings in the same
// LetFn/LetVal continuation chain) need to be threaded as params. Variables
// from parent scopes remain visible after a one-level lift.
//
// This is different from the old capture analysis which asked "what does this
// fn reference from any outer scope?" — that includes bindings that are still
// in scope after lifting and don't need to be captured.
//
// ## Algorithm
//
// 1. Run scope-aware capture analysis: for each nested fn, compute which of
//    its free variables are defined in the *same* scope (siblings) — these
//    would become out of scope after a one-level lift.
//
// 2. Find all fns that have such captures (at any depth).
//
// 3. Lift each one level to the parent scope:
//    - Add captured bindings as leading params on the lifted fn.
//    - At the original site, emit `·fn_closure lifted_fn, cap_0, cap_1, ...`
//      to partially apply the hoisted fn with its captures.
//    - Inline `Cont::Expr` bodies become named `LetFn` nodes (what
//      cont_lifting used to do separately).
//
// 4. Repeat from step 1 until no captures remain.
//
// Each iteration peels off one layer of nesting. A depth-N closure takes N
// iterations. The loop terminates because each lift strictly reduces the
// maximum nesting depth of closures.
//
// ## Comparison with old passes
//
// - **cont_lifting**: hoisted all inline `Cont::Expr` bodies into `LetFn`
//   nodes in one pass, allocating a spurious phantom cont param for each.
//   Now unnecessary — inline conts are just fns like any other, lifted by
//   the same mechanism.
//
// - **closure_lifting**: hoisted fns with captures to the outermost scope
//   in one pass, using a separate capture graph. Forward ref bugs arose
//   because it hoisted past bind definition sites. The iterative one-level
//   approach avoids this entirely.
//
// ## Open questions
//
// - Can step 1 be done once upfront, or must it be re-run each iteration?
//   Bottom-up (deepest first) lifting only affects the parent scope, so
//   batching same-depth lifts before re-analyzing may be safe.
//
// - Should pure fns (no captures) also be lifted, or left nested? Currently
//   they could stay nested since they don't create closures. Lifting them
//   would flatten the tree further but isn't required for correctness.

use crate::ast::AstId;
use crate::passes::cps::ir::{CpsResult, Expr};
use crate::propgraph::PropGraph;

/// Lift all nested fns with captures, one level at a time, until no captures
/// remain. Returns the lifted CPS tree.
pub fn lift<'src>(
  result: CpsResult<'src>,
  _ast_index: &PropGraph<AstId, Option<&'src crate::ast::Node<'src>>>,
) -> CpsResult<'src> {
  // TODO: implement iterative lifting
  result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use test_macros::include_fink_tests;

  use crate::ast::build_index;
  use crate::parser::parse;
  use crate::passes::cps::fmt::Ctx;
  use crate::passes::cps::transform::lower_expr;
  use crate::passes::cps_flat::fmt_flat;

  #[allow(unused)]
  fn lift(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let cps = lower_expr(&r.root);
        let lifted = super::lift(cps, &ast_index);
        let ctx = Ctx {
          origin: &lifted.origin,
          ast_index: &ast_index,
          captures: None,
        };
        fmt_flat(&lifted.root, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  include_fink_tests!("src/passes/lifting/test_lifting.fnk");
}
