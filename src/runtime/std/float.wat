;; Float types and operations.
;;
;; Step 3c-i: $F64 primitives live here with concrete-type signatures.
;; Field is still f64 (shared $Num slot); narrowing per-subtype is a
;; follow-up. num.wat's polymorphic op_* dispatches to these for the
;; $F64 arm.

(module

  ;; Type imports
  (import "std/num.wat" "Num" (type $Num (sub any) (struct (field $val f64))))

  ;; $F64 — IEEE 754 binary64. Subtype of $Num; for now shares $Num's
  ;; `f64 $val` slot.
  (type $F64 (@pub) (sub final $Num (struct (field $val f64))))

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

  ;; -- TODO: float math primitives ------------------------------------
  ;;
  ;; `pow` (and friends `sqrt`/`log`/`exp`/trig) need either host imports
  ;; or in-wasm implementations. Wasm provides `f64.sqrt` natively but
  ;; nothing else from the math.h surface. Plan: add std/math.wat with
  ;; host-imported transcendentals once the host story is settled.
  ;;
  ;; Until then `op_pow` traps so users get a clear error rather than
  ;; silent integer truncation.

  (func $op_pow (@pub)
    (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (unreachable))

)
