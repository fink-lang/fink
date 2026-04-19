# `src/compile` — `fink compile` entry points

Two output modes for the `fink compile` subcommand:

- **`compile_to_wasm`** — runs the full pipeline (
  [`compile_package`](../passes/wasm-link/) under the hood) and writes
  the linked WASM bytes to a file. Plain WASM, runnable by anything
  that can load it.
- **`compile_to_native`** — produces a standalone native executable for
  a target triple. The trick: take a copy of the prebuilt `finkrt` for
  that target, append the compiled WASM bytes, then append a 16-byte
  magic trailer. At runtime, `finkrt` reads its own tail, finds the
  payload offset, and runs the embedded WASM.

## Trailer format

Last 16 bytes of the produced executable:

```
[u64 LE offset to payload start] [b"f1nkw4sm" magic]
```

This is the Deno-style binary-append pattern. Cheap, opaque, no
relocations needed — the OS loader sees a normal executable; finkrt
sees its own bytes plus a payload.

## Where finkrt comes from

- **Packaged releases (Homebrew):** `<fink_dir>/targets/<triple>/finkrt`,
  one per supported target — so `fink compile --target=<any>` works
  offline for every supported triple.
- **Dev (cargo build):** the sibling `finkrt` in `target/<profile>/`,
  which is **only** valid for the host target. For cross-target
  compilation in dev, run `cargo build --target=<triple>` and point
  `FINK_TARGETS_DIR` at the staged output.

The host target the running `fink` binary was compiled for is exposed
via `env!("TARGET")` (set by [`build.rs`](../../build.rs)) and used to
resolve `--target=native`.
