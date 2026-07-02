;; Float types and operations.
;;
;; Step 3c-i: $F64 primitives live here with concrete-type signatures.
;; Field is still f64 (shared $Num slot); narrowing per-subtype is a
;; follow-up. num.wat's polymorphic op_* dispatches to these for the
;; $F64 arm.

(module

  ;; Type imports
  (import "rt/num.wat" "Num" (type $Num (sub any) (struct)))
  (import "rt/str.wat" "Str" (type $Str (sub any) (struct)))
  (import "rt/str.wat" "ByteArray" (type $ByteArray (array (mut i8))))
  (import "rt/str.wat" "from_bytes"
    (func $str_from_bytes (param (ref $ByteArray)) (result (ref $Str))))
  (import "rt/types.wat" "Type" (type $Type (sub any)))
  (import "rt/types.wat" "new_unit_type"
    (func $new_unit_type (param (ref i31)) (result (ref $Type))))

  ;; $F64 — IEEE 754 binary64. Subtype of $Num; for now shares $Num's
  ;; `f64 $val` slot.
  (type $F64 (@pub) (sub final $Num (struct (field $val f64))))


  ;; ---- Intrinsic type singleton ----
  ;;
  ;; `$FloatType` reifies the built-in `float` as a first-class `$Type` (guard /
  ;; match arm). `is_instance` bridges a value via `ref.test (ref $F64)`. Mirrors
  ;; the `str` intrinsic (rt/str.wat). Registered in the module (start).
  ;;
  ;; NAME: the reserved `float` symbol -- id 4 -> word (4 << 3) | 0b010 = 34. MUST
  ;; equal reserved_symbol_word("float") in src/passes/wasm/link.rs.
  (global $_float_type (mut (ref null $Type)) (ref.null none))

  (func $register_float_type (@pub)
    (if (ref.is_null (global.get $_float_type))
      (then
        (global.set $_float_type
          (call $new_unit_type (ref.i31 (i32.const 34)))))))

  (func $FloatType (@pub) (result (ref $Type))
    (ref.as_non_null (global.get $_float_type)))

  ;; -- Arithmetic ------------------------------------------------------

  (func $op_plus (@pub)
    (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.add
      (struct.get $F64 $val (local.get $a))
      (struct.get $F64 $val (local.get $b)))))

  (func $op_minus (@pub)
    (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.sub
      (struct.get $F64 $val (local.get $a))
      (struct.get $F64 $val (local.get $b)))))

  (func $op_mul (@pub)
    (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.mul
      (struct.get $F64 $val (local.get $a))
      (struct.get $F64 $val (local.get $b)))))

  (func $op_div (@pub)
    (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.div
      (struct.get $F64 $val (local.get $a))
      (struct.get $F64 $val (local.get $b)))))

  ;; -- Comparison — return raw i32 -----------------------------------

  (func $op_eq (@pub)
    (param $a (ref $F64)) (param $b (ref $F64)) (result i32)
    (f64.eq
      (struct.get $F64 $val (local.get $a))
      (struct.get $F64 $val (local.get $b))))

  (func $op_neq (@pub)
    (param $a (ref $F64)) (param $b (ref $F64)) (result i32)
    (f64.ne
      (struct.get $F64 $val (local.get $a))
      (struct.get $F64 $val (local.get $b))))

  (func $op_lt (@pub)
    (param $a (ref $F64)) (param $b (ref $F64)) (result i32)
    (f64.lt
      (struct.get $F64 $val (local.get $a))
      (struct.get $F64 $val (local.get $b))))

  (func $op_lte (@pub)
    (param $a (ref $F64)) (param $b (ref $F64)) (result i32)
    (f64.le
      (struct.get $F64 $val (local.get $a))
      (struct.get $F64 $val (local.get $b))))

  (func $op_gt (@pub)
    (param $a (ref $F64)) (param $b (ref $F64)) (result i32)
    (f64.gt
      (struct.get $F64 $val (local.get $a))
      (struct.get $F64 $val (local.get $b))))

  (func $op_gte (@pub)
    (param $a (ref $F64)) (param $b (ref $F64)) (result i32)
    (f64.ge
      (struct.get $F64 $val (local.get $a))
      (struct.get $F64 $val (local.get $b))))

  ;; op_pow on floats — delegates to std/libm.wat:pow (faithfully-rounded
  ;; port of rust-libm). The `**` operator routes here via num.wat for
  ;; `$F64` operands and mixed Int/F64 (after Int→F64 widening).
  (import "rt/libm.wat" "pow"
    (func $libm_pow (param (ref $F64)) (param (ref $F64)) (result (ref $F64))))

  (func $op_pow (@pub)
    (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (return_call $libm_pow (local.get $a) (local.get $b)))

  ;; -- Math primitives — float arms of std/math.fnk dispatch ------------
  ;;
  ;; std/math.wat dispatches polymorphic abs/neg/min/max/etc. to these
  ;; for the $F64 arm. Each wraps a single f64.* instruction (or a short
  ;; sequence) on the unboxed payload.

  (func $abs (@pub) (param $a (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.abs (struct.get $F64 $val (local.get $a)))))

  (func $neg (@pub) (param $a (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.neg (struct.get $F64 $val (local.get $a)))))

  (func $ceil (@pub) (param $a (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.ceil (struct.get $F64 $val (local.get $a)))))

  (func $floor (@pub) (param $a (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.floor (struct.get $F64 $val (local.get $a)))))

  (func $trunc (@pub) (param $a (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.trunc (struct.get $F64 $val (local.get $a)))))

  ;; round half away from zero ("natural" rounding):
  ;; copysign(floor(abs(x) + 0.5), x).
  (func $round (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $v f64)
    (local.set $v (struct.get $F64 $val (local.get $a)))
    (struct.new $F64
      (f64.copysign
        (f64.floor (f64.add (f64.abs (local.get $v)) (f64.const 0.5)))
        (local.get $v))))

  ;; round half-to-even (banker's / IEEE 754) — wasm's native f64.nearest.
  (func $round_even (@pub) (param $a (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.nearest (struct.get $F64 $val (local.get $a)))))

  (func $sqrt (@pub) (param $a (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.sqrt (struct.get $F64 $val (local.get $a)))))

  ;; sign(x): 1.0 if x > 0, -1.0 if x < 0, 0.0 if x == 0, NaN if x is NaN.
  (func $sign (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $v f64)
    (local.set $v (struct.get $F64 $val (local.get $a)))
    (if (f64.ne (local.get $v) (local.get $v))
      (then (return (struct.new $F64 (local.get $v)))))
    (if (f64.eq (local.get $v) (f64.const 0))
      (then (return (struct.new $F64 (f64.const 0)))))
    (struct.new $F64 (f64.copysign (f64.const 1) (local.get $v))))

  ;; fract(x): x - trunc(x). Sign matches x.
  (func $fract (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $v f64)
    (local.set $v (struct.get $F64 $val (local.get $a)))
    (struct.new $F64
      (f64.sub (local.get $v) (f64.trunc (local.get $v)))))

  (func $min (@pub) (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.min
      (struct.get $F64 $val (local.get $a))
      (struct.get $F64 $val (local.get $b)))))

  (func $max (@pub) (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.max
      (struct.get $F64 $val (local.get $a))
      (struct.get $F64 $val (local.get $b)))))

  (func $copysign (@pub) (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.copysign
      (struct.get $F64 $val (local.get $a))
      (struct.get $F64 $val (local.get $b)))))

  ;; clamp(lo, x, hi): max(lo, min(hi, x)).
  (func $clamp (@pub)
    (param $lo (ref $F64)) (param $x (ref $F64)) (param $hi (ref $F64))
    (result (ref $F64))
    (struct.new $F64 (f64.max
      (struct.get $F64 $val (local.get $lo))
      (f64.min
        (struct.get $F64 $val (local.get $hi))
        (struct.get $F64 $val (local.get $x))))))


  ;; -- Formatting ------------------------------------------------------
  ;;
  ;; from_f64 is the public entry: takes a raw f64 and returns its
  ;; decimal-string spelling. Used by float.wat:fmt, decimal.wat:fmt,
  ;; and (until range fmt moves out) by str.wat for range bounds.
  ;; Handles NaN, ±Infinity, integer-fits-i32, and fractional values.

  (func $fmt (@pub) (@impl "std/str.fnk:fmt" $F64) (param $v (ref $F64)) (result (ref $Str))
    (return_call $from_f64 (struct.get $F64 $val (local.get $v))))

  ;; repr — same as fmt for floats.
  (func $repr (@pub) (@impl "std/repr.fnk:repr" $F64)
    (param $v (ref $F64)) (result (ref $Str))
    (return_call $fmt (local.get $v)))

  (func $from_f64 (@pub) (param $v f64) (result (ref $Str))
    (local $i64v i64)

    ;; NaN check — f64.ne with itself is true only for NaN.
    (if (f64.ne (local.get $v) (local.get $v))
      (then (return (call $_ascii_3
        (i32.const 0x4E) (i32.const 0x61) (i32.const 0x4E))))) ;; "NaN"

    ;; +Infinity
    (if (f64.eq (local.get $v) (f64.const inf))
      (then (return (call $_ascii_8
        (i32.const 0x49) (i32.const 0x6E) (i32.const 0x66) (i32.const 0x69)
        (i32.const 0x6E) (i32.const 0x69) (i32.const 0x74) (i32.const 0x79)))))

    ;; -Infinity
    (if (f64.eq (local.get $v) (f64.const -inf))
      (then (return (call $_ascii_9
        (i32.const 0x2D)
        (i32.const 0x49) (i32.const 0x6E) (i32.const 0x66) (i32.const 0x69)
        (i32.const 0x6E) (i32.const 0x69) (i32.const 0x74) (i32.const 0x79)))))

    ;; If the value is an integer that fits in i32, render as integer.
    (if (f64.eq (local.get $v) (f64.trunc (local.get $v)))
      (then
        (local.set $i64v (i64.trunc_sat_f64_s (local.get $v)))
        (if (i32.and
              (i64.le_s (local.get $i64v) (i64.const 2147483647))
              (i64.ge_s (local.get $i64v) (i64.const -2147483648)))
          (then
            (return (call $_fmt_i32
              (i32.wrap_i64 (local.get $i64v))))))))

    ;; Non-integer float.
    (call $_fmt_frac (local.get $v)))

  ;; Render a signed i32 as decimal digits. Private — used only by
  ;; from_f64's integer-fits fast path. (int.wat has its own native
  ;; i64-aware formatter; this one stays here because it's purpose-built
  ;; for the f64 → integer-string fast path.)
  (func $_fmt_i32 (param $v i32) (result (ref $Str))
    (local $neg i32)
    (local $abs i32)
    (local $digits i32)
    (local $tmp i32)
    (local $buf (ref $ByteArray))
    (local $pos i32)

    (if (i32.eqz (local.get $v))
      (then
        (local.set $buf (array.new $ByteArray (i32.const 0) (i32.const 1)))
        (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x30))
        (return_call $str_from_bytes (local.get $buf))))

    (local.set $neg (i32.lt_s (local.get $v) (i32.const 0)))
    (if (local.get $neg)
      (then (local.set $abs (i32.sub (i32.const 0) (local.get $v))))
      (else (local.set $abs (local.get $v))))

    (local.set $digits (i32.const 0))
    (local.set $tmp (local.get $abs))
    (block $done
      (loop $count
        (local.set $digits (i32.add (local.get $digits) (i32.const 1)))
        (local.set $tmp (i32.div_u (local.get $tmp) (i32.const 10)))
        (br_if $count (local.get $tmp))))

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
          (i32.add (i32.const 0x30) (i32.rem_u (local.get $tmp) (i32.const 10))))
        (local.set $tmp (i32.div_u (local.get $tmp) (i32.const 10)))
        (local.set $pos (i32.sub (local.get $pos) (i32.const 1)))
        (br_if $write (local.get $tmp))))

    (return_call $str_from_bytes (local.get $buf)))

  ;; Format a non-integer f64 as "int.frac" with trailing zeros stripped.
  ;; Multiplies the fractional part by 1e15, renders as i64, strips
  ;; trailing zeros. 15 digits covers f64 precision.
  (func $_fmt_frac (param $v f64) (result (ref $Str))
    (local $neg i32)
    (local $abs f64)
    (local $int_part f64)
    (local $frac f64)
    (local $frac_i64 i64)
    (local $int_buf (ref $ByteArray))
    (local $int_len i32)
    (local $frac_buf (ref $ByteArray))
    (local $frac_digits i32)
    (local $frac_len i32)
    (local $tmp i64)
    (local $buf (ref $ByteArray))
    (local $total i32)
    (local $pos i32)
    (local $i i32)

    (local.set $neg (f64.lt (local.get $v) (f64.const 0)))
    (if (local.get $neg)
      (then (local.set $abs (f64.neg (local.get $v))))
      (else (local.set $abs (local.get $v))))

    (local.set $int_part (f64.trunc (local.get $abs)))
    (local.set $frac (f64.sub (local.get $abs) (local.get $int_part)))

    (local.set $int_buf (array.new $ByteArray (i32.const 0) (i32.const 20)))
    (local.set $int_len (i32.const 0))
    (block $int_zero
      (local.set $tmp (i64.trunc_sat_f64_u (local.get $int_part)))
      (if (i64.eqz (local.get $tmp))
        (then
          (array.set $ByteArray (local.get $int_buf) (i32.const 0) (i32.const 0x30))
          (local.set $int_len (i32.const 1))
          (br $int_zero)))
      (loop $iloop
        (array.set $ByteArray (local.get $int_buf) (local.get $int_len)
          (i32.add (i32.const 0x30) (i32.wrap_i64 (i64.rem_u (local.get $tmp) (i64.const 10)))))
        (local.set $int_len (i32.add (local.get $int_len) (i32.const 1)))
        (local.set $tmp (i64.div_u (local.get $tmp) (i64.const 10)))
        (br_if $iloop (i64.ne (local.get $tmp) (i64.const 0)))))

    (local.set $frac_i64 (i64.trunc_sat_f64_u
      (f64.add
        (f64.mul (local.get $frac) (f64.const 1e15))
        (f64.const 0.5))))

    (local.set $frac_buf (array.new $ByteArray (i32.const 0) (i32.const 15)))
    (local.set $frac_digits (i32.const 15))
    (local.set $tmp (local.get $frac_i64))
    (local.set $i (i32.const 14))
    (loop $floop
      (array.set $ByteArray (local.get $frac_buf) (local.get $i)
        (i32.add (i32.const 0x30) (i32.wrap_i64 (i64.rem_u (local.get $tmp) (i64.const 10)))))
      (local.set $tmp (i64.div_u (local.get $tmp) (i64.const 10)))
      (if (local.get $i)
        (then
          (local.set $i (i32.sub (local.get $i) (i32.const 1)))
          (br $floop))))

    (local.set $frac_len (local.get $frac_digits))
    (loop $strip
      (if (i32.and
            (i32.gt_s (local.get $frac_len) (i32.const 1))
            (i32.eq
              (array.get_u $ByteArray (local.get $frac_buf)
                (i32.sub (local.get $frac_len) (i32.const 1)))
              (i32.const 0x30)))
        (then
          (local.set $frac_len (i32.sub (local.get $frac_len) (i32.const 1)))
          (br $strip))))

    (local.set $total (i32.add
      (i32.add (local.get $neg) (local.get $int_len))
      (i32.add (i32.const 1) (local.get $frac_len))))

    (local.set $buf (array.new $ByteArray (i32.const 0) (local.get $total)))
    (local.set $pos (i32.const 0))

    (if (local.get $neg)
      (then
        (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x2D))
        (local.set $pos (i32.const 1))))

    (local.set $i (i32.sub (local.get $int_len) (i32.const 1)))
    (loop $wcopy
      (array.set $ByteArray (local.get $buf) (local.get $pos)
        (array.get_u $ByteArray (local.get $int_buf) (local.get $i)))
      (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
      (if (local.get $i)
        (then
          (local.set $i (i32.sub (local.get $i) (i32.const 1)))
          (br $wcopy))))

    (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x2E))
    (local.set $pos (i32.add (local.get $pos) (i32.const 1)))

    (local.set $i (i32.const 0))
    (loop $fcopy
      (array.set $ByteArray (local.get $buf) (local.get $pos)
        (array.get_u $ByteArray (local.get $frac_buf) (local.get $i)))
      (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $fcopy (i32.lt_u (local.get $i) (local.get $frac_len))))

    (return_call $str_from_bytes (local.get $buf)))

  ;; ASCII helpers — private. Used by from_f64 for NaN / Infinity /
  ;; -Infinity literals.

  (func $_ascii_3 (param $a i32) (param $b i32) (param $c i32) (result (ref $Str))
    (local $buf (ref $ByteArray))
    (local.set $buf (array.new $ByteArray (i32.const 0) (i32.const 3)))
    (array.set $ByteArray (local.get $buf) (i32.const 0) (local.get $a))
    (array.set $ByteArray (local.get $buf) (i32.const 1) (local.get $b))
    (array.set $ByteArray (local.get $buf) (i32.const 2) (local.get $c))
    (return_call $str_from_bytes (local.get $buf)))

  (func $_ascii_8
    (param $a i32) (param $b i32) (param $c i32) (param $d i32)
    (param $e i32) (param $f i32) (param $g i32) (param $h i32)
    (result (ref $Str))
    (local $buf (ref $ByteArray))
    (local.set $buf (array.new $ByteArray (i32.const 0) (i32.const 8)))
    (array.set $ByteArray (local.get $buf) (i32.const 0) (local.get $a))
    (array.set $ByteArray (local.get $buf) (i32.const 1) (local.get $b))
    (array.set $ByteArray (local.get $buf) (i32.const 2) (local.get $c))
    (array.set $ByteArray (local.get $buf) (i32.const 3) (local.get $d))
    (array.set $ByteArray (local.get $buf) (i32.const 4) (local.get $e))
    (array.set $ByteArray (local.get $buf) (i32.const 5) (local.get $f))
    (array.set $ByteArray (local.get $buf) (i32.const 6) (local.get $g))
    (array.set $ByteArray (local.get $buf) (i32.const 7) (local.get $h))
    (return_call $str_from_bytes (local.get $buf)))

  (func $_ascii_9
    (param $a i32) (param $b i32) (param $c i32) (param $d i32)
    (param $e i32) (param $f i32) (param $g i32) (param $h i32)
    (param $i i32)
    (result (ref $Str))
    (local $buf (ref $ByteArray))
    (local.set $buf (array.new $ByteArray (i32.const 0) (i32.const 9)))
    (array.set $ByteArray (local.get $buf) (i32.const 0) (local.get $a))
    (array.set $ByteArray (local.get $buf) (i32.const 1) (local.get $b))
    (array.set $ByteArray (local.get $buf) (i32.const 2) (local.get $c))
    (array.set $ByteArray (local.get $buf) (i32.const 3) (local.get $d))
    (array.set $ByteArray (local.get $buf) (i32.const 4) (local.get $e))
    (array.set $ByteArray (local.get $buf) (i32.const 5) (local.get $f))
    (array.set $ByteArray (local.get $buf) (i32.const 6) (local.get $g))
    (array.set $ByteArray (local.get $buf) (i32.const 7) (local.get $h))
    (array.set $ByteArray (local.get $buf) (i32.const 8) (local.get $i))
    (return_call $str_from_bytes (local.get $buf)))

)
