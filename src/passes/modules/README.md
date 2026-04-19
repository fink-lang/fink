# `src/passes/modules` — host-neutral source loading

Defines the `SourceLoader` trait that the compiler core uses to read
module sources. Hosts provide concrete implementations:

- **`FileSourceLoader`** — wraps `std::fs`. Used by the native CLI and
  by tests that point at real files on disk.
- **`InMemorySourceLoader`** — in-memory `path → source` map. Used by
  inline-source entry points (`to_wasm(src, path)`, REPL,
  ad-hoc test sources).
- A future wasm32 / browser host would provide a callback-backed impl
  without pulling `std::fs` into the compiler core.

The loader's job is **strictly source loading**. It does not compile, it
does not understand URL schemes, it does not know about Fink. Compile
orchestration (URL canonicalisation, dep walking, fragment compilation,
linking) lives in [`../wasm-link/`](../wasm-link/), which consumes a
`SourceLoader` to fetch the source bytes it needs.

This split is what lets the compiler core stay wasm32-safe: nothing in
the per-unit pipeline reaches for `std::fs` directly. Any host-specific
I/O sits behind this trait.
