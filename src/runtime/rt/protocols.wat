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
  ;; Same anchor pattern for rt/types.wat: codegen calls `new_type`
  ;; directly (resolved via the runtime func-name table), so the module
  ;; must be in the link DAG even though no runtime module dispatches to it.
  (import "rt/types.wat"     "new_type"
    (func $_link_types_anchor
      (param (ref null any)) (param i32) (param i32) (param (ref null any))))
  (import "rt/types.wat"     "Type"         (type $Type         (sub any)))
  (import "rt/types.wat"     "Union"        (type $Union        (sub $Type)))
  (import "rt/types.wat"     "union_eq"
    (func $union_eq (param (ref $Union)) (param (ref $Union)) (result i32)))
  (import "rt/types.wat"     "Inst"         (type $Inst         (sub any)))
  (import "rt/types.wat"     "Rec"          (type $Rec_inst     (sub $Inst)))
  (import "rt/types.wat"     "Tuple"        (type $Tuple_inst   (sub $Inst)))
  (import "rt/types.wat"     "inst_eq"
    (func $inst_eq (param (ref $Inst)) (param (ref $Inst)) (result i32)))
  (import "rt/types.wat"     "inst_payload"
    (func $inst_payload (param (ref null any)) (result (ref null any))))
  (import "rt/types.wat"     "is_instance"
    (func $is_instance (param (ref null any)) (param (ref null any)) (result i32)))
  (import "std/dict.wat"     "Dict"         (type $Dict         (sub any)))
  (import "std/set.wat"      "Set"         (type $Set         (sub any)))
  (import "std/range.wat"    "Range"       (type $Range       (sub any)))

  ;; Func imports — list helpers
  (import "rt/apply.wat" "apply_0" (func $apply_0 (;apply-ctx;) (param (ref null any)) (param $cont (ref null any))))
  (import "rt/apply.wat" "apply_1" (func $apply_1 (;apply-ctx;) (param (ref null any)) (param $val (ref null any)) (param $cont (ref null any))))
  (import "rt/apply.wat" "apply_2_vals" (func $apply_2_vals (;apply-ctx;) (param (ref null any)) (param (ref null any)) (param (ref null any)) (param (ref null any))))
  (import "rt/apply.wat" "make_guard_branch"
    (func $make_guard_branch (param (ref null any)) (param (ref any)) (param (ref any)) (param (ref null any)) (result (ref $Closure))))
  (import "rt/apply.wat" "Closure" (type $Closure (sub any)))
  (import "rt/apply.wat" "op_eq"  (func $clos_op_eq  (param (ref $Closure)) (param (ref $Closure)) (result i32)))
  (import "rt/apply.wat" "op_neq" (func $clos_op_neq (param (ref $Closure)) (param (ref $Closure)) (result i32)))
  (import "rt/opaque.wat" "Opaque" (type $Opaque (sub any)))
  (import "rt/opaque.wat" "op_eq"  (func $opaque_op_eq  (param (ref $Opaque)) (param (ref $Opaque)) (result i32)))
  (import "rt/opaque.wat" "op_neq" (func $opaque_op_neq (param (ref $Opaque)) (param (ref $Opaque)) (result i32)))
  (import "std/list.wat" "op_empty"
    (func $list_op_empty (param $val (ref null any)) (result i32)))
  (import "std/list.wat" "seq_pop"
    (func $list_seq_pop (param $ctx (ref null any)) (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))))
  (import "std/list.wat" "seq_pop_back"
    (func $list_seq_pop_back (param $ctx (ref null any)) (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))))
  (import "std/list.wat" "seq_prepend"
    (func $list_seq_prepend (param $ctx (ref null any)) (param $val (ref null any)) (param $list (ref null any)) (param $cont (ref null any))))
  (import "std/list.wat" "seq_concat"
    (func $list_seq_concat (param $ctx (ref null any)) (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))))

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
  (import "std/set.wat" "seq_pop"     (func $set_seq_pop     (param $ctx (ref null any)) (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))))

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
  (import "std/str.wat" "op_dot" (func $str_op_dot (param (ref null any)) (param (ref null any)) (param (ref null any)) (param (ref null any))))

  ;; Func imports — dict ops
  (import "std/dict.wat" "op_in"     (func $dict_op_in     (param (ref $Dict)) (param (ref eq)) (result i32)))
  (import "std/dict.wat" "op_not_in" (func $dict_op_notin  (param (ref $Dict)) (param (ref eq)) (result i32)))
  (import "std/dict.wat" "rec_deep_eq" (func $rec_deep_eq  (param (ref $Dict)) (param (ref $Dict)) (result i32)))
  (import "std/list.wat" "list_deep_eq" (func $list_deep_eq (param (ref $List)) (param (ref $List)) (result i32)))
  (import "std/range.wat" "range_deep_eq" (func $range_deep_eq (param (ref $Range)) (param (ref $Range)) (result i32)))
  (import "std/dict.wat" "op_empty"  (func $dict_op_empty  (param (ref null any)) (result i32)))
  (import "std/dict.wat" "op_dot"    (func $dict_op_dot    (param (ref null any)) (param (ref null any)) (param (ref null any)) (param (ref null any))))
  (import "std/list.wat" "op_dot"    (func $list_op_dot    (param (ref null any)) (param (ref null any)) (param (ref null any)) (param (ref null any))))

  ;; Func imports — range ops
  (import "std/range.wat" "op_in"     (func $range_op_in     (param (ref $I64)) (param (ref $Range)) (result i32)))
  (import "std/range.wat" "op_not_in" (func $range_op_not_in (param (ref $I64)) (param (ref $Range)) (result i32)))

  ;; Func imports — interop (host bridge)
  ;; ctx-aware: each leading (ref null any) is the caller's $Ctx.

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
      (return_call $apply_1
      (local.get $ctx)
        (call $set_op_plus
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b)))
        (local.get $cont)))

    ;; Default: $Num add
    (return_call $apply_1
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
      (return_call $apply_1
      (local.get $ctx)
        (call $set_op_minus
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b)))
        (local.get $cont)))

    ;; Default: $Num sub
    (return_call $apply_1
      (local.get $ctx)
      (call $num_op_minus
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $op_mul (@pub) (@impl "std/operators.fnk:op_mul")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (call $num_op_mul
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $op_div (@pub) (@impl "std/operators.fnk:op_div")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
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
    (return_call $apply_1
      (local.get $ctx)
      (call $num_op_intdiv
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $op_rem (@pub) (@impl "std/operators.fnk:op_rem")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (call $num_op_rem
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $op_intmod (@pub) (@impl "std/operators.fnk:op_intmod")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (call $num_op_intmod
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $op_pow (@pub) (@impl "std/operators.fnk:op_pow")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (call $num_op_pow
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $op_divmod (@pub) (@impl "std/operators.fnk:op_divmod")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (call $num_op_divmod
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $op_rotl (@pub) (@impl "std/operators.fnk:op_rotl")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (call $num_op_rotl
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $op_rotr (@pub) (@impl "std/operators.fnk:op_rotr")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
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

    ;; Try $Dict — structural compare. If b is not a $Dict, not equal.
    (block $not_rec
      (block $is_rec (result (ref $Dict))
        (br $not_rec
          (br_on_cast $is_rec (ref eq) (ref $Dict)
            (local.get $a))))
      (drop)
      (if (i32.eqz (ref.test (ref $Dict) (local.get $b)))
        (then (return (i32.const 0))))
      (return (call $rec_deep_eq
        (ref.cast (ref $Dict) (local.get $a))
        (ref.cast (ref $Dict) (local.get $b)))))

    ;; Try $List — structural compare. If b is not a $List, not equal.
    (block $not_list
      (block $is_list (result (ref $List))
        (br $not_list
          (br_on_cast $is_list (ref eq) (ref $List)
            (local.get $a))))
      (drop)
      (if (i32.eqz (ref.test (ref $List) (local.get $b)))
        (then (return (i32.const 0))))
      (return (call $list_deep_eq
        (ref.cast (ref $List) (local.get $a))
        (ref.cast (ref $List) (local.get $b)))))

    ;; Try $Set — structural compare via set's own op_eq. If b is not a
    ;; $Set, not equal.
    (block $not_set
      (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref eq) (ref $Set)
            (local.get $a))))
      (drop)
      (if (i32.eqz (ref.test (ref $Set) (local.get $b)))
        (then (return (i32.const 0))))
      (return (call $set_op_eq
        (ref.cast (ref $Set) (local.get $a))
        (ref.cast (ref $Set) (local.get $b)))))

    ;; Try $Range — structural compare. If b is not a $Range, not equal.
    (block $not_range
      (block $is_range (result (ref $Range))
        (br $not_range
          (br_on_cast $is_range (ref eq) (ref $Range)
            (local.get $a))))
      (drop)
      (if (i32.eqz (ref.test (ref $Range) (local.get $b)))
        (then (return (i32.const 0))))
      (return (call $range_deep_eq
        (ref.cast (ref $Range) (local.get $a))
        (ref.cast (ref $Range) (local.get $b)))))

    ;; Fallback: ref.eq (i31ref, other GC types)
    (ref.eq (local.get $a) (local.get $b)))

  ;; Polymorphic ==: dispatch on $a's type.
  ;;   $Num    → f64.eq
  ;;   $Str    → str_op_eq
  ;;   $Set    → set:op_eq
  ;;
  ;; TODO: protocols.wat should be a pure dispatcher -- each arm should
  ;; call the type's own op_eq impl, which then decides how to compare
  ;; (it may call deep_eq, or compare fields directly). The $Set arm does
  ;; this correctly (-> set:op_eq). The $Dict / $List / $Range arms instead
  ;; call protocols-local _rec_eq / _list_eq / _range_eq kernels that hold
  ;; the ref.eq / mixed-type / deep_eq logic here -- that comparison logic
  ;; belongs in dict.wat / list.wat / range.wat as those types' op_eq
  ;; impls, with these arms reduced to a single dispatch call. Same applies
  ;; to op_neq below. (deep_eq's per-type arms are already correct: they
  ;; dispatch to each type's deep_eq impl.)
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
      (return_call $apply_1
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
      (return_call $apply_1
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
      (return_call $apply_1
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
      (return_call $apply_1
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
      (return_call $apply_1
        (local.get $ctx)
        (ref.i31 (call $clos_op_eq
          (ref.cast (ref $Closure) (local.get $a))
          (ref.cast (ref $Closure) (local.get $b))))
        (local.get $cont)))

    ;; Try $Opaque -- identity equality. A non-Opaque b is never equal
    ;; (mixed-type == is false, not a trap).
    (block $not_opaque
      (drop
        (block $is_opaque (result (ref $Opaque))
          (br $not_opaque
            (br_on_cast $is_opaque (ref null any) (ref $Opaque)
              (local.get $a)))))
      (return_call $apply_1
        (local.get $ctx)
        (ref.i31
          (if (result i32) (ref.test (ref $Opaque) (local.get $b))
            (then (call $opaque_op_eq
              (ref.cast (ref $Opaque) (local.get $a))
              (ref.cast (ref $Opaque) (local.get $b))))
            (else (i32.const 0))))
        (local.get $cont)))

    ;; Try $Dict — structural. ref.eq short-circuits identical recs; a
    ;; non-Rec b is never equal (mixed-type == is false, not a trap).
    (block $not_rec
      (drop
        (block $is_rec (result (ref $Dict))
          (br $not_rec
            (br_on_cast $is_rec (ref null any) (ref $Dict)
              (local.get $a)))))
      (return_call $apply_1
        (local.get $ctx)
        (ref.i31 (call $_rec_eq (local.get $a) (local.get $b)))
        (local.get $cont)))

    ;; Try $List — structural, positional. Same kernel shape as $Dict.
    (block $not_list
      (drop
        (block $is_list (result (ref $List))
          (br $not_list
            (br_on_cast $is_list (ref null any) (ref $List)
              (local.get $a)))))
      (return_call $apply_1
        (local.get $ctx)
        (ref.i31 (call $_list_eq (local.get $a) (local.get $b)))
        (local.get $cont)))

    ;; Try $Range — structural. Same kernel shape as $Dict / $List.
    (block $not_range
      (drop
        (block $is_range (result (ref $Range))
          (br $not_range
            (br_on_cast $is_range (ref null any) (ref $Range)
              (local.get $a)))))
      (return_call $apply_1
        (local.get $ctx)
        (ref.i31 (call $_range_eq (local.get $a) (local.get $b)))
        (local.get $cont)))

    ;; Try $Inst (typed instances $Rec/$Tuple) — nominal + structural eq,
    ;; delegated to types.wat's inst_eq.
    (block $not_inst
      (drop
        (block $is_inst (result (ref $Inst))
          (br $not_inst
            (br_on_cast $is_inst (ref null any) (ref $Inst)
              (local.get $a)))))
      (return_call $apply_1
        (local.get $ctx)
        (ref.i31
          (if (result i32) (ref.test (ref $Inst) (local.get $b))
            (then (call $inst_eq
              (ref.cast (ref $Inst) (local.get $a))
              (ref.cast (ref $Inst) (local.get $b))))
            (else (i32.const 0))))
        (local.get $cont)))

    ;; Try $Union — STRUCTURAL set equality (a union references types, it has
    ;; no identity; same member set = equal). MUST precede the $Type arm since
    ;; $Union <: $Type. Mixed-type b → false.
    (block $not_union
      (drop
        (block $is_union (result (ref $Union))
          (br $not_union
            (br_on_cast $is_union (ref null any) (ref $Union)
              (local.get $a)))))
      (return_call $apply_1
        (local.get $ctx)
        (ref.i31
          (if (result i32) (ref.test (ref $Union) (local.get $b))
            (then (call $union_eq
              (ref.cast (ref $Union) (local.get $a))
              (ref.cast (ref $Union) (local.get $b))))
            (else (i32.const 0))))
        (local.get $cont)))

    ;; Try $Type (and its non-union subtypes $Enum/$RecType/...) — IDENTITY
    ;; equality. type/enum OWN their identity (each declaration is distinct);
    ;; two refs to the same type are equal, distinct types differ. Mixed → false.
    (block $not_type
      (drop
        (block $is_type (result (ref $Type))
          (br $not_type
            (br_on_cast $is_type (ref null any) (ref $Type)
              (local.get $a)))))
      (return_call $apply_1
        (local.get $ctx)
        (ref.i31
          (if (result i32) (ref.test (ref $Type) (local.get $b))
            (then (ref.eq
              (ref.cast (ref eq) (local.get $a))
              (ref.cast (ref eq) (local.get $b))))
            (else (i32.const 0))))
        (local.get $cont)))

    (unreachable))

  ;; Record equality kernel shared by op_eq / op_neq.
  ;;   ref.eq    → identical allocation, equal
  ;;   b non-Rec → not equal (mixed-type)
  ;;   else      → structural rec_deep_eq
  ;; (A hash-inequality fast-reject slots between ref.eq and rec_deep_eq
  ;; once records carry a content hash; today the hash is stubbed to 0.)
  (func $_rec_eq (param $a (ref null any)) (param $b (ref null any)) (result i32)
    (if (ref.eq
          (ref.cast (ref eq) (local.get $a))
          (ref.cast (ref eq) (local.get $b)))
      (then (return (i32.const 1))))
    (if (i32.eqz (ref.test (ref $Dict) (local.get $b)))
      (then (return (i32.const 0))))
    (call $rec_deep_eq
      (ref.cast (ref $Dict) (local.get $a))
      (ref.cast (ref $Dict) (local.get $b))))

  ;; List equality kernel shared by op_eq / op_neq.
  ;;   ref.eq     → identical allocation, equal
  ;;   b non-List → not equal (mixed-type)
  ;;   else       → structural list_deep_eq
  (func $_list_eq (param $a (ref null any)) (param $b (ref null any)) (result i32)
    (if (ref.eq
          (ref.cast (ref eq) (local.get $a))
          (ref.cast (ref eq) (local.get $b)))
      (then (return (i32.const 1))))
    (if (i32.eqz (ref.test (ref $List) (local.get $b)))
      (then (return (i32.const 0))))
    (call $list_deep_eq
      (ref.cast (ref $List) (local.get $a))
      (ref.cast (ref $List) (local.get $b))))

  ;; Range equality kernel shared by op_eq / op_neq.
  ;;   ref.eq      → identical allocation, equal
  ;;   b non-Range → not equal (mixed-type)
  ;;   else        → structural range_deep_eq
  (func $_range_eq (param $a (ref null any)) (param $b (ref null any)) (result i32)
    (if (ref.eq
          (ref.cast (ref eq) (local.get $a))
          (ref.cast (ref eq) (local.get $b)))
      (then (return (i32.const 1))))
    (if (i32.eqz (ref.test (ref $Range) (local.get $b)))
      (then (return (i32.const 0))))
    (call $range_deep_eq
      (ref.cast (ref $Range) (local.get $a))
      (ref.cast (ref $Range) (local.get $b))))

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
      (return_call $apply_1
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
      (return_call $apply_1
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
      (return_call $apply_1
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
      (return_call $apply_1
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
      (return_call $apply_1
        (local.get $ctx)
        (ref.i31 (call $clos_op_neq
          (ref.cast (ref $Closure) (local.get $a))
          (ref.cast (ref $Closure) (local.get $b))))
        (local.get $cont)))

    ;; Try $Opaque -- identity inequality. A non-Opaque b is never equal,
    ;; so != is true.
    (block $not_opaque
      (drop
        (block $is_opaque (result (ref $Opaque))
          (br $not_opaque
            (br_on_cast $is_opaque (ref null any) (ref $Opaque)
              (local.get $a)))))
      (return_call $apply_1
        (local.get $ctx)
        (ref.i31
          (if (result i32) (ref.test (ref $Opaque) (local.get $b))
            (then (call $opaque_op_neq
              (ref.cast (ref $Opaque) (local.get $a))
              (ref.cast (ref $Opaque) (local.get $b))))
            (else (i32.const 1))))
        (local.get $cont)))

    ;; Try $Dict — negation of the structural equality kernel.
    (block $not_rec
      (drop
        (block $is_rec (result (ref $Dict))
          (br $not_rec
            (br_on_cast $is_rec (ref null any) (ref $Dict)
              (local.get $a)))))
      (return_call $apply_1
        (local.get $ctx)
        (ref.i31 (i32.eqz (call $_rec_eq (local.get $a) (local.get $b))))
        (local.get $cont)))

    ;; Try $List — negation of the structural equality kernel.
    (block $not_list
      (drop
        (block $is_list (result (ref $List))
          (br $not_list
            (br_on_cast $is_list (ref null any) (ref $List)
              (local.get $a)))))
      (return_call $apply_1
        (local.get $ctx)
        (ref.i31 (i32.eqz (call $_list_eq (local.get $a) (local.get $b))))
        (local.get $cont)))

    ;; Try $Range — negation of the structural equality kernel.
    (block $not_range
      (drop
        (block $is_range (result (ref $Range))
          (br $not_range
            (br_on_cast $is_range (ref null any) (ref $Range)
              (local.get $a)))))
      (return_call $apply_1
        (local.get $ctx)
        (ref.i31 (i32.eqz (call $_range_eq (local.get $a) (local.get $b))))
        (local.get $cont)))

    ;; Try $Inst — negation of nominal+structural instance eq.
    (block $not_inst
      (drop
        (block $is_inst (result (ref $Inst))
          (br $not_inst
            (br_on_cast $is_inst (ref null any) (ref $Inst)
              (local.get $a)))))
      (return_call $apply_1
        (local.get $ctx)
        (ref.i31
          (i32.eqz
            (if (result i32) (ref.test (ref $Inst) (local.get $b))
              (then (call $inst_eq
                (ref.cast (ref $Inst) (local.get $a))
                (ref.cast (ref $Inst) (local.get $b))))
              (else (i32.const 0)))))
        (local.get $cont)))

    ;; Try $Union — negation of structural set equality. MUST precede $Type.
    (block $not_union
      (drop
        (block $is_union (result (ref $Union))
          (br $not_union
            (br_on_cast $is_union (ref null any) (ref $Union)
              (local.get $a)))))
      (return_call $apply_1
        (local.get $ctx)
        (ref.i31
          (i32.eqz
            (if (result i32) (ref.test (ref $Union) (local.get $b))
              (then (call $union_eq
                (ref.cast (ref $Union) (local.get $a))
                (ref.cast (ref $Union) (local.get $b))))
              (else (i32.const 0)))))
        (local.get $cont)))

    ;; Try $Type — negation of identity equality (see op_eq's $Type arm).
    (block $not_type
      (drop
        (block $is_type (result (ref $Type))
          (br $not_type
            (br_on_cast $is_type (ref null any) (ref $Type)
              (local.get $a)))))
      (return_call $apply_1
        (local.get $ctx)
        (ref.i31
          (i32.eqz
            (if (result i32) (ref.test (ref $Type) (local.get $b))
              (then (ref.eq
                (ref.cast (ref eq) (local.get $a))
                (ref.cast (ref eq) (local.get $b))))
              (else (i32.const 0)))))
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
      (return_call $apply_1
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
      (return_call $apply_1
      (local.get $ctx)
        (ref.i31 (call $set_op_lt
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b))))
        (local.get $cont)))

    (return_call $apply_1
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
      (return_call $apply_1
      (local.get $ctx)
        (ref.i31 (call $set_op_lte
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b))))
        (local.get $cont)))

    (return_call $apply_1
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
      (return_call $apply_1
      (local.get $ctx)
        (ref.i31 (call $set_op_gt
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b))))
        (local.get $cont)))

    (return_call $apply_1
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
      (return_call $apply_1
      (local.get $ctx)
        (ref.i31 (call $set_op_gte
          (local.get $a_set)
          (ref.cast (ref $Set) (local.get $b))))
        (local.get $cont)))

    (return_call $apply_1
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
      (return_call $apply_1
        (local.get $ctx)
        (call $num_op_not (local.get $a_num))
        (local.get $cont)))

    ;; Fallback: i31ref boolean not
    (return_call $apply_1
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
      (return_call $apply_1
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
      (return_call $apply_1
      (local.get $ctx)
        (call $num_op_and
          (local.get $a_num)
          (ref.cast (ref $Num) (local.get $b)))
        (local.get $cont)))

    ;; Fallback: i31ref boolean and
    (return_call $apply_1
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
      (return_call $apply_1
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
      (return_call $apply_1
      (local.get $ctx)
        (call $num_op_or
          (local.get $a_num)
          (ref.cast (ref $Num) (local.get $b)))
        (local.get $cont)))

    ;; Fallback: i31ref boolean or
    (return_call $apply_1
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
      (return_call $apply_1
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
      (return_call $apply_1
      (local.get $ctx)
        (call $num_op_xor
          (local.get $a_num)
          (ref.cast (ref $Num) (local.get $b)))
        (local.get $cont)))

    ;; Fallback: i31ref boolean xor
    (return_call $apply_1
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

    ;; A $Tuple instance IS seq-like -- unwrap to its bare $List payload and
    ;; succeed with THAT (destructured value is bare; reads strip the type).
    (if (ref.test (ref $Tuple_inst) (local.get $val))
      (then (return_call $apply_1
        (local.get $ctx) (call $inst_payload (local.get $val)) (local.get $succ))))

    ;; $List
    (block $not_list
      (block $is_list (result (ref $List))
        (br $not_list
          (br_on_cast $is_list (ref null any) (ref $List)
            (local.get $val))))
      (drop)
      (return_call $apply_1
      (local.get $ctx) (local.get $val) (local.get $succ)))

    ;; $Set
    (block $not_set
      (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $val))))
      (drop)
      (return_call $apply_1
      (local.get $ctx) (local.get $val) (local.get $succ)))

    (return_call $apply_0
      (local.get $ctx) (local.get $fail)))

  ;; is_rec_like(val, succ, fail): succ(payload) if $Dict or a $Rec instance,
  ;; else fail(). A $Rec instance IS rec-like -- unwrap to its bare $Dict
  ;; payload and succeed with THAT (the destructured value is bare; reads strip
  ;; the type). Plain $Dict succeeds with itself.
  (func $is_rec_like (@pub) (@impl "std/operators.fnk:is_rec_like")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $val (ref null any)) (param $succ (ref null any)) (param $fail (ref null any))
    (if (ref.test (ref $Rec_inst) (local.get $val))
      (then (return_call $apply_1
        (local.get $ctx) (call $inst_payload (local.get $val)) (local.get $succ))))
    (block $not_rec
      (block $is_rec (result (ref $Dict))
        (br $not_rec
          (br_on_cast $is_rec (ref null any) (ref $Dict)
            (local.get $val))))
      (drop)
      (return_call $apply_1
      (local.get $ctx) (local.get $val) (local.get $succ)))
    (return_call $apply_0
      (local.get $ctx) (local.get $fail)))

  ;; guard_apply(ctx, guard, val, succ, fail): unified pattern guard.
  ;; The `guard` is a runtime value in pattern-guard position:
  ;;   - a $Type    -> instance test: succ(val) if val is-instance-of guard,
  ;;                   else fail(). (Bare structural `{...}`/`[...]` patterns
  ;;                   are lowered to is_rec_like/is_seq_like, not here.)
  ;;   - a $Closure -> a fink predicate fn: call guard(val) with a branch cont;
  ;;                   the predicate reports its verdict by calling the cont
  ;;                   with a bool, which then resumes succ(val) or fail().
  (func $guard_apply (@pub) (@impl "std/operators.fnk:guard_apply")
    (param $ctx (ref null any))
    (param $guard (ref null any)) (param $val (ref null any))
    (param $succ (ref null any)) (param $fail (ref null any))
    ;; Type guard: instance-of test, branch directly.
    (if (ref.test (ref $Type) (local.get $guard))
      (then
        (if (call $is_instance (local.get $val) (local.get $guard))
          (then (return_call $apply_1
            (local.get $ctx) (local.get $val) (local.get $succ))))
        (return_call $apply_0 (local.get $ctx) (local.get $fail))))
    ;; Predicate guard: apply guard(val) with a branch cont (conts-first).
    (return_call $apply_2_vals
      (local.get $ctx)
      (call $make_guard_branch
        (local.get $ctx)
        (ref.as_non_null (local.get $succ)) (ref.as_non_null (local.get $fail))
        (local.get $val))
      (local.get $val)
      (local.get $guard)))

  ;; =========================================================================
  ;; Collection predicates (polymorphic — dispatch on type tag)
  ;; =========================================================================

  ;; Polymorphic empty: dispatch on value type to module predicates.
  ;;   null     → true (always empty)
  ;;   $List    → list_op_empty
  ;;   $Dict     → rec_op_empty
  (func $op_empty (@pub) (@impl "std/operators.fnk:op_empty")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $val (ref null any)) (param $cont (ref null any))
    (local $a_set (ref $Set))

    ;; null = empty
    (if (ref.is_null (local.get $val))
      (then
        (return_call $apply_1
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
      (return_call $apply_1
      (local.get $ctx)
        (ref.i31 (call $list_op_empty (local.get $val)))
        (local.get $cont)))

    ;; $Dict → rec_op_empty
    (block $not_rec
      (block $is_rec (result (ref $Dict))
        (br $not_rec
          (br_on_cast $is_rec (ref null any) (ref $Dict)
            (local.get $val))))
      (drop)
      (return_call $apply_1
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
      (return_call $apply_1
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
      (param $ctx (ref null any))
    (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))

    ;; $Set → set:seq_pop
    (block $not_set
      (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref null any) (ref $Set)
            (local.get $cursor))))
      (drop)
      (return_call $set_seq_pop
        (local.get $ctx)
        (local.get $cursor) (local.get $fail) (local.get $succ)))

    ;; Default: list (or $Nil)
    (return_call $list_seq_pop
      (local.get $ctx)
      (local.get $cursor) (local.get $fail) (local.get $succ)))

  ;; seq_prepend(val, seq, cont): cons-style prepend for any seq-like
  ;; container. Today $List only — sets and other seq types could
  ;; gain a typed impl in future.
  (func $seq_prepend (@pub) (@impl "std/seq.fnk:prepend")
      (param $ctx (ref null any))
    (param $val (ref null any)) (param $seq (ref null any)) (param $cont (ref null any))
    (return_call $list_seq_prepend
      (local.get $ctx)
      (local.get $val) (local.get $seq) (local.get $cont)))

  ;; seq_concat(a, b, cont): concatenate two seqs. Today $List only;
  ;; other seq types could gain a typed impl in future. Used for list
  ;; literals containing a spread (`[..xs, y]`, `[..a, ..b]`).
  (func $seq_concat (@pub) (@impl "std/seq.fnk:concat")
      (param $ctx (ref null any))
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $list_seq_concat
      (local.get $ctx)
      (local.get $a) (local.get $b) (local.get $cont)))

  ;; seq_pop_back(cursor, fail, succ): peel one element off the END of a
  ;; seq-like container. Currently only $List is supported (sets have no
  ;; defined ordering, so "last" isn't meaningful).
  ;; If empty: tail-call fail() with no args.
  ;; Else: tail-call succ(init, last) with two args.
  (func $seq_pop_back (@pub) (@impl "std/seq.fnk:pop_back")
      (param $ctx (ref null any))
    (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))

    (return_call $list_seq_pop_back
      (local.get $ctx)
      (local.get $cursor) (local.get $fail) (local.get $succ)))

  ;; =========================================================================
  ;; Membership: `in` / `not in` — dispatch on container type
  ;; =========================================================================

  ;; op_in(val, container, cont) → bool
  (func $op_in (@pub) (@impl "std/operators.fnk:op_in")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (local $range (ref $Range))
    (local $rec (ref $Dict))
    (local $set (ref $Set))

    ;; Try $Range
    (block $not_range
      (block $is_range (result (ref $Range))
        (br $not_range
          (br_on_cast $is_range (ref null any) (ref $Range)
            (local.get $b))))
      (local.set $range)
      (return_call $apply_1
      (local.get $ctx)
        (ref.i31 (call $range_op_in
          (ref.cast (ref $I64) (local.get $a))
          (local.get $range)))
        (local.get $cont)))

    ;; Try $Dict
    (block $not_rec
      (block $is_rec (result (ref $Dict))
        (br $not_rec
          (br_on_cast $is_rec (ref null any) (ref $Dict)
            (local.get $b))))
      (local.set $rec)
      (return_call $apply_1
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
      (return_call $apply_1
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
    (local $rec (ref $Dict))
    (local $set (ref $Set))

    ;; Try $Range
    (block $not_range
      (block $is_range (result (ref $Range))
        (br $not_range
          (br_on_cast $is_range (ref null any) (ref $Range)
            (local.get $b))))
      (local.set $range)
      (return_call $apply_1
      (local.get $ctx)
        (ref.i31 (call $range_op_not_in
          (ref.cast (ref $I64) (local.get $a))
          (local.get $range)))
        (local.get $cont)))

    ;; Try $Dict
    (block $not_rec
      (block $is_rec (result (ref $Dict))
        (br $not_rec
          (br_on_cast $is_rec (ref null any) (ref $Dict)
            (local.get $b))))
      (local.set $rec)
      (return_call $apply_1
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
      (return_call $apply_1
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
  ;;   $Dict  → rec_op_dot
  ;;   $List → list_op_dot
  (func $op_dot (@pub) (@impl "std/operators.fnk:op_dot")
      (param $ctx (ref null any))
    (param $container (ref null any)) (param $key (ref null any)) (param $cont (ref null any))

    ;; Try $Inst (typed instance) — unwrap to the bare payload and re-dispatch.
    ;; Reads strip the type; the bare-collection arm below does the work.
    (if (ref.test (ref $Inst) (local.get $container))
      (then
        (return_call $op_dot
          (local.get $ctx)
          (call $inst_payload (local.get $container))
          (local.get $key)
          (local.get $cont))))

    ;; Try $Str
    (block $not_str
      (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref null any) (ref $Str)
            (local.get $container))))
      (drop)
      (return_call $str_op_dot
        (local.get $ctx)
        (local.get $container)
        (local.get $key)
        (local.get $cont)))

    ;; Try $Dict
    (block $not_rec
      (block $is_rec (result (ref $Dict))
        (br $not_rec
          (br_on_cast $is_rec (ref null any) (ref $Dict)
            (local.get $container))))
      (drop)
      (return_call $dict_op_dot
        (local.get $ctx)
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
        (local.get $ctx)
        (local.get $container)
        (local.get $key)
        (local.get $cont)))

    (unreachable))

  ;; =========================================================================
  ;; Shift left: `<<` — numeric bitwise shift
  ;; =========================================================================

  (func $op_shl (@pub) (@impl "std/operators.fnk:op_shl")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))

    (return_call $apply_1
      (local.get $ctx)
      (call $num_op_shl
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  ;; =========================================================================
  ;; Shift right: `>>` — numeric bitwise shift
  ;; =========================================================================

  (func $op_shr (@pub) (@impl "std/operators.fnk:op_shr")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))

    ;; Fallback: numeric shift right
    (return_call $apply_1
      (local.get $ctx)
      (call $num_op_shr
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))


)
