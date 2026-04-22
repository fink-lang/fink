# DAP

The Debug Adapter Protocol server behind `fink dap <file>`. Speaks DAP on stdin/stdout, drives compiled ƒink programs through Wasmtime with `guest_debug` enabled, and maps WASM PCs back to source locations via debug marks.

```text
editor ←DAP stdin/stdout→ fink dap ←Wasmtime debug API→ WASM
```

## Execution model

`fink dap` compiles the entry `.fnk` file and runs it through the same `_run_main` bootstrap as the production runner (see [../runner/wasmtime_runner.rs](../runner/wasmtime_runner.rs)), but via `.call_async` so Wasmtime's async `guest_debug` mode can pause execution at breakpoints.

Every `.fnk` expression that the [debug_marks](../passes/debug_marks/mod.rs) pass identifies as a step-stop is installed as a Wasmtime breakpoint. A shared filter decides which breakpoint fires are exposed to the editor:

- **StepAny** — every mark surfaces. Used for `stopOnEntry` and all step commands.
- **ContinueUntilUserBp** — only marks whose source line is in the user-placed breakpoint set surface. Intermediate marks auto-resume inside the debug handler, invisible to the editor.

`setBreakpoints` requests populate the user-breakpoint set keyed by `(path, line)`. Lines with no matching mark come back `verified: false` so the editor greys them out.

## What works today

- Entry / gutter breakpoints resolved against debug marks.
- `Continue` runs to the next user breakpoint or termination.
- `stopOnEntry: false` runs from start to the first user breakpoint.
- `Next` / `StepIn` / `StepOut` all resume until the next mark (no call-depth-aware stepping — every ƒink call is a `return_call`, so WASM has no call stack to walk).
- Program output (`>> stdout` / `>> stderr`) surfaces in the editor's debug console via DAP `Output` events.
- Clean session end: `Exited` + `Terminated` events, process exits so the editor's UI updates.

## Known gaps

- `host_read` is stubbed — programs that read from stdin under the debugger will fail.
- No user-placed breakpoint auto-snap: a breakpoint on a line with no mark stays unverified rather than moving to the nearest valid line.
- Panic messages don't carry source locations yet.
- Runner bootstrap is duplicated from [../runner/wasmtime_runner.rs](../runner/wasmtime_runner.rs). Unifying sync and async paths is planned.

## Key files

- [mod.rs](mod.rs) — everything: the DAP loop, the debug handler, the async bootstrap, the host-import wiring.

## Related

- [../passes/debug_marks/mod.rs](../passes/debug_marks/mod.rs) — the pass that picks which CPS nodes are step-stops.
- [../passes/wasm/sourcemap.rs](../passes/wasm/sourcemap.rs) — `WasmMapping`, the legacy PC → source fallback when a stop doesn't match a mark.
- [../runner/wasmtime_runner.rs](../runner/wasmtime_runner.rs) — the production runner whose bootstrap the DAP mirrors.
- [../../docs/execution-model.md](../../docs/execution-model.md) — why every ƒink call is a tail call (CPS realisation of effects).
