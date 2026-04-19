# Calling Convention v2 — Captures + Args + Optional Cont

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

## Why not unified $Fn2?

Unified $Fn2 (cont always in args list) was attempted but doesn't work because:
- The caller doesn't always know whether the callee is CpsFunction or CpsClosure
- After lifting, CpsFunction closures go through `_apply`/`_apply_cont` dispatch
- `_apply_cont` needs runtime type checking (`ref.test $Fn3`) to know whether to
  pass cont as a separate param or prepend it to the args list
- Without this distinction, CpsClosure callees would see an unexpected cont at
  the head of their args list

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

`fn ..rest:` → spread takes the whole args list. No cont mixed in.

```wat
;; fn {k}, [..items], cont:
(func $first (type $Fn3) (param $caps ...) (param $args (ref $List)) (param $cont (ref any))
  ;; items = args (the whole list)
  (local.set $items (local.get $args))
  ...)
```

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

## Migration from v1

- `$FnN` per-arity types → `$Fn2` + `$Fn3` (two types total)
- `$Captures` array → per-function struct subtypes
- `_apply` / `_apply_cont` (trivial dispatch)
- `$SpreadArgs` wrapper → removed (spread = take whole args list)
- `$VarArgs` array → still used by `str_fmt` (builtin-internal, unchanged)

## v2.1 changes (CpsFnKind)

- `CpsFnKind` enum on `ExprKind::LetFn`: set by CPS transform at creation time
- `scan_cont_call_targets` removed — `CpsFnKind` carries the information directly
- Closure targets set to `CpsClosure` during lifting (they go through dispatch)
- `has_cont` on `CollectedFn` derived from `CpsFnKind` at collection time
