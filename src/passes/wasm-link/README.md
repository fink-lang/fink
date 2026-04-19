# `src/passes/wasm-link` — multi-module package compiler + linker

Drives the full multi-module compile: takes an entry `.fnk` path and a
`SourceLoader`, walks the import graph, compiles each module to a WASM
fragment, and links the fragments into a single self-contained binary.

This is what `fink run foo.fnk` and `fink compile foo.fnk` use under the
hood. Single-module compiles also flow through here — the entry module
without imports is a degenerate package of one.

## Layering

```
modules/        host-neutral SourceLoader trait (no I/O policy)
   ↓
wasm/           single-module pipeline: CPS → fragment WASM bytes
   ↓
wasm-link/      compose: load → per-unit pipeline → link
```

`wasm/` knows how to compile **one** module to a fragment. `wasm-link/`
orchestrates the package compile and runs the linker.

## Multi-module pipeline

`compile_package` does three things:

1. **Compile entry** under its canonical URL `./<basename>`.
2. **Walk transitive imports** via a work queue. For each raw import URL
   in a fragment, `canonicalise_url` converts it to canonical form
   (entry-module-relative, lexically normalised), dedups against
   already-seen canonical URLs, resolves to disk via
   `resolve_canonical_to_disk`, compiles, enqueues the dep's own
   imports.
3. **Link** the fragments in dependency order:
   `[@fink/runtime, dep1, dep2, …, entry]`.

## Canonical URLs — why they matter

Two consumers reaching the same file via different relative URLs (e.g.
`'./util.fnk'` from one source file, `'../lib/util.fnk'` from another)
produce the same canonical URL. The dep is compiled and linked exactly
once.

The CPS IR is immutable, so `BuiltIn::Import` calls keep raw `Lit::Str`
URLs as written in source. `compile_fragment` builds a raw→canonical
rewrite map (`url_rewrite`) and hands it to the emitter, which
translates the raw URL at emit time before looking it up in
`module_imports` — whose keys are also pre-rewritten to canonical form.

`resolve_canonical_to_disk` joins the canonical URL with the entry
module's directory to produce an absolute path.

## Dep init ordering

The linked binary exports each dep's `fink_module` as
`<canonical-url>:fink_module`, plus the entry's as `fink_module`. The
runner calls dep init functions **first** (in topological order via
post-order DFS over the dep edge map collected during the walk), then
the entry's `fink_module`. This populates each dep's export globals
before any consumer reads them.

> **Don't use reversed-BFS as a topological sort.** It works for chains
> but breaks on diamonds — if entry imports both `common` and `left`,
> and `left` also imports `common`, reversed-BFS may run `left` before
> `common`. Use post-order DFS (or Kahn's algorithm) over the dep edge
> map. Lesson learnt; the diamond test fixture under `test_modules/`
> guards against regressions.

## Files

- [`mod.rs`](mod.rs) — `compile_package`, `compile_fragment`, the
  canonical-URL helpers, the linker driver. Also defines the
  `ImportResolver` trait the linker uses to fetch fragments on demand.
- `test_modules/` — multi-module source-tree fixtures (each subdir is a
  small package).
- `test_multi_module.fnk` — `.fnk` test fixtures that exercise the
  package pipeline end-to-end.

## Known follow-ups

- **Source maps in `compile_package`** are currently broken (it returns
  `mappings: vec![]`); same for the WAT writer. Tracked separately;
  fixing it removes the `gen_wat_pkg_inner`/`gen_wat` duplicate and
  unblocks DAP debugging across module boundaries.
- **Helper-function dead-code elimination** for unused dep helpers is
  deferred to `wasm-opt` (post-link); see TODOs tagged `dep-helper-bodies`.
