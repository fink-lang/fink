# Calling Convention — `$Fn3(captures, ctx, args)`

Every ƒink function — user-defined, compiler-synthesised, match wrappers, pattern matchers, success/fail continuations — has the same WASM signature:

```wat
(type $Fn3 (func (param (ref null any) (ref null any) (ref null any))))
```

- **captures** (local 0) — the lexical environment. `null` for functions that capture nothing; otherwise an instance of a per-function `$Captures` array whose layout the emitter pins at compile time.
- **ctx** (local 1) — the universe context (an opaque `$Ctx`). Threaded as a native wasm param so callees don't have to peel it off the args list. See [`../../../docs/execution-model.md`](../../../docs/execution-model.md) for the language-level model.
- **args** (local 2) — a ƒink cons-list. Holds the call's positional arguments followed by the continuation as the last element (for CPS-function calls), or just the arguments (for CPS-closure calls that receive their continuation some other way).

One signature, one dispatch helper, one closure struct. No arity-specialised types.

## Single closure type

```wat
(type $Closure (struct (field $func funcref) (field $captures (ref null $Captures))))
```

Closure construction packages a funcref plus a `$Captures` array. A function that captures nothing uses `ref.null $Captures` for the second field.

Per-function `$Captures` types are emitted on demand, one per distinct capture count that appears in the module. Each is an array of `(ref null any)`.

## Single dispatch helper

All indirect calls go through `apply_3` in the runtime (defined in `rt/apply.wat`):

```text
apply_3(args: ref null any, ctx: ref null any, callee: ref null any)
```

`callee` is cast to `$Closure` at dispatch time; `apply_3` extracts the funcref and captures and tail-calls the funcref with `$Fn3(captures, ctx, args)`.

There is no `$Fn2` and no `_apply_cont`. Earlier designs went through several iterations:

- An arity-specialised set of `$Fn1`/`$Fn2`/... types with a per-arity dispatch helper.
- A single `$Fn2(captures, args)` shape with a single `_apply` helper (continuations rode in the args list).
- The current `$Fn3(captures, ctx, args)` shape, which adds the universe context as a native wasm param so callees don't pay the args-list head/tail dance to peel ctx off.

Continuations ride in the args list when the callee is a CPS function; CPS closures receive them as ordinary captures.

## Function entry

The emitter knows each function's `$Captures` layout and its positional param count. Entry unpacks both:

The compiler-emitted param names follow a `$:NAME` convention so they don't collide with user identifiers:

```wat
;; fn {k, x}, [a, b]:
(func $foo (type $Fn3)
  (param $:caps_param (ref null any))
  (param $:ctx_param (ref null any))
  (param $:params (ref null any))

  ;; Unpack captures from the $Captures array.
  (local.set $k
    (array.get $foo_caps
      (ref.cast (ref $foo_caps) (local.get $:caps_param))
      (i32.const 0)))
  (local.set $x
    (array.get $foo_caps
      (ref.cast (ref $foo_caps) (local.get $:caps_param))
      (i32.const 1)))

  ;; Unpack positional params from the args list.
  (local.set $a (call $args_head (local.get $:params)))
  (local.set $:params (call $args_tail (local.get $:params)))
  (local.set $b (call $args_head (local.get $:params)))
  ...)
```

`$:ctx_param` is just a local — callees that need ctx read it via `(local.get $:ctx_param)` and pass it forward at every tail-call site.

## Call sites

Args are built right-to-left via `args_prepend`. The continuation (when present) is the **tail** of the list the callee receives — the call site prepends every positional arg and lastly the continuation. Callees that need a cont pop it off the args list at the position their static arity implies.

ctx is passed as the second positional wasm arg to `apply_3` (not via the args list).

## Spread

`fn ..rest:` captures the whole args list as `rest`. Spread at a call site (`f a, ..b, c`) unpacks `b` and prepends its elements individually.

## Builtins

Fixed-arity builtins (`op_plus`, `seq_prepend`, `str_fmt`, etc.) keep their direct WASM signatures and are called without going through `apply_3`. Each takes ctx as its first param, followed by its value args and the continuation; it dispatches its result to the caller's continuation via `apply_1` (which threads the same ctx). See [`rt/protocols.wat`](../../runtime/protocols.wat) for the dispatch pattern.

## Apply helpers

`apply_0` / `apply_1` / `apply_2_vals` are the wrappers used by builtins to dispatch a result (of arity 0/1/2) to a continuation. They take ctx as their first param and route through `apply_3` internally:

```text
apply_1(ctx, value, cont)
  → apply_3(args=[value], ctx, cont)
```

## CPS-side shape

The CPS IR tags every `LetFn` with a `CpsFnKind`:

```rust
enum CpsFnKind {
    /// Called with an `Arg::Cont` at the call site.
    CpsFunction,
    /// Never called with an explicit `Arg::Cont`. Includes inline cont bodies,
    /// match arm bodies, PatternMatch bodies/matchers, and success wrappers.
    CpsClosure,
}
```

Both compile to the same `$Fn3`. The distinction only affects how continuations are routed at emit time: `CpsFunction` callees expect a continuation appended to their args list; `CpsClosure` callees don't.

## Thunks and async resumption

Suspended continuations are parked as `$Closure` thunks whose captures hold `[cont, value, ctx]`. When userland later fires a thunk, the thunk's body calls `apply_3` with the **captured ctx** — restoring the suspender's universe rather than running under the resumer's. The `suspend` primitive hands the current continuation to userland as such a thunk-closure; userland (e.g. `std/tasks.fnk`) decides when to resume it. See [`rt/apply.wat`](../../runtime/apply.wat) `$_thunk_fn`, `$make_thunk`, and `$suspend_apply`.

## See also

- [../../../docs/execution-model.md](../../../docs/execution-model.md) — why every function takes implicit context and continuation arguments (the language-level model this convention realises).
- [`rt/apply.wat`](../../runtime/apply.wat) — `apply_3`, `apply_0/1/2_vals`, `make_thunk`, `_thunk_fn`.
