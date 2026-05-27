//! Closure conversion + hoist.
//!
//! Two passes that together produce a fully closure-converted, top-level-flat
//! IR ready for codegen. Both live in this module so they can share helpers,
//! but they run as distinct stages so their effects are independently
//! inspectable.
//!
//! Pipeline:
//!
//! ```text
//!   convert(cps)  → Closure / LetCaps inserted, body refs rewritten to locals
//!   hoist(cps)    → nested LetFn definitions lifted to top-level
//! ```
//!
//! [`convert`] implements closure conversion against the LetRec/Set/slot
//! IR produced by `cps::transform` + `cps::thread_ctx`. Every user fn
//! gets a leading `ƒcaps` param; the body is rewritten so refs to
//! captured outer bindings resolve to fresh local CpsIds bound by
//! `LetCaps` at fn entry; each fn-definition site emits a `Closure`
//! node that carries the captured values from the construction scope.
//!
//! [`hoist`] is the follow-on pass that flattens nested fn definitions
//! out of the module body — once captures are explicit, fn bodies are
//! closed and can be lifted to the top level. Currently a stub.
//!
//! See `test_convert.fnk` for the curated convert-only shape and
//! `test_full.fnk` for the regression set inherited from the legacy
//! lifting pass.

pub mod convert;
pub mod hoist;

pub use convert::convert;
pub use hoist::hoist;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use crate::passes::cps::fmt::Ctx;

  /// Run CPS → thread_ctx → convert. Renders the convert-only shape.
  fn cps_closures(src: &str) -> String {
    match crate::to_desugared(src, "test") {
      Ok(desugared) => {
        let cps = crate::passes::lower(&desugared);
        let threaded = crate::passes::cps::thread_ctx::thread_ctx(cps.result);
        let result = super::convert(threaded);
        let bk = crate::passes::cps::ir::collect_bind_kinds(&result.root);
        let ctx = Ctx {
          origin: &result.origin,
          ast: &desugared.ast,
          captures: None,
          param_info: None,
          bind_kinds: Some(&bk),
        };
        let (output, srcmap) = crate::passes::cps::fmt::fmt_with_mapped_native(&result.root, &ctx);
        let _ = src;
        let b64 = srcmap.encode_base64url();
        format!("{output}\n# sm:{b64}")
      }
      Err(e) => format!("ERROR: {e}"),
    }
  }

  /// Run CPS → thread_ctx → convert → hoist. Renders the post-hoist shape.
  fn cps_hoisted(src: &str) -> String {
    match crate::to_desugared(src, "test") {
      Ok(desugared) => {
        let cps = crate::passes::lower(&desugared);
        let threaded = crate::passes::cps::thread_ctx::thread_ctx(cps.result);
        let converted = super::convert(threaded);
        let result = super::hoist(converted);
        let bk = crate::passes::cps::ir::collect_bind_kinds(&result.root);
        let ctx = Ctx {
          origin: &result.origin,
          ast: &desugared.ast,
          captures: None,
          param_info: None,
          bind_kinds: Some(&bk),
        };
        let (output, srcmap) = crate::passes::cps::fmt::fmt_with_mapped_native(&result.root, &ctx);
        let _ = src;
        let b64 = srcmap.encode_base64url();
        format!("{output}\n# sm:{b64}")
      }
      Err(e) => format!("ERROR: {e}"),
    }
  }

  test_macros::include_fink_tests!("src/passes/closures/test_convert.fnk");
  test_macros::include_fink_tests!("src/passes/closures/test_hoist.fnk");
}
