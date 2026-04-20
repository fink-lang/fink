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
//   1. Compile entry module under its canonical URL `./<basename>`.
//   2. Work-queue: for each raw import URL in a fragment, compute the
//      dep's canonical URL via `canonicalise_url`, dedup against seen
//      canonical URLs, resolve to disk via `resolve_canonical_to_disk`,
//      compile the dep, extract its imports, enqueue transitive deps.
//   3. Link: [@fink/runtime, dep1, dep2, ..., entry] in dependency order.
//
// ## Canonical URLs
//
// Every module (entry and dep) has a canonical URL: entry-module-relative,
// lexically normalised (`./sub/foo.fnk`, `../lib/util.fnk`). The canonical
// form is the one-and-only identity string used downstream: WASM import
// section, WAT symbol names, linker keys, dedup keys.
//
// `canonicalise_url` computes a dep's canonical URL from the importer's
// canonical URL + the raw URL the importer wrote in source. Two consumers
// reaching the same file via different relative URLs produce the same
// canonical URL, so the dep is compiled and linked exactly once.
//
// The CPS IR is immutable, so raw Lit::Str URLs in `BuiltIn::Import` calls
// stay as written in source. `compile_fragment` builds a raw→canonical
// rewrite map (`url_rewrite`) and hands it to the emitter, which translates
// the Lit::Str URL at `BuiltIn::Import` emit sites before looking it up in
// `module_imports` (whose keys are also pre-rewritten to canonical form).
//
// `resolve_canonical_to_disk` turns a canonical URL into an absolute disk
// path by joining it with the entry module's directory.
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

  // The entry's canonical URL is always `./<filename>`. This is the single
  // string the entry is known by throughout the compile: passed to `to_lifted`
  // as the module's identity, used as the importer key when canonicalising
  // its own imports, and kept out of the linker's dep table (the entry is
  // linked as `@fink/user`, not under its canonical URL).
  let entry_dir = entry_path
    .parent()
    .ok_or_else(|| format!("entry path has no parent directory: {}", entry_path.display()))?
    .to_path_buf();
  let entry_file_name = entry_path
    .file_name()
    .ok_or_else(|| format!("entry path has no file name: {}", entry_path.display()))?
    .to_str()
    .ok_or_else(|| format!("entry file name is not valid UTF-8: {}", entry_path.display()))?;
  let entry_canonical_url = format!("./{entry_file_name}");

  // Compile the entry module first to discover its imports.
  let entry_source = loader.load(entry_path)?;
  let (entry_lifted, entry_desugared) = crate::to_lifted(&entry_source, &entry_canonical_url)?;
  let entry_raw_imports = entry_lifted.result.module_imports.clone();

  // Canonicalise the entry's imports. These become the BFS seeds and the
  // `url_rewrite` map the emitter uses to translate the entry's own
  // `BuiltIn::Import` Lit::Str URLs to canonical form.
  let entry_url_rewrite: BTreeMap<String, String> = entry_raw_imports
    .keys()
    .map(|raw| (raw.clone(), canonicalise_url(&entry_canonical_url, raw)))
    .collect();

  // Compile the entry module into a WASM fragment. `compile_fragment`
  // applies the rewrite map to both `module_imports` keys and the
  // emitter's `url_rewrite` side-channel, so the entry's WASM import
  // section and global labels use canonical URLs throughout.
  let entry_output = compile_fragment(
    &entry_lifted,
    &entry_desugared,
    &entry_canonical_url,
    &entry_source,
    &entry_url_rewrite,
  );
  let entry_fragment = entry_output.wasm;
  // Mark PCs are fragment-local at this point (absolute offsets into
  // `entry_fragment`). The link-time shift is applied below, after
  // `link()` produces the final code-section layout.
  let entry_mark_records = entry_output.mark_records;

  // Walk all transitive deps, keyed on canonical URL so that two
  // consumers reaching the same file via different relative paths dedup
  // to a single fragment. During the walk we build:
  //
  //   `compiled`: canonical_url → wasm_bytes
  //   `dep_edges`: canonical_url → list of dep canonical URLs it imports
  //
  // The BFS walk order is only used to drive the *compile* schedule; the
  // *link* order is derived from `dep_edges` via a post-order DFS from
  // the entry module so that every dep is emitted after all of its own
  // transitive deps, regardless of BFS discovery order.
  //
  // Why not just reverse BFS order? Reversed-BFS happens to produce a
  // valid topological sort only when BFS first-visit order matches a
  // reverse-topological order — which breaks as soon as a parent imports
  // a transitive dep directly (e.g. entry imports both `./util.fnk` and
  // `./helpers.fnk`, where `helpers.fnk` itself imports `./util.fnk`).
  // BFS discovers `util` before `helpers`, so reversing puts `util`
  // after `helpers` and `helpers`'s init reads an uninitialised global.
  // A proper post-order DFS avoids this.
  let mut compiled: BTreeMap<String, Vec<u8>> = BTreeMap::new();
  let mut dep_edges: BTreeMap<String, Vec<String>> = BTreeMap::new();

  // Record entry's own dep edges so the post-order walk below can start
  // from the entry's imports in a stable, source-declared order.
  let entry_direct_deps: Vec<String> = entry_raw_imports
    .keys()
    .map(|raw| entry_url_rewrite.get(raw).cloned().unwrap_or_else(|| raw.clone()))
    .collect();

  // BFS compile schedule. Queue order doesn't affect link order; it
  // just ensures each module is compiled once.
  let mut queue: VecDeque<String> = VecDeque::new();
  for canonical_url in &entry_direct_deps {
    queue.push_back(canonical_url.clone());
  }

  let mut visited: BTreeSet<String> = BTreeSet::new();
  while let Some(dep_canonical_url) = queue.pop_front() {
    if visited.contains(&dep_canonical_url) {
      continue;
    }
    visited.insert(dep_canonical_url.clone());

    // Canonical URL is entry-relative → the dep's disk path is
    // `entry_dir.join(<canonical_url without leading ./>)`.
    let dep_abs_path = resolve_canonical_to_disk(&entry_dir, &dep_canonical_url);

    let dep_source = loader.load(&dep_abs_path)?;
    let (dep_lifted, dep_desugared) = crate::to_lifted(&dep_source, &dep_canonical_url)?;
    let dep_raw_imports = dep_lifted.result.module_imports.clone();

    // Build the dep's rewrite map from its raw import URLs.
    let dep_url_rewrite: BTreeMap<String, String> = dep_raw_imports
      .keys()
      .map(|raw| (raw.clone(), canonicalise_url(&dep_canonical_url, raw)))
      .collect();

    // Record this dep's outgoing edges (in source-declared order) for
    // the post-order DFS below.
    let dep_direct: Vec<String> = dep_raw_imports
      .keys()
      .map(|raw| dep_url_rewrite.get(raw).cloned().unwrap_or_else(|| raw.clone()))
      .collect();
    dep_edges.insert(dep_canonical_url.clone(), dep_direct.clone());

    let dep_output = compile_fragment(
      &dep_lifted,
      &dep_desugared,
      &dep_canonical_url,
      &dep_source,
      &dep_url_rewrite,
    );
    // Dep fragments produce their own debug-mark records, but Step 3
    // ships single-module-only — multi-module needs the link-time PC
    // shift before dep records are usable. Drop them for now; the
    // emitter still computes them, so wiring the shift later is
    // additive (see the multi-module TODO at the end of this function).
    let _ = dep_output.mark_records;

    // Enqueue transitive deps by their canonical URLs.
    for transitive_canonical in &dep_direct {
      if !visited.contains(transitive_canonical) {
        queue.push_back(transitive_canonical.clone());
      }
    }

    compiled.insert(dep_canonical_url.clone(), dep_output.wasm);
  }

  // Topological sort: post-order DFS starting from the entry's direct
  // imports. Each dep is appended to `topo_order` only after all of its
  // transitive deps have been appended, so link inputs are in a valid
  // provider-before-consumer order.
  //
  // Cycles are not expected today — the language does not yet support
  // circular imports — but the `in_progress` set guards against a
  // malformed graph by breaking cycles arbitrarily (we simply skip a
  // back-edge). A proper diagnostic should land alongside cycle support.
  let mut topo_order: Vec<String> = Vec::new();
  let mut done: BTreeSet<String> = BTreeSet::new();
  let mut in_progress: BTreeSet<String> = BTreeSet::new();
  // Iterative DFS with an explicit frame stack: (node, iter over children).
  // Each frame advances one child at a time; when all children are done
  // the frame's node is appended to `topo_order` (post-order).
  enum Frame {
    Enter(String),
    Exit(String),
  }
  let mut stack: Vec<Frame> = Vec::new();
  for dep in &entry_direct_deps {
    stack.push(Frame::Enter(dep.clone()));
  }
  while let Some(frame) = stack.pop() {
    match frame {
      Frame::Enter(node) => {
        if done.contains(&node) || in_progress.contains(&node) {
          continue;
        }
        in_progress.insert(node.clone());
        // Push Exit first so it runs after all children.
        stack.push(Frame::Exit(node.clone()));
        if let Some(children) = dep_edges.get(&node) {
          // Push children in reverse so the leftmost is popped first —
          // preserves source-declared order in the final post-order.
          for child in children.iter().rev() {
            if !done.contains(child) && !in_progress.contains(child) {
              stack.push(Frame::Enter(child.clone()));
            }
          }
        }
      }
      Frame::Exit(node) => {
        in_progress.remove(&node);
        if done.insert(node.clone()) {
          topo_order.push(node);
        }
      }
    }
  }

  // Link: @fink/runtime + deps (in post-order: providers before
  // consumers) + entry.
  //
  // The linker preserves fragment order in its export section, so the
  // linked binary's `<url>:fink_module` exports end up in init order —
  // the runner iterates them in declaration order without needing a
  // separate dep-graph side channel.
  static RUNTIME_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime.wasm"));

  let mut link_inputs: Vec<LinkInput> = vec![
    LinkInput { module_name: "@fink/runtime".into(), wasm: RUNTIME_WASM.to_vec() },
  ];

  for canonical_url in &topo_order {
    if let Some(dep_wasm) = compiled.remove(canonical_url) {
      link_inputs.push(LinkInput {
        module_name: canonical_url.clone(),
        wasm: dep_wasm,
      });
    }
  }

  // Snapshot the entry fragment's per-body byte ranges before handing
  // it to the linker — `shift_marks_to_linked` needs them to map each
  // mark's intra-body offset to the rewritten body in the linked
  // binary. We can't borrow `entry_fragment` after the move below.
  let entry_bodies = body_ranges(&entry_fragment);

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
  //
  // TODO(debug_marks): dep-fragment marks are dropped during the BFS walk
  // above — the link-time shift below only handles the entry fragment.
  // Multi-module DAP support needs the same per-body shift extended to
  // every fragment in `compiled`.

  // Shift the entry's mark PCs from fragment-local to linked-binary
  // coordinates. The linker preserves function order across fragments
  // and the entry is always last, so the entry's bodies are the last
  // K bodies in the linked binary (K = `entry_bodies.len()`).
  // `rewrite_body` may change body byte-lengths (LEB128 of remapped
  // function indices, string-literal peephole rewrites) so a uniform
  // shift won't do — each body gets its own.
  let marks = shift_marks_to_linked(
    entry_mark_records,
    &entry_bodies,
    &linked.wasm,
  );

  Ok(crate::passes::Wasm {
    binary: linked.wasm,
    mappings: vec![],
    marks,
  })
}

/// Extract every code-section body's absolute byte range from a WASM
/// binary. Used to map mark PCs between fragment and linked layouts.
#[cfg(feature = "compile")]
fn body_ranges(bytes: &[u8]) -> Vec<(u32, u32)> {
  use wasmparser::{Parser, Payload};
  let mut out = Vec::new();
  for p in Parser::new(0).parse_all(bytes) {
    if let Ok(Payload::CodeSectionEntry(b)) = p {
      out.push((b.range().start as u32, b.range().end as u32));
    }
  }
  out
}

/// Translate fragment-local mark PCs into linked-binary PCs by remapping
/// each mark to its corresponding body in the linked binary.
///
/// Assumes the entry fragment's K function bodies are the **last** K
/// bodies in the linked binary's code section — the linker enforces this
/// by always placing `@fink/user` last in `link_inputs`.
///
/// Marks whose PC doesn't fall inside any entry body (shouldn't happen
/// in practice — emit only records inside bodies) are dropped.
///
/// Within a body, the intra-body offset is preserved. `rewrite_body`
/// can shift instruction boundaries by a few LEB128 bytes; for
/// breakpoints this is "near right" — wasmtime fires at the next valid
/// boundary at or after the requested PC, which still lands the user
/// on the right source span via the `MarkRecord.source` we report
/// independently of the PC.
#[cfg(feature = "compile")]
fn shift_marks_to_linked(
  marks: Vec<crate::passes::debug_marks::MarkRecord>,
  entry_bodies: &[(u32, u32)],
  linked_wasm: &[u8],
) -> Vec<crate::passes::debug_marks::MarkRecord> {
  if marks.is_empty() {
    return marks;
  }

  let linked_bodies = body_ranges(linked_wasm);

  if entry_bodies.is_empty() || linked_bodies.len() < entry_bodies.len() {
    return marks;
  }

  let entry_in_linked: &[(u32, u32)] = &linked_bodies[linked_bodies.len() - entry_bodies.len()..];

  marks.into_iter().filter_map(|m| {
    let body_idx = entry_bodies.iter().position(|(s, e)| m.wasm_pc >= *s && m.wasm_pc < *e)?;
    let intra = m.wasm_pc - entry_bodies[body_idx].0;
    Some(crate::passes::debug_marks::MarkRecord {
      wasm_pc: entry_in_linked[body_idx].0 + intra,
      cps_id: m.cps_id,
      source: m.source,
    })
  }).collect()
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
/// Output of `compile_fragment`: the WASM bytes plus any debug-mark
/// records produced by the emitter.
///
/// The `mark_records` carry **fragment-local** absolute byte offsets —
/// i.e. offsets into `wasm`, before that fragment is merged into the
/// linked binary. `compile_package` is responsible for shifting them
/// to linked-binary coordinates (see the multi-module TODO there).
#[cfg(feature = "compile")]
struct FragmentOutput {
  wasm: Vec<u8>,
  mark_records: Vec<crate::passes::debug_marks::MarkRecord>,
}

#[cfg(feature = "compile")]
fn compile_fragment(
  lifted: &crate::passes::LiftedCps,
  desugared: &crate::passes::DesugaredAst<'_>,
  url: &str,
  src: &str,
  url_rewrite: &BTreeMap<String, String>,
) -> FragmentOutput {
  use crate::passes::wasm::{collect, dwarf, emit};

  let ir_ctx = collect::IrCtx::new(&lifted.result.origin, &desugared.ast);

  // Rewrite module_imports keys from raw source URLs to canonical URLs
  // before handing the Module to the emitter. This is the one place the
  // raw→canonical map gets applied to the map itself; the CPS IR stays
  // immutable (Lit::Str URLs remain as written in source) and the emitter
  // consults `url_rewrite` at BuiltIn::Import sites to look up the key.
  let canonical_module_imports: BTreeMap<String, Vec<String>> = lifted
    .result
    .module_imports
    .iter()
    .map(|(raw, names)| {
      let canonical = url_rewrite.get(raw).cloned().unwrap_or_else(|| raw.clone());
      (canonical, names.clone())
    })
    .collect();

  let mut module = collect::collect(
    &lifted.result.root,
    &ir_ctx,
    &lifted.result.module_locals,
    canonical_module_imports,
  );
  module.url_rewrite = url_rewrite.clone();

  let ir_ctx = ir_ctx.with_globals(module.globals.clone());

  // Debug-marker pass — decides which CpsIds the DAP stops at. The
  // emitter consults this to record fragment-local PCs alongside its
  // existing source mappings. Step 2 plumbing: marks are produced but
  // not yet aggregated into `Wasm.marks` (Step 3).
  let debug_marks = crate::passes::debug_marks::analyse(lifted, desugared);
  let mut result = emit::emit(&module, &ir_ctx, Some(&debug_marks));

  let dwarf_sections = dwarf::emit_dwarf(url, Some(src), &result.offset_mappings);
  dwarf::append_dwarf_sections(&mut result.wasm, &dwarf_sections);

  FragmentOutput { wasm: result.wasm, mark_records: result.mark_records }
}

/// Resolve a canonical (entry-relative) URL to an absolute disk path by
/// joining it with the entry module's directory. The canonical URL's
/// leading `./` is stripped so the join produces a clean path; a leading
/// `../` chain is preserved since the URL may legitimately escape the
/// entry's directory.
///
/// Purely lexical — no `fs::canonicalize`, no symlink collapse. The
/// loader is responsible for making sense of the resulting `PathBuf`.
fn resolve_canonical_to_disk(entry_dir: &Path, canonical_url: &str) -> PathBuf {
  let rest = canonical_url.strip_prefix("./").unwrap_or(canonical_url);
  entry_dir.join(rest)
}

/// Canonicalise an import URL to an entry-module-relative, lexically
/// normalised form.
///
/// The canonical form is the single string used everywhere downstream:
/// WASM import section, WAT symbol names, linker keys, dedup keys. Two
/// consumers reaching the same file via different relative paths must
/// produce the same canonical URL so the linker links the dep exactly
/// once and both consumers' imports resolve to it.
///
/// - `importer_canonical_url` is the importer's own canonical URL
///   (already entry-relative, e.g. `./sub/left.fnk`).
/// - `raw_url` is the URL string as written in the importer's source
///   (importer-relative, e.g. `./common.fnk`).
///
/// Returns the dep's canonical URL (entry-relative, e.g. `./sub/common.fnk`).
///
/// Only relative URLs (starting with `./` or `../`) are canonicalised.
/// Anything else (bare identifier, `@fink/*`, etc.) passes through
/// unchanged — the policy is "if we don't know how to resolve it, don't
/// touch it". Operates purely on strings; no filesystem access, no
/// `..`-collapsing beyond the source path. On macOS this means
/// case-insensitive clashes are the user's problem.
fn canonicalise_url(importer_canonical_url: &str, raw_url: &str) -> String {
  if !is_relative_url(raw_url) {
    return raw_url.to_string();
  }

  // Compute the importer's directory (entry-relative) by stripping the
  // final path segment from its canonical URL. An empty importer URL
  // means "the entry is its own importer" — treat directory as `.`.
  let importer_dir = importer_dir_from_canonical(importer_canonical_url);

  // Join the importer's directory with the raw URL, then lex-normalise.
  let joined = join_segments(&importer_dir, raw_url);
  normalise_segments(&joined)
}

/// A URL is "relative" if it starts with `./` or `../`. These are the
/// only forms the compiler currently understands as filesystem imports.
fn is_relative_url(url: &str) -> bool {
  url.starts_with("./") || url.starts_with("../")
}

/// Return the importer's directory as a canonical-URL prefix — the
/// importer's canonical URL with its final segment stripped. Always
/// ends with `/` (or is the bare `./` for the entry's directory).
fn importer_dir_from_canonical(importer_canonical_url: &str) -> String {
  if importer_canonical_url.is_empty() {
    return "./".to_string();
  }
  match importer_canonical_url.rfind('/') {
    Some(idx) => importer_canonical_url[..=idx].to_string(),
    None => "./".to_string(),
  }
}

/// Concatenate an importer directory (ending with `/`) and a raw URL.
/// Strips a leading `./` from the raw URL first so the result doesn't
/// grow a redundant `./` in the middle.
fn join_segments(importer_dir: &str, raw_url: &str) -> String {
  let rest = raw_url.strip_prefix("./").unwrap_or(raw_url);
  format!("{importer_dir}{rest}")
}

/// Lexically normalise a canonical URL: collapse `.` and `..` segments,
/// preserve any leading `../` chain (the URL may escape the entry's
/// directory), and always produce a leading `./` prefix so we never
/// emit a bare or absolute-looking path.
fn normalise_segments(url: &str) -> String {
  let mut stack: Vec<&str> = Vec::new();
  let mut leading_parents: usize = 0;

  for segment in url.split('/') {
    match segment {
      "" | "." => continue,
      ".." => {
        if stack.pop().is_none() {
          leading_parents += 1;
        }
      }
      other => stack.push(other),
    }
  }

  let mut out = String::new();
  if leading_parents == 0 {
    out.push_str("./");
  } else {
    for _ in 0..leading_parents {
      out.push_str("../");
    }
  }
  out.push_str(&stack.join("/"));
  out
}

#[cfg(test)]
mod tests {
  use std::path::{Path, PathBuf};

  use super::*;
  use crate::passes::modules::{FileSourceLoader, InMemorySourceLoader, SourceLoader};

  // -- canonicalise_url helper -------------------------------------------------

  #[test]
  fn canonicalise_entry_importing_sibling() {
    // Entry at ./entry.fnk imports a sibling.
    assert_eq!(canonicalise_url("./entry.fnk", "./lib.fnk"), "./lib.fnk");
  }

  #[test]
  fn canonicalise_into_subdir() {
    assert_eq!(
      canonicalise_url("./entry.fnk", "./sub/left.fnk"),
      "./sub/left.fnk",
    );
  }

  #[test]
  fn canonicalise_subdir_importing_sibling() {
    // Importer lives in ./sub/; its ./common.fnk is ./sub/common.fnk entry-relative.
    assert_eq!(
      canonicalise_url("./sub/left.fnk", "./common.fnk"),
      "./sub/common.fnk",
    );
  }

  #[test]
  fn canonicalise_parent_traversal() {
    // ./a/b/c.fnk importing ../d.fnk → ./a/d.fnk
    assert_eq!(
      canonicalise_url("./a/b/c.fnk", "../d.fnk"),
      "./a/d.fnk",
    );
  }

  #[test]
  fn canonicalise_two_paths_to_same_dep() {
    // The bug we're fixing: different relative URLs, same target.
    // left.fnk lives in ./sub/, imports ./common.fnk
    // right.fnk lives at the top, imports ./sub/common.fnk
    // Both must canonicalise to the same string.
    let left = canonicalise_url("./sub/left.fnk", "./common.fnk");
    let right = canonicalise_url("./right.fnk", "./sub/common.fnk");
    assert_eq!(left, right);
    assert_eq!(left, "./sub/common.fnk");
  }

  #[test]
  fn canonicalise_redundant_dot_slash() {
    assert_eq!(
      canonicalise_url("./entry.fnk", "./a/./b.fnk"),
      "./a/b.fnk",
    );
  }

  #[test]
  fn canonicalise_escapes_entry_dir() {
    // Importer at ./a/b.fnk imports ../../out.fnk → escapes entry → ../out.fnk
    assert_eq!(
      canonicalise_url("./a/b.fnk", "../../out.fnk"),
      "../out.fnk",
    );
  }

  #[test]
  fn canonicalise_non_relative_passes_through() {
    // @fink/* and bare identifiers are not filesystem URLs — leave alone.
    assert_eq!(canonicalise_url("./entry.fnk", "@fink/meta"), "@fink/meta");
    assert_eq!(canonicalise_url("./entry.fnk", "bare"), "bare");
  }

  #[test]
  fn canonicalise_empty_importer_is_entry() {
    // Empty importer URL = the entry is importing. Equivalent to importer
    // at the entry directory root.
    assert_eq!(canonicalise_url("", "./foo.fnk"), "./foo.fnk");
    assert_eq!(canonicalise_url("", "./sub/foo.fnk"), "./sub/foo.fnk");
  }

  // -- Rust proof-of-life tests ------------------------------------------------
  // Kept as the baseline until the .fnk tests below cover the same ground.

  #[test]
  fn compile_package_single_module_no_imports() {
    // Single module with no imports should work as before.
    let src = "foo = fn x: x * 2\nfoo";
    let mut loader = InMemorySourceLoader::single("test.fnk", src);
    let wasm = compile_package(Path::new("test.fnk"), &mut loader).unwrap();
    assert!(!wasm.binary.is_empty());
    assert!(wasm.binary.starts_with(b"\0asm"));
  }

  #[test]
  fn compile_package_populates_marks() {
    // Step 3 contract: `Wasm.marks` is non-empty for a program with
    // step-stops. PCs are fragment-local (linker shift not yet
    // applied) — this test only asserts the records flow out, not
    // that they line up with the linked binary.
    let src = "main = fn: 1 + 2";
    let mut loader = InMemorySourceLoader::single("test.fnk", src);
    let wasm = compile_package(Path::new("test.fnk"), &mut loader).unwrap();
    assert!(!wasm.marks.is_empty(), "expected at least one MarkRecord");
    for r in &wasm.marks {
      assert!(r.wasm_pc > 0);
      assert!(r.source.start.line > 0);
    }
  }

  /// Sanity check: every mark PC sits inside a code-section body in
  /// the **linked** binary (post-shift), and the count stays in single
  /// digits for a tiny program (catches policy regressions cheaply).
  #[test]
  fn compile_package_marks_land_inside_linked_bodies() {
    use wasmparser::{Parser, Payload};
    let src = "main = fn: 1 + 2";
    let mut loader = InMemorySourceLoader::single("test.fnk", src);
    let wasm = compile_package(Path::new("test.fnk"), &mut loader).unwrap();

    assert!(!wasm.marks.is_empty(), "expected at least one mark");
    assert!(
      wasm.marks.len() < 20,
      "tiny program produced {} marks — policy has regressed",
      wasm.marks.len(),
    );

    let mut bodies: Vec<(u32, u32)> = Vec::new();
    for payload in Parser::new(0).parse_all(&wasm.binary) {
      if let Ok(Payload::CodeSectionEntry(body)) = payload {
        let r = body.range();
        bodies.push((r.start as u32, r.end as u32));
      }
    }
    assert!(!bodies.is_empty(), "linked binary should have code bodies");

    let in_a_body = |pc: u32| bodies.iter().any(|(s, e)| pc >= *s && pc < *e);
    for r in &wasm.marks {
      assert!(
        in_a_body(r.wasm_pc),
        "mark pc={} lands outside any linked code body — link-time shift broken",
        r.wasm_pc,
      );
    }

    // Sanity bound on a richer program: a 3-statement function body
    // produces a single-digit number of marks. Catches policy
    // regressions.
    let bigger = "\
main = fn:
  a = 1 + 2
  b = a * 3
  b
";
    let mut loader2 = InMemorySourceLoader::single("test.fnk", bigger);
    let wasm2 = compile_package(Path::new("test.fnk"), &mut loader2).unwrap();
    assert!(
      wasm2.marks.len() < 15,
      "3-stmt fn produced {} marks — policy may have regressed",
      wasm2.marks.len(),
    );
  }

  #[test]
  fn compile_package_two_modules() {
    // entry.fnk imports foo from lib.fnk and calls it.
    let lib_src = "foo = fn x: x * 2";
    let entry_src = "{foo} = import './lib.fnk'\nresult = foo 21";

    let mut loader = InMemorySourceLoader::new();
    loader.add("./lib.fnk", lib_src);
    loader.add("./entry.fnk", entry_src);

    let wasm = compile_package(Path::new("./entry.fnk"), &mut loader).unwrap();
    assert!(!wasm.binary.is_empty());
    assert!(wasm.binary.starts_with(b"\0asm"));

    // The linked binary should export both fink_module (entry) and
    // "./lib.fnk:fink_module" (dep).
    let wat = wasmprinter::print_bytes(&wasm.binary).unwrap();
    assert!(wat.contains("\"fink_module\""), "missing entry fink_module export");
    assert!(wat.contains("\"./lib.fnk:fink_module\""), "missing dep fink_module export");
  }

  #[test]
  fn compile_package_diamond_two_raw_paths_same_dep() {
    // Diamond: entry → left, entry → right, both → common.fnk via
    // *different* raw relative URLs that canonicalise to the same
    // `./sub/common.fnk`. Verifies:
    //   (a) common.fnk is linked exactly once,
    //   (b) both consumers' imports resolve to the shared definition,
    //   (c) the linked binary still validates as WASM.
    let common_src = "base = 10";
    let left_src = "{base} = import './common.fnk'\nleft_val = base + 1";
    // Right deliberately uses a twisted raw path to the SAME file.
    let right_src = "{base} = import '../sub/common.fnk'\nright_val = base + 2";
    let entry_src = "\
{left_val} = import './sub/left.fnk'
{right_val} = import './sub/right.fnk'
total = left_val + right_val
";

    let mut loader = InMemorySourceLoader::new();
    loader.add("./sub/common.fnk", common_src);
    loader.add("./sub/left.fnk", left_src);
    loader.add("./sub/right.fnk", right_src);
    loader.add("./entry.fnk", entry_src);

    let wasm = compile_package(Path::new("./entry.fnk"), &mut loader).unwrap();
    assert!(wasm.binary.starts_with(b"\0asm"));

    let wat = wasmprinter::print_bytes(&wasm.binary).unwrap();

    // common.fnk must appear EXACTLY once as a defined fink_module
    // function and exactly once as an export.
    //
    // `wasmprinter` writes func defs with a `(func $…` prefix followed by a
    // space — exports write `(func $…)` with a trailing paren. Matching on
    // `(func $./sub/common.fnk:fink_module ` (trailing space) counts only
    // definitions.
    let common_func_defs = wat
      .matches("(func $./sub/common.fnk:fink_module ")
      .count();
    assert_eq!(
      common_func_defs, 1,
      "common.fnk:fink_module should be defined exactly once; WAT:\n{wat}"
    );
    let common_exports = wat.matches("\"./sub/common.fnk:fink_module\"").count();
    assert_eq!(
      common_exports, 1,
      "common.fnk:fink_module must be exported exactly once"
    );

    // There should be exactly one defined `base` global for common.fnk.
    let common_base_globals = wat
      .matches("global $./sub/common.fnk:base")
      .count();
    assert_eq!(
      common_base_globals, 1,
      "common.fnk:base global should be defined once"
    );

    // Both left and right fragments must be present.
    assert!(
      wat.contains("func $./sub/left.fnk:fink_module"),
      "left.fnk:fink_module missing"
    );
    assert!(
      wat.contains("func $./sub/right.fnk:fink_module"),
      "right.fnk:fink_module missing"
    );
  }

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

    // Format WASM → WAT with native source map. Structural locs are not
    // yet plumbed through compile_package (see the TODO in
    // compile_package about multi-module source mappings), so we pass an
    // empty slice for now — the WAT is still correct, just with fewer
    // mapping anchors.
    let _ = src;
    let (wat_output, wat_srcmap) = crate::passes::wasm::fmt::format_mapped_native(
      &wasm.binary, &[],
    );
    let wat_b64 = wat_srcmap.encode_base64url();

    format!("{}\n;; sm:{wat_b64}", wat_output.trim())
  }

  test_macros::include_fink_tests!("src/passes/wasm-link/test_multi_module.fnk");
}
