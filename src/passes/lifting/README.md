# `src/passes/lifting` — unified closure + cont lifting

Takes a CPS tree where `LetFn` nodes can be deeply nested and returns one
where every fn lives at the top level of its module. Captured variables
become explicit leading params; the original call sites are rewritten to
build closures via `·fn_closure`.

This pass replaces the older separate `cont_lifting` and `closure_lifting`
passes with a single iterative one — hence "unified".

## Why it's iterative

Lifting one fn out of its parent can introduce **new** captures in the
parent (the moved fn's name is now a sibling reference) or expose new
nested fns (a previously-inline `Cont::Expr` becomes a named LetFn). So:

1. Run name resolution + capture analysis on the current tree.
2. Walk every `LetFn fn_body`: if it contains a nested `LetFn`, extract
   it into the parent's cont chain. Also hoist inline `Cont::Expr`
   bodies into named `LetFn`s.
3. Repeat until no nested LetFn remains anywhere.

Convergence is guaranteed because each iteration strictly reduces nesting
depth. There's a `MAX_ROUNDS` cap (currently 20) as a safety net; hitting
it is a bug.

## Capture rule

> If a fn moves up one level, which of its free variables would become
> out of scope?

Only variables bound by the **immediate** enclosing scope (siblings in
the same `LetFn`/`LetVal` continuation chain) need to be threaded as
leading params. Variables from parent scopes remain visible after a
one-level lift, so they don't need threading at this step — the next
iteration handles them when the parent itself gets lifted.

## Closure allocation strategy

A cont body that closes over a local value (e.g. an outer cont param
`·v_N`) is wrapped in `·closure` to bake that value in. This is forced
by the calling convention: builtins like `·op_mul` invoke their cont
with **exactly one** argument (the result). There's no slot to thread
extra captured values through, so the closure is the only place to put
them.

Example: `x = double 5; inc x` lowers to a cont `·v_K` that captures the
outer cont `·v_N`. Lifting emits `·closure ·v_K, ·v_N` at the call site
— `·v_N` is a fn param, not a global, so it has to ride along.

See [`../wasm/calling-convention.md`](../wasm/calling-convention.md) for
how `·closure` lowers to the runtime `$Closure(funcref, $Captures)`
struct.

## Files

- [`mod.rs`](mod.rs) — the pass itself: capture analysis, the iterative
  driver, the lift-one-level rewrite, and the `·fn_closure` insertion.
- [`fmt.rs`](fmt.rs) — formatter for lifted CPS, used by the `fink cps
  --lifted` CLI subcommand and by lifting tests for golden output.
- `test_lifting.fnk` — `.fnk` test fixtures (see
  [`crates/test-macros/README.md`](../../../crates/test-macros/README.md)
  for the test-file shape).

## Future: closure elimination via param threading

Appel-style first-order CPS would thread every captured value as an
extra explicit param through every intermediate function in the call
chain — zero heap closures, every fn becomes static. The trade-off is
that intermediate fns must accept and forward params they don't use, so
calling conventions become variadic or specialised. Significant
redesign; deferred. The current closure-allocating approach is correct
and the runtime cost is small in practice.
