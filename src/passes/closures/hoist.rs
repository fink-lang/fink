//! Hoist nested fn definitions out of the module body to top level.
//!
//! After [`super::convert`], every `LetFn` body is closed (captures are
//! materialised via `LetCaps` from the `ƒcaps` param). Once that's true,
//! the fn body no longer depends on its surrounding lexical scope —
//! the definition can live anywhere. This pass flattens all `LetFn`
//! occurrences in the IR so they appear as siblings at the module's
//! top level, leaving the module body free of nested `·fn` definitions.
//!
//! Stub — not yet implemented. The convert pass currently produces
//! output with nested `LetFn`s; codegen handles them by walking the
//! tree. Hoisting is a future cleanup that makes the post-pass shape
//! match what codegen ultimately emits (top-level wasm `(func)`s plus
//! a module-init `(func)`).
//!
//! See `test_hoist.fnk` for the target shape.

use crate::passes::cps::ir::CpsResult;

/// Pass-through stub — see module comment.
pub fn hoist(cps: CpsResult) -> CpsResult {
  cps
}
