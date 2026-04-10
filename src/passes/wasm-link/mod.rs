// WASM-target compile + link orchestration.
//
// This pass is responsible for turning an entry fink module + host source
// loader into a linked WASM binary. It's "target-specific" in that it
// knows about WASM fragments and the linker; it doesn't know about
// alternative targets (JS, direct-native, etc.) that would live in
// sibling directories.
//
// Layering:
//   - `modules/` provides the host-neutral `SourceLoader` trait.
//   - `wasm/` compiles a single CPS result to a self-contained WASM
//     fragment (one module in, one fragment out).
//   - `wasm-link/` (this pass) composes the above: takes an entry path +
//     a `SourceLoader`, loads source, runs the per-unit pipeline, and
//     eventually (Slice 5+) drives the linker's resolution phase by
//     providing an `ImportResolver` that wraps the loader.
//
// Current state (Slice 3): `compile_package` is a thin wrapper around
// the existing single-source pipeline (`to_wasm`). It loads the entry
// source via the loader, hands off to `to_wasm`, and returns the result.
// No multi-module behaviour yet — that arrives in Slices 4 (emitter
// changes) and 5 (linker resolution phase).
//
// The `ImportResolver` trait is declared here so Slice 4's emitter work
// and Slice 5's linker work can reference a stable type from the start.

use std::path::Path;

use crate::passes::modules::SourceLoader;

/// Resolve an import from a module to a target fragment.
///
/// This is the linker-level abstraction for producing WASM bytes on
/// demand. When the linker encounters an unresolved cross-module import,
/// it calls `resolve_import` to fetch the target fragment.
///
/// - `module_id` is the stable ID of the requesting fragment — matches
///   the fragment's `module_name` in the link set. An empty string means
///   "initial entry load with no parent context".
/// - `import_url` is the raw URL string the user wrote in source.
///
/// Returns `(stable_id, bytes)` where `stable_id` is the canonical ID
/// for the target. The invariant is: two calls that resolve to the same
/// logical target must return the same `stable_id`, so the linker can
/// memoize by ID and avoid linking the same module twice.
///
/// Not used by the linker yet — Slice 5 adds the resolution phase. The
/// trait is defined here so Slice 4's emitter work can reference a
/// stable type from the start.
pub trait ImportResolver {
  fn resolve_import(
    &mut self,
    module_id: &str,
    import_url: &str,
  ) -> Result<(String, Vec<u8>), String>;
}

/// Compile a package rooted at `entry_path` to a linked WASM binary.
///
/// For Slice 3 this is a thin wrapper that loads the entry source via
/// the loader and runs the existing single-source pipeline (parse →
/// desugar → lower → lift → emit_wasm). No cross-module handling yet —
/// if the entry file contains any `import` statements the existing
/// pipeline will fail (lifting rejects them today).
///
/// Slices 4 and 5 extend this to actually drive the multi-module
/// pipeline: the emitter will produce cross-module imports/exports in
/// each fragment, and the linker will resolve them by calling an
/// `ImportResolver` that knows how to run the per-unit pipeline on
/// additional loaded sources.
///
/// The `url` embedded in the AST Module node (added in Slice 2) is the
/// entry_path as a UTF-8 string.
#[cfg(feature = "compile")]
pub fn compile_package(
  entry_path: &Path,
  loader: &mut dyn SourceLoader,
) -> Result<crate::passes::Wasm, String> {
  let source = loader.load(entry_path)?;
  let entry_url = entry_path.to_str().ok_or_else(|| {
    format!("entry path is not valid UTF-8: {}", entry_path.display())
  })?;

  // Run the single-source pipeline directly. `crate::to_wasm` delegates
  // here via an `InMemorySourceLoader`, so we can't call it back without
  // infinite recursion.
  let (lifted, desugared) = crate::to_lifted(&source, entry_url)?;
  Ok(crate::passes::emit_wasm(&lifted, &desugared, entry_url, &source))
}
