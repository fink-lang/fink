;; Integer bitwise and shift operations — CPS functions.
;;
;; Each operator unboxes $Num (f64) to i64 via i64.trunc_f64_s,
;; applies the integer operation, converts back via f64.convert_i64_s,
;; and boxes the result as $Num.
;;
;; These are the phase-0 implementations operating on concrete $Num types.
;; Protocol-based overloading (future) will replace these with dispatch
;; through user-defined protocol implementations.

(module

  ;; =========================================================================
  ;; Bitwise: unbox $Num → i64, bitwise op, i64 → $Num → apply_1(result, cont)
  ;; =========================================================================

  (func $op_bitand (export "op_bitand")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (struct.new $Num (f64.convert_i64_s (i64.and
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $a))))
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))))
      (local.get $cont)))

  (func $op_bitxor (export "op_bitxor")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (struct.new $Num (f64.convert_i64_s (i64.xor
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $a))))
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))))
      (local.get $cont)))

  (func $op_bitnot (export "op_bitnot")
    (param $a (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (struct.new $Num (f64.convert_i64_s (i64.xor
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $a))))
        (i64.const -1))))
      (local.get $cont)))

  ;; =========================================================================
  ;; Shifts: unbox $Num → i64, shift, i64 → $Num → apply_1(result, cont)
  ;; =========================================================================

  (func $op_shl (export "op_shl")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (struct.new $Num (f64.convert_i64_s (i64.shl
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $a))))
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))))
      (local.get $cont)))

  (func $op_shr (export "op_shr")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (struct.new $Num (f64.convert_i64_s (i64.shr_s
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $a))))
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))))
      (local.get $cont)))

  ;; =========================================================================
  ;; Rotations: unbox $Num → i64, rotate, i64 → $Num → apply_1(result, cont)
  ;; =========================================================================

  (func $op_rotl (export "op_rotl")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (struct.new $Num (f64.convert_i64_s (i64.rotl
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $a))))
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))))
      (local.get $cont)))

  (func $op_rotr (export "op_rotr")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (struct.new $Num (f64.convert_i64_s (i64.rotr
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $a))))
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))))
      (local.get $cont)))
)
