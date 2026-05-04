;; Integer operations — arithmetic, comparison, bitwise, shift, rotation.
;;
;; Shape today: ops take/return (ref $Int) (or (ref $Num) for legacy bitwise
;; helpers). Field is still f64-shared with $Num — narrowing to i64 is a
;; follow-up step. num.wat dispatches polymorphic op_* to these for the
;; $Int arm; the f64-trunc dance below is a stepping stone.
;;
;; Power and DivMod have richer behaviours (negative exponent fast-path,
;; tuple return).

(module

  ;; Type imports
  (import "std/num.wat"  "Num"  (type $Num  (sub any) (struct (field $val f64))))
  (import "std/list.wat" "List" (type $List (sub any)))

  ;; $Int — abstract integer parent. Concrete subtypes ($I64 / $U64)
  ;; live below. Sharing $Num's `f64 $val` slot for now; will narrow to
  ;; i64 in a later step.
  (type $Int (@pub) (sub $Num (struct (field $val f64))))
    (type $I64 (@pub) (sub final $Int (struct (field $val f64))))
    (type $U64 (@pub) (sub final $Int (struct (field $val f64))))

  ;; =========================================================================
  ;; Arithmetic on $Int — result widens to $I64 when sign info is lost.
  ;; (Sub-dispatch by I64/U64 is a future refinement.)
  ;; =========================================================================

  (func $op_plus (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (struct.new $I64 (f64.add
      (struct.get $Int $val (local.get $a))
      (struct.get $Int $val (local.get $b)))))

  (func $op_minus (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (struct.new $I64 (f64.sub
      (struct.get $Int $val (local.get $a))
      (struct.get $Int $val (local.get $b)))))

  (func $op_mul (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (struct.new $I64 (f64.mul
      (struct.get $Int $val (local.get $a))
      (struct.get $Int $val (local.get $b)))))

  (func $op_div (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (struct.new $I64 (f64.div
      (struct.get $Int $val (local.get $a))
      (struct.get $Int $val (local.get $b)))))

  ;; -- Comparison — return raw i32 -----------------------------------

  (func $op_eq (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (f64.eq
      (struct.get $Int $val (local.get $a))
      (struct.get $Int $val (local.get $b))))

  (func $op_neq (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (f64.ne
      (struct.get $Int $val (local.get $a))
      (struct.get $Int $val (local.get $b))))

  (func $op_lt (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (f64.lt
      (struct.get $Int $val (local.get $a))
      (struct.get $Int $val (local.get $b))))

  (func $op_lte (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (f64.le
      (struct.get $Int $val (local.get $a))
      (struct.get $Int $val (local.get $b))))

  (func $op_gt (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (f64.gt
      (struct.get $Int $val (local.get $a))
      (struct.get $Int $val (local.get $b))))

  (func $op_gte (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (f64.ge
      (struct.get $Int $val (local.get $a))
      (struct.get $Int $val (local.get $b))))

  ;; Func imports — list constructors via the public API.
  (import "std/list.wat" "empty"
    (func $list_empty (result (ref $List))))
  (import "std/list.wat" "prepend"
    (func $list_prepend (param $head (ref any)) (param $tail (ref $List)) (result (ref $List))))

  ;; =========================================================================
  ;; Bitwise: direct-style helpers — protocol impls under op_*  $Num $Num.
  ;; =========================================================================

  (func $op_and (@impl "std/operators.fnk:op_and" $Num $Num)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.and
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  (func $op_or (@impl "std/operators.fnk:op_or" $Num $Num)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.or
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  (func $op_xor (@impl "std/operators.fnk:op_xor" $Num $Num)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.xor
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  (func $op_not (@impl "std/operators.fnk:op_not" $Num)
    (param $a (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.xor
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.const -1)))))

  ;; =========================================================================
  ;; Integer arithmetic — protocol impls.
  ;; =========================================================================

  (func $op_intdiv (@impl "std/operators.fnk:op_intdiv" $Num $Num)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.div_s
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  (func $op_rem (@impl "std/operators.fnk:op_rem" $Num $Num)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.rem_s
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  (func $op_intmod (@impl "std/operators.fnk:op_intmod" $Num $Num)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.rem_s
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  ;; =========================================================================
  ;; Shifts — protocol impls.
  ;; =========================================================================

  (func $op_shl (@impl "std/operators.fnk:op_shl" $Num $Num)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.shl
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  (func $op_shr (@impl "std/operators.fnk:op_shr" $Num $Num)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.shr_s
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  ;; =========================================================================
  ;; Rotations — protocol impls.
  ;; =========================================================================

  (func $op_rotl (@impl "std/operators.fnk:op_rotl" $Num $Num)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.rotl
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  (func $op_rotr (@impl "std/operators.fnk:op_rotr" $Num $Num)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.convert_i64_s (i64.rotr
      (i64.trunc_f64_s (struct.get $Num $val (local.get $a)))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $b)))))))

  ;; =========================================================================
  ;; Power — integer exponentiation by square-and-multiply.
  ;; Negative exponents return 0 (pow(a, n<0) = 1/a^|n|, integer-truncated).
  ;; =========================================================================

  (func $op_pow (@impl "std/operators.fnk:op_pow" $Num $Num)
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
  ;; DivMod — returns [quotient, remainder] as a 2-element list.
  ;; =========================================================================

  (func $op_divmod (@impl "std/operators.fnk:op_divmod" $Num $Num)
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

    ;; Build [q, r] via the list constructor: prepend(q, prepend(r, nil)).
    (call $list_prepend
      (local.get $q)
      (call $list_prepend
        (local.get $r)
        (call $list_empty))))
)
