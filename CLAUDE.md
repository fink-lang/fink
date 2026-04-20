# Fink Compiler

Fink is a functional programming language and compiler toolchain, built in Rust. It is a refined successor to the original [Fink](https://github.com/fink-lang) (which compiled to JS and was self-hosted). Long-term goal: self-hosting, targeting WASM.

## Project Structure

See [src/README.md](src/README.md) for the current source map — it's authoritative and easy to scan. This top-level file doesn't mirror it.

High level: compiler implementation lives under [src/](src/) (per-subsystem READMEs next to the code, design contracts as sibling `*.md` files); the `include_fink_tests!` proc macro lives in [crates/test-macros/](crates/test-macros/); language-level docs live under [docs/](docs/); doc-writing conventions at [docs/docs-conventions.md](docs/docs-conventions.md); contributor entry point at [CONTRIBUTING.md](CONTRIBUTING.md).

## Language Design Goals

- Ergonomic and consistent
- Practical functional (FP principles, not dogmatic)
- Immutable values by default
- Inferred static typing — Hindley-Milner style; annotations discouraged
- Types used primarily for: protocol implementations and pattern matching
- Indentation-based blocks (significant whitespace)
- Targets WASM (primary), others TBD
- Tooling generates interface files from inferred types (like OCaml `.mli` / TS `.d.ts`)

## File Extension

`.fnk` (may change)

## Comment Syntax

```
---
block comment
---

# end-of-line comment
```

## Key Syntax Reference

See [docs/language.md](docs/language.md) for the authoritative user-facing syntax reference and [docs/roadmap.md](docs/roadmap.md) for designed-but-unshipped features.

## Implementation Notes

- Uses Pratt parser.
- Pipeline: `parse → desugar (partial + scopes) → lower (CPS) → lift (unified closure + cont lifting) → compile_package (collect → emit → DWARF → link)`. See [src/passes/README.md](src/passes/README.md) for the per-stage chain and [src/passes/ast/arena-contract.md](src/passes/ast/arena-contract.md) / [src/passes/cps/transform-contract.md](src/passes/cps/transform-contract.md) for the contracts each pass must uphold.
- Flag before implementing anything that requires decisions on: protocols vs typeclasses, nominal vs structural typing.

## Rust Conventions

- Edition 2024
- Prefer `Edit` over `Write` for existing files

## Testing Conventions

- Tests live in the file that implements the feature (`#[cfg(test)] mod tests` at the bottom), or in a sibling `.fnk` file loaded via `test_macros::include_fink_tests!("path/to/tests.fnk")`.
- Never put tests for module A inside module B.
- **Bug workflow**: when investigating a bug, first write a failing test that reproduces it — don't dive into the code before you have a repro test.

## Code Style

Prefer named builder helpers over ad-hoc inline construction. When a
pattern recurs (e.g. building an AST node, synthesizing a continuation,
constructing a test fixture), extract it into a small named function so
the call site reads like a DSL rather than construction code.

## No Stringly-Typed Logic

Strings are for input parsing and output generation only — never for internal logic.

- **Parsing**: strings are read from source and immediately converted to typed representations (enums, counters, slices).
- **Internal code**: all branching and data representation uses types — enums, `u32` counters, source slices (`&'src str`). Never inspect string content (e.g. `starts_with`, `contains`, matching on string values) to make decisions.
- **Output**: strings are only materialized at format/codegen time.

If you find yourself switching on a string value or inspecting a string prefix to derive meaning, that distinction belongs in a typed enum variant instead.
