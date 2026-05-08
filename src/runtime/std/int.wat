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
  (import "std/num.wat"   "Num"  (type $Num  (sub any) (struct)))
  (import "std/float.wat" "F64"  (type $F64  (sub $Num (struct (field $val f64)))))
  (import "std/list.wat"  "List" (type $List (sub any)))
  (import "std/str.wat"   "Str"  (type $Str  (sub any) (struct)))
  (import "std/str.wat"   "ByteArray" (type $ByteArray (array (mut i8))))
  (import "std/str.wat"   "from_bytes" (func $str_from_bytes
    (param (ref $ByteArray)) (result (ref $Str))))

  ;; $Int — abstract, nominal-only supertype. No fields. Storage
  ;; (`$ival i64`) lives on the leaves $I64/$U64. They differ only in
  ;; nominal type; signedness is interpreted by the operations, not
  ;; the storage. Reads through $Int go via the $_int_ival helper,
  ;; which dispatches per concrete leaf.
  (type $Int (@pub) (sub $Num (struct)))
    (type $I64 (@pub) (sub final $Int (struct (field $ival i64))))
    (type $U64 (@pub) (sub final $Int (struct (field $ival i64))))

  ;; =========================================================================
  ;; Arithmetic on $Int — result widens to $I64 when sign info is lost.
  ;; (Sub-dispatch by I64/U64 is a future refinement.)
  ;; =========================================================================

  (func $_box_i64 (@pub) (param $v i64) (result (ref $I64))
    (struct.new $I64 (local.get $v)))

  ;; Read the i64 payload from any $Int. $Int itself is an empty
  ;; abstract supertype; storage lives on the leaves $I64/$U64. This
  ;; helper centralises the per-subtype dispatch so call sites stay
  ;; legible.
  (func $_int_ival (@pub) (param $n (ref $Int)) (result i64)
    (if (result i64) (ref.test (ref $I64) (local.get $n))
      (then (struct.get $I64 $ival (ref.cast (ref $I64) (local.get $n))))
      (else (struct.get $U64 $ival (ref.cast (ref $U64) (local.get $n))))))

  (func $op_plus (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (call $_box_i64 (i64.add
      (call $_int_ival (local.get $a))
      (call $_int_ival (local.get $b)))))

  (func $op_minus (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (call $_box_i64 (i64.sub
      (call $_int_ival (local.get $a))
      (call $_int_ival (local.get $b)))))

  (func $op_mul (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (call $_box_i64 (i64.mul
      (call $_int_ival (local.get $a))
      (call $_int_ival (local.get $b)))))

  ;; op_div on $Int — fink `/` is real division; converts both i64
  ;; operands to f64, divides, and returns a boxed $F64. For integer
  ;; truncated division use op_intdiv (`//` at the source level).
  (func $op_div (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $F64))
    (struct.new $F64 (f64.div
      (f64.convert_i64_s (call $_int_ival (local.get $a)))
      (f64.convert_i64_s (call $_int_ival (local.get $b))))))

  ;; -- Comparison — return raw i32 -----------------------------------
  ;;
  ;; Comparison uses i64 signed semantics. When sub-dispatch by
  ;; signed/unsigned lands, $U64 comparisons will route through
  ;; i64.{lt,le,gt,ge}_u versions instead.

  (func $op_eq (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (i64.eq
      (call $_int_ival (local.get $a))
      (call $_int_ival (local.get $b))))

  (func $op_neq (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (i64.ne
      (call $_int_ival (local.get $a))
      (call $_int_ival (local.get $b))))

  ;; Ordering ops dispatch on signedness: $U64 → unsigned compare,
  ;; $I64 → signed compare. num.wat's $check_compat already traps on
  ;; mixed signed/unsigned operands, so by the time we get here both
  ;; operands share a family — testing $a is enough.

  (func $op_lt (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (if (result i32) (ref.test (ref $U64) (local.get $a))
      (then (i64.lt_u
        (call $_int_ival (local.get $a))
        (call $_int_ival (local.get $b))))
      (else (i64.lt_s
        (call $_int_ival (local.get $a))
        (call $_int_ival (local.get $b))))))

  (func $op_lte (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (if (result i32) (ref.test (ref $U64) (local.get $a))
      (then (i64.le_u
        (call $_int_ival (local.get $a))
        (call $_int_ival (local.get $b))))
      (else (i64.le_s
        (call $_int_ival (local.get $a))
        (call $_int_ival (local.get $b))))))

  (func $op_gt (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (if (result i32) (ref.test (ref $U64) (local.get $a))
      (then (i64.gt_u
        (call $_int_ival (local.get $a))
        (call $_int_ival (local.get $b))))
      (else (i64.gt_s
        (call $_int_ival (local.get $a))
        (call $_int_ival (local.get $b))))))

  (func $op_gte (@pub)
    (param $a (ref $Int)) (param $b (ref $Int)) (result i32)
    (if (result i32) (ref.test (ref $U64) (local.get $a))
      (then (i64.ge_u
        (call $_int_ival (local.get $a))
        (call $_int_ival (local.get $b))))
      (else (i64.ge_s
        (call $_int_ival (local.get $a))
        (call $_int_ival (local.get $b))))))

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
      (call $_int_ival (local.get $a))
      (call $_int_ival (local.get $b)))))

  (func $op_rem (@impl "std/operators.fnk:op_rem" $Int $Int)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (return_call $_box_i64 (i64.rem_s
      (call $_int_ival (local.get $a))
      (call $_int_ival (local.get $b)))))

  (func $op_intmod (@impl "std/operators.fnk:op_intmod" $Int $Int)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (return_call $_box_i64 (i64.rem_s
      (call $_int_ival (local.get $a))
      (call $_int_ival (local.get $b)))))

  ;; =========================================================================
  ;; Shifts — value uint, count signed int. Return $U64.
  ;; =========================================================================

  (func $op_shl (@impl "std/operators.fnk:op_shl" $U64 $Int)
    (param $a (ref $U64)) (param $b (ref $Int)) (result (ref $U64))
    (return_call $_box_u64 (i64.shl
      (struct.get $U64 $ival (local.get $a))
      (call $_int_ival (local.get $b)))))

  (func $op_shr (@impl "std/operators.fnk:op_shr" $U64 $Int)
    (param $a (ref $U64)) (param $b (ref $Int)) (result (ref $U64))
    (return_call $_box_u64 (i64.shr_u
      (struct.get $U64 $ival (local.get $a))
      (call $_int_ival (local.get $b)))))

  ;; =========================================================================
  ;; Rotations — value uint, count signed int. Return $U64.
  ;; =========================================================================

  (func $op_rotl (@impl "std/operators.fnk:op_rotl" $U64 $Int)
    (param $a (ref $U64)) (param $b (ref $Int)) (result (ref $U64))
    (return_call $_box_u64 (i64.rotl
      (struct.get $U64 $ival (local.get $a))
      (call $_int_ival (local.get $b)))))

  (func $op_rotr (@impl "std/operators.fnk:op_rotr" $U64 $Int)
    (param $a (ref $U64)) (param $b (ref $Int)) (result (ref $U64))
    (return_call $_box_u64 (i64.rotr
      (struct.get $U64 $ival (local.get $a))
      (call $_int_ival (local.get $b)))))

  ;; =========================================================================
  ;; Power — integer exponentiation by square-and-multiply.
  ;; Negative exponents return 0 (pow(a, n<0) = 1/a^|n|, integer-truncated).
  ;; =========================================================================

  (func $op_pow (@impl "std/operators.fnk:op_pow" $Int $Int)
    (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (local $base i64)
    (local $exp i64)
    (local $acc i64)

    (local.set $base (call $_int_ival (local.get $a)))
    (local.set $exp  (call $_int_ival (local.get $b)))

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

    (local.set $a_i (call $_int_ival (local.get $a)))
    (local.set $b_i (call $_int_ival (local.get $b)))

    ;; Build [q, r] via the list constructor: prepend(q, prepend(r, nil)).
    (call $list_prepend
      (call $_box_i64 (i64.div_s (local.get $a_i) (local.get $b_i)))
      (call $list_prepend
        (call $_box_i64 (i64.rem_s (local.get $a_i) (local.get $b_i)))
        (call $list_empty))))


  ;; =========================================================================
  ;; Math primitives — int arms of std/math.fnk dispatch.
  ;;
  ;; Result subtype matches input subtype where defined: $I64 → $I64,
  ;; $U64 → $U64. Ops not meaningful on the bits family (neg, copysign)
  ;; trap on $U64 input.
  ;; =========================================================================

  ;; abs — magnitude. $U64: identity (already non-negative). $I64: sign-
  ;; extract trick `(v ^ (v >> 63)) - (v >> 63)`.
  (func $abs (@pub) (param $a (ref $Int)) (result (ref $Int))
    (local $v i64) (local $m i64)
    (if (ref.test (ref $U64) (local.get $a))
      (then (return (local.get $a))))
    (local.set $v (call $_int_ival (local.get $a)))
    (local.set $m (i64.shr_s (local.get $v) (i64.const 63)))
    (return_call $_box_i64
      (i64.sub
        (i64.xor (local.get $v) (local.get $m))
        (local.get $m))))

  ;; neg — negation. Traps on $U64 (bits family has no signed negation).
  (func $neg (@pub) (param $a (ref $Int)) (result (ref $Int))
    (if (ref.test (ref $U64) (local.get $a))
      (then (unreachable)))
    (return_call $_box_i64
      (i64.sub (i64.const 0) (call $_int_ival (local.get $a)))))

  ;; sign — -1/0/1 in same subtype as input. $U64 result is 0 or 1.
  (func $sign (@pub) (param $a (ref $Int)) (result (ref $Int))
    (local $v i64)
    (local.set $v (call $_int_ival (local.get $a)))
    (if (ref.test (ref $U64) (local.get $a))
      (then
        (return_call $_box_u64
          (i64.extend_i32_u (i64.ne (local.get $v) (i64.const 0))))))
    ;; signed: (v > 0) - (v < 0) → -1, 0, or 1
    (return_call $_box_i64
      (i64.sub
        (i64.extend_i32_u (i64.gt_s (local.get $v) (i64.const 0)))
        (i64.extend_i32_u (i64.lt_s (local.get $v) (i64.const 0))))))

  ;; min / max — pairwise. $U64 uses unsigned comparison; $I64 uses signed.
  ;; check_compat (in num.wat) already enforces same family at the call site;
  ;; here we only need to pick the right comparison per subtype.
  (func $min (@pub) (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (local $av i64) (local $bv i64)
    (local.set $av (call $_int_ival (local.get $a)))
    (local.set $bv (call $_int_ival (local.get $b)))
    (if (ref.test (ref $U64) (local.get $a))
      (then
        (return_call $_box_u64
          (select (local.get $av) (local.get $bv)
            (i64.lt_u (local.get $av) (local.get $bv))))))
    (return_call $_box_i64
      (select (local.get $av) (local.get $bv)
        (i64.lt_s (local.get $av) (local.get $bv)))))

  (func $max (@pub) (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (local $av i64) (local $bv i64)
    (local.set $av (call $_int_ival (local.get $a)))
    (local.set $bv (call $_int_ival (local.get $b)))
    (if (ref.test (ref $U64) (local.get $a))
      (then
        (return_call $_box_u64
          (select (local.get $av) (local.get $bv)
            (i64.gt_u (local.get $av) (local.get $bv))))))
    (return_call $_box_i64
      (select (local.get $av) (local.get $bv)
        (i64.gt_s (local.get $av) (local.get $bv)))))

  ;; copysign — magnitude of `a`, sign of `b`. Traps on $U64 (no sign).
  (func $copysign (@pub) (param $a (ref $Int)) (param $b (ref $Int)) (result (ref $Int))
    (local $av i64) (local $bv i64) (local $abs i64) (local $sign i64)
    (if (i32.or
          (ref.test (ref $U64) (local.get $a))
          (ref.test (ref $U64) (local.get $b)))
      (then (unreachable)))
    (local.set $av (call $_int_ival (local.get $a)))
    (local.set $bv (call $_int_ival (local.get $b)))
    ;; |a| via the same trick as $abs.
    (local.set $sign (i64.shr_s (local.get $av) (i64.const 63)))
    (local.set $abs
      (i64.sub
        (i64.xor (local.get $av) (local.get $sign))
        (local.get $sign)))
    ;; sign of b: -1 if b < 0 else +1 (treating 0 as +).
    (return_call $_box_i64
      (select
        (i64.sub (i64.const 0) (local.get $abs))
        (local.get $abs)
        (i64.lt_s (local.get $bv) (i64.const 0)))))


  ;; =========================================================================
  ;; Formatting — render a $Int as a decimal string. $U64 prints unsigned;
  ;; $I64 prints signed. The output is always a $Str backed by a
  ;; $ByteArray (built locally, wrapped via str.wat:from_bytes).
  ;; =========================================================================

  (func $_fmt_i64 (param $v i64) (result (ref $Str))
    (local $neg i32)
    (local $abs i64)
    (local $digits i32)
    (local $tmp i64)
    (local $buf (ref $ByteArray))
    (local $pos i32)

    ;; Zero special case.
    (if (i64.eqz (local.get $v))
      (then
        (local.set $buf (array.new $ByteArray (i32.const 0) (i32.const 1)))
        (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x30))
        (return_call $str_from_bytes (local.get $buf))))

    ;; Sign handling. i64::MIN negated stays i64::MIN (two's complement),
    ;; but i64.div_u / i64.rem_u below treat $abs as unsigned, so the
    ;; arithmetic is correct for that one edge case.
    (local.set $neg (i64.lt_s (local.get $v) (i64.const 0)))
    (if (local.get $neg)
      (then (local.set $abs (i64.sub (i64.const 0) (local.get $v))))
      (else (local.set $abs (local.get $v))))

    ;; Count digits.
    (local.set $digits (i32.const 0))
    (local.set $tmp (local.get $abs))
    (block $done
      (loop $count
        (local.set $digits (i32.add (local.get $digits) (i32.const 1)))
        (local.set $tmp (i64.div_u (local.get $tmp) (i64.const 10)))
        (br_if $count (i32.wrap_i64 (local.get $tmp)))))

    (local.set $buf
      (array.new $ByteArray (i32.const 0)
        (i32.add (local.get $digits) (local.get $neg))))

    (if (local.get $neg)
      (then (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x2D))))

    (local.set $pos
      (i32.sub
        (i32.add (local.get $digits) (local.get $neg))
        (i32.const 1)))
    (local.set $tmp (local.get $abs))
    (block $done
      (loop $write
        (array.set $ByteArray (local.get $buf) (local.get $pos)
          (i32.add (i32.const 0x30)
            (i32.wrap_i64 (i64.rem_u (local.get $tmp) (i64.const 10)))))
        (local.set $tmp (i64.div_u (local.get $tmp) (i64.const 10)))
        (local.set $pos (i32.sub (local.get $pos) (i32.const 1)))
        (br_if $write (i32.wrap_i64 (local.get $tmp)))))

    (return_call $str_from_bytes (local.get $buf)))

  (func $_fmt_u64 (param $v i64) (result (ref $Str))
    (local $digits i32)
    (local $tmp i64)
    (local $buf (ref $ByteArray))
    (local $pos i32)

    (if (i64.eqz (local.get $v))
      (then
        (local.set $buf (array.new $ByteArray (i32.const 0) (i32.const 1)))
        (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x30))
        (return_call $str_from_bytes (local.get $buf))))

    (local.set $digits (i32.const 0))
    (local.set $tmp (local.get $v))
    (block $done
      (loop $count
        (local.set $digits (i32.add (local.get $digits) (i32.const 1)))
        (local.set $tmp (i64.div_u (local.get $tmp) (i64.const 10)))
        (br_if $count (i32.wrap_i64 (local.get $tmp)))))

    (local.set $buf (array.new $ByteArray (i32.const 0) (local.get $digits)))

    (local.set $pos (i32.sub (local.get $digits) (i32.const 1)))
    (local.set $tmp (local.get $v))
    (block $done
      (loop $write
        (array.set $ByteArray (local.get $buf) (local.get $pos)
          (i32.add (i32.const 0x30)
            (i32.wrap_i64 (i64.rem_u (local.get $tmp) (i64.const 10)))))
        (local.set $tmp (i64.div_u (local.get $tmp) (i64.const 10)))
        (local.set $pos (i32.sub (local.get $pos) (i32.const 1)))
        (br_if $write (i32.wrap_i64 (local.get $tmp)))))

    (return_call $str_from_bytes (local.get $buf)))

  (func $fmt (@pub) (@impl "std/str.fnk:fmt" $Int) (param $n (ref $Int)) (result (ref $Str))
    (if (result (ref $Str)) (ref.test (ref $U64) (local.get $n))
      (then (call $_fmt_u64 (struct.get $U64 $ival (ref.cast (ref $U64) (local.get $n)))))
      (else (call $_fmt_i64 (struct.get $I64 $ival (ref.cast (ref $I64) (local.get $n)))))))

  ;; repr — same as fmt for ints (no quoting/escaping needed).
  (func $repr (@pub) (@impl "std/repr.fnk:repr" $Int)
    (param $n (ref $Int)) (result (ref $Str))
    (return_call $fmt (local.get $n)))
)
