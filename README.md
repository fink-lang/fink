# ƒink

A functional programming language and compiler toolchain, written in Rust and
targeting WebAssembly.

ƒink is a refined successor to the [original fink](https://github.com/fink-lang)
(which compiled to JavaScript and was self-hosted). The long-term goal is a
self-hosting compiler targeting WASM.

> **Status:** early and experimental. Language, syntax, and tooling are all
> subject to change.

## Install

### macOS and Linux (Homebrew / Linuxbrew)

```sh
brew tap fink-lang/tap
brew install fink
```

This installs the `fink` compiler along with prebuilt `finkrt` runtimes for
all tier-1 targets (`aarch64`/`x86_64` on macOS and Linux), so
`fink compile --target <triple>` works out of the box.

### From source

ƒink builds with stable Rust (edition 2024). Standard targets are exposed via
the `Makefile`:

```sh
make deps-install              # fetch pinned dependencies
make build                     # cargo build (debug)
make test                      # run the test suite
make release                   # cargo build --release (host target)
make release-all VERSION=x.y.z # build cross-target release tarballs
```

The targets above are the only supported build entry points.

## Quickstart

```fink
# hello.fnk
main = fn:
  'hello, world'
```

```sh
fink run hello.fnk
```

See [docs/language.md](docs/language.md) for a
by-example tour of the language.

## Documentation

Two surfaces, two audiences:

- **For users** writing Fink programs: the language tour, install guide, and
  in-browser playground at **[fink-lang.org](https://fink-lang.org/)**. The
  by-example syntax reference also lives in-repo at
  [docs/language.md](docs/language.md) for now;
  upcoming work converts it to [docs/language.md](docs/) and consolidates
  it with deeper sections (formal grammar, evaluation model, terminology).
- **For contributors** modifying the compiler: start at
  [src/README.md](src/) — a source map that lists every subsystem and links
  to per-subsystem READMEs and design contracts (only those that exist; an
  absent README is a deliberate signal of an undescribed gap, not a defect).
  Project conventions live in [CLAUDE.md](CLAUDE.md); contribution basics in
  [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE) © fink-lang
