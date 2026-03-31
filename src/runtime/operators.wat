;; Operator implementations — CPS functions for arithmetic, comparison, and logic.
;;
;; Each operator follows the CPS calling convention:
;;   (func $op_plus (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
;;     ;; unbox args, compute, box result, tail-call _croc_1(result, cont)
;;   )
;;
;; Type conventions:
;;   - Numbers: $Num struct (f64 field)
;;   - Booleans: i31ref (0 = false, 1 = true)
;;   - Continuation dispatch via _croc_1 (imported from dispatch module)
;;
;; These are the phase-0 implementations operating on concrete types.
;; Protocol-based overloading (future) will replace these with dispatch
;; through user-defined protocol implementations.

(module

  ;; _croc_1 is provided by the compiler's emitted module (user code fragment).
  (import "@fink/user" "_croc_1" (func $croc_1 (param (ref null any)) (param (ref null any))))

  ;; =========================================================================
  ;; Arithmetic: unbox two $Num, f64 op, box result → _croc_1(result, cont)
  ;; =========================================================================

  (func $op_plus (export "op_plus")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (struct.new $Num (f64.add
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $op_minus (export "op_minus")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (struct.new $Num (f64.sub
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $op_mul (export "op_mul")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (struct.new $Num (f64.mul
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $op_div (export "op_div")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (struct.new $Num (f64.div
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  ;; =========================================================================
  ;; Integer arithmetic: unbox $Num → f64 → i64, op, i64 → f64 → box
  ;; =========================================================================

  (func $op_intdiv (export "op_intdiv")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (struct.new $Num (f64.convert_i64_s (i64.div_s
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $a))))
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))))
      (local.get $cont)))

  (func $op_rem (export "op_rem")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (struct.new $Num (f64.convert_i64_s (i64.rem_s
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $a))))
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))))
      (local.get $cont)))

  (func $op_intmod (export "op_intmod")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (struct.new $Num (f64.convert_i64_s (i64.rem_s
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $a))))
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))))
      (local.get $cont)))

  ;; =========================================================================
  ;; Comparison: unbox two $Num, f64 compare → i31ref (0/1)
  ;; =========================================================================

  ;; Direct-style deep equality. Used by HAMT for key comparison.
  ;;   i31ref  → ref.eq (identity — fine for small ints and booleans)
  ;;   $Num    → f64.eq
  ;;   $StrVal → str_eq
  (func $deep_eq (export "deep_eq")
    (param $a (ref eq)) (param $b (ref eq)) (result i32)

    ;; Try $Num
    (block $not_num
      (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref eq) (ref $Num)
            (local.get $a))))
      (return (f64.eq
        (struct.get $Num $val)
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b))))))

    ;; Try $StrVal
    (block $not_str
      (block $is_str (result (ref $StrVal))
        (br $not_str
          (br_on_cast $is_str (ref eq) (ref $StrVal)
            (local.get $a))))
      (return (call $str_eq
        (ref.cast (ref $StrVal) (local.get $b)))))

    ;; Fallback: ref.eq (i31ref, other GC types)
    (ref.eq (local.get $a) (local.get $b)))

  ;; Polymorphic ==: dispatch on $a's type.
  ;;   $Num    → f64.eq
  ;;   $StrVal → str_eq
  (func $op_eq (export "op_eq")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))

    ;; Try $Num
    (block $not_num
      (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref null any) (ref $Num)
            (local.get $a))))
      ;; $a is $Num — cast $b and compare
      (return_call $croc_1
        (ref.i31 (f64.eq
          (struct.get $Num $val)
          (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
        (local.get $cont)))

    ;; Try $StrVal
    (block $not_str
      (block $is_str (result (ref $StrVal))
        (br $not_str
          (br_on_cast $is_str (ref null any) (ref $StrVal)
            (local.get $a))))
      ;; $a is $StrVal — cast $b and call str_eq
      (return_call $croc_1
        (ref.i31 (call $str_eq
          (ref.cast (ref $StrVal) (local.get $b))))
        (local.get $cont)))

    (unreachable))

  ;; Polymorphic !=: dispatch on $a's type.
  ;;   $Num    → f64.ne
  ;;   $StrVal → !str_eq
  (func $op_neq (export "op_neq")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))

    ;; Try $Num
    (block $not_num
      (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref null any) (ref $Num)
            (local.get $a))))
      ;; $a is $Num — cast $b and compare
      (return_call $croc_1
        (ref.i31 (f64.ne
          (struct.get $Num $val)
          (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
        (local.get $cont)))

    ;; Try $StrVal
    (block $not_str
      (block $is_str (result (ref $StrVal))
        (br $not_str
          (br_on_cast $is_str (ref null any) (ref $StrVal)
            (local.get $a))))
      ;; $a is $StrVal — cast $b, call str_eq, invert
      (return_call $croc_1
        (ref.i31 (i32.eqz (call $str_eq
          (ref.cast (ref $StrVal) (local.get $b)))))
        (local.get $cont)))

    (unreachable))

  (func $op_lt (export "op_lt")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (f64.lt
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $op_lte (export "op_lte")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (f64.le
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $op_gt (export "op_gt")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (f64.gt
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $op_gte (export "op_gte")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (f64.ge
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  ;; =========================================================================
  ;; Logic: i31ref bool ops
  ;; =========================================================================

  (func $op_not (export "op_not")
    (param $a (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (i32.eqz (i31.get_s (ref.cast (ref i31) (local.get $a)))))
      (local.get $cont)))

  (func $op_and (export "op_and")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (i32.and
        (i31.get_s (ref.cast (ref i31) (local.get $a)))
        (i31.get_s (ref.cast (ref i31) (local.get $b)))))
      (local.get $cont)))

  (func $op_or (export "op_or")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (i32.or
        (i31.get_s (ref.cast (ref i31) (local.get $a)))
        (i31.get_s (ref.cast (ref i31) (local.get $b)))))
      (local.get $cont)))

  (func $op_xor (export "op_xor")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (i32.xor
        (i31.get_s (ref.cast (ref i31) (local.get $a)))
        (i31.get_s (ref.cast (ref i31) (local.get $b)))))
      (local.get $cont)))

  ;; =========================================================================
  ;; Collection predicates (polymorphic — dispatch on type tag)
  ;; =========================================================================

  ;; Polymorphic empty: dispatch on value type to module predicates.
  ;;   null     → true (always empty)
  ;;   $List    → list_is_empty
  ;;   $Rec     → rec_is_empty
  (func $op_empty (export "empty")
    (param $val (ref null any)) (param $cont (ref null any))

    ;; null = empty
    (if (ref.is_null (local.get $val))
      (then
        (return_call $croc_1
          (ref.i31 (i32.const 1))
          (local.get $cont))))

    ;; $List → list_is_empty
    (block $not_list
      (block $is_list (result (ref $List))
        (br $not_list
          (br_on_cast $is_list (ref null any) (ref $List)
            (local.get $val))))
      (drop)
      (return_call $croc_1
        (ref.i31 (call $list_is_empty (local.get $val)))
        (local.get $cont)))

    ;; $Rec → rec_is_empty
    (block $not_rec
      (block $is_rec (result (ref $Rec))
        (br $not_rec
          (br_on_cast $is_rec (ref null any) (ref $Rec)
            (local.get $val))))
      (drop)
      (return_call $croc_1
        (ref.i31 (call $rec_is_empty (local.get $val)))
        (local.get $cont)))

    (unreachable))

)
