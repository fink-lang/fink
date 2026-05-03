;; Number — fink's boxed float type.
;;
;; Small integers (-2^30..2^30-1) use i31ref directly; larger or
;; non-integer values are boxed as $Num. Numeric operations live in
;; std/int.wat (integer-specific) and rt/protocols.wat (polymorphic
;; dispatchers). This file owns the type itself.

(module

  ;; $Num — boxed float / large number.
  ;; Small integers use i31ref directly (no struct needed).
  (type $Num (@pub) (struct
    (field $val f64)
  ))


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
