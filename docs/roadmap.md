# ƒink — Roadmap

Features that are designed but not yet implemented, plus active design
questions. Each section is short on purpose; long-form discussion lives
on GitHub issues, linked per-section.

## Conventions

- **Settled** sections of the language reference have a
  `Status: implemented` or `Status: designed` banner in
  [language.md](language.md). This file holds the *delivery* status —
  what's pending, what's open, who needs to decide what.
- Section anchors are stable kebab-case (`#types`, `#protocols`, etc.)
  so cross-links from `language.md` and the README don't break when
  this file grows.

## Types

Settled syntax sketches in
[examples/type-system.fnk](examples/type-system.fnk).

**Settled:** opaque types, product, sum / variant, generic parameters,
type construction & matching, the numeric hierarchy (`Num`, `Int`,
`Unsigned`, `Signed`, `Float`).

**Open:**

- Alternative type-construction notation — `Foo = type T: ...` vs
  `Foo = fn T: type ...`. The second composes more cleanly with
  protocol implementations but is more verbose.
- Refinement-type syntax — currently sketched as pattern guards
  (`NumOption = variant T match: Num: ..., Str: ...`); the constraint
  form isn't fully settled.
- Dependent-type ergonomics — `Buffer = type size: [..size u8]` works
  for the simple case; how it surfaces in error messages is open.

**Implementation:** type inference + checking lives in a future
`src/passes/types/` (or similar); not yet started. Will get its own
contract docs co-located there.

## Protocols

Sketches in [examples/protocols.fnk](examples/protocols.fnk).
Conceptually similar to Rust traits / Haskell typeclasses / Java
interfaces — abstract function signatures parameterised by a type, with
per-type implementations. Multi-function protocols
(`Iterator = type T: { next: ..., done: ... }`) are part of the design.

**Open:**

- Type-level protocol-impl syntax — `iter Range = Iterator { next:
  ..., done: ... }` vs piping through impl generators
  (`UserId = type: ..u64 | impl_add | impl_sub`). Both syntaxes appear
  in the sketches; pick one.

## Macros

Sketches in [examples/macros.fnk](examples/macros.fnk). Compile-time
AST manipulation: a `macro` annotation marks a function as running at
compile time over a `ctx` and an AST; `gen_ast` materialises Fink AST
from values; `eval` runs sub-expressions at compile time.

**Open:**

- The `fink/compile` import surface (`macro`, `gen_ast`, `eval`) — the
  shape is sketched but the actual API isn't pinned down.
- Import hooks via macros — sketched as `my_import = macro fn ctx: fn
  url: ...` returning generated AST. How this plays with the
  multi-module pipeline ([`src/passes/wasm-link/`](../src/passes/wasm-link/))
  is unclear.

## First-class patterns

Sketches in [examples/unresolved.fnk](examples/unresolved.fnk).
`pattern: ...` constructs a value that can be reused across `match`
arms — useful for parsing, regex-style matchers, etc.

**Open:** lowest-priority feature among the WIP set. Syntax and
semantics both still in flux.

## Async and concurrency (language surface)

Sketches in [examples/unresolved.fnk](examples/unresolved.fnk).

**Settled:**
- Runtime building blocks exist —
  [`src/runtime/scheduler.wat`](../src/runtime/scheduler.wat) for
  cooperative multitasking, [`src/runtime/channel.wat`](../src/runtime/channel.wat)
  for multi-message channels, `$Future` for awaitable results.
- Builtins `spawn`, `await`, `yield`, `channel`, `receive`, plus
  `>>`/`<<` for channel send/receive at the language level work.

**Open:**

- `await_all` / `await_race` library shape — currently sketched as
  `[r1, r2] = await_all task1, task2` and `[first, ..rest] =
  await_race task1, task2`.
- Implicit-await on access (`body = task1.body` suspends until `task1`
  completes). Settled as a design intent; needs the type system to
  cleanly distinguish `Future T` from `T`.
- Streaming — composing `Future` with `Iterator` so `fetch 'url' | map
  parse_chunk` does what you'd expect with backpressure.
- Cancellation via send (sketched: `task_group ()` returns a `(group,
  send)` pair; `send ()` cancels every task in the group).

## Context / effects

Sketches in [examples/unresolved.fnk](examples/unresolved.fnk).

**Settled (intent):** algebraic-effects-flavoured. `context` declares a
context type; `with` blocks introduce one; `get_ctx` reads it inside
any function called transitively from the `with` block. Conceptually
similar to Multicore OCaml's algebraic effects or Haskell's IO-monad-
with-extensible-effects.

**Open:** everything past the headline syntax. How effects compose,
how they interact with `try`/`Ok`/`Err`, how the type system tracks
which effects a function performs.

## Chan-op precedence (`<<` / `>>`)

Sketched in [examples/unresolved.fnk](examples/unresolved.fnk) lines
89–129. `src/passes/ast/parser.rs:736` carries a `TODO(chan-op)`
comment pointing here.

**Problem:** `<<` and `>>` currently double as bitwise shift (tight
precedence, above `==`) **and** channel send/receive (wants loose
precedence, below `==`). Contradictory.

**Options:**

1. Keep `<<`/`>>` for channels, rename bitwise to `shl`/`shr`
   keywords. Preserves the statement-shape arrow for IO code; chained
   sends `a << b << c` require `<<` to return the channel.
2. Drop `<<`/`>>` for channels entirely; channels are callable
   (`stdout 'hello'`). `<<`/`>>` stay bitwise with tight precedence.
   Loses the arrow but removes a whole operator row.
3. Status quo — parenthesise the RHS of `<<` when comparing. Rejected:
   the footgun is real (C++/Ruby suffer for it).

**Decision needed:** which option ships. Once chosen, `parser.rs:736`
gets the precedence change and the `TODO(chan-op)` comment can drop
its pointer here.

## Ranges

Sketched in [language.md#ranges](language.md#ranges).

**Open:** commonly-used ranges with a step (`1..10..2`). Possibilities:
keep ranges step-less and use `range 1, 2, 3` as the general form, or
add literal-step syntax. Not urgent.

## Member-access shorthand

Sketched in [language.md#member-access](language.md#member-access).

**Open:** whether `foo.[k1, k2]` and `foo.'literal'` are real syntax
or whether `.([…])` and `.('literal')` stay the only forms. Both
shorthands are listed in the example file marked `# TODO: decide
later`.
