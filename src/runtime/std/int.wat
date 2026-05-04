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

  ;; $Int — abstract integer parent. Carries `$ival i64`, the canonical
  ;; integer value. $I64 and $U64 differ only in nominal type (signedness
  ;; is interpreted by the operations, not the storage).
  (type $Int (@pub) (sub $Num (struct (field $ival i64))))
    (type $I64 (@pub) (sub final $Int (struct (field $ival i64))))
    (type $U64 (@pub) (sub final $Int (struct (field $ival i64))))

  ;; =========================================================================
  ;; Arithmetic on $Int — result widens to $I64 when sign info is lost.
  ;; (Sub-dispatch by I64/U64 is a future refinement.)
  ;;
  ;; Internal helper $_box_i64_from_f64 wraps a computed f64 into the
  ;; two-field $I64 struct, supplying the i64 view via trunc_s. As ops
  ;; migrate to native i64 arithmetic this helper becomes the natural
  ;; place to drop the f64 field.
  ;; =========================================================================

  (func $_box_i64_from_f64 (param $v f64) (result (ref $I64))
    (struct.new $I64 (i64.trunc_f64_s (local.get $v))))

  (func $_box_i64 (@pub) (param $v i64) (result (ref $I64))
    (struct.new $I64 (local.get $v)))

  (func $op_plus (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (call $_box_i64 (i64.add
      (struct.get $Int $ival (local.get $a))
      (struct.get $Int $ival (local.get $b)))))

  (func $op_minus (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (call $_box_i64 (i64.sub
      (struct.get $Int $ival (local.get $a))
      (struct.get $Int $ival (local.get $b)))))

  (func $op_mul (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (call $_box_i64 (i64.mul
      (struct.get $Int $ival (local.get $a))
      (struct.get $Int $ival (local.get $b)))))

  ;; op_div on $Int — fink `/` is real division; convert i64 operands
  ;; to f64 for the divide, truncate the result back to i64 when boxing.
  ;; TODO: result type should be $F64 once cross-family coercion is
  ;; wired through num.wat's dispatcher.
  (func $op_div (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (call $_box_i64_from_f64 (f64.div
      (f64.convert_i64_s (struct.get $Int $ival (local.get $a)))
      (f64.convert_i64_s (struct.get $Int $ival (local.get $b))))))

  ;; -- Comparison — return raw i32 -----------------------------------
  ;;
  ;; Comparison uses i64 signed semantics. When sub-dispatch by
  ;; signed/unsigned lands, $U64 comparisons will route through
  ;; i64.{lt,le,gt,ge}_u versions instead.

  (func $op_eq (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (i64.eq
      (struct.get $Int $ival (local.get $a))
      (struct.get $Int $ival (local.get $b))))

  (func $op_neq (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (i64.ne
      (struct.get $Int $ival (local.get $a))
      (struct.get $Int $ival (local.get $b))))

  (func $op_lt (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (i64.lt_s
      (struct.get $Int $ival (local.get $a))
      (struct.get $Int $ival (local.get $b))))

  (func $op_lte (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (i64.le_s
      (struct.get $Int $ival (local.get $a))
      (struct.get $Int $ival (local.get $b))))

  (func $op_gt (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (i64.gt_s
      (struct.get $Int $ival (local.get $a))
      (struct.get $Int $ival (local.get $b))))

  (func $op_gte (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (i64.ge_s
      (struct.get $Int $ival (local.get $a))
      (struct.get $Int $ival (local.get $b))))

  ;; Func imports — list constructors via the public API.
  (import "std/list.wat" "empty"
    (func $list_empty (result (ref $List))))
  (import "std/list.wat" "prepend"
    (func $list_prepend (param $head (ref any)) (param $tail (ref $List)) (result (ref $List))))

  ;; Internal box helper — wrap an i64 result as $U64.
  (func $_box_u64 (param $v i64) (result (ref $U64))
    (struct.new $U64 (local.get $v)))

  ;; =========================================================================
  ;; Bitwise — uint family. Take/return $U64.
  ;; =========================================================================

  (func $op_and (@impl "std/operators.fnk:op_and" $U64 $U64)
    (param $a (ref $U64)) (param $b (ref $U64)) (result (ref $U64))
    (return_call $_box_u64 (i64.and
      (struct.get $U64 $ival (local.get $a))
      (struct.get $U64 $ival (local.get $b)))))

  (func $op_or (@impl "std/operators.fnk:op_or" $U64 $U64)
    (param $a (ref $U64)) (param $b (ref $U64)) (result (ref $U64))
    (return_call $_box_u64 (i64.or
      (struct.get $U64 $ival (local.get $a))
      (struct.get $U64 $ival (local.get $b)))))

  (func $op_xor (@impl "std/operators.fnk:op_xor" $U64 $U64)
    (param $a (ref $U64)) (param $b (ref $U64)) (result (ref $U64))
    (return_call $_box_u64 (i64.xor
      (struct.get $U64 $ival (local.get $a))
      (struct.get $U64 $ival (local.get $b)))))

  (func $op_not (@impl "std/operators.fnk:op_not" $U64)
    (param $a (ref $U64)) (result (ref $U64))
    (return_call $_box_u64 (i64.xor
      (struct.get $U64 $ival (local.get $a))
      (i64.const -1))))

  ;; =========================================================================
  ;; Integer arithmetic — math family. Take/return $Int.
  ;; =========================================================================

  (func $op_intdiv (@impl "std/operators.fnk:op_intdiv" $Int $Int)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (return_call $_box_i64 (i64.div_s
      (struct.get $Int $ival (local.get $a))
      (struct.get $Int $ival (local.get $b)))))

  (func $op_rem (@impl "std/operators.fnk:op_rem" $Int $Int)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (return_call $_box_i64 (i64.rem_s
      (struct.get $Int $ival (local.get $a))
      (struct.get $Int $ival (local.get $b)))))

  (func $op_intmod (@impl "std/operators.fnk:op_intmod" $Int $Int)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (return_call $_box_i64 (i64.rem_s
      (struct.get $Int $ival (local.get $a))
      (struct.get $Int $ival (local.get $b)))))

  ;; =========================================================================
  ;; Shifts — value uint, count signed int. Return $U64.
  ;; =========================================================================

  (func $op_shl (@impl "std/operators.fnk:op_shl" $U64 $Int)
    (param $a (ref $U64)) (param $b (ref $Int)) (result (ref $U64))
    (return_call $_box_u64 (i64.shl
      (struct.get $U64 $ival (local.get $a))
      (struct.get $Int $ival (local.get $b)))))

  (func $op_shr (@impl "std/operators.fnk:op_shr" $U64 $Int)
    (param $a (ref $U64)) (param $b (ref $Int)) (result (ref $U64))
    (return_call $_box_u64 (i64.shr_u
      (struct.get $U64 $ival (local.get $a))
      (struct.get $Int $ival (local.get $b)))))

  ;; =========================================================================
  ;; Rotations — value uint, count signed int. Return $U64.
  ;; =========================================================================

  (func $op_rotl (@impl "std/operators.fnk:op_rotl" $U64 $Int)
    (param $a (ref $U64)) (param $b (ref $Int)) (result (ref $U64))
    (return_call $_box_u64 (i64.rotl
      (struct.get $U64 $ival (local.get $a))
      (struct.get $Int $ival (local.get $b)))))

  (func $op_rotr (@impl "std/operators.fnk:op_rotr" $U64 $Int)
    (param $a (ref $U64)) (param $b (ref $Int)) (result (ref $U64))
    (return_call $_box_u64 (i64.rotr
      (struct.get $U64 $ival (local.get $a))
      (struct.get $Int $ival (local.get $b)))))

  ;; =========================================================================
  ;; Power — integer exponentiation by square-and-multiply.
  ;; Negative exponents return 0 (pow(a, n<0) = 1/a^|n|, integer-truncated).
  ;; =========================================================================

  (func $op_pow (@impl "std/operators.fnk:op_pow" $Int $Int)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (local $base i64)
    (local $exp i64)
    (local $acc i64)

    (local.set $base (struct.get $Int $ival (local.get $a)))
    (local.set $exp  (struct.get $Int $ival (local.get $b)))

    ;; Negative exponent → 0 (integer truncation of fractional result).
    (if (i64.lt_s (local.get $exp) (i64.const 0))
      (then (return_call $_box_i64 (i64.const 0))))

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

    (return_call $_box_i64 (local.get $acc)))

  ;; =========================================================================
  ;; DivMod — returns [quotient, remainder] as a 2-element list.
  ;; =========================================================================

  (func $op_divmod (@impl "std/operators.fnk:op_divmod" $Int $Int)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $List))
    (local $a_i i64)
    (local $b_i i64)

    (local.set $a_i (struct.get $Int $ival (local.get $a)))
    (local.set $b_i (struct.get $Int $ival (local.get $b)))

    ;; Build [q, r] via the list constructor: prepend(q, prepend(r, nil)).
    (call $list_prepend
      (call $_box_i64 (i64.div_s (local.get $a_i) (local.get $b_i)))
      (call $list_prepend
        (call $_box_i64 (i64.rem_s (local.get $a_i) (local.get $b_i)))
        (call $list_empty))))
)
