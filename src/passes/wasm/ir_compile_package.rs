//! IR-pipeline package compiler — multi-fragment orchestration.
//!
//! Mirror of the OLD `wasm-link::compile_package` shape, redirected
//! at the IR pipeline. Walks the import graph from the entry source,
//! canonicalises raw import URLs into entry-relative form, dedups by
//! canonical URL, compiles each fragment via `ir_lower` under its
//! own FQN prefix, hands all fragments to `ir_link::link`, then
//! `ir_emit::emit`s the result.
//!
//! Shape:
//!
//! ```text
//!   compile_package(entry_path, &mut loader)
//!     1. read entry source
//!     2. compile entry under `./<basename>` canonical URL
//!     3. walk module_imports — canonicalise + dedup
//!     4. compile each dep under its canonical URL
//!     5. ir_link::link(&[entry, deps...]) → merged fragment
//!     6. ir_emit::emit(merged) → wasm bytes
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
//! pass through `ir_lower::lower_import`'s existing per-name
//! accessor path unchanged — they don't need fragment compilation.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::passes::modules::SourceLoader;
use super::ir::{Fragment, ModuleId};

/// Compile a package rooted at `entry_path` into a single linked
/// `Fragment`. Caller is expected to pass the result to
/// `ir_emit::emit` for byte serialisation.
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
  // as the prefix for ir_lower's symbol namespacing.
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

  // Compile entry first.
  let entry_frag = compile_one(
    &entry_canonical_url,
    entry_path,
    ModuleId(0),
    loader,
  )?;
  let entry_imports = entry_frag.module_imports.clone();
  compiled.insert(entry_canonical_url.clone(), entry_frag);
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

    let dep_frag = compile_one(
      &dep_canonical_url,
      &dep_path,
      dep_id,
      loader,
    )?;
    let dep_imports = dep_frag.module_imports.clone();
    compiled.insert(dep_canonical_url.clone(), dep_frag);

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
      .ok_or_else(|| format!("ir_compile_package: ModuleId {id} has no URL"))?;
    let mut frag = compiled.remove(url)
      .ok_or_else(|| format!("ir_compile_package: ModuleId {id} ({url}) was not compiled"))?;

    // Canonicalise cross-fragment user-import declarations on this
    // fragment's FuncDecls. ir_lower writes them with the raw URL
    // string as written in the source (importer-relative, e.g.
    // `./foobar/spam.fnk` from inside `./test_link/simple.fnk`). The
    // linker resolves cross-fragment refs by matching against
    // producer fragments' canonical URLs (`./test_link/foobar/spam.fnk`),
    // so canonicalise here so the keys match.
    canonicalise_fragment_imports(&mut frag, url);

    ordered.push(frag);
  }

  let merged = super::ir_link::link(&ordered);

  Ok(CompiledPackage {
    fragment: merged,
    url_to_id,
    entry_canonical_url,
  })
}

/// Output of `compile_package`. The merged fragment is ready to feed
/// to `ir_emit::emit`; the URL→ModuleId map lets callers identify
/// the entry's ModuleId for host-side invocation.
#[cfg(feature = "compile")]
pub struct CompiledPackage {
  pub fragment: Fragment,
  pub url_to_id: BTreeMap<String, ModuleId>,
  pub entry_canonical_url: String,
}

/// Rewrite this fragment's cross-fragment user-import URLs from
/// importer-relative (as written in source) to entry-relative
/// canonical form, so the linker's display-name match works.
///
/// Only applies to FuncDecls with `import: Some(ImportKey { module:
/// <relative_url>, name: "fink_module" })` — i.e. the placeholder
/// imports that `ir_lower::lower_import_user_fragment` emits at every
/// `import './foo.fnk'` site. Runtime imports (`rt/*`, `std/*`,
/// `interop/*`) and virtual stdlib imports (`std/io.fnk:stdout`)
/// pass through unchanged.
#[cfg(feature = "compile")]
fn canonicalise_fragment_imports(frag: &mut Fragment, importer_canonical_url: &str) {
  for f in &mut frag.funcs {
    if let Some(key) = &mut f.import
      && key.name == "fink_module"
      && is_relative_url(&key.module)
    {
      key.module = canonicalise_url(importer_canonical_url, &key.module);
    }
  }
}

/// Compile one source file as a Fragment under the given canonical URL.
#[cfg(feature = "compile")]
fn compile_one(
  canonical_url: &str,
  disk_path: &Path,
  module_id: ModuleId,
  loader: &mut dyn SourceLoader,
) -> Result<Fragment, String> {
  let source = loader.load(disk_path)?;
  let (lifted, desugared) = crate::to_lifted(&source, canonical_url)?;
  let fqn_prefix = format!("{canonical_url}:");
  let mut frag = super::ir_lower::lower(&lifted.result, &desugared.ast, &fqn_prefix);
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
  Ok(frag)
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
  // Skip non-relative imports — virtual stdlib namespaces
  // (`std/io.fnk`), bare identifiers, future remote schemes —
  // they're not user fragments and don't need package-compile.
  if !is_relative_url(raw_url) {
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

// ──────────────────────────────────────────────────────────────────
// URL canonicalisation — string-only, lifted from wasm-link/mod.rs.
// ──────────────────────────────────────────────────────────────────

fn resolve_canonical_to_disk(entry_dir: &Path, canonical_url: &str) -> PathBuf {
  let rest = canonical_url.strip_prefix("./").unwrap_or(canonical_url);
  entry_dir.join(rest)
}

fn canonicalise_url(importer_canonical_url: &str, raw_url: &str) -> String {
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
