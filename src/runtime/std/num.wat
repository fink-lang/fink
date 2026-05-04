;; Number — fink's boxed float type and the numeric op dispatcher.
;;
;; Small integers (-2^30..2^30-1) use i31ref directly; larger or
;; non-integer values are boxed as $Num. This file owns the $Num type
;; and the numeric arms of op_* dispatch — protocols.wat delegates here
;; for the non-collection (numeric) cases. Integer-specific primitives
;; (intdiv, bitwise, shifts, pow, divmod) still live in std/int.wat.

(module

  ;; Type imports — int.wat / float.wat ops we re-route through num.wat
  (import "std/list.wat" "List" (type $List (sub any)))
  (import "std/int.wat"   "Int" (type $Int (sub $Num (struct (field $val f64) (field $ival i64)))))
  (import "std/int.wat"   "I64" (type $I64 (sub $Int (struct (field $val f64) (field $ival i64)))))
  (import "std/int.wat"   "U64" (type $U64 (sub $Int (struct (field $val f64) (field $ival i64)))))
  (import "std/float.wat" "F64" (type $F64 (sub $Num (struct (field $val f64)))))

  ;; Func imports — int.wat arithmetic + comparison on $Int.
  (import "std/int.wat" "op_plus"  (func $int_op_plus  (param (ref $Int)) (param (ref $Int)) (result (ref $Int))))
  (import "std/int.wat" "op_minus" (func $int_op_minus (param (ref $Int)) (param (ref $Int)) (result (ref $Int))))
  (import "std/int.wat" "op_mul"   (func $int_op_mul   (param (ref $Int)) (param (ref $Int)) (result (ref $Int))))
  (import "std/int.wat" "op_div"   (func $int_op_div_  (param (ref $Int)) (param (ref $Int)) (result (ref $Int))))
  (import "std/int.wat" "op_eq"    (func $int_op_eq    (param (ref $Int)) (param (ref $Int)) (result i32)))
  (import "std/int.wat" "op_neq"   (func $int_op_neq   (param (ref $Int)) (param (ref $Int)) (result i32)))
  (import "std/int.wat" "op_lt"    (func $int_op_lt    (param (ref $Int)) (param (ref $Int)) (result i32)))
  (import "std/int.wat" "op_lte"   (func $int_op_lte   (param (ref $Int)) (param (ref $Int)) (result i32)))
  (import "std/int.wat" "op_gt"    (func $int_op_gt    (param (ref $Int)) (param (ref $Int)) (result i32)))
  (import "std/int.wat" "op_gte"   (func $int_op_gte   (param (ref $Int)) (param (ref $Int)) (result i32)))

  ;; Func imports — float.wat primitives. The $F64 arm of every numeric
  ;; op delegates here. Field is still f64-shared with $Num, so the
  ;; result is numerically identical to the $Num path.
  (import "std/float.wat" "op_plus"  (func $float_op_plus  (param (ref $F64)) (param (ref $F64)) (result (ref $F64))))
  (import "std/float.wat" "op_minus" (func $float_op_minus (param (ref $F64)) (param (ref $F64)) (result (ref $F64))))
  (import "std/float.wat" "op_mul"   (func $float_op_mul   (param (ref $F64)) (param (ref $F64)) (result (ref $F64))))
  (import "std/float.wat" "op_div"   (func $float_op_div   (param (ref $F64)) (param (ref $F64)) (result (ref $F64))))
  (import "std/float.wat" "op_eq"    (func $float_op_eq    (param (ref $F64)) (param (ref $F64)) (result i32)))
  (import "std/float.wat" "op_neq"   (func $float_op_neq   (param (ref $F64)) (param (ref $F64)) (result i32)))
  (import "std/float.wat" "op_lt"    (func $float_op_lt    (param (ref $F64)) (param (ref $F64)) (result i32)))
  (import "std/float.wat" "op_lte"   (func $float_op_lte   (param (ref $F64)) (param (ref $F64)) (result i32)))
  (import "std/float.wat" "op_gt"    (func $float_op_gt    (param (ref $F64)) (param (ref $F64)) (result i32)))
  (import "std/float.wat" "op_gte"   (func $float_op_gte   (param (ref $F64)) (param (ref $F64)) (result i32)))
  (import "std/float.wat" "op_pow"   (func $float_op_pow   (param (ref $F64)) (param (ref $F64)) (result (ref $F64))))

  ;; Func imports — int.wat primitives. num.wat re-routes these so
  ;; protocols.wat only needs to know about num.wat for numeric ops.
  (import "std/int.wat" "op_intdiv" (func $int_op_div    (param (ref $Int)) (param (ref $Int)) (result (ref $Int))))
  (import "std/int.wat" "op_rem"    (func $int_op_rem    (param (ref $Int)) (param (ref $Int)) (result (ref $Int))))
  (import "std/int.wat" "op_intmod" (func $int_op_mod    (param (ref $Int)) (param (ref $Int)) (result (ref $Int))))
  (import "std/int.wat" "op_pow"    (func $int_op_pow    (param (ref $Int)) (param (ref $Int)) (result (ref $Int))))
  (import "std/int.wat" "op_divmod" (func $int_op_divmod (param (ref $Int)) (param (ref $Int)) (result (ref $List))))
  (import "std/int.wat" "op_rotl"   (func $int_op_rotl   (param (ref $U64)) (param (ref $Int)) (result (ref $U64))))
  (import "std/int.wat" "op_rotr"   (func $int_op_rotr   (param (ref $U64)) (param (ref $Int)) (result (ref $U64))))
  (import "std/int.wat" "op_not"    (func $int_op_not    (param (ref $U64)) (result (ref $U64))))
  (import "std/int.wat" "op_and"    (func $int_op_and    (param (ref $U64)) (param (ref $U64)) (result (ref $U64))))
  (import "std/int.wat" "op_or"     (func $int_op_or     (param (ref $U64)) (param (ref $U64)) (result (ref $U64))))
  (import "std/int.wat" "op_xor"    (func $int_op_xor    (param (ref $U64)) (param (ref $U64)) (result (ref $U64))))
  (import "std/int.wat" "op_shl"    (func $int_op_shl    (param (ref $U64)) (param (ref $Int)) (result (ref $U64))))
  (import "std/int.wat" "op_shr"    (func $int_op_shr    (param (ref $U64)) (param (ref $Int)) (result (ref $U64))))

  ;; $Num — abstract numeric base type.
  ;; Concrete subtypes ($I64 / $U64 in int.wat, $F64 in float.wat) extend
  ;; this shape with their own value field. For now all subtypes share the
  ;; same `f64 $val` slot so existing ops work uniformly; the field type
  ;; will narrow per subtype in a later step.
  ;; Small integers use i31ref directly (no struct needed).
  ;;
  ;; `@todo-no-rec` emits $Num as a singleton before the merged rec group
  ;; so subtypes in int.wat / float.wat (which land in the rec group) can
  ;; reference it. WasmGC requires supertype to precede subtype, and the
  ;; wat-linker doesn't yet topologically order types within the rec
  ;; group. Promote out once the linker handles supertype ordering.
  (type $Num (@pub) (@todo-no-rec) (sub (struct
    (field $val f64)
  )))


  ;; =========================================================================
  ;; Numeric op handlers — called by protocols.wat for the $Num arm of each
  ;; polymorphic op. Pure compute, no continuation plumbing.
  ;;
  ;; Float arithmetic stays inline (f64 ops); integer arithmetic delegates
  ;; to int.wat. When int/float types split, this is where dispatch will
  ;; route between them.
  ;; =========================================================================

  ;; -- Coercion helpers ----------------------------------------------
  ;;
  ;; Given a $Num (any concrete subtype), return it as the requested
  ;; concrete type by re-boxing. Field is f64-shared today so the value
  ;; transfers verbatim. When fields narrow per subtype, these helpers
  ;; will do the actual conversion (e.g. f64.convert_i64_s for Int→F64).

  (func $as_f64 (param $n (ref $Num)) (result (ref $F64))
    ;; Already $F64 → no-op.
    (block $not_f64
      (block $is_f64 (result (ref $F64))
        (br $not_f64
          (br_on_cast $is_f64 (ref $Num) (ref $F64) (local.get $n))))
      (return))
    ;; Otherwise re-box the f64 slot under $F64.
    (struct.new $F64 (struct.get $Num $val (local.get $n))))

  (func $as_int (param $n (ref $Num)) (result (ref $Int))
    ;; Already $Int → no-op.
    (block $not_int
      (block $is_int (result (ref $Int))
        (br $not_int
          (br_on_cast $is_int (ref $Num) (ref $Int) (local.get $n))))
      (return))
    ;; Otherwise re-box under $I64 (default int representation).
    ;; Both fields populated: f64 for legacy readers, i64 for new int ops.
    (struct.new $I64
      (struct.get $Num $val (local.get $n))
      (i64.trunc_f64_s (struct.get $Num $val (local.get $n)))))

  ;; True if either side is $F64 — picks the float arm.
  (func $is_float_op (param $a (ref $Num)) (param $b (ref $Num)) (result i32)
    (i32.or
      (ref.test (ref $F64) (local.get $a))
      (ref.test (ref $F64) (local.get $b))))

  ;; Type-family compatibility check for binary numeric ops.
  ;;
  ;; Rule: math family (signed int + float) mixes freely; bits family
  ;; ($U64) does not mix with anything else. Mixed bits/math → trap.
  ;;
  ;; Concretely: if exactly one side is $U64 (i.e. xor of the U64 tests
  ;; is true), the operands are from different families.
  (func $check_compat (param $a (ref $Num)) (param $b (ref $Num))
    (if (i32.xor
          (ref.test (ref $U64) (local.get $a))
          (ref.test (ref $U64) (local.get $b)))
      (then (unreachable))))

  ;; Reject float operands on integer-math ops (intdiv, rem, intmod,
  ;; divmod). Mixed signed/unsigned still goes through `check_compat`.
  (func $check_int (param $a (ref $Num)) (param $b (ref $Num))
    (if (i32.or
          (ref.test (ref $F64) (local.get $a))
          (ref.test (ref $F64) (local.get $b)))
      (then (unreachable))))

  ;; Reject any operand that is not $U64. Bitwise ops (and/or/xor/not/
  ;; shl/shr/rotl/rotr) are unsigned-only — bit patterns belong to the
  ;; bits family, signed math doesn't.
  (func $check_uint (param $a (ref $Num)) (param $b (ref $Num))
    (if (i32.eqz (ref.test (ref $U64) (local.get $a)))
      (then (unreachable)))
    (if (i32.eqz (ref.test (ref $U64) (local.get $b)))
      (then (unreachable))))

  (func $check_uint_unary (param $a (ref $Num))
    (if (i32.eqz (ref.test (ref $U64) (local.get $a)))
      (then (unreachable))))

  ;; Shift / rotate ops take a uint value and a signed-int count.
  ;; First arg must be $U64 (the bit pattern); second arg must be a
  ;; non-float integer (the bit-position offset, math-family).
  (func $check_shift (param $a (ref $Num)) (param $b (ref $Num))
    (if (i32.eqz (ref.test (ref $U64) (local.get $a)))
      (then (unreachable)))
    (if (i32.or
          (ref.test (ref $F64) (local.get $b))
          (ref.test (ref $U64) (local.get $b)))
      (then (unreachable))))

  ;; -- Arithmetic — dispatch on $F64 vs $Int -------------------------

  (func $op_plus (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (call $check_compat (local.get $a) (local.get $b))
    (if (result (ref $Num)) (call $is_float_op (local.get $a) (local.get $b))
      (then (call $float_op_plus
              (call $as_f64 (local.get $a))
              (call $as_f64 (local.get $b))))
      (else (call $int_op_plus
              (call $as_int (local.get $a))
              (call $as_int (local.get $b))))))

  (func $op_minus (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (call $check_compat (local.get $a) (local.get $b))
    (if (result (ref $Num)) (call $is_float_op (local.get $a) (local.get $b))
      (then (call $float_op_minus
              (call $as_f64 (local.get $a))
              (call $as_f64 (local.get $b))))
      (else (call $int_op_minus
              (call $as_int (local.get $a))
              (call $as_int (local.get $b))))))

  (func $op_mul (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (call $check_compat (local.get $a) (local.get $b))
    (if (result (ref $Num)) (call $is_float_op (local.get $a) (local.get $b))
      (then (call $float_op_mul
              (call $as_f64 (local.get $a))
              (call $as_f64 (local.get $b))))
      (else (call $int_op_mul
              (call $as_int (local.get $a))
              (call $as_int (local.get $b))))))

  (func $op_div (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (call $check_compat (local.get $a) (local.get $b))
    (if (result (ref $Num)) (call $is_float_op (local.get $a) (local.get $b))
      (then (call $float_op_div
              (call $as_f64 (local.get $a))
              (call $as_f64 (local.get $b))))
      (else (call $int_op_div_
              (call $as_int (local.get $a))
              (call $as_int (local.get $b))))))

  ;; -- Comparison — dispatch on $F64 vs $Int --------------------------

  (func $op_eq (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result i32)
    (call $check_compat (local.get $a) (local.get $b))
    (if (result i32) (call $is_float_op (local.get $a) (local.get $b))
      (then (call $float_op_eq
              (call $as_f64 (local.get $a))
              (call $as_f64 (local.get $b))))
      (else (call $int_op_eq
              (call $as_int (local.get $a))
              (call $as_int (local.get $b))))))

  (func $op_neq (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result i32)
    (call $check_compat (local.get $a) (local.get $b))
    (if (result i32) (call $is_float_op (local.get $a) (local.get $b))
      (then (call $float_op_neq
              (call $as_f64 (local.get $a))
              (call $as_f64 (local.get $b))))
      (else (call $int_op_neq
              (call $as_int (local.get $a))
              (call $as_int (local.get $b))))))

  (func $op_lt (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result i32)
    (call $check_compat (local.get $a) (local.get $b))
    (if (result i32) (call $is_float_op (local.get $a) (local.get $b))
      (then (call $float_op_lt
              (call $as_f64 (local.get $a))
              (call $as_f64 (local.get $b))))
      (else (call $int_op_lt
              (call $as_int (local.get $a))
              (call $as_int (local.get $b))))))

  (func $op_lte (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result i32)
    (call $check_compat (local.get $a) (local.get $b))
    (if (result i32) (call $is_float_op (local.get $a) (local.get $b))
      (then (call $float_op_lte
              (call $as_f64 (local.get $a))
              (call $as_f64 (local.get $b))))
      (else (call $int_op_lte
              (call $as_int (local.get $a))
              (call $as_int (local.get $b))))))

  (func $op_gt (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result i32)
    (call $check_compat (local.get $a) (local.get $b))
    (if (result i32) (call $is_float_op (local.get $a) (local.get $b))
      (then (call $float_op_gt
              (call $as_f64 (local.get $a))
              (call $as_f64 (local.get $b))))
      (else (call $int_op_gt
              (call $as_int (local.get $a))
              (call $as_int (local.get $b))))))

  (func $op_gte (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result i32)
    (call $check_compat (local.get $a) (local.get $b))
    (if (result i32) (call $is_float_op (local.get $a) (local.get $b))
      (then (call $float_op_gte
              (call $as_f64 (local.get $a))
              (call $as_f64 (local.get $b))))
      (else (call $int_op_gte
              (call $as_int (local.get $a))
              (call $as_int (local.get $b))))))

  ;; -- Integer ops — delegate to int.wat ------------------------------

  (func $op_intdiv (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (call $check_int (local.get $a) (local.get $b))
    (call $check_compat (local.get $a) (local.get $b))
    (return_call $int_op_div
      (ref.cast (ref $Int) (local.get $a))
      (ref.cast (ref $Int) (local.get $b))))

  (func $op_rem (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (call $check_int (local.get $a) (local.get $b))
    (call $check_compat (local.get $a) (local.get $b))
    (return_call $int_op_rem
      (ref.cast (ref $Int) (local.get $a))
      (ref.cast (ref $Int) (local.get $b))))

  (func $op_intmod (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (call $check_int (local.get $a) (local.get $b))
    (call $check_compat (local.get $a) (local.get $b))
    (return_call $int_op_mod
      (ref.cast (ref $Int) (local.get $a))
      (ref.cast (ref $Int) (local.get $b))))

  (func $op_pow (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (call $check_compat (local.get $a) (local.get $b))
    (if (result (ref $Num)) (call $is_float_op (local.get $a) (local.get $b))
      (then (call $float_op_pow
              (call $as_f64 (local.get $a))
              (call $as_f64 (local.get $b))))
      (else (return_call $int_op_pow
              (ref.cast (ref $Int) (local.get $a))
              (ref.cast (ref $Int) (local.get $b))))))

  (func $op_divmod (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $List))
    (call $check_int (local.get $a) (local.get $b))
    (call $check_compat (local.get $a) (local.get $b))
    (return_call $int_op_divmod
      (ref.cast (ref $Int) (local.get $a))
      (ref.cast (ref $Int) (local.get $b))))

  ;; Bitwise ops — unsigned-only (bits family). Cast to $U64 (post check).

  (func $op_rotl (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (call $check_shift (local.get $a) (local.get $b))
    (return_call $int_op_rotl
      (ref.cast (ref $U64) (local.get $a))
      (ref.cast (ref $Int) (local.get $b))))

  (func $op_rotr (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (call $check_shift (local.get $a) (local.get $b))
    (return_call $int_op_rotr
      (ref.cast (ref $U64) (local.get $a))
      (ref.cast (ref $Int) (local.get $b))))

  (func $op_not (@pub)
    (param $a (ref $Num)) (result (ref $Num))
    (call $check_uint_unary (local.get $a))
    (return_call $int_op_not (ref.cast (ref $U64) (local.get $a))))

  (func $op_and (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (call $check_uint (local.get $a) (local.get $b))
    (return_call $int_op_and
      (ref.cast (ref $U64) (local.get $a))
      (ref.cast (ref $U64) (local.get $b))))

  (func $op_or (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (call $check_uint (local.get $a) (local.get $b))
    (return_call $int_op_or
      (ref.cast (ref $U64) (local.get $a))
      (ref.cast (ref $U64) (local.get $b))))

  (func $op_xor (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (call $check_uint (local.get $a) (local.get $b))
    (return_call $int_op_xor
      (ref.cast (ref $U64) (local.get $a))
      (ref.cast (ref $U64) (local.get $b))))

  (func $op_shl (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (call $check_shift (local.get $a) (local.get $b))
    (return_call $int_op_shl
      (ref.cast (ref $U64) (local.get $a))
      (ref.cast (ref $Int) (local.get $b))))

  (func $op_shr (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (call $check_shift (local.get $a) (local.get $b))
    (return_call $int_op_shr
      (ref.cast (ref $U64) (local.get $a))
      (ref.cast (ref $Int) (local.get $b))))


  ;; -- Hashing impl ----------------------------------------------------

  ;; hash_i31 — fold a $Num's f64 bits to a 31-bit hash.
  ;;
  ;; XOR the upper and lower 32-bit halves, then mask to 31 bits
  ;; so the result fits in i31ref without overflow.
  (func $hash_i31 (@pub) (@impl "std/hashing.fnk:hash_i31" $Num)
    (param $n (ref $Num))
    (result i32)

    (local $bits i64)
    (local.set $bits
      (i64.reinterpret_f64 (struct.get $Num $val (local.get $n))))

    (i32.and
      (i32.xor
        (i32.wrap_i64 (local.get $bits))
        (i32.wrap_i64 (i64.shr_u (local.get $bits) (i64.const 32))))
      (i32.const 0x7fffffff))
  )

)
