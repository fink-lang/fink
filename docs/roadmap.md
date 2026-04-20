# Roadmap

What's designed but not yet usable from ƒink source. Features listed here have some presence in the compiler or runtime; they just aren't reachable to a ƒink programmer yet.

For features that *work today*, see [language.md](language.md).

## Error handling (`try`)

`try` parses and lowers through CPS as a passthrough. The language-level semantics — `Ok` / `Err` values, propagation from the enclosing function, `match Ok / Err` patterns — aren't wired yet. Once they are, `try foo` will unwrap on `Ok` and propagate the `Err` up the call stack.

```fink
content = try read_file 'config.toml'
# on Ok: content bound; on Err: propagate out of this fn
```

## Sets

The runtime has a HAMT-based set type (`src/runtime/set.wat`), but there's no language-level constructor yet — `set { 1, 2, 3 }` doesn't parse as a set. Runtime's ready; the front-end builtin is missing.

```fink
uniq = set { 1, 2, 3 }
1 in uniq
```

## Dicts

Records today are structurally dicts at runtime — they share the same HAMT implementation and records aren't enforced as a distinct kind. The language-level `dict { 'a': 1, 'b': 2 }` form with dynamic string keys (vs records' compile-time-known identifier keys) isn't parsed yet. The split is about surface syntax and enforcement, not runtime shape.

## Macros

Compile-time AST manipulation — `macro` definitions, `eval`, `gen_ast`-style APIs. Entirely future work; nothing in the compiler.

## Context and effects (`with`, `get_ctx`)

Scoped ambient values — a structured alternative to implicit globals. Designed in sketch form, no compiler support.

```fink
DB_CTX = context DB
with db_ctx:
  result = foo ()
```

## Types and protocols

Deferred pending a broader design conversation. Not documented here until the model is settled.

## Historical sketches

Early `.fnk` sketches that shaped the current language — and the features above — live in [examples/](examples/). They are not authoritative; treat them as reference material from the design phase.
