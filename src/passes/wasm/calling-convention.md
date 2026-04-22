# Calling Convention — `$Fn2(captures, args)`

Every ƒink function — user-defined, compiler-synthesised, match wrappers, pattern matchers, success/fail continuations — has the same WASM signature:

```wat
(type $Fn2 (func (param (ref null any) (ref null any))))
```

- **captures** (local 0) — the lexical environment. `null` for functions that capture nothing; otherwise an instance of a per-function `$Captures` array whose layout the emitter pins at compile time.
- **args** (local 1) — a ƒink cons-list. Holds the call's positional arguments followed by the continuation as the last element (for CPS-function calls), or just the arguments (for CPS-closure calls that receive their continuation some other way).

One signature, one dispatch helper, one closure struct. No arity-specialised types.

## Single closure type

```wat
(type $Closure (struct (field $func funcref) (field $captures (ref null $Captures))))
```

Closure construction packages a funcref plus a `$Captures` array. A function that captures nothing uses `ref.null $Captures` for the second field.

Per-function `$Captures` types are emitted on demand, one per distinct capture count that appears in the module. Each is an array of `(ref null any)`.

## Single dispatch helper

All indirect calls go through `_apply` in the runtime (not emitted by the compiler — lives in the runtime WAT under `dispatch.wat`):

```text
_apply(args: ref null any, callee: ref null any)
```

`callee` is cast to `$Closure` at dispatch time; `_apply` extracts the funcref and captures and tail-calls the funcref with `$Fn2(captures, args)`.

There is no `$Fn3` and no `_apply_cont`. An earlier design kept continuations as a dedicated WASM param (so `_apply` had to know whether the callee expected a cont in its args list or as a separate param); collapsing to a single signature removed the dispatch branch. Continuations ride in the args list when the callee is a CPS function; CPS closures receive them as ordinary captures.

## Function entry

The emitter knows each function's `$Captures` layout and its positional param count. Entry unpacks both:

```wat
;; fn {k, x}, [a, b]:
(func $foo (type $Fn2)
  (param $caps (ref null any))
  (param $args (ref null any))

  ;; Unpack captures from the $Captures array at local 0.
  (local.set $k
    (array.get $foo_caps
      (ref.cast (ref $foo_caps) (local.get $caps))
      (i32.const 0)))
  (local.set $x
    (array.get $foo_caps
      (ref.cast (ref $foo_caps) (local.get $caps))
      (i32.const 1)))

  ;; Unpack positional params from the args list.
  (local.set $a (call $list_head (local.get $args)))
  (local.set $args (call $list_tail (local.get $args)))
  (local.set $b (call $list_head (local.get $args)))
  ...)
```

## Call sites

Args are built right-to-left via `list_prepend`. The continuation (when present) is the **tail** of the list the callee receives — the call site prepends every positional arg and lastly the continuation. Callees that need a cont pop it off the args list at the position their static arity implies.

## Spread

`fn ..rest:` captures the whole args list as `rest`. Spread at a call site (`f a, ..b, c`) unpacks `b` and prepends its elements individually.

## Builtins

Fixed-arity builtins (`op_add`, `seq_prepend`, `str_fmt`, etc.) keep their direct WASM signatures and are called without going through `_apply`. They dispatch their result to the caller's continuation via `_apply` at the end.

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

Both compile to the same `$Fn2`. The distinction only affects how continuations are routed at emit time: `CpsFunction` callees expect a continuation appended to their args list; `CpsClosure` callees don't.

## See also

- [../../../docs/execution-model.md](../../../docs/execution-model.md) — why every function takes implicit context and continuation arguments (the language-level model this convention realises).
