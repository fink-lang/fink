# WASM

Takes lifted CPS and emits a standalone WebAssembly binary with DWARF debug info and native byte-offset source maps. The runtime (under [../../runtime/](../../runtime/)) is linked in at the same stage.

## Key files

- [collect.rs](collect.rs) — walks the lifted CPS and gathers per-module structure (`IrCtx`, `CollectedFn`, builtin table).
- [emit.rs](emit.rs) — `wasm-encoder` binary output. WasmGC types, imports, code section, name section; tracks byte offsets and structural locations for the source map.
- [dwarf.rs](dwarf.rs) — `gimli::write` line tables (`.debug_info`, `.debug_line`, `.debug_abbrev`, `.debug_str`).
- [fmt.rs](fmt.rs) — WASM → WAT formatter using `wasmparser` + `gimli::read`, for debug output and snapshot tests.
- [link.rs](link.rs) — static linker: merges the runtime, rewrites `@fink/` imports, adjusts DWARF offsets, assigns dep global slots.
- [sourcemap.rs](sourcemap.rs) — `WasmMapping` type, consumed by the DAP server.
- [builtins.rs](builtins.rs) — imported builtin function signatures.
- [compile.rs](compile.rs) — legacy WAT-text → WASM wrapper around the `wat` crate.

## Contracts and design

- [calling-convention.md](calling-convention.md) — the unified `$Fn2(captures, args)` signature, `$Closure` layout, the single `_apply` dispatch helper, call-site args-list construction, and how spread / varargs fit in.

## Entry point

Start in [collect.rs](collect.rs) to see what the CPS → WASM walk collects, then [emit.rs](emit.rs) for the actual binary encoding. Linker and debug-info concerns come after.
