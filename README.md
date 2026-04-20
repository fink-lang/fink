# ƒink

A functional programming language and compiler toolchain, written in Rust and targeting WebAssembly.

ƒink is a refined successor to the [original fink](https://github.com/fink-lang) (which compiled to JavaScript and was self-hosted). The long-term goal is a self-hosting compiler.

> **Status:** early and experimental. Language, syntax, and tooling are all subject to change.

## Install

### macOS and Linux (Homebrew / Linuxbrew)

```sh
brew tap fink-lang/tap
brew install fink
```

This installs the `fink` compiler along with prebuilt `finkrt` runtimes for all tier-1 targets (`aarch64`/`x86_64` on macOS and Linux), so `fink compile --target <triple>` works out of the box.

### From source

ƒink builds with stable Rust (edition 2024). See [CONTRIBUTING.md](CONTRIBUTING.md) for the full Makefile-driven workflow.

```sh
make deps-install
make build
make test
```

## Hello, ƒink

Save as `hello.fnk`:

```fink
main = fn args, stdin, stdout, stderr:
  'Hello, ƒink!' >> stdout
  0
```

Run it:

```sh
fink run hello.fnk
```

## Documentation

- [docs/language.md](docs/language.md) — the language reference.
- [docs/roadmap.md](docs/roadmap.md) — designed features not yet reachable.
- [CONTRIBUTING.md](CONTRIBUTING.md) — build, test, contribute.
- [fink-lang.org](https://fink-lang.org/) — the same docs rendered, with an in-browser playground.

## License

[MIT](LICENSE) © fink-lang
