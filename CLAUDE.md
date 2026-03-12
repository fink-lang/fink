# Fink Compiler

Fink is a functional programming language and compiler toolchain, built in Rust. It is a refined successor to the original [Fink](https://github.com/fink-lang) (which compiled to JS and was self-hosted). Long-term goal: self-hosting, targeting WASM.

## Project Structure

```
fink/
├── src/
│   ├── lib.rs          # shared compiler library
│   ├── lexer/
│   ├── parser/
│   ├── ast/
│   ├── codegen/
│   └── bin/
│       └── fink.rs     # main compiler driver CLI
├── docs/
│   └── examples/       # language spec by example (.fnk files)
└── CLAUDE.md
```

Source files live in `src/`, specs co-located with source. User docs in `docs/`.

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

See `docs/examples/lang features.fnk` for the authoritative syntax reference (excluding types, protocols, macros which are WIP in separate files).

### Significant topics

- **Literals**: integers (sized by value/sign), floats, decimals (`1.0d`), tagged literals (`10sec == sec 10`), strings (single-quoted, interpolation `${}`), tagged templates (`fmt'...'`), sequences `[]`, records `{}`, dicts `dict {}`, sets `set`
- **Identifiers**: UTF-8 graphemes, may include `-` and `_`
- **Operators**: arithmetic, logical (`not`/`and`/`or`/`xor`), comparison (chainable), bitwise, set operators, spread `..`/`...`, ranges `0..10` (exclusive) / `0...10` (inclusive), member access `.`/`.(expr)`, pipe `|`, partial `?`
- **Binding**: `=` (left-hand), `|=` (right-hand), full pattern matching with guards, spread, string patterns
- **Functions**: `fn args: body`, `fn match` sugar, default args, closures, higher-order, mutual recursion via forward refs at module level
- **Application**: prefix `foo bar`, nested right-to-left, multiline indented args, `;` as strong separator, postfix tagged `[1,2,3]foo`, partial `?`
- **Pipes**: `foo | bar | spam == spam (bar foo)`
- **Error handling**: `try` (unwrap or propagate), `match Ok/Err`, error chaining
- **Modules**: `{foo, bar} = import './foobar.fnk'`
- **Types** (WIP): product, sum/variant, generic, dependent, opaque, union, type spread
- **Protocols** (WIP): abstract functions, specialization per type
- **Macros** (WIP): compile-time AST manipulation
- **Async/concurrency** (WIP): `spawn`, `await_all`, implicit await on access
- **Context/effects** (WIP): `context`, `with`, `get_ctx`
- **Patterns as first-class values** (WIP)

## Implementation Notes

- Use Pratt parser (used successfully in Fink via Prattler library)
- Start with: tokenizer → parser → AST → codegen
- Flag before implementing anything that requires decisions on: protocols vs typeclasses, nominal vs structural typing

## Rust Conventions

- Edition 2024
- Prefer `Edit` over `Write` for existing files

## Testing Conventions

- Tests live in the file that implements the feature (`#[cfg(test)] mod tests` at the bottom), or in a sibling `.fnk` file loaded via `#[test_template]`.
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
