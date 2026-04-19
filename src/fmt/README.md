# `src/fmt` — canonical Fink-source pretty-printer

The Stage-2 formatter. Reads an AST, lays out a canonical version with
correct indentation / wrapping / spacing, and prints it back as Fink
source. Invoked by `fink fmt2 FILE`.

This is distinct from [`src/passes/ast/fmt.rs`](../passes/ast/fmt.rs),
which is an s-expression-style debug printer (`fink fmt FILE`). Stage-2
produces output a Fink user actually wants to read.

## Two-stage pipeline

```
raw AST  ──[layout]──►  formatted AST  ──[print]──►  String
```

- [`layout.rs`](layout.rs) — walks the input AST and produces a new
  `Ast` whose locs satisfy the formatting rules (max line width,
  indentation, wrap decisions per node kind). All formatting decisions
  happen here.
- [`print.rs`](print.rs) — takes a layout-canonical `Ast` and
  materialises it as a `String` by placing token bytes at their loc
  positions. **No formatting decisions here** — print is the identity
  observer of the loc contract.

The split means the layout pass can be tested independently of the
final string, and any future fix-it / refactor tooling can reuse the
layout output without going through string printing.

## Configuration

```rust
pub struct FmtConfig { pub max_width: u32, pub indent: u32 }
```

Defaults: 80 columns, 2-space indent. No other knobs — adding new
options is intentionally hard so the canonical layout stays canonical.

## Tests

`test_fmt.fnk` and `test_print.fnk` are `.fnk` test fixtures loaded via
[`include_fink_tests!`](../../crates/test-macros/README.md) and
exercise layout + print respectively. Run `make bless` to update
expected output after intentional formatter changes.

## Origin tracking — not wired

`print` derives source maps from the per-token `Loc.start` values — an
identity map of the formatted output. There's no map back to the
pre-layout source. If a future consumer needs that (e.g. to power a
fix-it style refactor against the original file), the layout pass
would need to carry a `PropGraph<AstId, AstId>` from output node id
back to input node id. No current consumer needs it; not built.
