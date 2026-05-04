;; Number — fink's boxed float type and the numeric op dispatcher.
;;
;; Small integers (-2^30..2^30-1) use i31ref directly; larger or
;; non-integer values are boxed as $Num. This file owns the $Num type
;; and the numeric arms of op_* dispatch — protocols.wat delegates here
;; for the non-collection (numeric) cases. Integer-specific primitives
;; (intdiv, bitwise, shifts, pow, divmod) still live in std/int.wat.

(module

  ;; Type imports — int.wat ops we re-route through num.wat
  (import "std/list.wat" "List" (type $List (sub any)))

  ;; Func imports — int.wat primitives. num.wat re-routes these so
  ;; protocols.wat only needs to know about num.wat for numeric ops.
  (import "std/int.wat" "op_intdiv" (func $int_op_div    (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/int.wat" "op_rem"    (func $int_op_rem    (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/int.wat" "op_intmod" (func $int_op_mod    (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/int.wat" "op_pow"    (func $int_op_pow    (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/int.wat" "op_divmod" (func $int_op_divmod (param (ref $Num)) (param (ref $Num)) (result (ref $List))))
  (import "std/int.wat" "op_rotl"   (func $int_op_rotl   (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/int.wat" "op_rotr"   (func $int_op_rotr   (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/int.wat" "op_not"    (func $int_op_not    (param (ref $Num)) (result (ref $Num))))
  (import "std/int.wat" "op_and"    (func $int_op_and    (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/int.wat" "op_or"     (func $int_op_or     (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/int.wat" "op_xor"    (func $int_op_xor    (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/int.wat" "op_shl"    (func $int_op_shl    (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))
  (import "std/int.wat" "op_shr"    (func $int_op_shr    (param (ref $Num)) (param (ref $Num)) (result (ref $Num))))

  ;; $Num — boxed float / large number.
  ;; Small integers use i31ref directly (no struct needed).
  (type $Num (@pub) (struct
    (field $val f64)
  ))


  ;; =========================================================================
  ;; Numeric op handlers — called by protocols.wat for the $Num arm of each
  ;; polymorphic op. Pure compute, no continuation plumbing.
  ;;
  ;; Float arithmetic stays inline (f64 ops); integer arithmetic delegates
  ;; to int.wat. When int/float types split, this is where dispatch will
  ;; route between them.
  ;; =========================================================================

  ;; -- Float arithmetic ------------------------------------------------

  (func $op_plus (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.add
      (struct.get $Num $val (local.get $a))
      (struct.get $Num $val (local.get $b)))))

  (func $op_minus (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.sub
      (struct.get $Num $val (local.get $a))
      (struct.get $Num $val (local.get $b)))))

  (func $op_mul (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.mul
      (struct.get $Num $val (local.get $a))
      (struct.get $Num $val (local.get $b)))))

  (func $op_div (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (struct.new $Num (f64.div
      (struct.get $Num $val (local.get $a))
      (struct.get $Num $val (local.get $b)))))

  ;; -- Comparison — return raw i32 (caller boxes via ref.i31) ---------

  (func $op_eq (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result i32)
    (f64.eq
      (struct.get $Num $val (local.get $a))
      (struct.get $Num $val (local.get $b))))

  (func $op_neq (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result i32)
    (f64.ne
      (struct.get $Num $val (local.get $a))
      (struct.get $Num $val (local.get $b))))

  (func $op_lt (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result i32)
    (f64.lt
      (struct.get $Num $val (local.get $a))
      (struct.get $Num $val (local.get $b))))

  (func $op_lte (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result i32)
    (f64.le
      (struct.get $Num $val (local.get $a))
      (struct.get $Num $val (local.get $b))))

  (func $op_gt (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result i32)
    (f64.gt
      (struct.get $Num $val (local.get $a))
      (struct.get $Num $val (local.get $b))))

  (func $op_gte (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result i32)
    (f64.ge
      (struct.get $Num $val (local.get $a))
      (struct.get $Num $val (local.get $b))))

  ;; -- Integer ops — delegate to int.wat ------------------------------

  (func $op_intdiv (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (return_call $int_op_div (local.get $a) (local.get $b)))

  (func $op_rem (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (return_call $int_op_rem (local.get $a) (local.get $b)))

  (func $op_intmod (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (return_call $int_op_mod (local.get $a) (local.get $b)))

  (func $op_pow (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (return_call $int_op_pow (local.get $a) (local.get $b)))

  (func $op_divmod (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $List))
    (return_call $int_op_divmod (local.get $a) (local.get $b)))

  (func $op_rotl (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (return_call $int_op_rotl (local.get $a) (local.get $b)))

  (func $op_rotr (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (return_call $int_op_rotr (local.get $a) (local.get $b)))

  (func $op_not (@pub)
    (param $a (ref $Num)) (result (ref $Num))
    (return_call $int_op_not (local.get $a)))

  (func $op_and (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (return_call $int_op_and (local.get $a) (local.get $b)))

  (func $op_or (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (return_call $int_op_or (local.get $a) (local.get $b)))

  (func $op_xor (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (return_call $int_op_xor (local.get $a) (local.get $b)))

  (func $op_shl (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (return_call $int_op_shl (local.get $a) (local.get $b)))

  (func $op_shr (@pub)
    (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (return_call $int_op_shr (local.get $a) (local.get $b)))


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
