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
//     drives the linker's resolution phase via `compile_fragment`.
//
// ## Multi-module pipeline
//
// `compile_package` drives the full multi-module compile:
//
//   1. Compile entry module → extract module_imports (URL → [name]).
//   2. Work-queue: for each imported URL, resolve the absolute path,
//      compile the dep, extract its imports, enqueue transitive deps.
//   3. Link: [@fink/runtime, dep1, dep2, ..., entry] in dependency order.
//
// ## URL resolution
//
// Import URLs in source are relative to the importing module's directory.
// `resolve_url` converts a relative URL + the importing module's absolute
// path into the dep's absolute path on disk.
//
// The dep's `module_name` in the `LinkInput` is the *canonical URL* as
// seen by the consumer — the raw URL string from source (e.g. `./foo.fnk`).
// This matches the string emitted in the WASM import section, so the
// linker can resolve cross-fragment global imports by module name.
//
// ## Dep init ordering
//
// The linked binary exports each dep's `fink_module` as `<url>:fink_module`
// and the entry's as `fink_module`. The runner calls dep init functions
// first (in topological order) before calling the entry's `fink_module`.
// This populates each dep's export globals before the consumer reads them.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

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
/// Discovers all transitive dependencies via module_imports in each
/// compiled fragment, compiles each dependency, and links everything
/// together into a single self-contained WASM binary.
///
/// The `url` embedded in the AST Module node is the entry_path as a
/// UTF-8 string — used as the entry module's stable identity.
#[cfg(feature = "compile")]
pub fn compile_package(
  entry_path: &Path,
  loader: &mut dyn SourceLoader,
) -> Result<crate::passes::Wasm, String> {
  use crate::passes::wasm::link::{LinkInput, link};

  let entry_url = entry_path
    .to_str()
    .ok_or_else(|| format!("entry path is not valid UTF-8: {}", entry_path.display()))?
    .to_string();

  // Compile the entry module first to discover its imports.
  let entry_source = loader.load(entry_path)?;
  let (entry_lifted, entry_desugared) = crate::to_lifted(&entry_source, &entry_url)?;
  let entry_imports = entry_lifted.result.module_imports.clone();

  // Compile the entry module into a WASM fragment.
  let entry_fragment = compile_fragment(&entry_lifted, &entry_desugared, &entry_url, &entry_source);

  // Work-queue: compile all transitive deps.
  // `compiled`: canonical_url → (wasm_bytes, module_imports of that dep)
  // `order`: dep canonical URLs in the order they were first discovered (BFS).
  type DepImports = BTreeMap<String, Vec<String>>;
  let mut compiled: BTreeMap<String, (Vec<u8>, DepImports)> = BTreeMap::new();
  let mut order: Vec<String> = Vec::new();

  // Seed the queue with the entry module's imports.
  // Each item: (importer_abs_path, import_url_from_source)
  let mut queue: VecDeque<(PathBuf, String)> = VecDeque::new();
  for import_url in entry_imports.keys() {
    queue.push_back((entry_path.to_path_buf(), import_url.clone()));
  }

  // BFS: compile each dep once (memoized by canonical URL).
  //
  // TODO(dedup-by-abs-path): `visited` keys on the raw URL string from the
  // consumer's source. Two different importers using different relative
  // paths to reach the same on-disk file (e.g. `./foo.fnk` vs
  // `../dir/foo.fnk`) will each compile a fresh fragment and the linker
  // will end up with two copies of the same dep. Fix: canonicalize
  // `dep_abs_path` (realpath) and key `visited` on that, while still
  // using the raw `import_url` as the `LinkInput.module_name` so the
  // WASM import section matches what each consumer emitted.
  let mut visited: BTreeSet<String> = BTreeSet::new();
  while let Some((importer_path, import_url)) = queue.pop_front() {
    let dep_abs_path = resolve_url(&importer_path, &import_url)?;
    let canonical_url = import_url.clone(); // URL as written by the consumer

    if visited.contains(&canonical_url) {
      continue;
    }
    visited.insert(canonical_url.clone());

    let dep_source = loader.load(&dep_abs_path)?;
    let (dep_lifted, dep_desugared) = crate::to_lifted(&dep_source, &canonical_url)?;
    let dep_module_imports = dep_lifted.result.module_imports.clone();

    let dep_wasm = compile_fragment(&dep_lifted, &dep_desugared, &canonical_url, &dep_source);

    // Enqueue transitive deps.
    for transitive_url in dep_module_imports.keys() {
      if !visited.contains(transitive_url) {
        queue.push_back((dep_abs_path.clone(), transitive_url.clone()));
      }
    }

    compiled.insert(canonical_url.clone(), (dep_wasm, dep_module_imports));
    order.push(canonical_url);
  }

  // Link: @fink/runtime + deps (in topological order: providers before
  // consumers) + entry. The BFS above visits consumers before their
  // providers (entry discovers its direct imports first, which in turn
  // push their own imports onto the queue). Reversing the discovery
  // order produces a valid topological sort for the acyclic case: the
  // last fragment BFS visits is always a leaf dep, which must initialize
  // first because everything above it in the graph depends on its
  // exported globals being populated.
  //
  // The linker preserves fragment order in its export section, so the
  // linked binary's `<url>:fink_module` exports end up in init order —
  // the runner iterates them in declaration order without needing a
  // separate dep-graph side channel.
  static RUNTIME_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime.wasm"));

  let mut link_inputs: Vec<LinkInput> = vec![
    LinkInput { module_name: "@fink/runtime".into(), wasm: RUNTIME_WASM.to_vec() },
  ];

  // Reverse BFS order → topological (providers before consumers).
  for canonical_url in order.iter().rev() {
    if let Some((dep_wasm, _)) = compiled.remove(canonical_url) {
      link_inputs.push(LinkInput {
        module_name: canonical_url.clone(),
        wasm: dep_wasm,
      });
    }
  }

  // Entry last (so its global imports resolve to deps' already-assigned globals).
  link_inputs.push(LinkInput {
    module_name: "@fink/user".into(),
    wasm: entry_fragment,
  });

  let linked = link(&link_inputs);

  // TODO(sourcemaps): multi-module compiles currently return empty mappings.
  //
  // For the debugger to work across multi-module programs we need to:
  //   1. Plumb offset_mappings out of compile_fragment for every compiled
  //      module (entry + each dep), tagged with that module's source URL.
  //   2. Adjust each fragment's wasm_offset by the byte offset at which its
  //      code section was merged into the linked binary — the linker already
  //      tracks this for DWARF (runtime_code_size in link.rs emit_module),
  //      extend it to also shift user/dep offset mappings.
  //   3. Aggregate into a single Vec<WasmMapping> with per-entry source URL,
  //      and update the WasmMapping type if it doesn't carry a source id yet.
  //   4. DWARF adjustment already runs for user code — extend it to also
  //      process DWARF from dep fragments so stepping works across modules.
  //
  // Without this, setting breakpoints in imported modules or stepping from
  // entry code into a dep will not map back to source. Single-module compiles
  // are equally affected since `to_wasm` now routes through `compile_package`
  // — there is no longer a parallel "direct emit" path that builds mappings.
  Ok(crate::passes::Wasm { binary: linked.wasm, mappings: vec![] })
}

/// Compile a single lifted CPS module into raw WASM bytes (without linking).
///
/// Runs: collect → emit → DWARF append. Does NOT link — the caller is
/// responsible for linking all fragments together.
///
/// TODO(dep-helper-bodies): every compiled fragment today emits full bodies
/// for the host-facing runtime helpers (`_box_func`, `_apply_export`,
/// `_list_nil`, `_list_prepend`, `_fn2_stub`, `_channel_new_export`,
/// `_run_main_export`, `_settle_future_export`, `_str_wrap_bytes_export`).
/// In a dep fragment these are dead weight — they're never called by the
/// dep's own code and the entry fragment already exports them once. The
/// formatter hides them from snapshot output but the bytes are still in
/// the linked binary. Fix: add a `CompileMode::Dep` flag (or equivalent)
/// that skips emitter synthesis of these host helpers when compiling a
/// dep fragment. This is the "duplicated code signals runtime belonging"
/// cleanup Jan flagged during Slice 2 planning.
#[cfg(feature = "compile")]
fn compile_fragment(
  lifted: &crate::passes::LiftedCps,
  desugared: &crate::passes::DesugaredAst<'_>,
  url: &str,
  src: &str,
) -> Vec<u8> {
  use crate::passes::wasm::{collect, dwarf, emit};

  let ir_ctx = collect::IrCtx::new(&lifted.result.origin, &desugared.ast);
  let module = collect::collect(
    &lifted.result.root,
    &ir_ctx,
    &lifted.result.module_locals,
    lifted.result.module_imports.clone(),
  );
  let ir_ctx = ir_ctx.with_globals(module.globals.clone());
  let mut result = emit::emit(&module, &ir_ctx);

  let dwarf_sections = dwarf::emit_dwarf(url, Some(src), &result.offset_mappings);
  dwarf::append_dwarf_sections(&mut result.wasm, &dwarf_sections);

  result.wasm
}

/// Resolve an import URL relative to the importing module's absolute path.
///
/// `importer_path` is the absolute path of the importing module.
/// `import_url` is the URL string as written in source (e.g. `./foo.fnk`).
///
/// Returns the absolute path to the dependency on disk.
fn resolve_url(importer_path: &Path, import_url: &str) -> Result<PathBuf, String> {
  let importer_dir = importer_path
    .parent()
    .ok_or_else(|| format!("importer path has no parent directory: {}", importer_path.display()))?;

  // Normalize the path (resolve `..` etc.) for correct filesystem access.
  // We keep the URL as-is for the module_name (canonical identity from consumer's POV).
  let dep_path = importer_dir.join(import_url);
  Ok(dep_path)
}

#[cfg(test)]
mod tests {
  use std::path::{Path, PathBuf};

  use super::*;
  use crate::passes::modules::{FileSourceLoader, SourceLoader};

  // -- .fnk snapshot tests -----------------------------------------------------
  //
  // These tests pin the linker's multi-module output. `gen_wat_pkg` takes the
  // entry module source verbatim (from a `ƒink:` block), compiles it via
  // `compile_package` using a hybrid loader that provides the inline entry
  // and loads any imported dep files from the fixture directory next to this
  // file (src/passes/wasm-link/test_modules/), then formats the linked binary
  // to WAT for snapshotting.

  /// Hybrid source loader for test fixtures:
  /// - the inline entry source lives in-memory at
  ///   `<wasm-link dir>/__test_entry.fnk` (a synthetic path outside the
  ///   fixture tree so it never collides with a real disk file)
  /// - dep imports load from `src/passes/wasm-link/test_modules/` via
  ///   `FileSourceLoader` — the inline entry imports deps using paths
  ///   like `./test_modules/<name>.fnk`, which `compile_package` resolves
  ///   by joining with the inline entry's parent directory.
  struct TestLoader {
    entry_abs_path: PathBuf,
    entry_source: String,
    disk: FileSourceLoader,
  }

  impl SourceLoader for TestLoader {
    fn load(&mut self, path: &Path) -> Result<String, String> {
      if path == self.entry_abs_path {
        Ok(self.entry_source.clone())
      } else {
        self.disk.load(path)
      }
    }
  }

  /// Absolute path the inline entry source is registered at. Lives next to
  /// the `test_modules/` directory so that a test can write
  /// `import './test_modules/entry.fnk'` and have it resolve on disk.
  fn inline_entry_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("src/passes/wasm-link/__test_entry.fnk")
  }

  /// Compile a fink entry module (with optional dep imports resolved from the
  /// fixture directory) and return the linked binary as WAT text + sourcemap
  /// footer. Catches panics so failing tests produce a blessable string.
  ///
  /// TODO(merge-with-gen_wat): this helper duplicates `src/passes/wasm/mod.rs::gen_wat`.
  /// The two should collapse into a single `gen_wat` once:
  ///   1. `compile_package` plumbs `structural_locs` through to the caller so
  ///      the formatter can produce proper source maps (see the sourcemap
  ///      TODO in `compile_package`).
  ///   2. `gen_wat` in `src/passes/wasm/mod.rs` is rewritten to call
  ///      `compile_package` (via the same `TestLoader` pattern — single-file
  ///      tests just register the entry source with no disk-backed deps).
  ///
  /// Production already uses `compile_package` as the one entry point
  /// (`to_wasm` in `src/lib.rs` wraps the source in an `InMemorySourceLoader`).
  /// The test helpers should mirror that: one `gen_wat(src)` for every wasm /
  /// wasm-link test, with dep fixtures living on disk next to the test file
  /// when needed.
  fn gen_wat_pkg(src: &str) -> String {
    let src_owned = src.to_string();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
      gen_wat_pkg_inner(&src_owned)
    })) {
      Ok(s) => s,
      Err(e) => {
        let msg = if let Some(s) = e.downcast_ref::<&str>() {
          (*s).to_string()
        } else if let Some(s) = e.downcast_ref::<String>() {
          s.clone()
        } else {
          "<unknown panic>".to_string()
        };
        format!("PANIC: {msg}")
      }
    }
  }

  fn gen_wat_pkg_inner(src: &str) -> String {
    let entry_abs_path = inline_entry_path();
    let mut loader = TestLoader {
      entry_abs_path: entry_abs_path.clone(),
      entry_source: src.to_string(),
      disk: FileSourceLoader::new(),
    };
    let wasm = compile_package(&entry_abs_path, &mut loader)
      .unwrap_or_else(|e| panic!("compile_package failed: {e}"));

    // Format WASM → WAT. Structural locs are not yet plumbed through
    // compile_package (see the TODO in compile_package about multi-module
    // source mappings), so we pass an empty slice for now — the WAT is
    // still correct, just with fewer source map anchors.
    let (wat_output, wat_srcmap) = crate::passes::wasm::fmt::format_mapped_with_locs(
      &wasm.binary, &[], "__test_entry.fnk", src,
    );
    let wat_json = wat_srcmap.to_json();
    let wat_b64 = crate::sourcemap::base64_encode(wat_json.as_bytes());

    // Dump files for review — set `DUMP_WAT_DIR=<path>` to enable, unset
    // to skip. No default path: if the env var is missing the block is a
    // no-op.
    if let Some(dir) = std::env::var_os("DUMP_WAT_DIR") {
      let dir = std::path::PathBuf::from(dir);
      let name = crate::test_context::name();
      let slug: String = name.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
      let _ = std::fs::create_dir_all(&dir);
      let wat_content = format!(
        "{}\n//# sourceMappingURL=data:application/json;base64,{wat_b64}",
        wat_output.trim()
      );
      let _ = std::fs::write(dir.join(format!("{slug}.wat.js")), &wat_content);
    }

    format!("{}\n;;sourcemaps:{wat_b64}", wat_output.trim())
  }

  test_macros::include_fink_tests!("src/passes/wasm-link/test_multi_module.fnk");
}
