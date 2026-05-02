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
  ;; Rotations: direct-style helpers (called from polymorphic operators.wat)
  ;; =========================================================================
  (func $std/int.wat:op_rotl (export "std/int.wat:op_rotl")
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.rotl
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  (func $std/int.wat:op_rotr (export "std/int.wat:op_rotr")
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.rotr
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  ;; =========================================================================
  ;; Power: integer exponentiation by square-and-multiply.
  ;; Negative exponents return 0 (pow(a, n<0) = 1/a^|n|, integer-truncated).
  ;; =========================================================================
  (func $std/int.wat:op_pow (export "std/int.wat:op_pow")
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (local $base i64)
    (local $exp i64)
    (local $acc i64)

    (local.set $base (i64.trunc_f64_s (struct.get $Num $val (local.get $a))))
    (local.set $exp  (i64.trunc_f64_s (struct.get $Num $val (local.get $b))))

    ;; Negative exponent → 0 (integer truncation of fractional result).
    (if (i64.lt_s (local.get $exp) (i64.const 0))
      (then (return (struct.new $Num (f64.const 0)))))

    (local.set $acc (i64.const 1))
    (block $done
      (loop $loop
        (br_if $done (i64.eqz (local.get $exp)))
        ;; if (exp & 1) acc *= base
        (if (i32.wrap_i64 (i64.and (local.get $exp) (i64.const 1)))
          (then (local.set $acc (i64.mul (local.get $acc) (local.get $base)))))
        (local.set $base (i64.mul (local.get $base) (local.get $base)))
        (local.set $exp  (i64.shr_u (local.get $exp) (i64.const 1)))
        (br $loop)))

    (struct.new $Num (f64.convert_i64_s (local.get $acc))))

  ;; =========================================================================
  ;; DivMod: returns [quotient, remainder] as a 2-element list.
  ;; =========================================================================
  (func $std/int.wat:op_divmod (export "std/int.wat:op_divmod")
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $List))
    (local $a_i i64)
    (local $b_i i64)
    (local $q (ref $Num))
    (local $r (ref $Num))

    (local.set $a_i (i64.trunc_f64_s (struct.get $Num $val (local.get $a))))
    (local.set $b_i (i64.trunc_f64_s (struct.get $Num $val (local.get $b))))
    (local.set $q (struct.new $Num (f64.convert_i64_s
      (i64.div_s (local.get $a_i) (local.get $b_i)))))
    (local.set $r (struct.new $Num (f64.convert_i64_s
      (i64.rem_s (local.get $a_i) (local.get $b_i)))))

    ;; Build [q, r] = Cons(q, Cons(r, Nil))
    (struct.new $Cons
      (local.get $q)
      (struct.new $Cons
        (local.get $r)
        (call $std/list.wat:nil))))
)
