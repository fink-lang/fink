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

  (func $exp (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $exp2 (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $expm1 (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $log (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $log2 (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $log10 (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $log1p (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))


  ;; -- Power / roots --------------------------------------------------

  (func $pow (@pub) (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $cbrt (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $hypot (@pub) (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (unreachable))


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

  (func $sinh (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $cosh (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $tanh (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $asinh (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $acosh (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

  (func $atanh (@pub) (param $a (ref $F64)) (result (ref $F64))
    (unreachable))

)
