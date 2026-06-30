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

pub mod cont_lift;
pub mod convert;
pub mod hoist;

pub use cont_lift::cont_lift;
pub use convert::convert;
pub use hoist::hoist;

/// Render the closure-converted CPS IR (CPS -> thread_ctx -> cont_lift ->
/// convert) with a base64url source-map line. Backs the `cps_closures` host
/// service (`fink/compile.fnk`) and the native cps_closures test file.
pub fn cps_closures_debug(src: &str) -> String {
  use crate::passes::cps::fmt::Ctx;
  match crate::to_desugared(src, "test") {
    Ok(desugared) => {
      let cps = crate::passes::lower(&desugared, src);
      let threaded = crate::passes::cps::thread_ctx::thread_ctx(cps.result);
      let lifted = cont_lift(threaded);
      let result = convert(lifted);
      let bk = crate::passes::cps::ir::collect_bind_kinds(&result.root);
      let ctx = Ctx { origin: &result.origin, ast: &desugared.ast, captures: None, param_info: None, bind_kinds: Some(&bk) };
      let (output, srcmap) = crate::passes::cps::fmt::fmt_with_mapped_native(&result.root, &ctx);
      let b64 = srcmap.encode_base64url();
      format!("{output}\n# sm:{b64}")
    }
    Err(e) => format!("ERROR: {e}"),
  }
}

/// Render the post-hoist CPS IR (CPS -> thread_ctx -> cont_lift -> convert ->
/// hoist) with a base64url source-map line. Backs the `cps_hoisted` host
/// service (`fink/compile.fnk`) and the native cps_hoisted test file.
pub fn cps_hoisted_debug(src: &str) -> String {
  use crate::passes::cps::fmt::Ctx;
  match crate::to_desugared(src, "test") {
    Ok(desugared) => {
      let cps = crate::passes::lower(&desugared, src);
      let threaded = crate::passes::cps::thread_ctx::thread_ctx(cps.result);
      let lifted = cont_lift(threaded);
      let converted = convert(lifted);
      let result = hoist(converted);
      let bk = crate::passes::cps::ir::collect_bind_kinds(&result.root);
      let ctx = Ctx { origin: &result.origin, ast: &desugared.ast, captures: None, param_info: None, bind_kinds: Some(&bk) };
      let (output, srcmap) = crate::passes::cps::fmt::fmt_with_mapped_native(&result.root, &ctx);
      let b64 = srcmap.encode_base64url();
      format!("{output}\n# sm:{b64}")
    }
    Err(e) => format!("ERROR: {e}"),
  }
}

