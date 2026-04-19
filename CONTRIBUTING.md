# Contributing to ƒink

ƒink is an experimental language under active development. Code, design
docs, and conventions all change. The notes below are the short version
of what every contributor needs.

## Where things live

- **Code:** [`src/`](src/) — see [`src/README.md`](src/README.md) for a
  per-subsystem map. Compiler passes live under [`src/passes/`](src/passes/).
- **Design docs:** **next to the code they govern**, not in a separate
  tree. Each `src/` subsystem with non-trivial design has a sibling
  `README.md` plus deeper `.md` files (e.g. [`src/passes/ast/arena-contract.md`](src/passes/ast/arena-contract.md),
  [`src/passes/wasm/calling-convention.md`](src/passes/wasm/calling-convention.md)).
- **Language-level docs** that describe Fink rather than the
  implementation: [`docs/`](docs/). The compiler will eventually
  self-host (Fink-on-Fink), so anything here must survive an
  implementation rewrite.
- **Project conventions and rules:** [`CLAUDE.md`](CLAUDE.md). Worth
  reading before opening a non-trivial PR.

## Build and test

```sh
make deps-install   # fetch pinned dependencies (one-time after pulling)
make build          # cargo build (debug)
make test           # cargo test
make test-full      # cargo test + cargo clippy -- -D warnings
make release        # cargo build --release (host)
```

**Run `make test-full` before opening a PR.** CI gates on it (clippy
`-D warnings` is enforced); plain `make test` won't catch lints.

## Tests

- Unit tests live in the file they cover, in a `#[cfg(test)] mod tests`
  block at the bottom.
- `.fnk`-based tests live in sibling files loaded by
  `test_macros::include_fink_tests!("path/to/tests.fnk")` — see existing
  call sites for the expected layout.
- When investigating a bug, **write a failing test first** that
  reproduces it before touching the implementation.

## PRs

- Branch off `main`. Conventional-commit style for the title (e.g.
  `fix(parser): handle nested chan-op precedence`).
- Keep PRs focused; small focused PRs are reviewed faster than large
  bundled ones.
- For non-trivial changes, sketch the design in a sibling `README.md` /
  contract `.md` first; for one-off fixes, just patch the code.
