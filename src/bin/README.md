# `src/bin` — CLI entry points

Two binaries:

- [`fink.rs`](fink.rs) — the **compiler driver**. Parses CLI args and
  dispatches to one of the subcommands:

  | Subcommand | What it does |
  |---|---|
  | `fink tokens FILE` | Print the token stream (debug) |
  | `fink ast FILE` | Print the parsed AST |
  | `fink fmt FILE` | S-expression-style formatter (debug) |
  | `fink fmt2 FILE` | Canonical Fink-source formatter ([`src/fmt/`](../fmt/)) |
  | `fink cps FILE [--lifted]` | Print CPS IR (raw or lifted) |
  | `fink wat FILE` | Compile to WASM and print the linked WAT |
  | `fink run FILE [args…]` | Compile + run via wasmtime ([`src/runner/`](../runner/)) |
  | `fink dap FILE` | Launch DAP server for debugging ([`src/dap/`](../dap/)) |
  | `fink compile FILE [--target] [-o]` | Produce a `.wasm` or standalone native binary ([`src/compile/`](../compile/)) |
  | `fink decode-sm SM` | Decode a base64url native source map for inspection |
  | `fink --version` | Print version (from `Cargo.toml` via `env!("CARGO_PKG_VERSION")`) |

  Most subcommands accept `--source-map` to emit the native byte-offset
  source map alongside the primary output.

- [`finkrt.rs`](finkrt.rs) — the **standalone runtime binary**. Has the
  wasmtime runner compiled in but no compiler. Used by `fink compile
  --target=<triple>` to produce native executables: the compiler emits
  WASM, then appends those bytes to a copy of `finkrt` along with a
  magic trailer (`f1nkw4sm` + 8-byte payload offset). At runtime, `finkrt`
  reads its own tail to find the payload and runs it. See
  [`src/compile/`](../compile/) for the trailer format.

## Per-target finkrt

In packaged Homebrew releases, every supported target has its own
prebuilt `finkrt` under `<fink_dir>/targets/<triple>/finkrt`, so
`fink compile --target=<any>` works offline. In the cargo dev workflow,
the sibling `finkrt` from `target/<profile>/` is **only** valid for the
host target — cross-target compilation needs `cargo build --target=<triple>`
plus `FINK_TARGETS_DIR` pointing at the staged output.
