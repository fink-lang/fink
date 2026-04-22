# Execution Model

How a ƒink module runs.

## 1. ƒink is functional

Values are first-class. Functions are pure expressions over values. Composition is application.

## 2. Immutability follows

If functions cannot mutate their inputs and values are first-class, there are no mutable cells. Lexical scope is an immutable map from names to values. Bindings do not overwrite; nested scopes shadow.

Every time a ƒink module looks "dynamic" — mutual recursion, operator overloading, mocking in tests, the host handing stdio to a module at startup — something principled has to be happening. The mechanism is **effects**.

## 3. Effects

An **effect context** is a value threaded through execution, carrying whatever state changes meaning for downstream computation — host capabilities, user-context installations, scheduler state, mutual-rec scopes, lazy-state slots. At the source level it is **entirely invisible**: ƒink code never names it, never declares a parameter for it, never installs it. Code reads as ordinary applicative expressions.

The CPS lowering makes it **entirely explicit**: every effectful function's actual signature takes a context argument and its continuation takes a potentially-new context. `read_file "foo.txt"` at source is really `read_file("foo.txt", ctx, k)` at the impl level, with the continuation invoked as `k(bytes, ctx')`. An iterator whose impl needs state never exposes that state in source code — the iterator's state lives in the context, step operations produce a new context with the advanced state. Same mechanism for file positions, scheduler state, lazy thunks, anything. Consuming a context — reading from it, passing it onward unchanged — is pure.

Registered protocol impls *conceptually* belong in the context, but operationally most of them never appear there. With static information the compiler resolves a protocol use to a direct call; with partial information it compiles to a closed set of statically-enumerated cases; with none it emits a call into a polymorphic dispatcher (like the ones in `operators.wat`). Each of these is a compiled-in dispatch, not a runtime lookup. The runtime context only carries what can't be baked in.

Lexical scope (which names are bound where in the source) is a separate thing. Scope is a compile-time construct about name visibility; the effect context is a runtime value about what impls are registered, what the host has supplied, and similar state that can't be resolved statically. The two meet at mutual recursion: the one place lexical scope itself is effectful, because admitting forward refs requires producing a new context in which both names are resolvable.

The effect is **producing a new context**. A computation that takes the current context and returns a new one (with an added impl, a host capability, ...) is an effect. Everything downstream that consumes the new context is pure again.

The criterion, at the source level:

> **A ƒink function is pure if its evaluation does not change the implicit context. It is effectful if it does.**

Effects are the mechanism for anything context-dependent in ƒink. They are narrow — most of a module is pure; effects are the exception.

CPS is a convenient lens for illustrating this: in a CPS-lowered program, you can *see* the shape — a pure step passes values onward; an effect step produces a new context that subsequent continuations consume. This compiler uses CPS. A compiler built on a different IR would state the criterion in its own terms, but the semantics are the same.

Things that are effects:

- Mutual recursion (forward references need a shared scope).
- Dynamic dispatch (resolution depends on run-time state).
- Impl registration (introducing a new resolution into scope).
- Host-provided capabilities (stdio, panic, scheduler yield — state entering from outside).
- Lazy evaluation (deferred state).

Things that are pure:

- Non-recursive binding.
- Eager import of a pure module.
- Statically-resolved application.
- Passing, returning, and constructing values.

## 4. Everything dynamic is a user of effects

Protocols, impls, and their registration are one user of the effects system. stdio is another. The scheduler is another. Mutual recursion is another. They are all the same mechanism applied at different levels.

### 4.1 Protocols and impls

- A **protocol** is a typed name. Declared as a pure value: `op_plus = type: Fn any, any`. Nothing special about it — it is a type.
- An **impl** satisfies a protocol for some pattern of types. Impls are registered into the current context by a pattern-match whose evaluation is an effect.

The registration syntax is just ƒink's pattern-match-assignment. The same construct that destructures and binds (`[a, b] = some_list`) also registers impls when the pattern's LHS has no binding slots and the head is a type-guard:

```fink
op_plus T1, T2 = fn a, b: ...
```

reads as a pattern match: `op_plus` is the type-guard, `T1, T2` are types being matched, nothing is destructured into, and the right-hand side is the impl to register for that type pattern. Evaluating this line is an effect: it registers the impl.

### 4.2 Dispatch

A protocol use (e.g. `a + b`) resolves against the impls in scope. The compiler uses whatever information it has:

- **Fully known** — emit a direct call to the specific impl. Pure.
- **Narrowed to a small closed set** — emit a static switch over those candidates. Pure; the dispatch is compiled in, not looked up.
- **Unknown** — emit a call into a polymorphic dispatcher (like `op_plus` in `operators.wat`). The dispatcher inspects the value and routes. Still a compiled-in routine, not a runtime context lookup.

None of these materialise a dictionary of impls in the runtime context. Dispatch happens via direct calls, static enumerations, or polymorphic dispatchers — all three are ordinary function calls, not context queries.

### 4.3 Bindings, mutual recursion, imports

- Non-recursive binding (`x = 5`) is pure — the name's value is fixed before anything references it. Ordinary lexical scoping.
- Mutual recursion (`ping = fn: pong() \n pong = fn: ping()`) is effectful — each name must be resolvable from the other's body before either body runs. This is the one case where lexical scope itself needs effect-context help: the construct produces a new context in which both names resolve.
- Eager import of a pure module is pure — same shape as binding a batch of names.
- Import of a module that registers impls or performs any other effect is effectful, inasmuch as its effects run at import time.
- Lazy import is effectful — deferred evaluation needs threaded state.

### 4.4 Host capabilities

stdio, panic, scheduler yield, and anything else a host provides are impls that the host registers into the module's root context before user code runs. Their presence is an effect — state enters from outside. User code consumes them through the same resolution mechanism as any other impl.

## 5. Module lifecycle within a host

A host doesn't "load and run" a compiled module. It **participates in populating the module's root context**.

1. The host starts.
2. The host asks the module to initialise its root context with its own impls (arithmetic, containers, apply, args, ...).
3. The module returns a handle to that context.
4. The host registers its own impls into the context (stdio, panic, scheduler yield, ...).
5. The host asks the module to run against the populated context.
6. User code runs. Resolutions happen against the complete context.

Steps 2 and 4 are effects: registrations into the module's root context. Step 5 is the module consuming the resulting context.

A module doesn't declare a target host. It declares which protocols it uses (by using them) and which it implements (e.g. a `main` function). The linker against a specific host checks the host's contract covers the module's uses. A module using stdio implicitly expects a host that provides stdio impls.

Different hosts provide different impls: the CLI provides OS-backed stdio and an OS-reactor scheduler; a browser provides console-backed stdio and a JS-event-loop scheduler; a library consumer provides neither and exposes public exports for the host to call directly. One lifecycle shape, many host realisations.

## 6. Concept vs. implementation

This document describes the concept. Implementation status is recorded in the source.

Notable current gaps:

- Type-guards in patterns are not yet implemented. Without them, the registration syntax in 4.1 cannot be written in ƒink source. The compiler hard-codes the currently-possible resolutions (operator dispatch on known types, container ops on known containers, etc.) in WAT instead of consulting a registry populated by ƒink-level registrations. The model is unchanged; the realisation is narrower than the model allows.
- The module-lifecycle handshake in section 5 is not staged in today's implementation. The host calls a single fixed entry; runtime and stdlib impls are wired in at link time rather than through host-driven registration.
- User-level contexts (section 7) are designed but syntax and semantics are not settled. Today the only handlers are compiler-internal (impl registration baked into the lowering, the scheduler, the host-root registration).

Each implementation file documents its own deviation from the concept. For the compiler's backend realisation story — how pure vs. effectful computations lower to WASM, how scopes and registries are realised, where compile-time resolution happens — see [src/passes/wasm/](../src/passes/wasm/).

## 7. Related work

The mechanism described here is **algebraic effects**. At the concept level it's a type-level construct: a function's signature reflects whether its evaluation changes the implicit context, and each protocol its body may use. The current ƒink compiler realises the mechanism via a CPS calling convention with an implicit context argument, but that is an impl choice — a different compiler could lower it differently without changing the language model.

The three defining pieces of algebraic effects are present:

- **Operations.** Protocol uses (`a + b`, `log x`, `yield`) resolve against the current context. They are operations in the Koka/Frank/OCaml-5 sense — their meaning is given by what's registered, not by declaration-site semantics.
- **Handlers.** Any ƒink construct that produces a new context is a handler: impl registration, mutual-recursive binding, host capability registration, user-defined context blocks (see below), and the scheduler (which parks and resumes continuations at `yield`).
- **Resumable continuations.** At the concept level, an effectful call receives an explicit rest-of-computation that the handler can resume, run multiple times, or discard. The current compiler represents this directly via CPS: every call site has an explicit continuation value, the scheduler parks and resumes continuations, `yield` is a language-level suspend point. A non-CPS compiler would represent the same continuations differently.

**User contexts** (a planned feature, design not yet settled) will make scoped handler installation source-visible — the only place context scoping shows up in ƒink source. The exact syntax and semantics are open; the sketch below is only to convey the shape, not to specify the feature:

```fink
# sketch, not final syntax
foo = context fn ...:
  ...

spam = fn ...:
  # does something that requires the foo context

with foo 1, 2:
  spam 3, 4
```

Something like `foo` would be declared as a context (a handler). A `with` form would install it for the duration of a block; code inside the block (like `spam`) would have the handler available; outside the block it would not. This is the construct that makes the "scoped override" feature — present in Koka, OCaml 5, Unison — available at the ƒink source level.

**Compared to other languages with algebraic effects:**

- **Koka, OCaml 5, Unison, Frank** — same concept-level semantics; different surface. They have dedicated `effect` declarations, `handler` blocks, and `perform` / `resume` primitives. ƒink unifies these: protocols declared as ordinary typed values play the role of Koka's named effect operations; constructs that produce a new context play the role of handlers; the implicit context threading plays the role of `perform`/`resume`.
- **Effect-row typing.** Koka and Unison annotate every function with the effects it may perform. ƒink's model has the same type-level notion. The compiler already infers effect information at protocol granularity — every `+` compiles to a call into the operator dispatcher in `operators.wat`, every protocol use routes through its dispatcher, and these routings are known at compile time. What's missing is exposing that information in the source as effect rows on function signatures. An impl gap pending type inference, not a model difference.
- **Handler syntax.** Koka has `with handler { ... } { ... }` as a dedicated form. ƒink's planned user-context form (the `with foo 1, 2: ...` sketch above) fills the same role; other ƒink handlers (impl registration, mutual-rec binding, host-root setup) are implicit in their constructs.

**Not to be confused with:**

- **Haskell's `IO` / `State` monads or F#'s computation expressions** — those are monadic encodings of effects, not algebraic effects. ƒink's model is operationally closer to Koka than to Haskell.
- **Typeclass dictionary passing (Haskell, Rust traits).** ƒink does not dictionary-pass at runtime. A protocol use compiles to a direct call (when the impl is fully known), a closed set of statically-enumerated cases (when the impl is narrowed to a few candidates), or a call into a polymorphic dispatcher like the ones in `operators.wat` (when it isn't). None of these materialise a dictionary in the runtime context. Haskell/Rust, by contrast, pass explicit dictionary arguments at every polymorphic call site.
- **Dynamic scope / implicit parameters (Racket parameters, Common Lisp specials, Scala `given`).** Closer than Haskell, but in those systems the implicit parameter is named at least once at source — declared, installed, or referenced explicitly. ƒink's implicit is *entirely invisible* at source: a `read_file "foo.txt"` has no visible parameter for the file system capability, an iterator has no visible state, a `yield` has no visible scheduler handle. Only the planned user-contexts feature surfaces the implicit. Those systems also typically lack continuation capture; ƒink's CPS-with-handlers makes `yield` + scheduler and other control-flow-shaped handlers first-class.

## 8. Glossary

- **Pure** — a ƒink function whose evaluation does not change the implicit context.
- **Effect** — a ƒink function whose evaluation produces a new effect context. The mechanism for all context-dependent behaviour.
- **Effect context** — a runtime value threaded through execution carrying state that affects downstream computation (host capabilities, scheduler state, user-context installations, mutual-rec scopes, lazy-state slots). Implicit at the source level — ƒink code never names or passes it. Protocol impls conceptually belong in the context but operationally most never appear there; dispatch compiles to direct calls, static enumerations, or polymorphic dispatchers, not runtime context lookups. Consuming a context is pure; producing a new one is an effect.
- **Lexical scope** — a compile-time construct: the map from names to values visible at a point in source. Static in the common case; only effectful for mutual recursion, where admitting forward refs requires producing a new context.
- **Protocol** — a typed name, declared as a regular value. Used as the guard in impl-registration patterns.
- **Impl** — a function registered for a protocol against a pattern of types.
- **Registration** — the effect of introducing an impl into the current context. Syntactically: a pattern-match whose LHS head is a type-guard and which has no binding slots.
- **Resolution** — looking up which impl applies to a protocol use. Compile-time when the compiler can see the applicable impl at the use site; run-time otherwise.
- **Realisation** — how an effect is implemented at run time on a given target (compile-time specialisation, context threading, host imports, thread-locals, ...). Backend concern; does not change the language model.
