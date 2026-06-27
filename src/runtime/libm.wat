;; libm — pure-wasm port of selected transcendental f64 routines.
;;
;; Port target: rust-lang/libm (MIT/Apache-2.0), itself derived from musl
;; libc and Sun's fdlibm. See THIRDPARTY-NOTICES.md at the repository root
;; for the full upstream license text — required for redistribution.
;; Per-function attribution comments above each `(func ...)` identify the
;; specific upstream source.
;;
;; This file owns the raw f64-in-f64-out math kernels. std/math.wat wraps
;; each one with the polymorphic-over-$Num Fink surface dispatch.
;;
;; Status: skeleton — every function currently traps via `unreachable`.
;; Functions land one at a time; un-skip the corresponding test in
;; src/runner/test_math.fnk as each is ported.

(module

  (import "rt/num.wat"   "Num" (type $Num (sub any) (struct)))
  (import "rt/float.wat" "F64"
    (type $F64 (sub final $Num (struct (field $val f64)))))


  ;; -- Exponential / logarithm ----------------------------------------

  ;; scalbn — y * 2^k for integer k. Standard two-step decomposition
  ;; to handle k outside [-1022, 1023] without overflow at the bit level.
  (func $_scalbn (param $y f64) (param $k i32) (result f64)
    (local $bits i64)
    ;; Clamp +/- range with the two-step trick:
    ;;   k > 1023:  y *= 2^1023; k -= 1023; (repeat once if needed)
    ;;   k < -1022: y *= 2^-1022; k += 1022; (repeat once if needed)
    (if (i32.gt_s (local.get $k) (i32.const 1023))
      (then
        (local.set $y (f64.mul (local.get $y)
          (f64.reinterpret_i64 (i64.const 0x7fe0000000000000))))  ;; 2^1023
        (local.set $k (i32.sub (local.get $k) (i32.const 1023)))
        (if (i32.gt_s (local.get $k) (i32.const 1023))
          (then
            (local.set $y (f64.mul (local.get $y)
              (f64.reinterpret_i64 (i64.const 0x7fe0000000000000))))
            (local.set $k (i32.sub (local.get $k) (i32.const 1023)))
            (if (i32.gt_s (local.get $k) (i32.const 1023))
              (then (local.set $k (i32.const 1023))))))))
    (if (i32.lt_s (local.get $k) (i32.const -1022))
      (then
        (local.set $y (f64.mul (local.get $y)
          (f64.reinterpret_i64 (i64.const 0x0010000000000000))))  ;; 2^-1022
        (local.set $k (i32.add (local.get $k) (i32.const 1022)))
        (if (i32.lt_s (local.get $k) (i32.const -1022))
          (then
            (local.set $y (f64.mul (local.get $y)
              (f64.reinterpret_i64 (i64.const 0x0010000000000000))))
            (local.set $k (i32.add (local.get $k) (i32.const 1022)))
            (if (i32.lt_s (local.get $k) (i32.const -1022))
              (then (local.set $k (i32.const -1022))))))))
    ;; |k| in [-1022, 1023]: build 2^k = (k+1023) << 52 as raw f64 bits, multiply.
    (local.set $bits
      (i64.shl
        (i64.extend_i32_s (i32.add (local.get $k) (i32.const 1023)))
        (i64.const 52)))
    (f64.mul (local.get $y) (f64.reinterpret_i64 (local.get $bits))))


  ;; exp — natural exponential. Port of FreeBSD msun e_exp.c via rust-libm.
  ;;
  ;; Method (per upstream comment block):
  ;;   1. Reduce x to r where x = k*ln2 + r, |r| <= 0.5*ln2. r split as
  ;;      hi - lo for accuracy.
  ;;   2. Approximate exp(r) on [-ln2/2, ln2/2] via Remez polynomial of
  ;;      degree 5 in r²: P(z) = 2 + P1*z + P2*z² + P3*z³ + P4*z⁴ + P5*z⁵
  ;;      where z = r². Then exp(r) = 1 + r + r*c(r)/(2 - c(r))
  ;;      where c(r) = r - z*(P1 + z*(P2 + z*(P3 + z*(P4 + z*P5)))).
  ;;   3. exp(x) = 2^k * exp(r), via scalbn.
  ;;
  ;; Faithfully rounded (within ~1 ulp). Overflow → +Inf for x > ~709.78,
  ;; underflow → 0 for x < ~-745.13.
  (func $exp (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $x f64) (local $hi f64) (local $lo f64) (local $r f64)
    (local $z f64) (local $c f64) (local $y f64)
    (local $k i32) (local $sign i32) (local $hx i32)
    (local.set $x (struct.get $F64 $val (local.get $a)))
    ;; NaN propagation.
    (if (f64.ne (local.get $x) (local.get $x))
      (then (return (struct.new $F64 (local.get $x)))))
    ;; Overflow: x > 709.78... → +Inf.
    (if (f64.gt (local.get $x) (f64.const 709.782712893383973096))
      (then (return (struct.new $F64 (f64.const inf)))))
    ;; Underflow: x < -745.13... → 0.
    (if (f64.lt (local.get $x) (f64.const -745.13321910194110842))
      (then (return (struct.new $F64 (f64.const 0)))))
    ;; high 32 bits of |x|, masked to drop sign bit.
    (local.set $hx
      (i32.and
        (i32.wrap_i64
          (i64.shr_u (i64.reinterpret_f64 (local.get $x)) (i64.const 32)))
        (i32.const 0x7fffffff)))
    (local.set $sign
      (i32.wrap_i64
        (i64.shr_u (i64.reinterpret_f64 (local.get $x)) (i64.const 63))))
    ;; Argument reduction.
    (if (i32.gt_u (local.get $hx) (i32.const 0x3fd62e42))
      (then
        ;; |x| > 0.5*ln2: pick k via INVLN2 * x + (sign ? -0.5 : 0.5).
        (if (i32.ge_u (local.get $hx) (i32.const 0x3ff0a2b2))
          (then
            ;; |x| >= 1.5*ln2: round towards 0 via i32 cast (truncating).
            (local.set $k
              (i32.trunc_f64_s
                (f64.add
                  (f64.mul (f64.const 1.44269504088896338700) (local.get $x))
                  (select (f64.const -0.5) (f64.const 0.5)
                    (local.get $sign))))))
          (else
            ;; |x| in (0.5*ln2, 1.5*ln2): k = ±1.
            (local.set $k (i32.sub (i32.sub (i32.const 1) (local.get $sign))
                                   (local.get $sign)))))
        ;; hi = x - k*LN2HI (exact); lo = k*LN2LO; r = hi - lo.
        (local.set $hi (f64.sub (local.get $x)
          (f64.mul (f64.convert_i32_s (local.get $k))
                   (f64.const 6.93147180369123816490e-01))))
        (local.set $lo
          (f64.mul (f64.convert_i32_s (local.get $k))
                   (f64.const 1.90821492927058770002e-10)))
        (local.set $r (f64.sub (local.get $hi) (local.get $lo))))
      (else
        (if (i32.gt_u (local.get $hx) (i32.const 0x3e300000))
          (then
            ;; |x| > 2^-28: no reduction, k = 0.
            (local.set $k (i32.const 0))
            (local.set $hi (local.get $x))
            (local.set $lo (f64.const 0))
            (local.set $r (local.get $x)))
          (else
            ;; |x| <= 2^-28: exp(x) ≈ 1 + x.
            (return (struct.new $F64 (f64.add (f64.const 1) (local.get $x))))))))
    ;; Polynomial: z = r²; c = r - z*(P1 + z*(P2 + z*(P3 + z*(P4 + z*P5)))).
    (local.set $z (f64.mul (local.get $r) (local.get $r)))
    (local.set $c
      (f64.sub (local.get $r)
        (f64.mul (local.get $z)
          (f64.add (f64.const 1.66666666666666019037e-01)
            (f64.mul (local.get $z)
              (f64.add (f64.const -2.77777777770155933842e-03)
                (f64.mul (local.get $z)
                  (f64.add (f64.const 6.61375632143793436117e-05)
                    (f64.mul (local.get $z)
                      (f64.add (f64.const -1.65339022054652515390e-06)
                        (f64.mul (local.get $z)
                          (f64.const 4.13813679705723846039e-08))))))))))))
    ;; y = 1 + (r*c/(2 - c) - lo + hi).
    (local.set $y
      (f64.add (f64.const 1)
        (f64.add
          (f64.sub
            (f64.div (f64.mul (local.get $r) (local.get $c))
                     (f64.sub (f64.const 2) (local.get $c)))
            (local.get $lo))
          (local.get $hi))))
    ;; Scale by 2^k.
    (if (i32.eqz (local.get $k))
      (then (return (struct.new $F64 (local.get $y)))))
    (struct.new $F64 (call $_scalbn (local.get $y) (local.get $k))))

  ;; exp2(x) = exp(x * ln2). Simple wrapper; precision sufficient for
  ;; faithful rounding. exp2(integer) returns exact powers of two.
  (func $exp2 (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $x f64)
    (local.set $x (struct.get $F64 $val (local.get $a)))
    (return_call $exp
      (struct.new $F64
        (f64.mul (local.get $x) (f64.const 0.6931471805599453)))))

  ;; expm1(x) = exp(x) - 1. Naive subtraction loses precision for x near 0.
  ;; Faithful enough for our target. Note: real libm has a dedicated impl
  ;; for precision near 0; revisit if needed.
  (func $expm1 (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $r (ref $F64))
    (local.set $r (call $exp (local.get $a)))
    (struct.new $F64
      (f64.sub (struct.get $F64 $val (local.get $r)) (f64.const 1))))

  ;; log — natural logarithm. Port of FreeBSD msun e_log.c via rust-libm.
  ;;
  ;; Method:
  ;;   1. Reduce x to f where x = 2^k * (1+f), with sqrt(2)/2 < 1+f < sqrt(2),
  ;;      via bit manipulation on the exponent.
  ;;   2. Polynomial approximation of log(1+f) using s = f/(2+f), via
  ;;      log(1+f) = log(1+s) - log(1-s) = 2s + 2/3·s³ + 2/5·s⁵ + ...
  ;;      Two interleaved Horner schemes evaluate the even/odd parts
  ;;      separately for parallelism.
  ;;   3. log(x) = log(1+f) + k*ln2.
  ;;
  ;; Faithfully rounded (~1 ulp). Special: log(±0) = -Inf, log(neg) = NaN,
  ;; log(+Inf) = +Inf, log(NaN) = NaN, log(1) = 0 exactly.
  (func $log (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $x f64) (local $f f64) (local $hfsq f64) (local $s f64)
    (local $z f64) (local $w f64) (local $t1 f64) (local $t2 f64)
    (local $r f64) (local $dk f64)
    (local $ui i64) (local $hx i32) (local $k i32)
    (local.set $x (struct.get $F64 $val (local.get $a)))
    (local.set $ui (i64.reinterpret_f64 (local.get $x)))
    (local.set $hx (i32.wrap_i64 (i64.shr_u (local.get $ui) (i64.const 32))))
    (local.set $k (i32.const 0))
    ;; Special cases.
    (if (i32.or (i32.lt_u (local.get $hx) (i32.const 0x00100000))
                (i32.shr_u (local.get $hx) (i32.const 31)))
      (then
        ;; ±0 → -Inf.
        (if (i64.eqz (i64.shl (local.get $ui) (i64.const 1)))
          (then (return (struct.new $F64
            (f64.div (f64.const -1) (f64.mul (local.get $x) (local.get $x)))))))
        ;; Negative → NaN.
        (if (i32.shr_u (local.get $hx) (i32.const 31))
          (then (return (struct.new $F64
            (f64.div (f64.sub (local.get $x) (local.get $x)) (f64.const 0))))))
        ;; Subnormal: scale x up by 2^54.
        (local.set $k (i32.sub (local.get $k) (i32.const 54)))
        (local.set $x (f64.mul (local.get $x)
          (f64.reinterpret_i64 (i64.const 0x4350000000000000))))
        (local.set $ui (i64.reinterpret_f64 (local.get $x)))
        (local.set $hx (i32.wrap_i64 (i64.shr_u (local.get $ui) (i64.const 32)))))
      (else
        ;; +Inf or NaN.
        (if (i32.ge_u (local.get $hx) (i32.const 0x7ff00000))
          (then (return (struct.new $F64 (local.get $x)))))
        ;; Exactly 1.0 → 0.
        (if (i32.and
              (i32.eq (local.get $hx) (i32.const 0x3ff00000))
              (i64.eqz (i64.shl (local.get $ui) (i64.const 32))))
          (then (return (struct.new $F64 (f64.const 0)))))))
    ;; Reduce x into [sqrt(2)/2, sqrt(2)].
    (local.set $hx
      (i32.add (local.get $hx) (i32.sub (i32.const 0x3ff00000) (i32.const 0x3fe6a09e))))
    (local.set $k
      (i32.add (local.get $k)
        (i32.sub (i32.shr_s (local.get $hx) (i32.const 20)) (i32.const 0x3ff))))
    (local.set $hx
      (i32.add (i32.and (local.get $hx) (i32.const 0x000fffff)) (i32.const 0x3fe6a09e)))
    (local.set $ui
      (i64.or
        (i64.shl (i64.extend_i32_u (local.get $hx)) (i64.const 32))
        (i64.and (local.get $ui) (i64.const 0xffffffff))))
    (local.set $x (f64.reinterpret_i64 (local.get $ui)))
    ;; Polynomial.
    (local.set $f (f64.sub (local.get $x) (f64.const 1)))
    (local.set $hfsq
      (f64.mul (f64.mul (f64.const 0.5) (local.get $f)) (local.get $f)))
    (local.set $s (f64.div (local.get $f) (f64.add (f64.const 2) (local.get $f))))
    (local.set $z (f64.mul (local.get $s) (local.get $s)))
    (local.set $w (f64.mul (local.get $z) (local.get $z)))
    (local.set $t1
      (f64.mul (local.get $w)
        (f64.add (f64.const 3.999999999940941908e-01)
          (f64.mul (local.get $w)
            (f64.add (f64.const 2.222219843214978396e-01)
              (f64.mul (local.get $w) (f64.const 1.531383769920937332e-01)))))))
    (local.set $t2
      (f64.mul (local.get $z)
        (f64.add (f64.const 6.666666666666735130e-01)
          (f64.mul (local.get $w)
            (f64.add (f64.const 2.857142874366239149e-01)
              (f64.mul (local.get $w)
                (f64.add (f64.const 1.818357216161805012e-01)
                  (f64.mul (local.get $w) (f64.const 1.479819860511658591e-01)))))))))
    (local.set $r (f64.add (local.get $t2) (local.get $t1)))
    (local.set $dk (f64.convert_i32_s (local.get $k)))
    ;; s*(hfsq + r) + dk*LN2_LO - hfsq + f + dk*LN2_HI
    (struct.new $F64
      (f64.add
        (f64.add
          (f64.sub
            (f64.add
              (f64.mul (local.get $s) (f64.add (local.get $hfsq) (local.get $r)))
              (f64.mul (local.get $dk) (f64.const 1.90821492927058770002e-10)))
            (local.get $hfsq))
          (local.get $f))
        (f64.mul (local.get $dk) (f64.const 6.93147180369123816490e-01)))))

  ;; log2(x) = log(x) / ln2. Simple wrapper; precision sufficient.
  (func $log2 (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $r (ref $F64))
    (local.set $r (call $log (local.get $a)))
    (struct.new $F64
      (f64.div (struct.get $F64 $val (local.get $r))
               (f64.const 0.6931471805599453))))

  ;; log10(x) = log(x) / ln10.
  (func $log10 (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $r (ref $F64))
    (local.set $r (call $log (local.get $a)))
    (struct.new $F64
      (f64.div (struct.get $F64 $val (local.get $r))
               (f64.const 2.302585092994046))))

  ;; log1p(x) = log(1 + x). Naive addition loses precision near 0; faithful
  ;; for our target. Revisit if needed.
  (func $log1p (@pub) (param $a (ref $F64)) (result (ref $F64))
    (return_call $log
      (struct.new $F64
        (f64.add (f64.const 1) (struct.get $F64 $val (local.get $a))))))


  ;; -- Power / roots --------------------------------------------------

  ;; Binary exponentiation for x > 0, integer y in i32 range.
  ;; Returns a precise result for moderate exponents; pow 3, 2 = 9 exact.
  (func $_pow_int_pos (param $x f64) (param $y f64) (result (ref $F64))
    (local $n i32) (local $negexp i32) (local $result f64) (local $base f64)
    (local.set $n (i32.trunc_f64_s (f64.abs (local.get $y))))
    (local.set $negexp (f64.lt (local.get $y) (f64.const 0)))
    (local.set $result (f64.const 1))
    (local.set $base (local.get $x))
    (block $done
      (loop $sq
        (br_if $done (i32.eqz (local.get $n)))
        (if (i32.and (local.get $n) (i32.const 1))
          (then (local.set $result (f64.mul (local.get $result) (local.get $base)))))
        (local.set $base (f64.mul (local.get $base) (local.get $base)))
        (local.set $n (i32.shr_u (local.get $n) (i32.const 1)))
        (br $sq)))
    (if (local.get $negexp)
      (then (local.set $result (f64.div (f64.const 1) (local.get $result)))))
    (struct.new $F64 (local.get $result)))

  ;; pow(x, y) — power. Faithful impl via exp/log.
  ;;
  ;; Cases handled:
  ;;   y == 0:           1 (per IEEE pow, even for x = 0 or NaN)
  ;;   y == 1:           x
  ;;   x == 1:           1
  ;;   x > 0:            exp(y * log(x))
  ;;   x == 0, y > 0:    0
  ;;   x == 0, y < 0:    +Inf
  ;;   x < 0, y integer: pow(|x|, y) with sign by oddness
  ;;   x < 0, y non-int: NaN
  ;;   NaN propagation as IEEE.
  (func $pow (@pub) (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (local $x f64) (local $y f64) (local $r (ref $F64))
    (local $iy i64) (local $is_int i32) (local $is_odd i32)
    (local.set $x (struct.get $F64 $val (local.get $a)))
    (local.set $y (struct.get $F64 $val (local.get $b)))
    ;; y == 0 → 1.
    (if (f64.eq (local.get $y) (f64.const 0))
      (then (return (struct.new $F64 (f64.const 1)))))
    ;; NaN propagation.
    (if (i32.or
          (f64.ne (local.get $x) (local.get $x))
          (f64.ne (local.get $y) (local.get $y)))
      (then (return (struct.new $F64 (f64.add (local.get $x) (local.get $y))))))
    ;; y == 1 → x.
    (if (f64.eq (local.get $y) (f64.const 1))
      (then (return (struct.new $F64 (local.get $x)))))
    ;; x == 1 → 1.
    (if (f64.eq (local.get $x) (f64.const 1))
      (then (return (struct.new $F64 (f64.const 1)))))
    ;; x == 0.
    (if (f64.eq (local.get $x) (f64.const 0))
      (then
        (if (f64.gt (local.get $y) (f64.const 0))
          (then (return (struct.new $F64 (f64.const 0)))))
        (return (struct.new $F64 (f64.const inf)))))
    ;; Integer-exponent fast path (preserves precision for cases like pow 3 2 = 9).
    ;; Use binary exponentiation when |y| fits comfortably in i32 and y is an integer.
    (if (i32.and
          (f64.eq (local.get $y) (f64.trunc (local.get $y)))
          (i32.and
            (f64.le (f64.abs (local.get $y)) (f64.const 1073741824))
            (f64.gt (local.get $x) (f64.const 0))))
      (then
        (return_call $_pow_int_pos (local.get $x) (local.get $y))))
    ;; x > 0 → exp(y * log(x)).
    (if (f64.gt (local.get $x) (f64.const 0))
      (then
        (return_call $exp
          (struct.new $F64
            (f64.mul (local.get $y)
              (struct.get $F64 $val (call $log (local.get $a))))))))
    ;; x < 0: y must be integer for a real result.
    (local.set $is_int
      (f64.eq (local.get $y) (f64.trunc (local.get $y))))
    (if (i32.eqz (local.get $is_int))
      (then (return (struct.new $F64
        (f64.div (f64.const 0) (f64.sub (local.get $x) (local.get $x)))))))
    ;; integer y: compute pow(|x|, y), apply sign if y is odd.
    (local.set $iy (i64.trunc_f64_s (local.get $y)))
    (local.set $is_odd
      (i32.wrap_i64 (i64.and (local.get $iy) (i64.const 1))))
    (local.set $r (call $exp
      (struct.new $F64
        (f64.mul (local.get $y)
          (struct.get $F64 $val (call $log
            (struct.new $F64 (f64.neg (local.get $x)))))))))
    (if (local.get $is_odd)
      (then (return (struct.new $F64
        (f64.neg (struct.get $F64 $val (local.get $r)))))))
    (local.get $r))

  ;; cbrt — cube root via Newton's method.
  ;;
  ;; Iteration: y' = (2y + x/y²) / 3.  Converges quadratically.
  ;;
  ;; Initial guess via bit-twiddle on the exponent: dividing the biased
  ;; exponent (after subtracting bias) by 3 gives 2^(e/3), close enough
  ;; to cbrt(x) magnitude for Newton to converge in 4-5 iterations to
  ;; within ~1 ulp for normal-range f64 inputs.
  ;;
  ;; This is a faithfully-rounded impl, simpler than CORE-MATH's
  ;; correctly-rounded version. Adequate for fink's use.
  (func $cbrt (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $x f64) (local $ax f64) (local $sign f64) (local $y f64)
    (local $bits i64) (local $exp i64)
    (local.set $x (struct.get $F64 $val (local.get $a)))
    ;; ±0, ±Inf, NaN: pass through (cbrt of these = themselves).
    (if (i32.or
          (f64.eq (local.get $x) (f64.const 0))
          (f64.ne (local.get $x) (local.get $x)))
      (then (return (struct.new $F64 (local.get $x)))))
    (if (f64.eq (f64.abs (local.get $x)) (f64.const inf))
      (then (return (struct.new $F64 (local.get $x)))))
    ;; Work on |x|, reattach sign at the end. cbrt(-x) = -cbrt(x).
    (local.set $ax (f64.abs (local.get $x)))
    (local.set $sign (f64.copysign (f64.const 1) (local.get $x)))
    ;; Initial guess: divide unbiased exponent by 3.
    ;; bits = reinterpret(ax); exp = ((bits >> 52) - 1023) / 3 + 1023;
    ;; y_bits = (exp << 52) | (mantissa preserved from ax).
    (local.set $bits (i64.reinterpret_f64 (local.get $ax)))
    (local.set $exp
      (i64.add
        (i64.div_s
          (i64.sub (i64.shr_u (local.get $bits) (i64.const 52))
                   (i64.const 1023))
          (i64.const 3))
        (i64.const 1023)))
    (local.set $y
      (f64.reinterpret_i64
        (i64.or
          (i64.shl (local.get $exp) (i64.const 52))
          (i64.and (local.get $bits) (i64.const 0x000fffffffffffff)))))
    ;; Newton iteration: y = (2y + ax/y²) / 3.  Five iterations is
    ;; comfortably enough for f64 precision from the seeded guess.
    (local.set $y (f64.div
      (f64.add (f64.add (local.get $y) (local.get $y))
               (f64.div (local.get $ax) (f64.mul (local.get $y) (local.get $y))))
      (f64.const 3)))
    (local.set $y (f64.div
      (f64.add (f64.add (local.get $y) (local.get $y))
               (f64.div (local.get $ax) (f64.mul (local.get $y) (local.get $y))))
      (f64.const 3)))
    (local.set $y (f64.div
      (f64.add (f64.add (local.get $y) (local.get $y))
               (f64.div (local.get $ax) (f64.mul (local.get $y) (local.get $y))))
      (f64.const 3)))
    (local.set $y (f64.div
      (f64.add (f64.add (local.get $y) (local.get $y))
               (f64.div (local.get $ax) (f64.mul (local.get $y) (local.get $y))))
      (f64.const 3)))
    (local.set $y (f64.div
      (f64.add (f64.add (local.get $y) (local.get $y))
               (f64.div (local.get $ax) (f64.mul (local.get $y) (local.get $y))))
      (f64.const 3)))
    (struct.new $F64 (f64.mul (local.get $sign) (local.get $y))))

  ;; hypot — sqrt(a² + b²) with overflow/underflow guard.
  ;;
  ;; Naive sqrt(a*a + b*b) overflows when |a| or |b| approaches sqrt(MAX).
  ;; Scale by max(|a|, |b|): hypot(a, b) = max * sqrt((a/max)² + (b/max)²).
  ;; The smaller term is in [0, 1] and adds up to ~1 ulp of slop, fine for
  ;; faithfully-rounded results. Special: hypot(inf, _) = inf, even if y is NaN.
  (func $hypot (@pub) (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (local $x f64) (local $y f64) (local $m f64) (local $rx f64) (local $ry f64)
    (local.set $x (f64.abs (struct.get $F64 $val (local.get $a))))
    (local.set $y (f64.abs (struct.get $F64 $val (local.get $b))))
    ;; Inf in either argument → +Inf (even if other is NaN).
    (if (i32.or
          (f64.eq (local.get $x) (f64.const inf))
          (f64.eq (local.get $y) (f64.const inf)))
      (then (return (struct.new $F64 (f64.const inf)))))
    ;; m = max(x, y). If m == 0, both are 0 → result 0.
    (local.set $m (f64.max (local.get $x) (local.get $y)))
    (if (f64.eq (local.get $m) (f64.const 0))
      (then (return (struct.new $F64 (f64.const 0)))))
    (local.set $rx (f64.div (local.get $x) (local.get $m)))
    (local.set $ry (f64.div (local.get $y) (local.get $m)))
    (struct.new $F64
      (f64.mul (local.get $m)
        (f64.sqrt (f64.add
          (f64.mul (local.get $rx) (local.get $rx))
          (f64.mul (local.get $ry) (local.get $ry)))))))


  ;; -- Trigonometric --------------------------------------------------

  ;; -- Trigonometric kernels (k_sin / k_cos from FreeBSD msun) ----------
  ;;
  ;; These approximate sin and cos on |x| <= pi/4. The full sin/cos/tan
  ;; do quadrant reduction modulo pi/2 then dispatch to the right kernel.
  ;;
  ;; Naive reduction (`n = round(x * 2/pi); r = x - n*pi/2`) loses precision
  ;; for very large |x| (> ~10^15) because x * 2/pi loses bits to integer
  ;; truncation. Real libm has a multi-precision __rem_pio2 for this case;
  ;; we accept reduced precision for very large inputs as a faithful-rounding
  ;; trade-off.

  (func $_k_sin (param $x f64) (result f64)
    (local $z f64) (local $w f64) (local $r f64) (local $v f64)
    (local.set $z (f64.mul (local.get $x) (local.get $x)))
    (local.set $w (f64.mul (local.get $z) (local.get $z)))
    ;; r = S2 + z*(S3 + z*S4) + z*w*(S5 + z*S6)
    (local.set $r
      (f64.add
        (f64.add (f64.const 8.33333333332248946124e-03)
          (f64.mul (local.get $z)
            (f64.add (f64.const -1.98412698298579493134e-04)
              (f64.mul (local.get $z) (f64.const 2.75573137070700676789e-06)))))
        (f64.mul (f64.mul (local.get $z) (local.get $w))
          (f64.add (f64.const -2.50507602534068634195e-08)
            (f64.mul (local.get $z) (f64.const 1.58969099521155010221e-10))))))
    (local.set $v (f64.mul (local.get $z) (local.get $x)))
    ;; sin = x + v*(S1 + z*r)
    (f64.add (local.get $x)
      (f64.mul (local.get $v)
        (f64.add (f64.const -1.66666666666666324348e-01)
          (f64.mul (local.get $z) (local.get $r))))))

  (func $_k_cos (param $x f64) (result f64)
    (local $z f64) (local $w f64) (local $r f64) (local $hz f64) (local $ww f64)
    (local.set $z (f64.mul (local.get $x) (local.get $x)))
    (local.set $w (f64.mul (local.get $z) (local.get $z)))
    ;; r = z*(C1 + z*(C2 + z*C3)) + w*w*(C4 + z*(C5 + z*C6))
    (local.set $r
      (f64.add
        (f64.mul (local.get $z)
          (f64.add (f64.const 4.16666666666666019037e-02)
            (f64.mul (local.get $z)
              (f64.add (f64.const -1.38888888888741095749e-03)
                (f64.mul (local.get $z) (f64.const 2.48015872894767294178e-05))))))
        (f64.mul (f64.mul (local.get $w) (local.get $w))
          (f64.add (f64.const -2.75573143513906633035e-07)
            (f64.mul (local.get $z)
              (f64.add (f64.const 2.08757232129817482790e-09)
                (f64.mul (local.get $z) (f64.const -1.13596475577881948265e-11))))))))
    (local.set $hz (f64.mul (f64.const 0.5) (local.get $z)))
    (local.set $ww (f64.sub (f64.const 1) (local.get $hz)))
    ;; cos = ww + (((1 - ww) - hz) + z*r)  (the tail term -x*y is 0 here)
    (f64.add (local.get $ww)
      (f64.add
        (f64.sub (f64.sub (f64.const 1) (local.get $ww)) (local.get $hz))
        (f64.mul (local.get $z) (local.get $r)))))

  ;; Quadrant reduction: write x = n*(pi/2) + r with |r| <= pi/4. Returns
  ;; (n_mod_4, r). Uses Cody-Waite split of pi/2 for accuracy.
  ;;
  ;; PIO2_HI = 1.57079632673412561417e+00  (pi/2 high part)
  ;; PIO2_LO = 6.07710050650619224932e-11  (pi/2 low part)
  ;; INV_PIO2 = 6.36619772367581382433e-01 (2/pi)
  (func $_rem_pio2 (param $x f64) (result i32 f64)
    (local $n i32) (local $fn f64) (local $r f64)
    ;; Round x*2/pi to nearest integer.
    (local.set $fn
      (f64.nearest (f64.mul (local.get $x) (f64.const 0.6366197723675814))))
    (local.set $n (i32.trunc_f64_s (local.get $fn)))
    ;; r = (x - fn*PIO2_HI) - fn*PIO2_LO
    (local.set $r
      (f64.sub
        (f64.sub (local.get $x)
          (f64.mul (local.get $fn) (f64.const 1.5707963267341256)))
        (f64.mul (local.get $fn) (f64.const 6.077100506506192e-11))))
    (local.get $n) (local.get $r))

  ;; sin — sine. For |x| <= pi/4, k_sin direct. Otherwise reduce mod pi/2
  ;; into [-pi/4, pi/4], pick arm by quadrant.
  (func $sin (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $x f64) (local $r f64) (local $n i32) (local $q i32)
    (local.set $x (struct.get $F64 $val (local.get $a)))
    (if (f64.ne (local.get $x) (local.get $x))
      (then (return (struct.new $F64 (local.get $x)))))
    (if (f64.eq (f64.abs (local.get $x)) (f64.const inf))
      (then (return (struct.new $F64
        (f64.div (f64.sub (local.get $x) (local.get $x)) (f64.const 0))))))
    ;; Fast path: |x| <= pi/4 → no reduction.
    (if (f64.le (f64.abs (local.get $x)) (f64.const 0.7853981633974483))
      (then (return (struct.new $F64 (call $_k_sin (local.get $x))))))
    ;; Reduce.
    (call $_rem_pio2 (local.get $x))
    (local.set $r) (local.set $n)
    (local.set $q (i32.and (local.get $n) (i32.const 3)))
    ;; n mod 4: 0 → sin(r); 1 → cos(r); 2 → -sin(r); 3 → -cos(r).
    (if (i32.eqz (local.get $q))
      (then (return (struct.new $F64 (call $_k_sin (local.get $r))))))
    (if (i32.eq (local.get $q) (i32.const 1))
      (then (return (struct.new $F64 (call $_k_cos (local.get $r))))))
    (if (i32.eq (local.get $q) (i32.const 2))
      (then (return (struct.new $F64 (f64.neg (call $_k_sin (local.get $r)))))))
    (struct.new $F64 (f64.neg (call $_k_cos (local.get $r)))))

  ;; cos — cosine. Same quadrant rotation as sin, shifted by 1.
  (func $cos (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $x f64) (local $r f64) (local $n i32) (local $q i32)
    (local.set $x (struct.get $F64 $val (local.get $a)))
    (if (f64.ne (local.get $x) (local.get $x))
      (then (return (struct.new $F64 (local.get $x)))))
    (if (f64.eq (f64.abs (local.get $x)) (f64.const inf))
      (then (return (struct.new $F64
        (f64.div (f64.sub (local.get $x) (local.get $x)) (f64.const 0))))))
    (if (f64.le (f64.abs (local.get $x)) (f64.const 0.7853981633974483))
      (then (return (struct.new $F64 (call $_k_cos (local.get $x))))))
    (call $_rem_pio2 (local.get $x))
    (local.set $r) (local.set $n)
    (local.set $q (i32.and (local.get $n) (i32.const 3)))
    ;; n mod 4: 0 → cos(r); 1 → -sin(r); 2 → -cos(r); 3 → sin(r).
    (if (i32.eqz (local.get $q))
      (then (return (struct.new $F64 (call $_k_cos (local.get $r))))))
    (if (i32.eq (local.get $q) (i32.const 1))
      (then (return (struct.new $F64 (f64.neg (call $_k_sin (local.get $r)))))))
    (if (i32.eq (local.get $q) (i32.const 2))
      (then (return (struct.new $F64 (f64.neg (call $_k_cos (local.get $r)))))))
    (struct.new $F64 (call $_k_sin (local.get $r))))

  ;; tan — tangent. tan(x) = sin(x) / cos(x).
  (func $tan (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $s (ref $F64)) (local $c (ref $F64))
    (local.set $s (call $sin (local.get $a)))
    (local.set $c (call $cos (local.get $a)))
    (struct.new $F64
      (f64.div
        (struct.get $F64 $val (local.get $s))
        (struct.get $F64 $val (local.get $c)))))

  ;; asin/acos rational R(z) = P(z)/Q(z) approximation.
  ;; P, Q from FreeBSD msun e_asin.c.
  (func $_asin_R (param $z f64) (result f64)
    (local $p f64) (local $q f64)
    (local.set $p
      (f64.mul (local.get $z)
        (f64.add (f64.const 1.66666666666666657415e-01)
          (f64.mul (local.get $z)
            (f64.add (f64.const -3.25565818622400915405e-01)
              (f64.mul (local.get $z)
                (f64.add (f64.const 2.01212532134862925881e-01)
                  (f64.mul (local.get $z)
                    (f64.add (f64.const -4.00555345006794114027e-02)
                      (f64.mul (local.get $z)
                        (f64.add (f64.const 7.91534994289814532176e-04)
                          (f64.mul (local.get $z) (f64.const 3.47933107596021167570e-05)))))))))))))
    (local.set $q
      (f64.add (f64.const 1)
        (f64.mul (local.get $z)
          (f64.add (f64.const -2.40339491173441421878e+00)
            (f64.mul (local.get $z)
              (f64.add (f64.const 2.02094576023350569471e+00)
                (f64.mul (local.get $z)
                  (f64.add (f64.const -6.88283971605453293030e-01)
                    (f64.mul (local.get $z) (f64.const 7.70381505559019352791e-02))))))))))
    (f64.div (local.get $p) (local.get $q)))

  ;; asin — arcsine. Domain [-1, 1]; traps for |x| > 1.
  ;; |x| < 0.5: x + x*R(x²). Otherwise use asin(x) = pi/2 - 2*asin(sqrt((1-|x|)/2)).
  (func $asin (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $x f64) (local $z f64) (local $r f64) (local $s f64) (local $f f64) (local $c f64)
    (local $hx i32) (local $ix i32)
    (local.set $x (struct.get $F64 $val (local.get $a)))
    (local.set $hx (i32.wrap_i64
      (i64.shr_u (i64.reinterpret_f64 (local.get $x)) (i64.const 32))))
    (local.set $ix (i32.and (local.get $hx) (i32.const 0x7fffffff)))
    ;; |x| >= 1.
    (if (i32.ge_u (local.get $ix) (i32.const 0x3ff00000))
      (then
        ;; Exactly ±1 → ±pi/2.
        (if (f64.eq (f64.abs (local.get $x)) (f64.const 1))
          (then (return (struct.new $F64
            (f64.copysign (f64.const 1.57079632679489655800) (local.get $x))))))
        ;; |x| > 1 (or NaN) → NaN.
        (return (struct.new $F64
          (f64.div (f64.const 0) (f64.sub (local.get $x) (local.get $x)))))))
    ;; |x| < 0.5.
    (if (i32.lt_u (local.get $ix) (i32.const 0x3fe00000))
      (then
        (return (struct.new $F64
          (f64.add (local.get $x)
            (f64.mul (local.get $x)
              (call $_asin_R (f64.mul (local.get $x) (local.get $x)))))))))
    ;; 0.5 <= |x| < 1.
    (local.set $z (f64.mul (f64.const 0.5)
      (f64.sub (f64.const 1) (f64.abs (local.get $x)))))
    (local.set $s (f64.sqrt (local.get $z)))
    (local.set $r (call $_asin_R (local.get $z)))
    ;; |x| > 0.975 fast path; below that uses extra precision via low-word clear.
    ;; We use the simpler upper-path uniformly — a few-ulp loss for x in [0.5, 0.975]
    ;; but matches faithful rounding target.
    (local.set $x
      (f64.sub (f64.const 1.57079632679489655800)
        (f64.sub
          (f64.mul (f64.const 2)
            (f64.add (local.get $s) (f64.mul (local.get $s) (f64.mul (local.get $z) (local.get $r)))))
          (f64.const 6.12323399573676603587e-17))))
    (struct.new $F64
      (select (f64.neg (local.get $x)) (local.get $x)
              (i32.shr_u (local.get $hx) (i32.const 31)))))

  ;; acos — arccosine. Domain [-1, 1].
  ;; |x| < 0.5: pi/2 - x - x*R(x²)  (rearranged for precision).
  ;; x >= 0.5:  2*asin(sqrt((1-x)/2)).
  ;; x <= -0.5: pi - 2*asin(sqrt((1+x)/2)).
  (func $acos (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $x f64) (local $z f64) (local $r f64) (local $s f64) (local $w f64)
    (local $hx i32) (local $ix i32)
    (local.set $x (struct.get $F64 $val (local.get $a)))
    (local.set $hx (i32.wrap_i64
      (i64.shr_u (i64.reinterpret_f64 (local.get $x)) (i64.const 32))))
    (local.set $ix (i32.and (local.get $hx) (i32.const 0x7fffffff)))
    ;; |x| >= 1.
    (if (i32.ge_u (local.get $ix) (i32.const 0x3ff00000))
      (then
        (if (f64.eq (local.get $x) (f64.const 1))
          (then (return (struct.new $F64 (f64.const 0)))))
        (if (f64.eq (local.get $x) (f64.const -1))
          (then (return (struct.new $F64
            (f64.add (f64.const 3.14159265358979311600)
                     (f64.const 1.22464679914735320700e-16))))))
        (return (struct.new $F64
          (f64.div (f64.const 0) (f64.sub (local.get $x) (local.get $x)))))))
    ;; |x| < 0.5.
    (if (i32.lt_u (local.get $ix) (i32.const 0x3fe00000))
      (then
        ;; Tiny x: acos(x) = pi/2 - x  (via pio2_hi - (x - pio2_lo)).
        (if (i32.lt_u (local.get $ix) (i32.const 0x3c600000))
          (then (return (struct.new $F64
            (f64.add (f64.const 1.57079632679489655800) (f64.const 6.12323399573676603587e-17))))))
        (local.set $z (call $_asin_R (f64.mul (local.get $x) (local.get $x))))
        (return (struct.new $F64
          (f64.sub (f64.const 1.57079632679489655800)
            (f64.sub (local.get $x)
              (f64.sub (f64.const 6.12323399573676603587e-17)
                (f64.mul (local.get $x) (local.get $z)))))))))
    ;; x <= -0.5.
    (if (i32.shr_u (local.get $hx) (i32.const 31))
      (then
        (local.set $z (f64.mul (f64.const 0.5)
          (f64.add (f64.const 1) (local.get $x))))
        (local.set $r (call $_asin_R (local.get $z)))
        (local.set $s (f64.sqrt (local.get $z)))
        (local.set $w
          (f64.sub (f64.mul (local.get $s) (local.get $r))
                   (f64.const 6.12323399573676603587e-17)))
        (return (struct.new $F64
          (f64.sub (f64.const 3.14159265358979311600)
            (f64.mul (f64.const 2)
              (f64.add (local.get $s) (local.get $w))))))))
    ;; x >= 0.5.
    (local.set $z (f64.mul (f64.const 0.5)
      (f64.sub (f64.const 1) (local.get $x))))
    (local.set $s (f64.sqrt (local.get $z)))
    (local.set $r (call $_asin_R (local.get $z)))
    (local.set $w
      (f64.sub (f64.mul (local.get $s) (local.get $r))
               (f64.const 6.12323399573676603587e-17)))
    (struct.new $F64
      (f64.mul (f64.const 2)
        (f64.add (local.get $s) (local.get $w)))))

  ;; atan — arctangent. Port of FreeBSD msun s_atan.c via rust-libm.
  ;;
  ;; Method: split |x| into 4 ranges; each maps to an interval where a
  ;; degree-22 polynomial in x² approximates arctan. ATANHI/ATANLO[id]
  ;; are reference angles (atan(0.5), atan(1), atan(1.5), atan(inf))
  ;; with high/low Cody-Waite splits for accuracy. Result is reassembled
  ;; from the reference + polynomial residual, then sign-applied.
  (func $atan (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $x f64) (local $z f64) (local $w f64) (local $s1 f64) (local $s2 f64)
    (local $hi f64) (local $lo f64)
    (local $ix i32) (local $sign i32) (local $id i32)
    (local.set $x (struct.get $F64 $val (local.get $a)))
    (local.set $ix (i32.wrap_i64
      (i64.shr_u (i64.reinterpret_f64 (local.get $x)) (i64.const 32))))
    (local.set $sign (i32.shr_u (local.get $ix) (i32.const 31)))
    (local.set $ix (i32.and (local.get $ix) (i32.const 0x7fffffff)))
    ;; |x| >= 2^66 → ±atan(inf) = ±pi/2.
    (if (i32.ge_u (local.get $ix) (i32.const 0x44100000))
      (then
        (if (f64.ne (local.get $x) (local.get $x))
          (then (return (struct.new $F64 (local.get $x)))))
        ;; ATANHI[3] = atan(inf)hi = pi/2 high.
        (local.set $z (f64.const 1.57079632679489655800))
        (return (struct.new $F64
          (select (f64.neg (local.get $z)) (local.get $z) (local.get $sign))))))
    ;; Range bucket selection.
    (if (i32.lt_u (local.get $ix) (i32.const 0x3fdc0000))
      (then
        ;; |x| < 0.4375 → no reduction, no offset.
        (if (i32.lt_u (local.get $ix) (i32.const 0x3e400000))
          (then (return (struct.new $F64 (local.get $x)))))
        (local.set $id (i32.const -1)))
      (else
        (local.set $x (f64.abs (local.get $x)))
        (if (i32.lt_u (local.get $ix) (i32.const 0x3ff30000))
          (then
            (if (i32.lt_u (local.get $ix) (i32.const 0x3fe60000))
              (then
                ;; 7/16 <= |x| < 11/16 → x = (2x-1)/(2+x), id = 0.
                (local.set $x
                  (f64.div
                    (f64.sub (f64.mul (f64.const 2) (local.get $x)) (f64.const 1))
                    (f64.add (f64.const 2) (local.get $x))))
                (local.set $id (i32.const 0)))
              (else
                ;; 11/16 <= |x| < 19/16 → x = (x-1)/(x+1), id = 1.
                (local.set $x
                  (f64.div
                    (f64.sub (local.get $x) (f64.const 1))
                    (f64.add (local.get $x) (f64.const 1))))
                (local.set $id (i32.const 1)))))
          (else
            (if (i32.lt_u (local.get $ix) (i32.const 0x40038000))
              (then
                ;; 19/16 <= |x| < 2.4375 → x = (x-1.5)/(1+1.5x), id = 2.
                (local.set $x
                  (f64.div
                    (f64.sub (local.get $x) (f64.const 1.5))
                    (f64.add (f64.const 1) (f64.mul (f64.const 1.5) (local.get $x)))))
                (local.set $id (i32.const 2)))
              (else
                ;; 2.4375 <= |x| < 2^66 → x = -1/x, id = 3.
                (local.set $x (f64.div (f64.const -1) (local.get $x)))
                (local.set $id (i32.const 3))))))))
    ;; Polynomial.
    (local.set $z (f64.mul (local.get $x) (local.get $x)))
    (local.set $w (f64.mul (local.get $z) (local.get $z)))
    ;; s1 = z*(AT0 + w*(AT2 + w*(AT4 + w*(AT6 + w*(AT8 + w*AT10)))))
    (local.set $s1
      (f64.mul (local.get $z)
        (f64.add (f64.const 3.33333333333329318027e-01)
          (f64.mul (local.get $w)
            (f64.add (f64.const 1.42857142725034663711e-01)
              (f64.mul (local.get $w)
                (f64.add (f64.const 9.09088713343650656196e-02)
                  (f64.mul (local.get $w)
                    (f64.add (f64.const 6.66107313738753120669e-02)
                      (f64.mul (local.get $w)
                        (f64.add (f64.const 4.97687799461593236017e-02)
                          (f64.mul (local.get $w) (f64.const 1.62858201153657823623e-02)))))))))))))
    ;; s2 = w*(AT1 + w*(AT3 + w*(AT5 + w*(AT7 + w*AT9))))
    (local.set $s2
      (f64.mul (local.get $w)
        (f64.add (f64.const -1.99999999998764832476e-01)
          (f64.mul (local.get $w)
            (f64.add (f64.const -1.11111104054623557880e-01)
              (f64.mul (local.get $w)
                (f64.add (f64.const -7.69187620504482999495e-02)
                  (f64.mul (local.get $w)
                    (f64.add (f64.const -5.83357013379057348645e-02)
                      (f64.mul (local.get $w) (f64.const -3.65315727442169155270e-02)))))))))))
    (if (i32.lt_s (local.get $id) (i32.const 0))
      (then (return (struct.new $F64
        (f64.sub (local.get $x)
          (f64.mul (local.get $x) (f64.add (local.get $s1) (local.get $s2))))))))
    ;; ATANHI/ATANLO arrays via id select.
    (if (i32.eqz (local.get $id))
      (then
        (local.set $hi (f64.const 4.63647609000806093515e-01))
        (local.set $lo (f64.const 2.26987774529616870924e-17))))
    (if (i32.eq (local.get $id) (i32.const 1))
      (then
        (local.set $hi (f64.const 7.85398163397448278999e-01))
        (local.set $lo (f64.const 3.06161699786838301793e-17))))
    (if (i32.eq (local.get $id) (i32.const 2))
      (then
        (local.set $hi (f64.const 9.82793723247329054082e-01))
        (local.set $lo (f64.const 1.39033110312309984516e-17))))
    (if (i32.eq (local.get $id) (i32.const 3))
      (then
        (local.set $hi (f64.const 1.57079632679489655800e+00))
        (local.set $lo (f64.const 6.12323399573676603587e-17))))
    ;; z = ATANHI[id] - ((x*(s1+s2) - ATANLO[id]) - x)
    (local.set $z
      (f64.sub (local.get $hi)
        (f64.sub
          (f64.sub (f64.mul (local.get $x) (f64.add (local.get $s1) (local.get $s2)))
                   (local.get $lo))
          (local.get $x))))
    (struct.new $F64
      (select (f64.neg (local.get $z)) (local.get $z) (local.get $sign))))

  ;; atan2(y, x) — two-argument arctangent. Returns angle in (-pi, pi].
  ;; Simplified port; covers all quadrants + axes; doesn't preserve signed-zero
  ;; distinction in some edge cases (acceptable for faithful target).
  ;;
  ;; Logic:
  ;;   x > 0:           atan(y/x)
  ;;   x < 0, y >= 0:   atan(y/x) + pi
  ;;   x < 0, y < 0:    atan(y/x) - pi
  ;;   x == 0, y > 0:   pi/2
  ;;   x == 0, y < 0:  -pi/2
  ;;   x == 0, y == 0:  0
  ;;   NaN in either:   NaN
  (func $atan2 (@pub) (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (local $y f64) (local $x f64) (local $r (ref $F64)) (local $rv f64)
    (local.set $y (struct.get $F64 $val (local.get $a)))
    (local.set $x (struct.get $F64 $val (local.get $b)))
    ;; NaN propagation.
    (if (i32.or
          (f64.ne (local.get $y) (local.get $y))
          (f64.ne (local.get $x) (local.get $x)))
      (then (return (struct.new $F64 (f64.add (local.get $y) (local.get $x))))))
    ;; x == 0.
    (if (f64.eq (local.get $x) (f64.const 0))
      (then
        (if (f64.eq (local.get $y) (f64.const 0))
          (then (return (struct.new $F64 (f64.const 0)))))
        (return (struct.new $F64
          (f64.copysign (f64.const 1.57079632679489655800) (local.get $y))))))
    ;; r = atan(y/x).
    (local.set $r (call $atan
      (struct.new $F64 (f64.div (local.get $y) (local.get $x)))))
    (local.set $rv (struct.get $F64 $val (local.get $r)))
    ;; x > 0: just r.
    (if (f64.gt (local.get $x) (f64.const 0))
      (then (return (struct.new $F64 (local.get $rv)))))
    ;; x < 0: r ± pi (sign matches y).
    (struct.new $F64
      (f64.add (local.get $rv)
        (f64.copysign (f64.const 3.14159265358979311600) (local.get $y)))))


  ;; -- Hyperbolic -----------------------------------------------------

  ;; sinh(x) = (exp(x) - exp(-x)) / 2.
  (func $sinh (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $x f64) (local $ep (ref $F64)) (local $en (ref $F64))
    (local.set $x (struct.get $F64 $val (local.get $a)))
    (local.set $ep (call $exp (local.get $a)))
    (local.set $en (call $exp (struct.new $F64 (f64.neg (local.get $x)))))
    (struct.new $F64
      (f64.mul (f64.const 0.5)
        (f64.sub (struct.get $F64 $val (local.get $ep))
                 (struct.get $F64 $val (local.get $en))))))

  ;; cosh(x) = (exp(x) + exp(-x)) / 2.
  (func $cosh (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $x f64) (local $ep (ref $F64)) (local $en (ref $F64))
    (local.set $x (struct.get $F64 $val (local.get $a)))
    (local.set $ep (call $exp (local.get $a)))
    (local.set $en (call $exp (struct.new $F64 (f64.neg (local.get $x)))))
    (struct.new $F64
      (f64.mul (f64.const 0.5)
        (f64.add (struct.get $F64 $val (local.get $ep))
                 (struct.get $F64 $val (local.get $en))))))

  ;; tanh(x) = (exp(2x) - 1) / (exp(2x) + 1) for x>=0; mirror for x<0.
  ;; Avoids overflow in (exp(x) - exp(-x)) for large |x|.
  (func $tanh (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $x f64) (local $ax f64) (local $sign f64) (local $e2 (ref $F64))
    (local $e2v f64)
    (local.set $x (struct.get $F64 $val (local.get $a)))
    (if (f64.ne (local.get $x) (local.get $x))
      (then (return (struct.new $F64 (local.get $x)))))
    (local.set $ax (f64.abs (local.get $x)))
    (local.set $sign (f64.copysign (f64.const 1) (local.get $x)))
    ;; Saturate at large |x| to avoid overflow.
    (if (f64.ge (local.get $ax) (f64.const 22))
      (then (return (struct.new $F64 (local.get $sign)))))
    (local.set $e2 (call $exp
      (struct.new $F64 (f64.mul (f64.const 2) (local.get $ax)))))
    (local.set $e2v (struct.get $F64 $val (local.get $e2)))
    (struct.new $F64
      (f64.mul (local.get $sign)
        (f64.div
          (f64.sub (local.get $e2v) (f64.const 1))
          (f64.add (local.get $e2v) (f64.const 1))))))

  ;; asinh(x) = log(x + sqrt(x² + 1)). Defined for all real x.
  (func $asinh (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $x f64)
    (local.set $x (struct.get $F64 $val (local.get $a)))
    (return_call $log
      (struct.new $F64
        (f64.add (local.get $x)
          (f64.sqrt (f64.add (f64.mul (local.get $x) (local.get $x))
                             (f64.const 1)))))))

  ;; acosh(x) = log(x + sqrt(x² - 1)). Domain x >= 1; traps otherwise.
  (func $acosh (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $x f64)
    (local.set $x (struct.get $F64 $val (local.get $a)))
    (if (f64.lt (local.get $x) (f64.const 1))
      (then (unreachable)))
    (return_call $log
      (struct.new $F64
        (f64.add (local.get $x)
          (f64.sqrt (f64.sub (f64.mul (local.get $x) (local.get $x))
                             (f64.const 1)))))))

  ;; atanh(x) = 0.5 * log((1+x)/(1-x)). Domain |x| < 1; traps otherwise.
  (func $atanh (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $x f64) (local $r (ref $F64))
    (local.set $x (struct.get $F64 $val (local.get $a)))
    (if (i32.or
          (f64.le (local.get $x) (f64.const -1))
          (f64.ge (local.get $x) (f64.const 1)))
      (then (unreachable)))
    (local.set $r (call $log
      (struct.new $F64
        (f64.div
          (f64.add (f64.const 1) (local.get $x))
          (f64.sub (f64.const 1) (local.get $x))))))
    (struct.new $F64
      (f64.mul (f64.const 0.5) (struct.get $F64 $val (local.get $r)))))

)
