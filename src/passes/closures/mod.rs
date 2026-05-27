//! Closure conversion — lifts every user fn to a top-level closure-converted
//! form. Each lifted fn takes its captures as a single `ƒcaps` record arg
//! (first user arg), followed by `ƒctx, ƒret, args...`. At each fn-definition
//! site, `App(MkClosure, [funcref, caps])` packages the captured values into
//! the record.
//!
//! Conventions:
//! - Lifted fn signature: `fn ƒcaps, ƒctx, ƒret, args...`. Pure fns get
//!   `ƒcaps = {}` (still passed).
//! - Body refs to captured names render as `ƒcaps.<name>` (record projection).
//! - MkClosure renders as `·mkclosure ƒfn_lifted, {name: ref, ...}`.
//!
//! Runs after `cps::transform::lower_module` + `cps::thread_ctx`. Input is a
//! LetRec/Set/slot-shaped CpsResult; output is the same shape with all fn
//! definitions lifted out and replaced by MkClosure calls.

use crate::passes::cps::ir::CpsResult;

/// Convert all fn definitions in `cps` to closure-converted form.
///
/// Stub — implementation pending. See `test_closures.fnk` for the target
/// shape (Option A: single caps record arg).
pub fn convert(cps: CpsResult) -> CpsResult {
  // TODO: implement.
  // 1. Walk the IR; collect every LetFn definition.
  // 2. For each fn, compute its free variables (refs to names defined
  //    outside the fn body — siblings in an enclosing LetRec, outer
  //    params, etc.).
  // 3. Lift each fn body to a top-level form, parameterised on a fresh
  //    `ƒcaps` record carrying the free vars.
  // 4. Rewrite refs to free vars inside the body to `ƒcaps.<name>`
  //    (record projection — an App(Get, [caps_ref, name_lit, cont])).
  // 5. Replace the original LetFn with App(MkClosure, [funcref, caps_record, cont]).
  cps
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use crate::passes::cps::fmt::Ctx;

  fn cps_closures(src: &str) -> String {
    match crate::to_desugared(src, "test") {
      Ok(desugared) => {
        let cps = crate::passes::lower(&desugared);
        let threaded = crate::passes::cps::thread_ctx::thread_ctx(cps.result);
        let result = super::convert(threaded);
        let bk = crate::passes::cps::ir::collect_bind_kinds(&result.root);
        let ctx = Ctx { origin: &result.origin, ast: &desugared.ast, captures: None, param_info: None, bind_kinds: Some(&bk) };
        let (output, srcmap) = crate::passes::cps::fmt::fmt_with_mapped_native(&result.root, &ctx);
        let _ = src;
        let b64 = srcmap.encode_base64url();
        format!("{output}\n# sm:{b64}")
      }
      Err(e) => format!("ERROR: {e}"),
    }
  }

  test_macros::include_fink_tests!("src/passes/closures/test_closures.fnk");
}
