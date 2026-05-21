;; Operator implementations — CPS functions for arithmetic, comparison, and logic.
;;
;; Each operator follows the CPS calling convention:
;;   (func $op_plus (param $ctx ...) (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
;;     ;; unbox args, compute, box result, tail-call _apply([result], cont)
;;   )
;;
;; Type conventions:
;;   - Numbers: $Num struct (f64 field)
;;   - Booleans: i31ref (0 = false, 1 = true)
;;   - Continuation dispatch via _apply (imported from dispatch module)
;;
;; These are the phase-0 implementations operating on concrete types.
;; Protocol-based overloading (future) will replace these with dispatch
;; through user-defined protocol implementations.
;;
;; ctx convention (2026-05-16): every op_* takes $ctx as the first param.
;; Every op_* in this file FORWARDS $ctx to its cont (via list_apply_N).
;; What it does NOT do is consult $ctx for dispatch — type dispatch is
;; purely by `ref.test` on $a, and the per-type kernels in num/set/int/
;; float/list/dict are pure compute (no user-callbacks reachable), so we
;; don't pass ctx down into them either. The (param $ctx ...) line in
;; each op_* is marked `;; TODO ctx: not consulted` as a reminder: when
;; user-defined protocol impls land, this is the boundary where dispatch
;; must start consulting ctx (e.g. for ctx-scoped operator overrides).

(module

  ;; Type imports
  (import "std/num.wat"      "Num"         (type $Num         (sub any)))
  (import "std/int.wat"      "Int"         (type $Int         (sub any)))
  (import "std/int.wat"      "I64"         (type $I64         (sub any)))
  (import "std/float.wat"    "F64"         (type $F64         (sub any)))
  (import "std/str.wat"      "Str"         (type $Str         (sub any)))
  (import "std/list.wat"     "List"        (type $List        (sub any)))
  ;; Pull std/math.wat into the link DAG. rt/protocols doesn't call any
  ;; math primitive directly; this import exists so the linker reaches
  ;; math.wat's `(@impl "std/math.fnk:...")` entries.
  (import "std/math.wat"     "abs_f64"
    (func $_link_math_anchor (param (ref $F64)) (result (ref $F64))))
  (import "std/dict.wat"     "Rec"         (type $Rec         (sub any)))
  (import "std/dict.wat"     "RecImpl"     (type $RecImpl     (sub any)))
  (import "std/set.wat"      "Set"         (type $Set         (sub any)))
  (import "std/range.wat"    "Range"       (type $Range       (sub any)))
  (import "std/channel.wat"  "Channel"     (type $Channel     (sub any)))
  (import "interop.wat" "HostChannel" (type $HostChannel (sub any)))

  ;; Func imports — list helpers
  (import "rt/apply.wat" "apply_0" (func $list_apply_0 (;apply-ctx;) (param (ref null any)) (param $cont (ref null any))))
  (import "rt/apply.wat" "apply_1" (func $list_apply_1 (;apply-ctx;) (param (ref null any)) (param $val (ref null any)) (param $cont (ref null any))))
  (import "rt/apply.wat" "Closure" (type $Closure (sub any)))
  (import "rt/apply.wat" "op_eq"  (func $clos_op_eq  (param (ref $Closure)) (param (ref $Closure)) (result i32)))
  (import "rt/apply.wat" "op_neq" (func $clos_op_neq (param (ref $Closure)) (param (ref $Closure)) (result i32)))
  (import "std/list.wat" "op_empty"
    (func $list_op_empty (param $val (ref null any)) (result i32)))
  (import "std/list.wat" "seq_pop"
    (func $list_seq_pop (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))))
  (import "std/list.wat" "seq_pop_back"
    (func $list_seq_pop_back (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))))
  (import "std/list.wat" "seq_prepend"
    (func $list_seq_prepend (param $val (ref null any)) (param $list (ref null any)) (param $cont (ref null any))))
  (import "std/list.wat" "seq_concat"
    (func $list_seq_concat (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))))

  ;; Func imports — set ops
  (import "std/set.wat" "op_plus"     (func $set_op_plus     (param $b (ref $Set)) (result (ref $Set))))
  (import "std/set.wat" "op_minus"    (func $set_op_minus    (param $b (ref $Set)) (result (ref $Set))))
  (import "std/set.wat" "op_eq"       (func $set_op_eq       (param $b (ref $Set)) (result i32)))
  (import "std/set.wat" "op_disjoint" (func $set_op_disjoint (param $b (ref $Set)) (result i32)))
  (import "std/set.wat" "op_lt"       (func $set_op_lt       (param $b (ref $Set)) (result i32)))
  (import "std/set.wat" "op_lte"      (func $set_op_lte      (param $b (ref $Set)) (result i32)))
  (import "std/set.wat" "op_gt"       (func $set_op_gt       (param $b (ref $Set)) (result i32)))
  (import "std/set.wat" "op_gte"      (func $set_op_gte      (param $b (ref $Set)) (result i32)))
  (import "std/set.wat" "op_and"      (func $set_op_and      (param $b (ref $Set)) (result (ref $Set))))
  (import "std/set.wat" "op_or"       (func $set_op_or       (param $b (ref $Set)) (result (ref $Set))))
  (import "std/set.wat" "op_xor"      (func $set_op_xor      (param $b (ref $Set)) (result (ref $Set))))
  (import "std/set.wat" "op_in"       (func $set_op_in       (param $set (ref $Set)) (param $key (ref eq)) (result i32)))
  (import "std/set.wat" "op_notin"    (func $set_op_notin    (param $set (ref $Set)) (param $key (ref eq)) (result i32)))
  (import "std/set.wat" "op_empty"    (func $set_op_empty    (result i32)))
  (import "std/set.wat" "seq_pop"     (func $set_seq_pop     (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))))

  ;; Func imports — int ops
  ;; Numeric ops — all routed through num.wat (the numeric dispatcher).
  (import "std/num.wat" "op_plus"   (func $num_op_plus   (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/num.wat" "op_minus"  (func $num_op_minus  (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/num.wat" "op_mul"    (func $num_op_mul    (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/num.wat" "op_div"    (func $num_op_div    (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/num.wat" "op_eq"     (func $num_op_eq     (param (ref $Num)) (param (ref $Num)) (result i32)))
  (import "std/num.wat" "op_neq"    (func $num_op_neq    (param (ref $Num)) (param (ref $Num)) (result i32)))
  (import "std/num.wat" "op_lt"     (func $num_op_lt     (param (ref $Num)) (param (ref $Num)) (result i32)))
  (import "std/num.wat" "op_lte"    (func $num_op_lte    (param (ref $Num)) (param (ref $Num)) (result i32)))
  (import "std/num.wat" "op_gt"     (func $num_op_gt     (param (ref $Num)) (param (ref $Num)) (result i32)))
  (import "std/num.wat" "op_gte"    (func $num_op_gte    (param (ref $Num)) (param (ref $Num)) (result i32)))
  (import "std/num.wat" "op_intdiv" (func $num_op_intdiv (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/num.wat" "op_rem"    (func $num_op_rem    (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/num.wat" "op_intmod" (func $num_op_intmod (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/num.wat" "op_pow"    (func $num_op_pow    (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/num.wat" "op_divmod" (func $num_op_divmod (param (ref $Num)) (param (ref $Num)) (result (ref $List))))
  (import "std/num.wat" "op_rotl"   (func $num_op_rotl   (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/num.wat" "op_rotr"   (func $num_op_rotr   (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/num.wat" "op_not"    (func $num_op_not    (param (ref $Num)) (result (ref $Num))))
  (import "std/num.wat" "op_and"    (func $num_op_and    (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/num.wat" "op_or"     (func $num_op_or     (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/num.wat" "op_xor"    (func $num_op_xor    (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/num.wat" "op_shl"    (func $num_op_shl    (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/num.wat" "op_shr"    (func $num_op_shr    (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))

  ;; Func imports — str ops
  (import "std/str.wat" "op_eq"  (func $str_op_eq  (param (ref $Str)) (result i32)))
  (import "std/str.wat" "op_dot" (func $str_op_dot (param (ref null any)) (param (ref null any)) (param (ref null any))))

  ;; Func imports — dict ops
  (import "std/dict.wat" "op_in"     (func $dict_op_in     (param (ref $RecImpl)) (param (ref eq)) (result i32)))
  (import "std/dict.wat" "op_not_in" (func $dict_op_notin  (param (ref $RecImpl)) (param (ref eq)) (result i32)))
  (import "std/dict.wat" "op_empty"  (func $dict_op_empty  (param (ref null any)) (result i32)))
  (import "std/dict.wat" "op_dot"    (func $dict_op_dot    (param (ref null any)) (param (ref null any)) (param (ref null any))))
  (import "std/list.wat" "op_dot"    (func $list_op_dot    (param (ref null any)) (param (ref null any)) (param (ref null any))))

  ;; Func imports — range ops
  (import "std/range.wat" "op_in"     (func $range_op_in     (param (ref $I64)) (param (ref $Range)) (result i32)))
  (import "std/range.wat" "op_not_in" (func $range_op_not_in (param (ref $I64)) (param (ref $Range)) (result i32)))

  ;; Func imports — channel
  (import "std/channel.wat" "op_shr"  (func $channel_op_shr  (param (ref null any)) (param (ref null any)) (param (ref null any)) (param (ref null any))))
  (import "std/channel.wat" "receive" (func $channel_receive (param (ref null any)) (param (ref null any)) (param (ref null any))))
  ;; Func imports — interop (host bridge)
  ;; ctx-aware: each leading (ref null any) is the caller's $Ctx.
  (import "interop.wat" "channel_send" (func $interop_channel_send (param (ref null any)) (param (ref null any)) (param (ref null any)) (param (ref null any))))
  (import "interop.wat" "op_read"      (func $interop_op_read      (param (ref null any)) (param (ref null any)) (param (ref null any)) (param (ref null any))))

  ;; =========================================================================
  ;; Arithmetic: unbox two $Num, f64 op, box result → _apply([result], cont)
  ;; =========================================================================

  (func $op_plus (@pub) (@impl "std/operators.fnk:op_plus")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (local $a_set (ref $Set))

    ;; Try $Set — union
    (block $not_set
      (local.set $a_set
        (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $a)))))
      (return_call $list_apply_1
      (local.get $ctx)
        (call $set_op_plus
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b)))
        (local.get $cont)))

    ;; Default: $Num add
    (return_call $list_apply_1
      (local.get $ctx)
      (call $num_op_plus
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $op_minus (@pub) (@impl "std/operators.fnk:op_minus")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (local $a_set (ref $Set))

    ;; Try $Set — difference
    (block $not_set
      (local.set $a_set
        (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $a)))))
      (return_call $list_apply_1
      (local.get $ctx)
        (call $set_op_minus
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b)))
        (local.get $cont)))

    ;; Default: $Num sub
    (return_call $list_apply_1
      (local.get $ctx)
      (call $num_op_minus
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $op_mul (@pub) (@impl "std/operators.fnk:op_mul")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $list_apply_1
      (local.get $ctx)
      (call $num_op_mul
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $op_div (@pub) (@impl "std/operators.fnk:op_div")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $list_apply_1
      (local.get $ctx)
      (call $num_op_div
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  ;; =========================================================================
  ;; Integer arithmetic: unbox $Num → f64 → i64, op, i64 → f64 → box
  ;; =========================================================================

  (func $op_intdiv (@pub) (@impl "std/operators.fnk:op_intdiv")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $list_apply_1
      (local.get $ctx)
      (call $num_op_intdiv
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $op_rem (@pub) (@impl "std/operators.fnk:op_rem")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $list_apply_1
      (local.get $ctx)
      (call $num_op_rem
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $op_intmod (@pub) (@impl "std/operators.fnk:op_intmod")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $list_apply_1
      (local.get $ctx)
      (call $num_op_intmod
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $op_pow (@pub) (@impl "std/operators.fnk:op_pow")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $list_apply_1
      (local.get $ctx)
      (call $num_op_pow
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $op_divmod (@pub) (@impl "std/operators.fnk:op_divmod")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $list_apply_1
      (local.get $ctx)
      (call $num_op_divmod
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $op_rotl (@pub) (@impl "std/operators.fnk:op_rotl")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $list_apply_1
      (local.get $ctx)
      (call $num_op_rotl
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $op_rotr (@pub) (@impl "std/operators.fnk:op_rotr")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $list_apply_1
      (local.get $ctx)
      (call $num_op_rotr
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  ;; =========================================================================
  ;; Comparison: unbox two $Num, f64 compare → i31ref (0/1)
  ;; =========================================================================

  ;; Direct-style deep equality. Used by HAMT for key comparison.
  ;;   i31ref  → ref.eq (identity — fine for small ints and booleans)
  ;;   $Num    → f64.eq
  ;;   $Str → str_op_eq
  (func $deep_eq (@pub)
    (param $a (ref eq)) (param $b (ref eq)) (result i32)

    ;; Try $Num — strict subtype match.
    ;;   1 !== 1.0 (different concrete subtypes never equal even if
    ;;   numerically equivalent). HAMT keys are strict; arithmetic
    ;;   operators may still coerce via num.wat's op_eq dispatcher.
    (block $not_num
      (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref eq) (ref $Num)
            (local.get $a))))
      (drop)
      (if (i32.xor
            (ref.test (ref $F64) (local.get $a))
            (ref.test (ref $F64) (local.get $b)))
        (then (return (i32.const 0))))
      (if (i32.xor
            (ref.test (ref $Int) (local.get $a))
            (ref.test (ref $Int) (local.get $b)))
        (then (return (i32.const 0))))
      (return (call $num_op_eq
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))))

    ;; Try $Str
    (block $not_str
      (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref eq) (ref $Str)
            (local.get $a))))
      (return (call $str_op_eq
        (ref.cast (ref $Str) (local.get $b)))))

    ;; Fallback: ref.eq (i31ref, other GC types)
    (ref.eq (local.get $a) (local.get $b)))

  ;; Polymorphic ==: dispatch on $a's type.
  ;;   $Num    → f64.eq
  ;;   $Str    → str_op_eq
  ;;   $Set    → set:op_eq
  (func $op_eq (@pub) (@impl "std/operators.fnk:op_eq")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (local $a_num (ref $Num))
    (local $a_str (ref $Str))
    (local $a_set (ref $Set))

    ;; Try $Num
    (block $not_num
      (local.set $a_num
        (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref null any) (ref $Num)
            (local.get $a)))))
      ;; $a is $Num — cast $b and compare
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (call $num_op_eq
          (local.get $a_num)
          (ref.cast (ref $Num) (local.get $b))))
        (local.get $cont)))

    ;; Try $Str
    (block $not_str
      (local.set $a_str
        (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref null any) (ref $Str)
            (local.get $a)))))
      ;; $a is $Str — cast $b and call str_op_eq
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (call $str_op_eq
          (local.get $a_str)
          (ref.cast (ref $Str) (local.get $b))))
        (local.get $cont)))

    ;; Try $Set
    (block $not_set
      (local.set $a_set
        (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $a)))))
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (call $set_op_eq
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b))))
        (local.get $cont)))

    ;; i31ref (booleans, small ints) — ref.eq identity
    (block $not_i31
      (drop
        (block $is_i31 (result (ref i31))
          (br $not_i31
            (br_on_cast $is_i31 (ref null any) (ref i31)
              (local.get $a)))))
      (return_call $list_apply_1
        (local.get $ctx)
        (ref.i31 (ref.eq
          (ref.cast (ref i31) (local.get $a))
          (ref.cast (ref i31) (local.get $b))))
        (local.get $cont)))

    ;; Try $Closure -- delegate to apply.wat which owns closure equality.
    (block $not_clos
      (drop
        (block $is_clos (result (ref $Closure))
          (br $not_clos
            (br_on_cast $is_clos (ref null any) (ref $Closure)
              (local.get $a)))))
      (return_call $list_apply_1
        (local.get $ctx)
        (ref.i31 (call $clos_op_eq
          (ref.cast (ref $Closure) (local.get $a))
          (ref.cast (ref $Closure) (local.get $b))))
        (local.get $cont)))

    (unreachable))

  ;; Polymorphic !=: dispatch on $a's type.
  ;;   $Num    → f64.ne
  ;;   $Str    → !str_op_eq
  ;;   $Set    → !set:op_eq
  (func $op_neq (@pub) (@impl "std/operators.fnk:op_neq")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (local $a_num (ref $Num))
    (local $a_str (ref $Str))
    (local $a_set (ref $Set))

    ;; Try $Num
    (block $not_num
      (local.set $a_num
        (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref null any) (ref $Num)
            (local.get $a)))))
      ;; $a is $Num — cast $b and compare
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (call $num_op_neq
          (local.get $a_num)
          (ref.cast (ref $Num) (local.get $b))))
        (local.get $cont)))

    ;; Try $Str
    (block $not_str
      (local.set $a_str
        (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref null any) (ref $Str)
            (local.get $a)))))
      ;; $a is $Str — cast $b, call str_op_eq, invert
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (i32.eqz (call $str_op_eq
          (local.get $a_str)
          (ref.cast (ref $Str) (local.get $b)))))
        (local.get $cont)))

    ;; Try $Set
    (block $not_set
      (local.set $a_set
        (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $a)))))
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (i32.eqz (call $set_op_eq
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b)))))
        (local.get $cont)))

    ;; i31ref (booleans, small ints) — !ref.eq identity
    (block $not_i31
      (drop
        (block $is_i31 (result (ref i31))
          (br $not_i31
            (br_on_cast $is_i31 (ref null any) (ref i31)
              (local.get $a)))))
      (return_call $list_apply_1
        (local.get $ctx)
        (ref.i31 (i32.eqz (ref.eq
          (ref.cast (ref i31) (local.get $a))
          (ref.cast (ref i31) (local.get $b)))))
        (local.get $cont)))

    ;; Try $Closure -- delegate to apply.wat which owns closure equality.
    (block $not_clos
      (drop
        (block $is_clos (result (ref $Closure))
          (br $not_clos
            (br_on_cast $is_clos (ref null any) (ref $Closure)
              (local.get $a)))))
      (return_call $list_apply_1
        (local.get $ctx)
        (ref.i31 (call $clos_op_neq
          (ref.cast (ref $Closure) (local.get $a))
          (ref.cast (ref $Closure) (local.get $b))))
        (local.get $cont)))

    (unreachable))

  ;; Disjoint predicate: true iff a and b have no common elements.
  ;; Partial-order escape hatch — for sets where the standard ordering
  ;; relations don't apply.
  ;;   $Set    → set:op_disjoint
  (func $op_disjoint (@pub) (@impl "std/operators.fnk:op_disjoint")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (local $a_set (ref $Set))

    ;; Try $Set
    (block $not_set
      (local.set $a_set
        (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $a)))))
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (call $set_op_disjoint
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b))))
        (local.get $cont)))

    (unreachable))

  (func $op_lt (@pub) (@impl "std/operators.fnk:op_lt")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (local $a_set (ref $Set))

    ;; Try $Set — strict subset
    (block $not_set
      (local.set $a_set
        (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $a)))))
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (call $set_op_lt
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b))))
        (local.get $cont)))

    (return_call $list_apply_1
      (local.get $ctx)
      (ref.i31 (call $num_op_lt
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b))))
      (local.get $cont)))

  (func $op_lte (@pub) (@impl "std/operators.fnk:op_lte")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (local $a_set (ref $Set))

    ;; Try $Set — subset
    (block $not_set
      (local.set $a_set
        (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $a)))))
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (call $set_op_lte
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b))))
        (local.get $cont)))

    (return_call $list_apply_1
      (local.get $ctx)
      (ref.i31 (call $num_op_lte
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b))))
      (local.get $cont)))

  (func $op_gt (@pub) (@impl "std/operators.fnk:op_gt")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (local $a_set (ref $Set))

    ;; Try $Set — strict superset
    (block $not_set
      (local.set $a_set
        (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $a)))))
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (call $set_op_gt
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b))))
        (local.get $cont)))

    (return_call $list_apply_1
      (local.get $ctx)
      (ref.i31 (call $num_op_gt
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b))))
      (local.get $cont)))

  (func $op_gte (@pub) (@impl "std/operators.fnk:op_gte")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (local $a_set (ref $Set))

    ;; Try $Set — superset
    (block $not_set
      (local.set $a_set
        (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $a)))))
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (call $set_op_gte
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b))))
        (local.get $cont)))

    (return_call $list_apply_1
      (local.get $ctx)
      (ref.i31 (call $num_op_gte
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b))))
      (local.get $cont)))

  ;; =========================================================================
  ;; Logic / bitwise: polymorphic — $Num → integer bitwise, i31ref → boolean
  ;; =========================================================================

  (func $op_not (@pub) (@impl "std/operators.fnk:op_not")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $cont (ref null any))
    (local $a_num (ref $Num))

    ;; Try $Num → delegate to int_op_not
    (block $not_num
      (local.set $a_num
        (block $is_num (result (ref $Num))
          (br $not_num
            (br_on_cast $is_num (ref null any) (ref $Num)
              (local.get $a)))))
      (return_call $list_apply_1
        (local.get $ctx)
        (call $num_op_not (local.get $a_num))
        (local.get $cont)))

    ;; Fallback: i31ref boolean not
    (return_call $list_apply_1
      (local.get $ctx)
      (ref.i31 (i32.eqz (i31.get_s (ref.cast (ref i31) (local.get $a)))))
      (local.get $cont)))

  (func $op_and (@pub) (@impl "std/operators.fnk:op_and")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (local $a_num (ref $Num))
    (local $a_set (ref $Set))

    ;; Try $Set — intersect
    (block $not_set
      (local.set $a_set
        (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $a)))))
      (return_call $list_apply_1
      (local.get $ctx)
        (call $set_op_and
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b)))
        (local.get $cont)))

    ;; Try $Num → delegate to int_op_and
    (block $not_num
      (local.set $a_num
        (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref null any) (ref $Num)
            (local.get $a)))))
      (return_call $list_apply_1
      (local.get $ctx)
        (call $num_op_and
          (local.get $a_num)
          (ref.cast (ref $Num) (local.get $b)))
        (local.get $cont)))

    ;; Fallback: i31ref boolean and
    (return_call $list_apply_1
      (local.get $ctx)
      (ref.i31 (i32.and
        (i31.get_s (ref.cast (ref i31) (local.get $a)))
        (i31.get_s (ref.cast (ref i31) (local.get $b)))))
      (local.get $cont)))

  (func $op_or (@pub) (@impl "std/operators.fnk:op_or")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (local $a_num (ref $Num))
    (local $a_set (ref $Set))

    ;; Try $Set — union
    (block $not_set
      (local.set $a_set
        (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $a)))))
      (return_call $list_apply_1
      (local.get $ctx)
        (call $set_op_or
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b)))
        (local.get $cont)))

    ;; Try $Num → delegate to int_op_or
    (block $not_num
      (local.set $a_num
        (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref null any) (ref $Num)
            (local.get $a)))))
      (return_call $list_apply_1
      (local.get $ctx)
        (call $num_op_or
          (local.get $a_num)
          (ref.cast (ref $Num) (local.get $b)))
        (local.get $cont)))

    ;; Fallback: i31ref boolean or
    (return_call $list_apply_1
      (local.get $ctx)
      (ref.i31 (i32.or
        (i31.get_s (ref.cast (ref i31) (local.get $a)))
        (i31.get_s (ref.cast (ref i31) (local.get $b)))))
      (local.get $cont)))

  (func $op_xor (@pub) (@impl "std/operators.fnk:op_xor")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (local $a_num (ref $Num))
    (local $a_set (ref $Set))

    ;; Try $Set — symmetric difference
    (block $not_set
      (local.set $a_set
        (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $a)))))
      (return_call $list_apply_1
      (local.get $ctx)
        (call $set_op_xor
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b)))
        (local.get $cont)))

    ;; Try $Num → delegate to int_op_xor
    (block $not_num
      (local.set $a_num
        (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref null any) (ref $Num)
            (local.get $a)))))
      (return_call $list_apply_1
      (local.get $ctx)
        (call $num_op_xor
          (local.get $a_num)
          (ref.cast (ref $Num) (local.get $b)))
        (local.get $cont)))

    ;; Fallback: i31ref boolean xor
    (return_call $list_apply_1
      (local.get $ctx)
      (ref.i31 (i32.xor
        (i31.get_s (ref.cast (ref i31) (local.get $a)))
        (i31.get_s (ref.cast (ref i31) (local.get $b)))))
      (local.get $cont)))

  ;; =========================================================================
  ;; Type guards — is_seq_like / is_rec_like
  ;; =========================================================================

  ;; is_seq_like(val, succ, fail): succ(val) if $List or $Set, else fail()
  (func $is_seq_like (@pub) (@impl "std/operators.fnk:is_seq_like")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $val (ref null any)) (param $succ (ref null any)) (param $fail (ref null any))

    ;; $List
    (block $not_list
      (block $is_list (result (ref $List))
        (br $not_list
          (br_on_cast $is_list (ref null any) (ref $List)
            (local.get $val))))
      (drop)
      (return_call $list_apply_1
      (local.get $ctx) (local.get $val) (local.get $succ)))

    ;; $Set
    (block $not_set
      (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $val))))
      (drop)
      (return_call $list_apply_1
      (local.get $ctx) (local.get $val) (local.get $succ)))

    (return_call $list_apply_0
      (local.get $ctx) (local.get $fail)))

  ;; is_rec_like(val, succ, fail): succ(val) if $Rec, else fail()
  (func $is_rec_like (@pub) (@impl "std/operators.fnk:is_rec_like")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $val (ref null any)) (param $succ (ref null any)) (param $fail (ref null any))
    (block $not_rec
      (block $is_rec (result (ref $Rec))
        (br $not_rec
          (br_on_cast $is_rec (ref null any) (ref $Rec)
            (local.get $val))))
      (drop)
      (return_call $list_apply_1
      (local.get $ctx) (local.get $val) (local.get $succ)))
    (return_call $list_apply_0
      (local.get $ctx) (local.get $fail)))

  ;; =========================================================================
  ;; Collection predicates (polymorphic — dispatch on type tag)
  ;; =========================================================================

  ;; Polymorphic empty: dispatch on value type to module predicates.
  ;;   null     → true (always empty)
  ;;   $List    → list_op_empty
  ;;   $Rec     → rec_op_empty
  (func $op_empty (@pub) (@impl "std/operators.fnk:op_empty")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $val (ref null any)) (param $cont (ref null any))
    (local $a_set (ref $Set))

    ;; null = empty
    (if (ref.is_null (local.get $val))
      (then
        (return_call $list_apply_1
      (local.get $ctx)
          (ref.i31 (i32.const 1))
          (local.get $cont))))

    ;; $List → list_op_empty
    (block $not_list
      (block $is_list (result (ref $List))
        (br $not_list
          (br_on_cast $is_list (ref null any) (ref $List)
            (local.get $val))))
      (drop)
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (call $list_op_empty (local.get $val)))
        (local.get $cont)))

    ;; $Rec → rec_op_empty
    (block $not_rec
      (block $is_rec (result (ref $Rec))
        (br $not_rec
          (br_on_cast $is_rec (ref null any) (ref $Rec)
            (local.get $val))))
      (drop)
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (call $dict_op_empty (local.get $val)))
        (local.get $cont)))

    ;; $Set → set:op_empty
    (block $not_set
      (local.set $a_set
        (block $is_set (result (ref $Set))
          (br $not_set
            (br_on_cast $is_set (ref null any) (ref $Set)
              (local.get $val)))))
      (return_call $list_apply_1
        (local.get $ctx)
        (ref.i31 (call $set_op_empty (local.get $a_set)))
        (local.get $cont)))

    (unreachable))

  ;; =========================================================================
  ;; Sequence destructure: `[head, ..tail]` patterns
  ;; =========================================================================

  ;; seq_pop(cursor, fail, succ): peel one element off any seq-like
  ;; container.
  ;;   $List → list:seq_pop
  ;;   $Set  → set:seq_pop
  ;; If empty: tail-call fail() with no args.
  ;; Else: tail-call succ(head, tail) with two args.
  (func $seq_pop (@pub) (@impl "std/seq.fnk:pop")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))

    ;; $Set → set:seq_pop
    (block $not_set
      (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $cursor))))
      (drop)
      (return_call $set_seq_pop
        (local.get $cursor) (local.get $fail) (local.get $succ)))

    ;; Default: list (or $Nil)
    (return_call $list_seq_pop
      (local.get $cursor) (local.get $fail) (local.get $succ)))

  ;; seq_prepend(val, seq, cont): cons-style prepend for any seq-like
  ;; container. Today $List only — sets and other seq types could
  ;; gain a typed impl in future.
  (func $seq_prepend (@pub) (@impl "std/seq.fnk:prepend")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $val (ref null any)) (param $seq (ref null any)) (param $cont (ref null any))
    (return_call $list_seq_prepend
      (local.get $val) (local.get $seq) (local.get $cont)))

  ;; seq_concat(a, b, cont): concatenate two seqs. Today $List only;
  ;; other seq types could gain a typed impl in future. Used for list
  ;; literals containing a spread (`[..xs, y]`, `[..a, ..b]`).
  (func $seq_concat (@pub) (@impl "std/seq.fnk:concat")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $list_seq_concat
      (local.get $a) (local.get $b) (local.get $cont)))

  ;; seq_pop_back(cursor, fail, succ): peel one element off the END of a
  ;; seq-like container. Currently only $List is supported (sets have no
  ;; defined ordering, so "last" isn't meaningful).
  ;; If empty: tail-call fail() with no args.
  ;; Else: tail-call succ(init, last) with two args.
  (func $seq_pop_back (@pub) (@impl "std/seq.fnk:pop_back")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))

    (return_call $list_seq_pop_back
      (local.get $cursor) (local.get $fail) (local.get $succ)))

  ;; =========================================================================
  ;; Membership: `in` / `not in` — dispatch on container type
  ;; =========================================================================

  ;; op_in(val, container, cont) → bool
  (func $op_in (@pub) (@impl "std/operators.fnk:op_in")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (local $range (ref $Range))
    (local $rec (ref $RecImpl))
    (local $set (ref $Set))

    ;; Try $Range
    (block $not_range
      (block $is_range (result (ref $Range))
        (br $not_range
          (br_on_cast $is_range (ref null any) (ref $Range)
            (local.get $b))))
      (local.set $range)
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (call $range_op_in
          (ref.cast (ref $I64) (local.get $a))
          (local.get $range)))
        (local.get $cont)))

    ;; Try $Rec
    (block $not_rec
      (block $is_rec (result (ref $RecImpl))
        (br $not_rec
          (br_on_cast $is_rec (ref null any) (ref $RecImpl)
            (local.get $b))))
      (local.set $rec)
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (call $dict_op_in
          (local.get $rec)
          (ref.cast (ref eq) (local.get $a))))
        (local.get $cont)))

    ;; Try $Set
    (block $not_set
      (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $b))))
      (local.set $set)
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (call $set_op_in
          (local.get $set)
          (ref.cast (ref eq) (local.get $a))))
        (local.get $cont)))

    (unreachable))

  ;; op_notin(val, container, cont) → bool
  (func $op_notin (@pub) (@impl "std/operators.fnk:op_notin")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (local $range (ref $Range))
    (local $rec (ref $RecImpl))
    (local $set (ref $Set))

    ;; Try $Range
    (block $not_range
      (block $is_range (result (ref $Range))
        (br $not_range
          (br_on_cast $is_range (ref null any) (ref $Range)
            (local.get $b))))
      (local.set $range)
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (call $range_op_not_in
          (ref.cast (ref $I64) (local.get $a))
          (local.get $range)))
        (local.get $cont)))

    ;; Try $Rec
    (block $not_rec
      (block $is_rec (result (ref $RecImpl))
        (br $not_rec
          (br_on_cast $is_rec (ref null any) (ref $RecImpl)
            (local.get $b))))
      (local.set $rec)
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (call $dict_op_notin
          (local.get $rec)
          (ref.cast (ref eq) (local.get $a))))
        (local.get $cont)))

    ;; Try $Set
    (block $not_set
      (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $b))))
      (local.set $set)
      (return_call $list_apply_1
      (local.get $ctx)
        (ref.i31 (call $set_op_notin
          (local.get $set)
          (ref.cast (ref eq) (local.get $a))))
        (local.get $cont)))

    (unreachable))

  ;; =========================================================================
  ;; Member access: `.` — dispatch on container type
  ;; =========================================================================

  ;; op_dot(container, key, cont) → val
  ;;   $Str  → str_op_dot
  ;;   $Rec  → rec_op_dot
  ;;   $List → list_op_dot
  (func $op_dot (@pub) (@impl "std/operators.fnk:op_dot")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $container (ref null any)) (param $key (ref null any)) (param $cont (ref null any))

    ;; Try $Str
    (block $not_str
      (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref null any) (ref $Str)
            (local.get $container))))
      (drop)
      (return_call $str_op_dot
        (local.get $container)
        (local.get $key)
        (local.get $cont)))

    ;; Try $Rec
    (block $not_rec
      (block $is_rec (result (ref $RecImpl))
        (br $not_rec
          (br_on_cast $is_rec (ref null any) (ref $RecImpl)
            (local.get $container))))
      (drop)
      (return_call $dict_op_dot
        (local.get $container)
        (local.get $key)
        (local.get $cont)))

    ;; Try $List
    (block $not_list
      (block $is_list (result (ref $List))
        (br $not_list
          (br_on_cast $is_list (ref null any) (ref $List)
            (local.get $container))))
      (drop)
      (return_call $list_op_dot
        (local.get $container)
        (local.get $key)
        (local.get $cont)))

    (unreachable))

  ;; =========================================================================
  ;; Shift left: `<<` — polymorphic ($Num → bitwise, $Channel → send)
  ;; =========================================================================

  ;; op_shl(a, b, cont):
  ;;   $HostChannel on a → interop_channel_send(a, b, cont)
  ;;   $Channel on a     → channel_op_shr(a, b, cont)  [ch << msg]
  ;;   otherwise         → int_op_shl(a, b)  [numeric shift]
  ;; NB: $HostChannel check must come before $Channel (subtype).
  (func $op_shl (@pub) (@impl "std/operators.fnk:op_shl")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))

    ;; Try $HostChannel on a → host channel send
    (block $not_host_channel
      (block $is_host_channel (result (ref $HostChannel))
        (br $not_host_channel
          (br_on_cast $is_host_channel (ref null any) (ref $HostChannel)
            (local.get $a))))
      (drop)
      (return_call $interop_channel_send
        (local.get $ctx)
        (local.get $a)
        (local.get $b)
        (local.get $cont)))

    ;; Try $Channel on a → channel send
    (block $not_channel
      (block $is_channel (result (ref $Channel))
        (br $not_channel
          (br_on_cast $is_channel (ref null any) (ref $Channel)
            (local.get $a))))
      (drop)
      (return_call $channel_op_shr
        (local.get $ctx)
        (local.get $a)
        (local.get $b)
        (local.get $cont)))

    ;; Fallback: numeric shift left
    (return_call $list_apply_1
      (local.get $ctx)
      (call $num_op_shl
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  ;; =========================================================================
  ;; Shift right: `>>` — polymorphic ($Num → bitwise, $Channel → send)
  ;; =========================================================================

  ;; op_shr(a, b, cont):
  ;;   $HostChannel on b → interop_channel_send(b, a, cont)
  ;;   $Channel on b     → channel_op_shr(b, a, cont)  [msg >> ch]
  ;;   otherwise         → int_op_shr(a, b)  [numeric shift]
  ;; NB: $HostChannel check must come before $Channel (subtype).
  (func $op_shr (@pub) (@impl "std/operators.fnk:op_shr")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))

    ;; Try $HostChannel on b → host channel send
    (block $not_host_channel
      (block $is_host_channel (result (ref $HostChannel))
        (br $not_host_channel
          (br_on_cast $is_host_channel (ref null any) (ref $HostChannel)
            (local.get $b))))
      (drop)
      (return_call $interop_channel_send
        (local.get $ctx)
        (local.get $b)
        (local.get $a)
        (local.get $cont)))

    ;; Try $Channel on b → channel send
    (block $not_channel
      (block $is_channel (result (ref $Channel))
        (br $not_channel
          (br_on_cast $is_channel (ref null any) (ref $Channel)
            (local.get $b))))
      (drop)
      (return_call $channel_op_shr
        (local.get $ctx)
        (local.get $b)
        (local.get $a)
        (local.get $cont)))

    ;; Fallback: numeric shift right
    (return_call $list_apply_1
      (local.get $ctx)
      (call $num_op_shr
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))


  ;; =========================================================================
  ;; receive — drain a runtime channel
  ;; =========================================================================

  ;; receive(ch, cont): tail-call into channel.wat's receive impl.
  ;; Host-channel reads no longer reach this path — `read` is the
  ;; host-coupled alternative (see std/io.fnk:read in interop/rust.wat).
  ;; The user-facing dispatcher is std/channel.wat's `receive` directly;
  ;; this trampoline exists only as the cross-wat handle (typed impl
  ;; on $Channel) for protocol-table consumers.
  (func $channels_receive (@pub) (@impl "std/channel.fnk:receive" $Channel)
      (param $ctx (ref null any))
    (param $ch (ref null any)) (param $cont (ref null any))
    (return_call $channel_receive
      (local.get $ctx)
      (local.get $ch)
      (local.get $cont)))


  ;; =========================================================================
  ;; read — async read from a stream
  ;; =========================================================================

  ;; op_read(ctx, stream, size, cont):
  ;;   Dispatches to interop_op_read for host channels.
  (func $op_read (@pub) (@impl "rt/protocols.wat:op_read")
    (param $ctx (ref null any))
    (param $stream (ref null any))
    (param $size (ref null any))
    (param $cont (ref null any))

    (return_call $interop_op_read
      (local.get $ctx)
      (local.get $stream)
      (local.get $size)
      (local.get $cont)))


)
