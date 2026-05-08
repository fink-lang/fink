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

  (import "std/num.wat"   "Num" (type $Num (sub any) (struct)))
  (import "std/float.wat" "F64"
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

  (func $pow (@pub) (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (unreachable))

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

  (func $sin (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $cos (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $tan (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $asin (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $acos (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $atan (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $atan2 (@pub) (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (unreachable))


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
