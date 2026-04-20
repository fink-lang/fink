# Source map

The ƒink compiler crate. [lib.rs](lib.rs) is the public entry and holds the `to_ast` / `to_cps` / `to_wasm` / `run` convenience functions; the real work lives in [passes/](passes/).

## Pipeline

```text
source → parse → desugar → lower (CPS) → lift → compile_package → WASM
```

Details, stage contracts, and per-stage READMEs in [passes/README.md](passes/README.md).

## Top-level layout

- [passes/](passes/) — the compile pipeline. Every stage is a subdirectory.
- [runtime/](runtime/) — `.wat` files merged into a runtime WASM module at build time: strings, records, lists, channels, scheduler, host bridge.
- [runner/](runner/) — the `fink run` command. Executes a compiled WASM binary under wasmtime with IO channels wired to stdin/stdout/stderr.
- [dap/](dap/) — the debug adapter protocol server behind `fink dap`.
- [compile/](compile/) — compile-side entry behind the `compile` feature.
- [fmt/](fmt/) — the Stage-2 source-code formatter (`fink fmt2`).
- [errors/](errors/) — diagnostic formatting.
- [strings/](strings/) — escape-sequence handling (bytes, not codepoints).
- [sourcemap/](sourcemap/) — `MappedWriter` (the accumulator used by `ast::fmt` and the WAT formatter), plus the native byte-offset source-map codec.
- [propgraph.rs](propgraph.rs) — `PropGraph<Id, T>`, the typed dense arena used throughout: one `Vec<Option<T>>` keyed by a newtype id, with `push` / `set` / `get` / `try_get`.
- [bin/fink.rs](bin/fink.rs) — the CLI dispatcher (`fink tokens|ast|fmt|cps|wat|run|dap …`).

## Entry points

- **Using the compiler as a library** — start at [lib.rs](lib.rs).
- **Working on a specific pass** — start at [passes/README.md](passes/README.md) and dive into the stage's subdirectory.
- **Working on the runtime or codegen ABI** — [passes/wasm/README.md](passes/wasm/README.md) and its contracts.
