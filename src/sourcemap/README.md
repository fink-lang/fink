# `src/sourcemap` — native byte-offset source map format

The compiler's in-tree source map representation: a flat list of
`(output-byte-offset, source-byte-range)` entries with a compact
base64url codec for embedding in output.

Replaces the older Source Map v3 implementation: byte-keyed instead of
line/column-keyed, varint instead of VLQ, smaller for the small inputs
we actually handle.

## Components

- [`mod.rs`](mod.rs) — `MappedWriter`: an output-tracking writer that
  collects mappings as text is written. `mark(loc)` records a `Loc` at
  the current output byte position. The writer also tracks line/col
  for consumers (e.g. [`fmt::print`](../fmt/)) that need to know where
  the cursor sits relative to line boundaries.
- [`native.rs`](native.rs) — the canonical `Mapping` type, the
  `ByteRange` source span, and the varint + base64url codec for
  serialisation. `decode_base64url` is the inverse, used by `fink
  decode-sm` ([`src/bin/`](../bin/)) to turn a `# sm:<b64>` trailer
  back into a human-readable list.

## Where it shows up in compiler output

- **CPS pretty-printer:** emits `# sm:<b64>` at the bottom of `fink cps`
  output (and CPS-test golden files).
- **WAT formatter:** emits `;; sm:<b64>` at the bottom of `fink wat`
  output and WAT-test golden files.
- **WASM emitter:** byte-offset mappings flow through to DWARF for
  wasmtime trap messages and into `WasmMapping` for the DAP server.

## Why not Source Map v3

SMv3 was JSON-keyed, line/column-based, and required a sibling `.map`
file. For the kinds of output we generate (CPS dumps, WAT, single-file
JS-style output) the byte-offset model is smaller, embeds inline as a
trailer, and requires no separate file. SMv3 was retired wholesale in
PR #104.
