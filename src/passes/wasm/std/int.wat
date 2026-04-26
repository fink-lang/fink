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

  (func $std/int.wat:op_and (export "std/int.wat:op_and")
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.and
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  (func $std/int.wat:op_or (export "std/int.wat:op_or")
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.or
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  (func $std/int.wat:op_xor (export "std/int.wat:op_xor")
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.xor
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  (func $std/int.wat:op_not (export "std/int.wat:op_not")
    (param $a (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.xor
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.const -1)))))

  ;; =========================================================================
  ;; Integer arithmetic: direct-style helpers called from operators.wat
  ;; =========================================================================

  (func $std/int.wat:op_div (export "std/int.wat:op_div")
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.div_s
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  (func $std/int.wat:op_rem (export "std/int.wat:op_rem")
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.rem_s
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  (func $std/int.wat:op_mod (export "std/int.wat:op_mod")
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.rem_s
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  ;; =========================================================================
  ;; Shifts: unbox $Num → i64, shift, i64 → $Num → apply_1(result, cont)
  ;; =========================================================================
  (func $std/int.wat:op_shl (export "std/int.wat:op_shl")
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.shl
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  (func $std/int.wat:op_shr (export "std/int.wat:op_shr")
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.shr_s
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  ;; =========================================================================
  ;; Rotations: unbox $Num → i64, rotate, i64 → $Num → apply_1(result, cont)
  ;; =========================================================================
  ;; TODO: should become int_op_rotl and operators.wat have op_rotl to dispatch
  (func $std/int.wat:op_rotl (export "std/int.wat:op_rotl")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $std/list.wat:apply_1
      (struct.new $Num (f64.convert_i64_s (i64.rotl
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $a))))
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))))
      (local.get $cont)))

  ;; TODO: should become int_op_rotr and operators.wat have op_rotr to dispatch
  (func $std/int.wat:op_rotr (export "std/int.wat:op_rotr")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $std/list.wat:apply_1
      (struct.new $Num (f64.convert_i64_s (i64.rotr
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $a))))
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))))
      (local.get $cont)))
)
