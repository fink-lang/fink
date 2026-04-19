# ƒink — Terminology

Alphabetical glossary of Fink terms, plus the way Fink uses some
common-but-overloaded words. If you've come from another language and
something reads strangely, this page is the place to disambiguate.

For concept-order introduction of these terms, read
[language.md](language.md) and [semantics.md](semantics.md). This file
is the lookup index, not the tutorial.

---

### apply / application

Calling a function. `add 1, 2` is "apply `add` to `1` and `2`". Fink
has no parentheses around argument lists in the call form — the prefix
position of `add` is what makes it a call, not the parens.

### binding

A name introduced into scope. `foo = 1` introduces the binding `foo`.
Every binding is a pattern match; the LHS can be any pattern, not just
an identifier (`[a, b] = …`, `{x, y} = …`).

### `bool`

Either `true` or `false`. Operands of `not` / `and` / `or` / `xor`
when used as logical operators must be `bool`; the same operators
work bitwise on integers (different protocol, same operator).

### channel

A multi-message asynchronous queue with a tag. Two operators speak
to channels: `>>` for send, `<<` for receive. The precedence of these
operators when also used for bitwise shift is an
[open question](roadmap.md#chan-op-precedence).

### closure

A function value that has captured (closed over) one or more
bindings from an enclosing scope. Implementation-side, every Fink
function is potentially a closure — the [lifting pass](../src/passes/lifting/)
makes captures explicit.

### context

Algebraic-effects-flavoured ambient state — a value carried implicitly
through a `with` block, accessible via `get_ctx`. Designed but not
yet implemented; see [roadmap.md#context-effects](roadmap.md#context--effects).

### cont, continuation

A function representing "what to do with the result". Fink is
internally CPS-shaped — every intermediate value is named and every
return is an explicit call to a continuation. Users don't write
continuations directly; the CPS transform synthesises them. See
[`src/passes/cps/`](../src/passes/cps/).

### CPS

Continuation-Passing Style. Transformation of a normal expression
tree into one where every call has an extra argument — the
continuation — that receives the result. See
[`src/passes/cps/transform-contract.md`](../src/passes/cps/transform-contract.md).

### desugar

Rewrite higher-level surface syntax into a smaller core. Fink's
desugar phase runs partial-application and scope-analysis passes; it
sits between parsing and CPS lowering.

### dict

A runtime-keyed key-value collection. **Different from `record`** —
records are static-keyed (compile-time-known field names); dicts take
arbitrary hashable keys at runtime. Same HAMT internals
([`src/runtime/rec.wat`](../src/runtime/rec.wat)) but distinct types
in the language.

### `fn`

The function-construction keyword. `fn a, b: a + b` is an anonymous
function of two args.

### Fink module

A `.fnk` file. Each module is its own scope; the bindings exported by
a module become the value of `import './path.fnk'`.

### lift / lifting

Move nested function definitions to the top level, threading
captured bindings through as explicit parameters. Done by the
[lifting pass](../src/passes/lifting/) before codegen.

### match

Pattern-matching control flow. `match foo: <pat>: <body>` tries each
arm in order; the first matching one runs. Same pattern language as
binding LHSes — anything that can appear after `=` can appear after
`:`.

### `Ok` / `Err`

The two variants of `Result`. `try` unwraps `Ok` or short-circuits
the function with `Err`; `match` lets you handle both arms
explicitly. See [language.md#error-handling](language.md#error-handling).

### partial application (`?`)

A `?` placeholder in an expression turns the surrounding expression
into a one-argument function whose param is bound to the placeholder.
`add 5, ?` is `fn $: add 5, $`. See
[`src/passes/partial/README.md`](../src/passes/partial/README.md) for
the scoping rules.

### pattern

The left-hand side of a binding (`=`, `|=`) or a `match` arm. Patterns
destructure values: literals match exactly, `_` is the wildcard, `[…]`
matches sequences, `{…}` matches records, `..rest` collects spread.
Identifiers in pattern position introduce bindings.

### pipe (`|`)

Left-to-right application. `foo | bar | spam` is `spam (bar foo)`.
Each pipe segment is a scope boundary for `?` partial-application.

### record

A **statically-keyed** key-value collection. Field names are fixed at
compile time. Different mental model from a JS object or a
Python dict — closer to an OCaml record, a Haskell record, or a
TypeScript object type with a fixed structural shape. For runtime
keys, use [dict](#dict) instead.

### `seq`

A sequence value. `[1, 2, 3]` and `seq 1, 2, 3` are equivalent
syntaxes. The runtime representation is a cons-cell list
([`src/runtime/list.wat`](../src/runtime/list.wat)); the compiler may
optimise small sequences to a tuple representation.

### set / `ordered_set`

A unique-element collection. `set` is unordered (insertion order not
preserved); `ordered_set` preserves insertion order.

### scope

A region of source code in which a binding is visible. Scopes nest:
`fn`, `match` arm, record-field body, parens, and the module top
each open a new scope. A binding declared in a scope is invisible
outside it — see [language.md#block-scoping](language.md#block-scoping).

### spread (`..`)

In a sequence or record literal, splice a collection's elements in
place: `[..a, ..b]`. In a function param or call arg, collect or
expand a variadic. In a pattern, match the rest of the sequence /
record.

### tagged literal

Postfix function application. `10sec` is `sec 10`. Useful for
units-of-measure-style constructors.

### tagged template

A string literal prefixed with a function name: `fmt'hello ${x}'`,
`sql'SELECT ...'`. The function receives the raw string segments
interleaved with the interpolated values.

### `try`

Unwrap a `Result` if `Ok`, otherwise short-circuit the enclosing
function with the `Err`. Equivalent to Rust's `?` operator.

### variant / sum type

A type with a fixed set of named alternatives, each carrying its
own payload. `variant T: Some T; None` defines `Option`. Designed
but not yet implemented; see [roadmap.md#types](roadmap.md#types).

### wildcard (`_`)

A non-binding placeholder. Used in pattern position
(`[_, x] = [1, 2]`) and function params (`fn _, b: b`). Cannot be
referenced as a value.

### yield

Suspend the current task and let the scheduler pick another runnable
one. Cooperative multitasking primitive; see
[`src/runtime/scheduler.wat`](../src/runtime/scheduler.wat).
