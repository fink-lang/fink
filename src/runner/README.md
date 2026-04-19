# `src/runner` — wasmtime runner

Runs compiled Fink WASM modules under [wasmtime](https://wasmtime.dev/),
the in-process WASM engine. This is what `fink run foo.fnk` invokes
behind the scenes — and what the test fixtures in this directory exercise
end-to-end.

## What lives here

- [`mod.rs`](mod.rs) — public entry points (`run_source`, `run_file`),
  CLI argv plumbing, and IO-stream type aliases.
- [`wasmtime_runner.rs`](wasmtime_runner.rs) — the wasmtime store/instance
  setup, host-import wiring (`host_resume`, `host_panic`, `host_channel_send`,
  `host_exit`), and stdin/stdout/stderr channel construction.
- `test_*.fnk` (14 files) — end-to-end test fixtures, one per language
  area (literals, bindings, operators, functions, strings, records,
  patterns, ranges, errors, `fn match`, tasks, modules, formatting, IO).

## Two run paths

- `run_source` — compile a single string and run it (no filesystem).
  Used by tests and by the in-browser playground.
- `run_file` — read a `.fnk` (or `.wasm`) from disk and run it. This goes
  through `compile_package`, so it's the multi-module path and the one
  `fink run` uses in production.

Both call into `wasmtime_runner::run` with a final binary plus
`(stdin, stdout, stderr)` `IoStream`s. Tests use in-memory streams so
golden output is deterministic; the CLI uses real `std::io` handles.

## Where the test fixtures fit

The 14 `test_*.fnk` files at this level are loaded into a single
`#[cfg(test)] mod tests` block in `mod.rs` via
`test_macros::include_fink_tests!`. Each file holds many `test '...', fn:`
blocks; the proc macro turns each into a `#[test]` that runs the snippet
through `run_source` and golden-compares the captured stdout/stderr
against the expected block.

This is the project-wide convention: `.fnk`-based tests for any
subsystem live in **that subsystem's directory** (here, `src/runner/`),
not in a top-level `tests/`. See
[`crates/test-macros/README.md`](../../crates/test-macros/README.md) for
the macro details and the BLESS workflow.

If you're adding a runner test:

1. Pick the right existing `test_*.fnk` file (or add a new one and
   register it in `mod.rs`'s `tests` module).
2. Follow the existing `test 'name', fn: expect <helper> ƒink: ... | equals ƒink: ...`
   shape.
3. Run `make bless` to capture the actual output, then review the diff.

## Multi-module fixtures

`test_modules/` holds source trees used by `test_modules.fnk` to exercise
the multi-module path. Each subdir is a small package the runner compiles
and executes as a unit; the `.fnk` test asserts the program's output.
