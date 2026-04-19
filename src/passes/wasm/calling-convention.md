# Calling Convention — Captures + Args + Optional Cont

## Background

Fink supports both variadic params (`fn a, ..rest:`) and call-site spread
(`f a, ..b, c`). The four permutations all have to work — varargs and
fixed-arity callees, with and without spread at the call site:

```fink
f = fn a, ..rest:       # varargs callee
f a, b                  # rest = [b]
f a, ..b, c             # rest = [..b, c]

g = fn a, b:            # fixed-arity callee
g a, b                  # normal
g a, ..b, c             # spread unpacked, must yield exactly 2 total args
```

Neither the caller nor the callee can know the other's shape at compile
time in general — closures, higher-order functions, and callbacks all
defer the resolution. Lifting can wrap any function (including
continuations and matcher funcs) in a `$Closure` when it captures
variables, so all functions must share a small, fixed set of WASM
signatures rather than one signature per arity.

Hence the design below: args travel as a single cons-cell `$List` so
varargs/spread fall out for free, and the cont (when present) rides
alongside as a separate WASM param so a closure can be invoked uniformly
without knowing whether the callee expected a cont or not.

## Overview

Every function takes `(captures, args)` or `(captures, args, cont)`:

- **captures**: the lexical environment — what the function closes over.
  Compile-time-known struct per function, accessed by field index.
- **args**: the user-visible arguments, as a cons-cell list.
  Spread (`fn ..rest:`) takes the whole list — no ambiguity.
- **cont**: CPS continuation — where to send the result.
  Only present for CpsFunction calls; absent for CpsClosure calls.

## CpsFnKind — the key distinction

The CPS transform tags every `LetFn` with its kind:

```rust
enum CpsFnKind {
  /// Called with `Arg::Cont` at the call site. Includes user-defined
  /// functions, match wrappers (m_0), and match matchers (mp_N).
  CpsFunction,

  /// Never called with `Arg::Cont`. Includes compiler-generated
  /// continuations (inline cont bodies), match arm bodies (mb_N),
  /// PatternMatch bodies and matchers, and success wrappers.
  CpsClosure,
}
```

Set once by the CPS transform at creation time. Preserved through lifting
(closure targets are set to CpsClosure since they go through dispatch).
The collect phase reads `CpsFnKind` to set `has_cont` — no call-site
scanning needed.

## WASM Types

```wat
;; Two function signatures:
(type $Fn2 (func (param (ref null $Captures) (ref $List))))       ;; CpsClosure
(type $Fn3 (func (param (ref null $Captures) (ref $List) (ref any))))  ;; CpsFunction

;; One closure type (funcref is untyped — cast at dispatch):
(type $Closure (struct (field $func funcref) (field $captures (ref null $Captures))))

;; Per-function capture structs (compile-time known layout):
(type $Captures (struct))                          ;; base type
(type $v_25_caps (sub $Captures (struct            ;; example: captures k, x
  (field $k (ref any))
  (field $x (ref any))
)))
```

## Dispatch

Two apply functions:

```wat
;; _apply: for CpsClosure calls (no cont param)
(func $_apply (param $args (ref $List)) (param $callee (ref any))
  ;; cast callee to $Closure, extract f + caps, tail-call f(caps, args)
  ...)

;; _apply_cont: for CpsFunction calls (with cont)
;; Tries $Fn3 first; if callee is $Fn2, prepends cont onto args.
(func $_apply_cont (param $args (ref $List)) (param $cont (ref any)) (param $callee (ref any))
  ...)
```

## Rejected alternatives

### Unified `$Fn2` — cont always in args list

Folding the cont into the args list (one signature instead of two) was
attempted and rejected:

- The caller doesn't always know whether the callee is CpsFunction or CpsClosure.
- After lifting, CpsFunction closures go through `_apply` / `_apply_cont` dispatch.
- `_apply_cont` would need runtime type checking (`ref.test $Fn3`) to decide
  whether to pass cont as a separate param or prepend it to the args list.
- Without the distinction, CpsClosure callees would see an unexpected cont at
  the head of their args list.

### Universal `$Fn(ref $VarArgs)` — single signature, everything in an array

A more aggressive variant was sketched: collapse all functions to one
signature `$Fn(ref $VarArgs)` where `$VarArgs` is a heterogeneous array
holding captures + value args + cont. Rejected for two reasons:

- Every function call would allocate a fresh `$VarArgs` array, and every
  function entry would unpack its params via `array.get`. The cons-cell
  `$List` we use instead is the same data structure as user-level
  sequences, so it composes for free with spread and varargs.
- Closure dispatch would have to allocate a *second* array (captures
  prepended to args). The current `$Closure(funcref, $Captures)` shape
  keeps captures in a fixed struct accessed by field index, no
  per-call allocation.

The "no per-call allocation for captures" + "args list is the same type
users already work with" wins outweighed the appeal of a single
signature.

## Call Site (emitter)

The emitter knows at compile time whether a call has a cont:

- `Arg::Cont` present → `_apply_cont(args_list, cont, callee)`
  - If callee is known CpsClosure (not in `cont_fns`), fold cont into args + use `_apply`
- No `Arg::Cont` → `_apply(args_list, callee)`

Args list is built via `list_prepend` in reverse order.

## Function Entry (emitter)

The emitter knows the function's capture struct type and param count:

```wat
;; CpsFunction: fn {k, x}, [a, b], cont:
(func $foo (type $Fn3) (param $caps (ref null $Captures)) (param $args (ref $List)) (param $cont (ref any))
  ;; Cast captures to specific struct type:
  (local.set $k (struct.get $foo_caps $k (ref.cast (ref $foo_caps) (local.get $caps))))
  (local.set $x (struct.get $foo_caps $x (ref.cast (ref $foo_caps) (local.get $caps))))
  ;; Unpack args from list:
  (local.set $a (call $list_head_any (local.get $args)))
  (local.set $args (call $list_tail_any (local.get $args)))
  (local.set $b (call $list_head_any (local.get $args)))
  ;; cont is already a local from the WASM param
  ...)
```

For CpsClosure (`$Fn2`), same but no cont param.

## Spread

### Callee side: `fn ..rest:`

`fn ..rest:` takes the whole args list. No cont mixed in.

```wat
;; fn {k}, [..items], cont:
(func $first (type $Fn3) (param $caps ...) (param $args (ref $List)) (param $cont (ref any))
  ;; items = args (the whole list)
  (local.set $items (local.get $args))
  ...)
```

For mixed `fn a, ..rest:` the emitter pops the leading fixed params off
the head of the list and binds `rest` to the tail.

### Call site: `f a, ..b, c`

The emitter builds the args list right-to-left via `list_prepend`. For
spread arguments (`..b`), it splices in `b`'s elements via `list_concat`
instead of prepending the value as a single element. The result is a
single `(ref $List)` regardless of how many spread/normal args appeared.

### Tagged templates

Tagged templates fall out for free. `tag'hello ${x} world'` compiles to
a call where the args list is `['hello ', x, ' world']` and the tag
function is itself a `fn ..parts:` — the parts (raw string segments and
interpolated values, interleaved) arrive as a single sequence the
function can pattern-match or iterate over.

## Closure Construction

```wat
;; closure(f, cap_a, cap_b) → $Closure(funcref, $Captures{a, b})
(struct.new $Closure
  (ref.func $f)
  (struct.new $f_caps (local.get $a) (local.get $b)))
```

Zero captures: `(ref.null $Captures)`.

## Builtins

Builtins (op_plus, seq_prepend, etc.) keep their fixed-arity signatures.
They dispatch results to continuations via `apply_1` → `_apply`:

```wat
;; op_plus computes result, dispatches to cont:
(return_call $apply_1
  (local.get $result)
  (local.get $cont))   ;; the closure
```

