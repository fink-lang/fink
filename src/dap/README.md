# `src/dap` — Debug Adapter Protocol server

A DAP server that lets editors (VSCode, anything else speaking DAP)
step through Fink programs at the source level. `fink dap <file>`
launches it; the editor speaks DAP over stdin/stdout.

```
VSCode  ←DAP stdin/stdout→  fink dap  ←Wasmtime debug API→  WASM
```

## How it works

- Compile the entry module to WASM (via [`src/passes/wasm/compile`](../passes/wasm/compile/mod.rs)).
- Launch wasmtime with `guest_debug` enabled so the engine surfaces
  per-instruction breakpoints to a `DebugHandler`.
- Run the WASM in a worker thread. When a breakpoint fires, the handler
  sends frame info to the DAP server via a channel and blocks waiting
  for a resume command (continue / step-over / step-in / step-out).
- DAP server receives the frame, translates the WASM PC offset to a
  `(line, col)` in the original Fink source via the source map produced
  during compile, and emits a `Stopped` event to the editor.

## PC-to-source translation

`pc_to_source_location` walks the source-map mappings for the closest
entry at-or-below the given PC. Mappings are roughly ordered by emission
offset, so this is a linear scan today; could become binary search if it
ever shows up in profiling.

The mappings come from
[`src/passes/wasm/sourcemap.rs::WasmMapping`](../passes/wasm/sourcemap.rs)
— the same byte-offset map the formatter uses for `fink wat` output.

## Known issues

- **No multi-module source maps yet.** `compile_package` currently
  returns empty mappings (tracked in `wasm-link`); under DAP this means
  PC-to-source lookup returns `None` for every frame in any program with
  imports. Fix lives in the wasm-link cleanup, not here.
- **Stdio host imports stubbed.** The DAP server wires every `env`
  import as a "not yet implemented" trap, including `print`. Any user
  program that writes to stdout traps under DAP. Work-around: use the
  runner (`fink run`) for programs you need to actually execute; reach
  for `fink dap` only for stepping-through. Long-term fix: factor a
  shared `wire_env_imports` helper used by both [`src/runner/wasmtime_runner.rs`](../runner/wasmtime_runner.rs)
  and this module.

## Files

- [`mod.rs`](mod.rs) — DAP server, request/response handling, the
  WASM-thread `DebugHandler`, and the PC→source mapper. Single file
  today; if it grows past one module, split per the usual convention.
