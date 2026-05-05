;; Decimal type and operations.
;;
;; Storage: `(coeff i64, exp i32)`. The decimal value is `coeff * 10^exp`.
;; This is the storage shape only — no decimal arithmetic operators are
;; implemented yet. Read sites that need an f64 view (formatter,
;; conversions, hash) compute it on the fly.

(module

  ;; Type imports
  (import "std/num.wat" "Num" (type $Num (sub any) (struct)))
  (import "std/str.wat" "Str" (type $Str (sub any) (struct)))
  (import "std/float.wat" "from_f64"
    (func $float_from_f64 (param f64) (result (ref $Str))))

  (type $Decimal (@pub) (sub final $Num
    (struct (field $coeff i64) (field $exp i32))))

  ;; Read a $Decimal as f64. Used by sites that need a numeric view
  ;; (formatter, num.wat $as_f64/$as_int/$hash_i31). Computes
  ;; `coeff * 10^exp`. Loses precision past f64's mantissa — acceptable
  ;; for read paths that already worked through f64 before this storage
  ;; change. No exact decimal arithmetic is implemented; that's future
  ;; work.
  (func $_as_f64 (@pub) (param $d (ref $Decimal)) (result f64)
    (local $coeff i64)
    (local $exp i32)
    (local $f f64)
    (local $i i32)
    (local.set $coeff (struct.get $Decimal $coeff (local.get $d)))
    (local.set $exp (struct.get $Decimal $exp (local.get $d)))
    (local.set $f (f64.convert_i64_s (local.get $coeff)))

    ;; Multiply by 10 |exp| times; sign of exp picks mul-by-10 vs div-by-10.
    (if (i32.ge_s (local.get $exp) (i32.const 0))
      (then
        (local.set $i (local.get $exp))
        (block $done
          (loop $loop
            (br_if $done (i32.eqz (local.get $i)))
            (local.set $f (f64.mul (local.get $f) (f64.const 10)))
            (local.set $i (i32.sub (local.get $i) (i32.const 1)))
            (br $loop))))
      (else
        (local.set $i (i32.sub (i32.const 0) (local.get $exp)))
        (block $done
          (loop $loop
            (br_if $done (i32.eqz (local.get $i)))
            (local.set $f (f64.div (local.get $f) (f64.const 10)))
            (local.set $i (i32.sub (local.get $i) (i32.const 1)))
            (br $loop)))))

    (local.get $f))

  ;; Render a $Decimal as a string. Today: render the f64 view via
  ;; float.wat:from_f64 — same output as before the (coeff, exp) repr
  ;; landed. A real exact-decimal formatter is future work.
  (func $fmt (@pub) (param $d (ref $Decimal)) (result (ref $Str))
    (return_call $float_from_f64 (call $_as_f64 (local.get $d))))

)
