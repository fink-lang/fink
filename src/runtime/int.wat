;; Integer operations — bitwise, shift, and rotation.
;;
;; Direct-style helpers operate on already-cast (ref $Num) values and return
;; (ref $Num). Called from polymorphic CPS operators in operators.wat.
;;
;; CPS shift/rotation operators are self-contained (no polymorphic dispatch).

(module

  ;; =========================================================================
  ;; Bitwise: direct-style helpers called from polymorphic operators.wat
  ;; =========================================================================

  (func $int_op_and (export "int_op_and")
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.and
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  (func $int_op_or (export "int_op_or")
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.or
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  (func $int_op_xor (export "int_op_xor")
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.xor
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  (func $int_op_not (export "int_op_not")
    (param $a (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.xor
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.const -1)))))

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
