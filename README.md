# ƒink

A functional programming language and compiler toolchain, written in Rust and
targeting WebAssembly.

ƒink is a refined successor to the [original fink](https://github.com/fink-lang)
(which compiled to JavaScript and was self-hosted). The long-term goal is a
self-hosting compiler targeting WASM.

> **Status:** early and experimental. Language, syntax, and tooling are all
> subject to change.

## Documentation

Full documentation, a language tour, and an in-browser playground live at
**[fink-lang.org](https://fink-lang.org/)**.

## Installation

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
make deps-install   # fetch pinned dependencies
make build          # cargo build --release
make test           # run the test suite
make release        # build cross-target release tarballs
```

## License

[MIT](LICENSE) © fink-lang
