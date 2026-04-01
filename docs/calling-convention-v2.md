# Calling Convention v2 — Captures + Args + Optional Cont

## Overview

Every function takes `(captures, args)` or `(captures, args, cont)`:

- **captures**: the lexical environment — what the function closes over.
  Compile-time-known struct per function, accessed by field index.
- **args**: the user-visible arguments, as a cons-cell list.
  Spread (`fn ..rest:`) takes the whole list — no ambiguity.
- **cont**: CPS continuation — where to send the result.
  Only present for user-visible function calls; absent for inline
  continuations (operator results, match arms).

## WASM Types

```wat
;; Two function signatures:
(type $Fn2 (func (param (ref null $Captures) (ref $List))))       ;; continuations
(type $Fn3 (func (param (ref null $Captures) (ref $List) (ref any))))  ;; user functions

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

Two apply functions, both trivial pass-through:

```wat
;; apply_2: for continuations (no cont param)
(func $_apply_2 (param $args (ref $List)) (param $callee (ref any))
  ;; cast callee to $Closure, extract f + caps, tail-call f(caps, args)
  ...)

;; apply_3: for user function calls (with cont)
(func $_apply_3 (param $args (ref $List)) (param $cont (ref any)) (param $callee (ref any))
  ;; cast callee to $Closure, extract f + caps, tail-call f(caps, args, cont)
  ...)
```

No prepending, no loops. Just pass captures and args through.

## Call Site (emitter)

The emitter knows at compile time whether a call has a cont:

- `Arg::Cont` present (or CPS adds one) → `apply_3(args_list, cont, callee)`
- No `Arg::Cont` → `apply_2(args_list, callee)`

Args list is built via `list_prepend` in reverse order.

## Function Entry (emitter)

The emitter knows the function's capture struct type and param count:

```wat
;; fn {k, x}, [a, b], cont:
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

For continuations (`$Fn2`), same but no cont param.

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
They dispatch results to continuations via `apply_2`:

```wat
;; op_plus computes result, dispatches to cont:
(return_call $_apply_2
  (struct.new $Cons (local.get $result) (call $list_nil))  ;; [result]
  (local.get $cont))                                        ;; the closure
```

## Migration from v1

- `$FnN` per-arity types → `$Fn2` + `$Fn3` (two types total)
- `$Captures` array → per-function struct subtypes
- `_croc` with prepend loop → `_apply_2` / `_apply_3` (trivial pass-through)
- `$SpreadArgs` wrapper → removed (spread = take whole args list)
- `$VarArgs` array → still used by `str_fmt` (builtin-internal, unchanged)
