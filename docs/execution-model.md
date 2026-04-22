# Execution Model

How a ƒink module runs.

## 1. ƒink is functional

Values are first-class. Functions are pure expressions over values. Composition is application.

## 2. Immutability follows

If functions cannot mutate their inputs and values are first-class, there are no mutable cells. Lexical scope is an immutable map from names to values. Bindings do not overwrite; nested scopes shadow.

Every time a ƒink module looks "dynamic" — mutual recursion, operator overloading, mocking in tests, the host handing stdio to a module at startup — something principled has to be happening. The mechanism is **effects**.

## 3. Effects

An **effect context** is a value threaded through execution, carrying registered impls and any other state that changes meaning for downstream computation. It is implicit at the source level — ƒink code never names or passes it; the compiler threads it where needed. Consuming a context — reading from it, resolving a protocol against it, passing it onward unchanged — is pure. Pure computation over immutable values, nothing special.

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

A protocol use (e.g. `a + b`) resolves against the current context's registered impls. When the compiler can see which impl applies at the use site, resolution happens at compile time and the emitted call is a direct function call — no effect. When it can't (types not known, or impls registered in a narrower context), resolution happens at run time via the threaded effect context.

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

Each implementation file documents its own deviation from the concept. For the compiler's backend realisation story — how pure vs. effectful computations lower to WASM, how scopes and registries are realised, where compile-time resolution happens — see [src/passes/wasm/](../src/passes/wasm/).

## 7. Glossary

- **Pure** — a ƒink function whose evaluation does not change the implicit context.
- **Effect** — a ƒink function whose evaluation produces a new effect context. The mechanism for all context-dependent behaviour.
- **Effect context** — a runtime value threaded through execution carrying registered impls and other state that affects downstream resolution. Implicit at the source level — ƒink code never names or passes it. Consuming a context is pure; producing a new one is an effect.
- **Lexical scope** — a compile-time construct: the map from names to values visible at a point in source. Static in the common case; only effectful for mutual recursion, where admitting forward refs requires producing a new context.
- **Protocol** — a typed name, declared as a regular value. Used as the guard in impl-registration patterns.
- **Impl** — a function registered for a protocol against a pattern of types.
- **Registration** — the effect of introducing an impl into the current context. Syntactically: a pattern-match whose LHS head is a type-guard and which has no binding slots.
- **Resolution** — looking up which impl applies to a protocol use. Compile-time when the compiler can see the applicable impl at the use site; run-time otherwise.
- **Realisation** — how an effect is implemented at run time on a given target (compile-time specialisation, context threading, host imports, thread-locals, ...). Backend concern; does not change the language model.
