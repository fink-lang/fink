// Match lowering pass — rewrites Match* builtins into primitive CPS.
//
// Runs after the CPS transform, before lifting. At this point the IR is
// nested (not yet flat), and closures don't exist — everything is lexically
// scoped LetFn/LetVal/App/If.
//
// ## What this pass does
//
// Rewrites every `App { BuiltIn::MatchBlock, ... }` (and its constituent
// MatchArm, MatchValue, MatchIf, etc.) into chains of If/LetFn/App nodes
// using only primitives the codegen already handles.
//
// After this pass, no Match* builtins remain in the IR. The lifting pass
// then handles capture analysis and closure creation as usual.
//
// ## Emitted structure
//
// Every match expression, regardless of complexity, produces the same
// three-part structure:
//
//   mb_N = fn ..binds, cont:    -- match body: receives bindings, calls cont with result
//     cont <result_expr>
//
//   mp_N = fn subj, succ, fail: -- match pattern: tests subject, dispatches
//     if <test>,
//       fn: mb_N ..binds, succ
//       fn: fail _
//
//   m_0 = fn subj, cont:        -- match block: wires the fail chain
//     mp_1 subj, cont,
//       fn: mp_2 subj, cont,
//         fn: panic _
//
//   m_0 <subject>, <outer_cont>
//
// ### Naming conventions
//
//   mb_N  — match body for arm N
//   mp_N  — match pattern for arm N
//   m_0   — match block entry point (orchestrator)
//
// ### Calling conventions
//
//   mp_N(subj, succ, fail)  — succ-first, like `if then else`
//   mb_N(..binds, cont)     — bindings then continuation
//   m_0(subj, cont)         — subject then result continuation
//
// The succ/fail order matches `if` (true branch first, false branch second).
// Previously the CPS transform used fail/succ order to make inline
// continuation nesting read better, but with the flat structure that
// motivation is gone.
//
// ### Consistency over cleverness
//
// The same mb_N/mp_N/m_0 structure is emitted for every match — a single-arm
// literal match and a multi-arm guarded match with bindings produce the same
// shape. This keeps the lowering pass mechanical and predictable. The
// optimizer (wasm-opt) handles inlining and simplification.
//
// ## Pattern lowering
//
// Each pattern type becomes a condition tested in mp_N:
//
//   Literal:   `if subj == lit`     (op_eq comparison)
//   Guard:     `if <guard_expr>`    (e.g. subj > 0)
//   Wildcard:  always succeeds      (mp_N directly calls mb_N)
//   Variable:  always succeeds      (bind subj, then call mb_N)
//   Or-guard:  `if a or b`          (short-circuit or of sub-patterns)
//
// Sequence and record patterns will thread cursor/field state through
// the mp_N function. The m_0 wrapper provides a place for setup/teardown
// of iteration state when needed.
//
// ## Bool match is the base case
//
// All pattern tests bottom out at `match <bool>: true: ...; false: ...`,
// which the CPS transform emits as `ExprKind::If` directly. No recursion
// through the lowering pass — bool match is handled by CPS, everything
// else is handled here.
//
// ## Design: naive first, optimize later
//
// The initial implementation emits a linear fail chain (test arms in
// source order). This can later be replaced with a decision-tree optimizer
// (Maranget-style) that reorders tests to minimize redundant comparisons,
// detects exhaustiveness, and shares common subtrees — all producing the
// same CPS primitives.
//
// ## Pipeline position
//
//   source → parse → partial → scopes → CPS → **match_lower** → lifting → collect → emit
//

use crate::passes::cps::ir::CpsResult;

/// Lower all Match* builtins in the CPS IR into primitive operations.
///
/// Identity pass (skeleton) — returns the IR unchanged.
pub fn lower(cps: CpsResult) -> CpsResult {
  cps
}

#[cfg(test)]
mod tests {
  use crate::parser::parse;
  use crate::ast::{build_index, NodeKind};
  use crate::passes::cps::fmt::Ctx;
  use crate::passes::scopes;
  use crate::passes::cps::transform::lower_module;

  fn match_lower(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let (root, node_count) = crate::passes::partial::apply(r.root, r.node_count)
          .unwrap_or_else(|e| panic!("partial pass failed: {:?}", e));
        let r = crate::ast::ParseResult { root, node_count };
        let ast_index = build_index(&r);
        let exprs = match &r.root.kind {
          NodeKind::Module(exprs) => exprs.items.as_slice(),
          _ => std::slice::from_ref(&r.root),
        };
        let scope = scopes::analyse(&r.root, r.node_count as usize, &[]);
        let cps = lower_module(exprs, &scope);
        let result = super::lower(cps);
        let ctx = Ctx { origin: &result.origin, ast_index: &ast_index, captures: None };
        crate::passes::cps::fmt::fmt_with(&result.root, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  test_macros::include_fink_tests!("src/passes/match_lower/test_match_lower.fnk");
}
