# ƒink — Semantics

The Fink evaluation model: how values, scopes, control flow, modules,
and errors actually behave when a program runs. This is the *what*
underneath the *how-to-write-it* in [language.md](language.md).

Cross-language analogies are inlined at the point of confusion — you'll
see "≈ OCaml's X" / "≈ Rust's Y" callouts where Fink uses a familiar
word with a non-obvious meaning. They're hints, not equivalences.

> **Status:** the model below is settled. Adding planned features
> (explicit types, protocols, effects) extends it rather than replacing
> it. Per-feature delivery status is in [roadmap.md](roadmap.md).

---

## Values

Every Fink expression evaluates to a value. Values are:

| Kind | Examples | Notes |
|---|---|---|
| Boolean | `true`, `false` | Two singletons. |
| Integer | `42`, `0xFF`, `-1` | Sized by the literal (`u8`, `i16`, …). |
| Float | `1.0`, `1.0e100_000` | `f32` or `f64` per literal. |
| Decimal | `1.0d` | Distinct from float; not interchangeable. |
| String | `'hello'`, `'a${x}b'` | Byte sequences (≈ Go `[]byte`, **not** ≈ Java `String`). UTF-8-validated subtype is [planned](roadmap.md#types). |
| Sequence | `[1, 2, 3]`, `seq 1, 2, 3` | Cons-cell list at runtime; small cases may optimise to tuple. |
| Record | `{x: 1, y: 2}` | **Static** keys (compile-time known). ≈ OCaml record. **Not** ≈ JS object. |
| Dict | `dict {x: 1}` | **Runtime** keys, any hashable type. ≈ OCaml `Hashtbl`, ≈ Python dict. |
| Set | `set 1, 2, 3` | Unordered unique. `ordered_set` preserves insertion order. |
| Range | `0..10`, `0...10` | Iterable; supports membership (`x in 0..10`) and patterns. |
| Function | `fn a: a + 1` | First-class; closures capture lexically. |
| Channel | `channel 'tag'` | Multi-message async queue (cooperative scheduling). |
| Future | `spawn fetch 'url'` | Settled-or-pending value; cooperative await. |

### Immutability

**All Fink values are immutable.** `{x: 1, ..rec}` doesn't modify
`rec` — it produces a new record sharing structure with `rec`. Same
for sequences (`[head, ..tail]`), strings, sets, dicts.

Implementation-side, the runtime uses HAMT-backed tries
([`src/runtime/rec.wat`](../src/runtime/rec.wat)) for records and
dicts, and cons cells ([`src/runtime/list.wat`](../src/runtime/list.wat))
for sequences — both naturally persistent. The compiler may optimise
small/local mutations into in-place updates when sharing isn't observable.

> **OCaml / Haskell / Rust analogy:** every Fink value behaves like an
> OCaml record (immutable; `with` produces a new record), Haskell data
> (no mutation), or Rust's `Arc<T>` snapshots (shared ownership of
> immutable state). **Not** like Python / JS / Java mutable objects.

---

## Identity vs equality

`==` and `!=` compare **values structurally**. Two records with the
same fields and values are `==`, regardless of whether they're the same
runtime allocation. Same for sequences, strings, dicts, sets, ranges.

There is no `is` operator and no observable identity for normal values.
Channels and futures are exceptions: they have observable identity
because they represent stateful effects, but they don't participate in
`==`.

> **Python analogy:** Fink's `==` is Python's `==`, not Python's `is`.
> **Java analogy:** Fink's `==` is `.equals()`, not `==`.

---

## Scope and bindings

### Lexical scoping

Scopes nest via syntax. A new scope is opened by:

- a function body (`fn …: <body>`),
- a `match` arm body,
- a record-field body,
- a parenthesised expression `(…)`,
- the module top.

A binding introduced inside a scope is **not** visible outside it.
[language.md#block-scoping](language.md#block-scoping) shows this
syntactically; the underlying model is just lexical scoping with no
hoisting and no `var`-style time-travel.

> **JS analogy:** Fink scoping is closer to `let` than to `var` —
> bindings live in their syntactic block, no function-level hoisting.
> **Python analogy:** Fink does not have Python's "the whole function
> body is one scope" rule; every block introduces a real scope.

### Forward references inside a module

Module-level bindings are **mutually recursive**. Inside a module, you
can reference any sibling binding regardless of source order:

```fink
is_even = fn n: match n: 0: true; _: is_odd  n - 1
is_odd  = fn n: match n: 0: false; _: is_even n - 1
```

Forward references inside a function body, by contrast, are **not**
allowed — function bodies are sequential.

> **OCaml analogy:** module-top is `let rec`-flavoured by default;
> function bodies are not.
> **Haskell analogy:** module-top is the natural Haskell behaviour;
> Fink's deviation is that function bodies *aren't*.

### Capture

A function value captures the bindings it references from enclosing
scopes. The [lifting pass](../src/passes/lifting/) makes this
explicit at codegen time, but the surface model is plain lexical
capture: a closure remembers its enclosing scope's bindings.

---

## Pattern matching

Every binding (`=`, `|=`) and every `match` arm uses the same pattern
language. A pattern can:

- **Match exactly** (literals, sequences, range patterns).
- **Bind** (identifiers in pattern position introduce a binding visible
  in the corresponding body).
- **Guard** (boolean expressions over already-bound names: `n > 0`).
- **Destructure** (`{x, y}`, `[head, ..tail]`).

Records destructure **partially** (extra fields are ignored unless you
write `..{}` to require empty); sequences destructure **exactly**
(use `..` to allow a tail).

A `match` tries arms top-to-bottom; the first matching arm runs. A
binding pattern that fails is a compile-time error if the failure can
be detected statically (e.g. `[x, y, z] = [1, 2]` — too few items),
otherwise traps at runtime.

> **OCaml / Rust analogy:** Fink's `match` and pattern semantics are
> close to OCaml's / Rust's. The differences: bindings on the LHS of
> `=` accept any pattern (not just irrefutable ones), and records are
> structurally typed (no `type ... = { x; y }` declaration needed).
> **Erlang analogy:** also close — every assignment is a pattern match,
> and unmatched patterns fail.

---

## Control flow

Fink has no statements. Every construct is an expression that produces
a value:

- `match ...` — value of the matching arm's body.
- `fn ...: body` — a function value.
- `(expr)` and indented blocks — value of the last expression.
- `if`-style branching is `match` against literal patterns.

There is no `for` / `while` loop. Iteration is library-level:
recursion, or `map` / `filter` / `fold` over sequences (most often
written through pipes).

### Try / Result

Errors are values. `Result` has two variants — `Ok x` and `Err e`. The
`try` keyword unwraps `Ok` or short-circuits the enclosing function
with `Err`:

```fink
fn foo:
  a = try bar _      # if bar returns Err, foo returns Err immediately
  b = try baz a
  Ok a + b
```

`match` lets you handle `Err` explicitly. There is no exception
mechanism — every fallible function returns `Result`.

> **Rust analogy:** `try` ≈ Rust's `?`; `Result` is the same shape.
> **Haskell analogy:** `Result` ≈ `Either e a` (with `Ok`/`Err` =
> `Right`/`Left`); `try` ≈ `do`-notation in `Either`.
> **Go analogy:** Fink does not use multi-return `(value, err)`; the
> error is part of the return value.

### Cooperative concurrency

`spawn fn_call` returns a `Future`. The current task continues; the
spawned task runs in the same OS thread on a cooperative scheduler.
Tasks yield to each other at well-defined points
([`src/runtime/scheduler.wat`](../src/runtime/scheduler.wat)):

- channel send (`>>`) when no receiver is waiting,
- channel receive (`<<`) when no message is queued,
- explicit `yield`,
- await on a not-yet-settled future.

Reading a field of a future implicitly awaits it (planned syntax —
[roadmap.md#async-and-concurrency-language-surface](roadmap.md#async-and-concurrency-language-surface)).

> **JS analogy:** ≈ JS's microtask queue + `await`. No threads, no
> shared mutability concerns.
> **Erlang analogy:** ≈ Erlang processes + message-passing channels,
> but cooperative rather than preemptive, and tasks share a heap (no
> per-process isolation).
> **Go analogy:** ≈ goroutines + channels, but cooperative + immutable
> shared state.

---

## Modules

Each `.fnk` file is a module. The set of module-level bindings forms
the module's value; `import './foo.fnk'` returns that value, typically
destructured at the call site:

```fink
{foo, bar} = import './foobar.fnk'
```

### Resolution

Paths in `import` are resolved relative to the importing module. The
multi-module compiler ([`src/passes/wasm-link/`](../src/passes/wasm-link/))
canonicalises every URL: two consumers reaching the same file via
different relative paths share **one** linked instance, not two.

### Initialisation order

Module top-level expressions evaluate in dependency order: every
import's module body runs before the importer's body. The compiler
computes this via post-order DFS over the dep graph, not BFS — this
matters for diamond imports.

> **JS / Python analogy:** ≈ ES modules / Python packages — top-level
> code runs once on first import, in dep order.
> **OCaml analogy:** ≈ functor-free OCaml modules — module names are
> first-class values; binding-via-destructure replaces the `open M`
> pattern.

---

## Continuations and execution shape (informational)

Internally, Fink lowers to Continuation-Passing Style: every
intermediate result is named, and every `return` is an explicit call
to a continuation function. Users never write CPS by hand — the
[CPS transform](../src/passes/cps/transform-contract.md) does it.

This shape gives Fink:

- **Tail calls always work.** Every call is tail-shaped after CPS, and
  the WASM emitter uses `return_call` / `return_call_ref`. Unbounded
  tail recursion runs in O(1) stack.
- **First-class continuations are available to compiler-internal
  features** (notably `try` and the cooperative scheduler) without
  needing `call/cc` in the surface language.
- **Effect-shaped features** (channels, await, yield) plug into the
  same scheduler, all expressed as continuation manipulation.

The user-facing language is direct-style; the CPS shape is invisible
unless you `fink cps file.fnk` to see the IR.

---

## What's not in the model yet

These belong here once they land. Tracked in
[roadmap.md](roadmap.md):

- **Static types** (`u8`, `Option T`, etc.) — currently inferred only
  for literals. Annotations and inference for general expressions are
  designed but not implemented.
- **Protocols** — operator overloading is sketched but not active;
  every operator currently has a single concrete implementation.
- **Algebraic effects** (`context`, `with`, `get_ctx`) — designed but
  not implemented.
- **Macros** — compile-time AST manipulation; designed but not
  implemented.

When any of these lands, this file gets a new chapter, not a rewrite.
