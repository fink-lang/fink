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

## Formatter Style (`cps_fmt.rs`)

Prefer named builder helpers over inline `node(NodeKind::...)` calls.
Every new output construct gets a small named function (e.g. `scope_fn`,
`cont_fn`, `id_tag`). The goal is for `to_node` to read like a DSL, not
like AST construction code.
