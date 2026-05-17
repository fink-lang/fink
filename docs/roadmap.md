# Roadmap

What's designed but not yet usable from ƒink source. Features listed here have some presence in the compiler or runtime; they just aren't reachable to a ƒink programmer yet.

For features that *work today*, see [language.md](language.md).

## Error handling (`try`)

`try` parses and lowers through CPS as a passthrough. The language-level semantics — `Ok` / `Err` values, propagation from the enclosing function, `match Ok / Err` patterns — aren't wired yet. Once they are, `try foo` will unwrap on `Ok` and propagate the `Err` up the call stack.

```fink
content = try read_file 'config.toml'
# on Ok: content bound; on Err: propagate out of this fn
```

## Dicts

The runtime has a HAMT-based dict type and most operations are wired (get / set / delete / size / merge / equality), but there's no user-facing constructor exposed under `std/dict.fnk` yet. Records today are structurally dicts at runtime — they share the same HAMT implementation — but the language-level `dict {...}` form with dynamic string keys (as opposed to records' compile-time-known identifier keys) isn't reachable from source.

```fink
{dict} = import 'std/dict.fnk'

scores = dict 'alice': 1, 'bob': 2
```

## Macros

Compile-time AST manipulation — `macro` definitions, `eval`, `gen_ast`-style APIs. Entirely future work; nothing in the compiler.

## Effect-handler libraries

The substrate (`with H: B` form + `abort v` primitive) ships today. The conventional libraries on top of it -- `cancel` / `catch` for error short-circuiting, `find` for early-return search, `state` for scoped mutable state, `cleanup` for resource lifecycle, transactional `commit`/`rollback`, etc. -- are not yet written. Each is pure ƒink code against the substrate; see [execution-model.md](execution-model.md) §7 for the shape.

## Float exponentiation

`**` lowers to integer-only square-and-multiply on i64. Float operands (e.g. `2.0 ** 0.5`) need `exp` / `ln` math primitives — blocked on a `std/float.wat` that doesn't exist yet.

## Ordering operator (`<=>`)

A three-way comparison returning `LT` / `EQ` / `GT` was designed but isn't shipped — `<=>` doesn't lex, and `LT`/`EQ`/`GT` aren't defined. The pairwise `<` / `<=` / `>` / `>=` / `==` / `!=` operators cover most needs today.

## Advanced pattern matchers

A few advanced match forms are parseable but don't lower end-to-end yet:

- Spread guards: `[..(is_odd), ..evens] = [1, 2, 3, 4, 5]`
- String range patterns as match arms: `'a'..'z'`
- Pattern-position call guards with spread capture: `[..(is_divisible_by ?, 3) |= divs, ..rest]`

## Types and protocols

Deferred pending a broader design conversation. Not documented here until the model is settled.
