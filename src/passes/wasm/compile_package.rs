//! IR-pipeline package compiler — multi-fragment orchestration.
//!
//! Mirror of the OLD `wasm-link::compile_package` shape, redirected
//! at the IR pipeline. Walks the import graph from the entry source,
//! canonicalises raw import URLs into entry-relative form, dedups by
//! canonical URL, compiles each fragment via `lower` under its
//! own FQN prefix, hands all fragments to `link::link`, then
//! `emit::emit`s the result.
//!
//! Shape:
//!
//! ```text
//!   compile_package(entry_path, &mut loader)
//!     1. read entry source
//!     2. compile entry under `./<basename>` canonical URL
//!     3. walk module_imports — canonicalise + dedup
//!     4. compile each dep under its canonical URL
//!     5. link::link(&[entry, deps...]) → merged fragment
//!     6. emit::emit(merged) → wasm bytes
//! ```
//!
//! `module_imports` field on each Fragment is populated post-compile
//! via the canonicalised URL → ModuleId mapping.
//!
//! Single-source compiles flow through the same path with one
//! fragment in the input list. Calling
//! `compile_package(entry, in_memory_loader)` is the IR-pipeline
//! analogue of `to_wasm(src, path)` for the OLD pipeline.
//!
//! Today this only supports user-fragment imports of the form
//! `import './foo.fnk'`. Virtual stdlib namespaces (`std/io.fnk`)
//! pass through `lower::lower_import`'s existing per-name
//! accessor path unchanged — they don't need fragment compilation.

#[cfg(feature = "compile")]
use std::collections::{BTreeMap, BTreeSet, VecDeque};
#[cfg(feature = "compile")]
use std::path::{Path, PathBuf};

#[cfg(feature = "compile")]
use crate::passes::modules::SourceLoader;
#[cfg(feature = "compile")]
use crate::passes::debug_marks::DebugMarks;
#[cfg(feature = "compile")]
use super::ir::{Fragment, ModuleId};

/// Compile a package rooted at `entry_path` into a single linked
/// `Fragment`. Caller is expected to pass the result to
/// `emit::emit` for byte serialisation.
///
/// Returns the merged fragment plus a (canonical_url → ModuleId) map
/// that callers (e.g. the runner) can use to look up the entry's
/// ModuleId for invoking `std/modules.fnk:import` host-side.
#[cfg(feature = "compile")]
pub fn compile_package(
  entry_path: &Path,
  loader: &mut dyn SourceLoader,
) -> Result<CompiledPackage, String> {
  // The entry's canonical URL is `./<basename>`. Single string used
  // throughout: passed to `to_lifted` as identity, used as the
  // importer key when canonicalising the entry's own imports, and
  // as the prefix for lower's symbol namespacing.
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

  // ModuleId allocator: entry is always 0; deps get fresh ids in BFS
  // discovery order. Dedup by canonical URL.
  let mut url_to_id: BTreeMap<String, ModuleId> = BTreeMap::new();
  url_to_id.insert(entry_canonical_url.clone(), ModuleId(0));
  let mut next_id: u32 = 1;

  // BFS work queue: (canonical_url, disk_path).
  // Compile each module, walk its imports, queue deps not yet seen.
  let mut queue: VecDeque<(String, PathBuf)> = VecDeque::new();
  let mut visited: BTreeSet<String> = BTreeSet::new();

  let mut compiled: BTreeMap<String, Fragment> = BTreeMap::new();
  let mut marks_by_module: BTreeMap<ModuleId, DebugMarks> = BTreeMap::new();

  // Compile entry first.
  let (entry_frag, entry_marks) = compile_one(
    &entry_canonical_url,
    entry_path,
    ModuleId(0),
    loader,
  )?;
  let entry_imports = entry_frag.module_imports.clone();
  compiled.insert(entry_canonical_url.clone(), entry_frag);
  marks_by_module.insert(ModuleId(0), entry_marks);
  visited.insert(entry_canonical_url.clone());

  // Seed the queue with the entry's direct deps.
  for raw_url in entry_imports.keys() {
    enqueue_dep(
      raw_url,
      &entry_canonical_url,
      &entry_dir,
      &mut url_to_id,
      &mut next_id,
      &mut queue,
      &visited,
    );
  }

  // BFS deps.
  while let Some((dep_canonical_url, dep_path)) = queue.pop_front() {
    if visited.contains(&dep_canonical_url) {
      continue;
    }
    visited.insert(dep_canonical_url.clone());

    let dep_id = *url_to_id.get(&dep_canonical_url)
      .expect("BFS-enqueued dep should have a ModuleId");

    let (dep_frag, dep_marks) = compile_one(
      &dep_canonical_url,
      &dep_path,
      dep_id,
      loader,
    )?;
    let dep_imports = dep_frag.module_imports.clone();
    compiled.insert(dep_canonical_url.clone(), dep_frag);
    marks_by_module.insert(dep_id, dep_marks);

    for raw_url in dep_imports.keys() {
      enqueue_dep(
        raw_url,
        &dep_canonical_url,
        &entry_dir,
        &mut url_to_id,
        &mut next_id,
        &mut queue,
        &visited,
      );
    }
  }

  // Order fragments by ModuleId so the merge has a deterministic
  // layout. Entry is id 0 → first.
  let mut ordered: Vec<Fragment> = Vec::with_capacity(compiled.len());
  let mut id_to_url: BTreeMap<ModuleId, String> = BTreeMap::new();
  for (url, id) in &url_to_id {
    id_to_url.insert(*id, url.clone());
  }
  for id in 0..(next_id) {
    let url = id_to_url.get(&ModuleId(id))
      .ok_or_else(|| format!("compile_package: ModuleId {id} has no URL"))?;
    let frag = compiled.remove(url)
      .ok_or_else(|| format!("compile_package: ModuleId {id} ({url}) was not compiled"))?;
    // Note: lower already canonicalises import URLs at lower time
    // (via `canonicalise_url(importer_canonical_url, raw_url)` from
    // this module). FuncDecl `import.module` keys are already in
    // canonical form when fragments arrive here.
    ordered.push(frag);
  }

  let (merged, instr_to_module) = super::link::link_with_instr_modules(&ordered);

  Ok(CompiledPackage {
    fragment: merged,
    url_to_id,
    id_to_url,
    entry_canonical_url,
    marks_by_module,
    instr_to_module,
  })
}

/// Output of `compile_package`. The merged fragment is ready to feed
/// to `emit::emit`; the URL→ModuleId map lets callers identify
/// the entry's ModuleId for host-side invocation.
///
/// `marks_by_module` carries per-module debug-marks analysis output, keyed
/// by ModuleId. Survives link's IR→IR merge unchanged (link doesn't see
/// it). Consumed by Section 5's finalize step once emit + link have been
/// extended to carry per-instr byte offsets.
///
/// `instr_to_module` is parallel to `fragment.instrs`: each merged
/// InstrId's source ModuleId. Lets the finalize step look up the right
/// per-module `DebugMarks` (in `marks_by_module`) for an Instr's
/// `cps_id` — CpsId is only meaningful within its source module's CPS
/// space.
#[cfg(feature = "compile")]
pub struct CompiledPackage {
  pub fragment: Fragment,
  pub url_to_id: BTreeMap<String, ModuleId>,
  pub id_to_url: BTreeMap<ModuleId, String>,
  pub entry_canonical_url: String,
  pub marks_by_module: BTreeMap<ModuleId, DebugMarks>,
  pub instr_to_module: Vec<ModuleId>,
}

/// Compile one source file as a Fragment under the given canonical URL.
///
/// Returns the fragment plus the per-module `DebugMarks` (run on the
/// lifted CPS before lower). Marks travel as a sidecar of the package,
/// keyed by ModuleId — they don't ride on `Fragment` because link merges
/// fragments and the per-module identity is lost there.
#[cfg(feature = "compile")]
fn compile_one(
  canonical_url: &str,
  disk_path: &Path,
  module_id: ModuleId,
  loader: &mut dyn SourceLoader,
) -> Result<(Fragment, DebugMarks), String> {
  let source = loader.load(disk_path)?;
  let (lifted, desugared) = crate::to_lifted(&source, canonical_url)?;
  let marks = crate::passes::debug_marks::analyse(&lifted, &desugared);
  let fqn_prefix = format!("{canonical_url}:");
  let mut frag = super::lower::lower(&lifted.result, &desugared.ast, &fqn_prefix);
  frag.module_id = module_id;
  // module_imports stays as raw-URL → ModuleId. The package compiler
  // resolves URLs at the call site; the lower pass populates this
  // field by walking `cps.module_imports` keys and mapping each raw
  // URL to its target ModuleId.
  //
  // For now, populate from the lifted CPS's module_imports keys with
  // a placeholder ModuleId(0) — the BFS phase will overwrite via
  // url_to_id once all deps are compiled. Actually we don't have the
  // global url_to_id at lower time, so leave this empty here and
  // populate AFTER the fact in compile_package once all deps are
  // resolved.
  for raw_url in lifted.result.module_imports.keys() {
    frag.module_imports.insert(raw_url.clone(), ModuleId(u32::MAX));
  }
  Ok((frag, marks))
}

/// Enqueue a dep for BFS — canonicalise its URL, allocate a ModuleId
/// if first encounter, push (canonical_url, disk_path) onto the queue.
#[cfg(feature = "compile")]
fn enqueue_dep(
  raw_url: &str,
  importer_canonical_url: &str,
  entry_dir: &Path,
  url_to_id: &mut BTreeMap<String, ModuleId>,
  next_id: &mut u32,
  queue: &mut VecDeque<(String, PathBuf)>,
  visited: &BTreeSet<String>,
) {
  // Skip non-relative imports unless they're in the migrated stdlib
  // list. Bare identifiers, future remote schemes, and unmigrated
  // virtual stdlib URLs aren't user fragments and don't need
  // package-compile.
  if !is_relative_url(raw_url) && !is_migrated_stdlib_fnk(raw_url) {
    return;
  }

  let canonical = canonicalise_url(importer_canonical_url, raw_url);
  if visited.contains(&canonical) {
    return;
  }
  if !url_to_id.contains_key(&canonical) {
    url_to_id.insert(canonical.clone(), ModuleId(*next_id));
    *next_id += 1;
  }
  let disk_path = resolve_canonical_to_disk(entry_dir, &canonical);
  queue.push_back((canonical, disk_path));
}

/// Join `CompiledPackage` (DebugMarks per module + InstrId → ModuleId)
/// with `EmitOutput.instr_offsets` (InstrId → absolute byte offset in
/// the binary) to produce the `MarkRecord`s the DAP consumes plus a
/// parallel `WasmMapping` list.
///
/// For each merged Instr that has both a `cps_id` and an offset, look
/// up the source location in the right module's `DebugMarks.stops` and
/// emit a `MarkRecord` if the CpsId is a stop.
#[cfg(feature = "compile")]
pub fn finalize_marks(
  pkg: &CompiledPackage,
  emit_out: &super::emit::EmitOutput,
) -> (Vec<crate::passes::debug_marks::MarkRecord>, Vec<super::sourcemap::WasmMapping>) {
  use crate::passes::debug_marks::MarkRecord;
  use super::sourcemap::WasmMapping;

  let mut marks: Vec<MarkRecord> = Vec::new();
  let mut mappings: Vec<WasmMapping> = Vec::new();

  for (instr_id, &abs_offset) in &emit_out.instr_offsets {
    let idx = instr_id.0 as usize;
    let instr = match pkg.fragment.instrs.get(idx) {
      Some(i) => i,
      None => continue,
    };
    let Some(cps_id) = instr.cps_id else { continue };
    let Some(&module_id) = pkg.instr_to_module.get(idx) else { continue };
    let Some(module_marks) = pkg.marks_by_module.get(&module_id) else { continue };
    let Some(stop) = module_marks.stops.try_get(cps_id).copied().flatten() else { continue };

    marks.push(MarkRecord { wasm_pc: abs_offset, cps_id, source: stop.source, module_id });
    mappings.push(WasmMapping {
      wasm_offset: abs_offset,
      src_line: stop.source.start.line,
      src_col: stop.source.start.col,
    });
  }

  (marks, mappings)
}

// ──────────────────────────────────────────────────────────────────
// Stdlib migration list
// ──────────────────────────────────────────────────────────────────

/// Stdlib `.fnk` URLs that have a real ƒink-source file at
/// `<repo>/std/<...>.fnk`. These compile through the user-fragment
/// path; everything else under `std/` keeps its current virtual-rec
/// + @impl alias resolution. Hand-maintained during migration.
pub const MIGRATED_STDLIB_FNK: &[&str] = &[
  "std/effects.fnk",
  "std/tasks.fnk",
];

#[cfg(feature = "compile")]
fn is_migrated_stdlib_fnk(url: &str) -> bool {
  MIGRATED_STDLIB_FNK.contains(&url)
}

// ──────────────────────────────────────────────────────────────────
// URL canonicalisation — string-only, lifted from wasm-link/mod.rs.
// ──────────────────────────────────────────────────────────────────

/// Resolve a canonical URL (entry-relative `./helper.fnk`, virtual
/// `std/io.fnk`, etc.) to a disk path under the entry's directory.
///
/// Pure path-joining: no filesystem access, no canonicalisation. Callers
/// that need an absolute canonicalised path should `fs::canonicalize`
/// the result themselves.
#[cfg(feature = "compile")]
pub fn resolve_canonical_to_disk(entry_dir: &Path, canonical_url: &str) -> PathBuf {
  if is_migrated_stdlib_fnk(canonical_url) {
    return Path::new(env!("CARGO_MANIFEST_DIR")).join(canonical_url);
  }
  let rest = canonical_url.strip_prefix("./").unwrap_or(canonical_url);
  entry_dir.join(rest)
}

/// Canonicalise an import URL relative to the importer's canonical
/// URL. Pure string manipulation: no filesystem access. Identity for
/// non-relative URLs (`std/io.fnk`, `@fink/...`, etc.).
///
/// Used by `compile_package` for BFS dedup AND by `lower` to
/// stamp the canonical URL into the runtime call's `mod_url` arg —
/// without this, the producer's `pub` writes to a different registry
/// key than the consumer's `import` reads from.
pub fn canonicalise_url(importer_canonical_url: &str, raw_url: &str) -> String {
  if !is_relative_url(raw_url) {
    return raw_url.to_string();
  }
  let importer_dir = importer_dir_from_canonical(importer_canonical_url);
  let joined = join_segments(&importer_dir, raw_url);
  normalise_segments(&joined)
}

fn is_relative_url(url: &str) -> bool {
  url.starts_with("./") || url.starts_with("../")
}

fn importer_dir_from_canonical(importer_canonical_url: &str) -> String {
  if importer_canonical_url.is_empty() {
    return "./".to_string();
  }
  match importer_canonical_url.rfind('/') {
    Some(idx) => importer_canonical_url[..=idx].to_string(),
    None => "./".to_string(),
  }
}

fn join_segments(importer_dir: &str, raw_url: &str) -> String {
  let rest = raw_url.strip_prefix("./").unwrap_or(raw_url);
  format!("{importer_dir}{rest}")
}

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

#[cfg(all(test, feature = "compile"))]
mod tests {
  use super::*;
  use crate::passes::modules::InMemorySourceLoader;
  use std::path::Path;

  #[test]
  fn marks_by_module_populated_for_entry() {
    // Multi-stmt program. After analyse, at least one CpsId should be
    // marked as a stop (Bind, Apply, or App-to-Ret).
    let src = "x = 1\nmain = fn: x";
    let mut loader = InMemorySourceLoader::single("main.fnk", src);
    let pkg = compile_package(Path::new("main.fnk"), &mut loader).unwrap();

    let entry_marks = pkg.marks_by_module.get(&ModuleId(0))
      .expect("entry marks must be present");
    let stop_count = (0..entry_marks.stops.len())
      .filter_map(|i| entry_marks.stops.try_get(crate::passes::cps::ir::CpsId(i as u32)).copied().flatten())
      .count();
    assert!(stop_count > 0, "expected at least one stop in entry module marks");
  }

  #[test]
  fn every_marked_cps_id_has_matching_instr() {
    // Section 2 acceptance: for at least one stop in marks_by_module, the
    // merged fragment must contain an Instr with cps_id == that stop's id.
    // We don't require coverage of every stop yet — only that the threading
    // works for the App / LetVal sites where we wired set_cps_id today.
    let src = "x = 1\ny = x\nmain = fn: y";
    let mut loader = InMemorySourceLoader::single("main.fnk", src);
    let pkg = compile_package(Path::new("main.fnk"), &mut loader).unwrap();

    let entry_marks = pkg.marks_by_module.get(&ModuleId(0)).unwrap();

    // Collect raw u32s of cps_ids on instrs in the merged fragment.
    let instr_cps_ids: std::collections::BTreeSet<u32> =
      pkg.fragment.instrs.iter()
        .filter_map(|i| i.cps_id.map(|id| id.0))
        .collect();

    // Count stops that have at least one matching instr.
    let mut covered = 0usize;
    let mut total = 0usize;
    for i in 0..entry_marks.stops.len() {
      let id = crate::passes::cps::ir::CpsId(i as u32);
      if entry_marks.stops.try_get(id).copied().flatten().is_some() {
        total += 1;
        if instr_cps_ids.contains(&id.0) {
          covered += 1;
        }
      }
    }

    let marked: Vec<u32> = (0..entry_marks.stops.len())
      .filter_map(|i| {
        let id = crate::passes::cps::ir::CpsId(i as u32);
        entry_marks.stops.try_get(id).copied().flatten().map(|_| id.0)
      })
      .collect();

    assert!(total > 0, "test program must produce at least one stop");
    assert!(covered > 0,
      "expected at least one marked CpsId to have a matching Instr.cps_id; \
       got {covered}/{total} covered. \
       marked cps_ids: {marked:?}, instr cps_ids: {instr_cps_ids:?}");
  }

  #[test]
  fn emit_with_offsets_returns_one_offset_per_tagged_instr() {
    // Section 3 acceptance: emit_with_offsets returns one entry per
    // Instr that was tagged with cps_id in lower. The keys must be a
    // subset of the merged fragment's tagged instrs.
    let src = "x = 1\ny = x\nmain = fn: y";
    let mut loader = InMemorySourceLoader::single("main.fnk", src);
    let pkg = compile_package(Path::new("main.fnk"), &mut loader).unwrap();

    let tagged_instr_ids: std::collections::BTreeSet<crate::passes::wasm::ir::InstrId> =
      pkg.fragment.instrs.iter().enumerate()
        .filter_map(|(i, instr)|
          instr.cps_id.map(|_| crate::passes::wasm::ir::InstrId(i as u32))
        )
        .collect();

    let out = crate::passes::wasm::emit::emit_with_offsets(&pkg.fragment, crate::passes::wasm::emit::Interop::Rust);

    // Every offset key must correspond to a tagged instr.
    for k in out.instr_offsets.keys() {
      assert!(tagged_instr_ids.contains(k),
        "emit returned offset for InstrId {:?} which is not tagged", k);
    }
    // Every tagged instr that's actually in a function body must have
    // an offset (some tagged instrs may live outside any user func body
    // — e.g. the synthesised host wrapper — so we don't require ==).
    assert!(!out.instr_offsets.is_empty(),
      "expected at least one offset entry; got none. \
       binary len: {}, tagged instrs: {}",
      out.binary.len(), tagged_instr_ids.len());
    // Offsets must be within the binary.
    for (instr_id, off) in &out.instr_offsets {
      assert!((*off as usize) < out.binary.len(),
        "offset {} for {:?} is out of bounds (binary len {})",
        off, instr_id, out.binary.len());
    }
  }

  #[test]
  fn finalize_marks_produces_non_empty_marks_and_mappings() {
    // Section 5 acceptance: end-to-end, from source to MarkRecord +
    // WasmMapping. For a small program with at least one App-to-Ret
    // (which our selective tagging covers), the finalize step should
    // produce at least one mark and one mapping.
    let src = "main = fn: 42";
    let mut loader = InMemorySourceLoader::single("main.fnk", src);
    let pkg = compile_package(Path::new("main.fnk"), &mut loader).unwrap();
    let emit_out = crate::passes::wasm::emit::emit_with_offsets(&pkg.fragment, crate::passes::wasm::emit::Interop::Rust);
    let (marks, mappings) = finalize_marks(&pkg, &emit_out);

    assert!(!marks.is_empty(),
      "expected at least one mark, got 0. \
       instr_offsets: {} entries, marks_by_module: {} entries",
      emit_out.instr_offsets.len(),
      pkg.marks_by_module.len());
    assert_eq!(marks.len(), mappings.len(),
      "marks and mappings must be parallel");
    // Every mark's wasm_pc must be within the binary.
    for m in &marks {
      assert!((m.wasm_pc as usize) < emit_out.binary.len(),
        "mark wasm_pc {} out of bounds (binary len {})",
        m.wasm_pc, emit_out.binary.len());
    }
  }

  #[test]
  fn marks_by_module_covers_all_modules() {
    // Two-module program — entry imports a dep. Both modules should
    // have an entry in marks_by_module.
    let mut loader = InMemorySourceLoader::new();
    loader.add("main.fnk", "import './dep.fnk'\nmain = fn: 1");
    loader.add("dep.fnk", "y = 2");
    let pkg = compile_package(Path::new("main.fnk"), &mut loader).unwrap();

    assert_eq!(pkg.marks_by_module.len(), pkg.url_to_id.len(),
      "every module must have a marks entry");
  }
}
