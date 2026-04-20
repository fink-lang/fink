# Contributing to ƒink

## Build and test

The [Makefile](Makefile) drives everything — CI invokes the same targets.

| Command | What it does |
|---|---|
| `make deps-install` | Fetch pinned crates. Run once, and again after pulling changes. |
| `make deps-check` | Verify the Rust toolchain matches [rust-toolchain.toml](rust-toolchain.toml). |
| `make build` | `cargo build`. |
| `make test` | `cargo test` — runs the full Rust + `.fnk` test suite. |
| `make bless` | Re-bless snapshot tests. Single-threaded (proc macro writes `.fnk` files). |
| `make test-full` | `make test` + `cargo clippy -- -D warnings`. **Run this before pushing** — CI rejects clippy warnings that `make test` won't catch. |
| `make clean` | Remove build artefacts. |

### Running a single test

`cargo test <name>` matches test names. Snapshot tests are generated from `.fnk` fixtures next to each pass — edit the fixture, run `make bless`, review the diff, commit.

### Bug workflow

When investigating a bug, **write a failing test that reproduces it first**. The repro test goes in the existing `.fnk` fixture nearest the code, or in a new one if no nearby fixture fits. Do not touch the implementation until the repro test exists and fails for the reason you expect.

## Where to start

- **Source map:** [src/README.md](src/README.md) — top-level tour of the crate.
- **Pipeline and pass docs:** [src/passes/README.md](src/passes/README.md) — one README per pass, with design contracts as sibling `*.md` files.
- **Project conventions and rules:** [CLAUDE.md](CLAUDE.md) — language design goals, Rust conventions, testing conventions, code style.

A first change is usually smallest in whichever pass the bug or feature lives. Read the pass's README + contract, write a failing `.fnk` test, then change the code.

## Git workflow

- Branch off `main`.
- Commit messages follow conventional-commit style (`feat:`, `fix:`, `docs:`, `refactor:`) — they drive semantic release.
- Open a PR; CI runs `make test-full`.

## Documentation

Docs follow [docs/docs-conventions.md](docs/docs-conventions.md). Read it before writing or materially revising docs.
