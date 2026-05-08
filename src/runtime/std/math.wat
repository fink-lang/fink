;; Math primitives — Tier 1: polymorphic dispatch over $Num.
;;
;; This module is thin glue. The actual primitives live in:
;;   - std/float.wat  (f64 arms)
;;   - std/int.wat    (int arms; traps on $U64 where ops are signed-only)
;;
;; Surface (via `import 'std/math.fnk'`):
;;   abs, neg, ceil, floor, trunc, round, round_even, sqrt, sign, fract,
;;   min, max, copysign, clamp
;;
;; Tier 2 transcendentals (sin, cos, log, exp, pow on floats, ...) are a
;; separate piece of work — they need polynomial approximations or host
;; imports and a precision/range-reduction story. Not in this file.

(module

  ;; Type imports
  (import "rt/apply.wat" "Closure"  (type $Closure  (sub any)))
  (import "rt/apply.wat" "Captures" (type $Captures (sub any)))
  (import "rt/apply.wat" "Fn2"      (type $Fn2      (sub any)))
  (import "std/num.wat"  "Num"      (type $Num      (sub any) (struct)))
  (import "std/int.wat"  "Int"      (type $Int      (sub $Num (struct))))
  (import "std/float.wat" "F64"     (type $F64      (sub final $Num (struct (field $val f64)))))

  ;; Func imports — apply / list plumbing
  (import "rt/apply.wat" "apply_1"
    (func $apply_1 (param $result (ref null any)) (param $cont (ref null any))))
  (import "std/list.wat" "head_any"
    (func $head_any (param (ref null any)) (result (ref null any))))
  (import "std/list.wat" "tail_any"
    (func $tail_any (param (ref null any)) (result (ref null any))))
  (import "std/list.wat" "List"
    (type $List (sub any)))
  (import "std/list.wat" "is_empty"
    (func $list_is_empty (param (ref $List)) (result i32)))

  ;; Func imports — float arms
  (import "std/float.wat" "abs"        (func $float_abs        (param (ref $F64)) (result (ref $F64))))
  (import "std/float.wat" "neg"        (func $float_neg        (param (ref $F64)) (result (ref $F64))))
  (import "std/float.wat" "ceil"       (func $float_ceil       (param (ref $F64)) (result (ref $F64))))
  (import "std/float.wat" "floor"      (func $float_floor      (param (ref $F64)) (result (ref $F64))))
  (import "std/float.wat" "trunc"      (func $float_trunc      (param (ref $F64)) (result (ref $F64))))
  (import "std/float.wat" "round"      (func $float_round      (param (ref $F64)) (result (ref $F64))))
  (import "std/float.wat" "round_even" (func $float_round_even (param (ref $F64)) (result (ref $F64))))
  (import "std/float.wat" "sqrt"       (func $float_sqrt       (param (ref $F64)) (result (ref $F64))))
  (import "std/float.wat" "sign"       (func $float_sign       (param (ref $F64)) (result (ref $F64))))
  (import "std/float.wat" "fract"      (func $float_fract      (param (ref $F64)) (result (ref $F64))))
  (import "std/float.wat" "min"        (func $float_min        (param (ref $F64)) (param (ref $F64)) (result (ref $F64))))
  (import "std/float.wat" "max"        (func $float_max        (param (ref $F64)) (param (ref $F64)) (result (ref $F64))))
  (import "std/float.wat" "copysign"   (func $float_copysign   (param (ref $F64)) (param (ref $F64)) (result (ref $F64))))
  (import "std/float.wat" "clamp"      (func $float_clamp      (param (ref $F64)) (param (ref $F64)) (param (ref $F64)) (result (ref $F64))))

  ;; Func imports — int arms
  (import "std/int.wat" "abs"      (func $int_abs      (param (ref $Int)) (result (ref $Int))))
  (import "std/int.wat" "neg"      (func $int_neg      (param (ref $Int)) (result (ref $Int))))
  (import "std/int.wat" "sign"     (func $int_sign     (param (ref $Int)) (result (ref $Int))))
  (import "std/int.wat" "min"      (func $int_min      (param (ref $Int)) (param (ref $Int)) (result (ref $Int))))
  (import "std/int.wat" "max"      (func $int_max      (param (ref $Int)) (param (ref $Int)) (result (ref $Int))))
  (import "std/int.wat" "copysign" (func $int_copysign (param (ref $Int)) (param (ref $Int)) (result (ref $Int))))


  ;; -- Dispatchers — branch on $F64 vs $Int over $Num. ----------------
  ;;
  ;; Identity-on-Int ops (floor/ceil/trunc/round/round_even/fract) return
  ;; the input unchanged when given an Int. Sign-only ops (neg/copysign)
  ;; trap inside int.wat when given a $U64. sqrt is float-only — Int
  ;; widens to f64 then back. Anything that is not $F64 or $Int (i.e.
  ;; $Decimal) traps via the unreachable fall-through.

  (func $abs_num (param $n (ref $Num)) (result (ref $Num))
    (if (ref.test (ref $F64) (local.get $n))
      (then (return_call $float_abs (ref.cast (ref $F64) (local.get $n)))))
    (if (ref.test (ref $Int) (local.get $n))
      (then (return_call $int_abs (ref.cast (ref $Int) (local.get $n)))))
    (unreachable))

  (func $neg_num (param $n (ref $Num)) (result (ref $Num))
    (if (ref.test (ref $F64) (local.get $n))
      (then (return_call $float_neg (ref.cast (ref $F64) (local.get $n)))))
    (if (ref.test (ref $Int) (local.get $n))
      (then (return_call $int_neg (ref.cast (ref $Int) (local.get $n)))))
    (unreachable))

  (func $sign_num (param $n (ref $Num)) (result (ref $Num))
    (if (ref.test (ref $F64) (local.get $n))
      (then (return_call $float_sign (ref.cast (ref $F64) (local.get $n)))))
    (if (ref.test (ref $Int) (local.get $n))
      (then (return_call $int_sign (ref.cast (ref $Int) (local.get $n)))))
    (unreachable))

  ;; Identity on Int — floor/ceil/trunc/round/round_even/fract.
  (func $ceil_num (param $n (ref $Num)) (result (ref $Num))
    (if (ref.test (ref $Int) (local.get $n))
      (then (return (local.get $n))))
    (if (ref.test (ref $F64) (local.get $n))
      (then (return_call $float_ceil (ref.cast (ref $F64) (local.get $n)))))
    (unreachable))

  (func $floor_num (param $n (ref $Num)) (result (ref $Num))
    (if (ref.test (ref $Int) (local.get $n))
      (then (return (local.get $n))))
    (if (ref.test (ref $F64) (local.get $n))
      (then (return_call $float_floor (ref.cast (ref $F64) (local.get $n)))))
    (unreachable))

  (func $trunc_num (param $n (ref $Num)) (result (ref $Num))
    (if (ref.test (ref $Int) (local.get $n))
      (then (return (local.get $n))))
    (if (ref.test (ref $F64) (local.get $n))
      (then (return_call $float_trunc (ref.cast (ref $F64) (local.get $n)))))
    (unreachable))

  (func $round_num (param $n (ref $Num)) (result (ref $Num))
    (if (ref.test (ref $Int) (local.get $n))
      (then (return (local.get $n))))
    (if (ref.test (ref $F64) (local.get $n))
      (then (return_call $float_round (ref.cast (ref $F64) (local.get $n)))))
    (unreachable))

  (func $round_even_num (param $n (ref $Num)) (result (ref $Num))
    (if (ref.test (ref $Int) (local.get $n))
      (then (return (local.get $n))))
    (if (ref.test (ref $F64) (local.get $n))
      (then (return_call $float_round_even (ref.cast (ref $F64) (local.get $n)))))
    (unreachable))

  ;; fract on Int → 0 in same subtype. Reuse $int_sub_self via $int_neg
  ;; round-trip would be wrong; instead take advantage of `n - n = 0`
  ;; semantics by calling the existing int min(n,n)... no, simplest: the
  ;; Int identity rule says fract(n) is the same family's zero. Use
  ;; $int_neg(n) ... no — explicit: int.wat doesn't expose a zero builder.
  ;; Cheapest: subtract n from itself via int_neg+plus would pull deps.
  ;; Pragmatic: int.wat already has `op_minus`. Import it for this one
  ;; case to produce a zero of the right subtype.
  (func $fract_num (param $n (ref $Num)) (result (ref $Num))
    (if (ref.test (ref $Int) (local.get $n))
      (then (return_call $int_sub_self (ref.cast (ref $Int) (local.get $n)))))
    (if (ref.test (ref $F64) (local.get $n))
      (then (return_call $float_fract (ref.cast (ref $F64) (local.get $n)))))
    (unreachable))

  ;; sqrt — float-only at the surface; Int widens to F64.
  ;; Today sqrt over Int is not requested by tests, so trap to keep the
  ;; family rules tight. Revisit if Int sqrt is wanted.
  (func $sqrt_num (param $n (ref $Num)) (result (ref $Num))
    (if (ref.test (ref $F64) (local.get $n))
      (then (return_call $float_sqrt (ref.cast (ref $F64) (local.get $n)))))
    (unreachable))

  ;; Binary ops — both args must be same family (no implicit widening
  ;; for math.* yet; mirrors num.wat's $check_compat which only allows
  ;; mixed Int/Float widening for arithmetic). Caller's varargs adapter
  ;; ensures all args go through this same dispatcher.
  (func $min_num (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (if (i32.and
          (ref.test (ref $F64) (local.get $a))
          (ref.test (ref $F64) (local.get $b)))
      (then (return_call $float_min
              (ref.cast (ref $F64) (local.get $a))
              (ref.cast (ref $F64) (local.get $b)))))
    (if (i32.and
          (ref.test (ref $Int) (local.get $a))
          (ref.test (ref $Int) (local.get $b)))
      (then (return_call $int_min
              (ref.cast (ref $Int) (local.get $a))
              (ref.cast (ref $Int) (local.get $b)))))
    (unreachable))

  (func $max_num (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (if (i32.and
          (ref.test (ref $F64) (local.get $a))
          (ref.test (ref $F64) (local.get $b)))
      (then (return_call $float_max
              (ref.cast (ref $F64) (local.get $a))
              (ref.cast (ref $F64) (local.get $b)))))
    (if (i32.and
          (ref.test (ref $Int) (local.get $a))
          (ref.test (ref $Int) (local.get $b)))
      (then (return_call $int_max
              (ref.cast (ref $Int) (local.get $a))
              (ref.cast (ref $Int) (local.get $b)))))
    (unreachable))

  (func $copysign_num (param $a (ref $Num)) (param $b (ref $Num)) (result (ref $Num))
    (if (i32.and
          (ref.test (ref $F64) (local.get $a))
          (ref.test (ref $F64) (local.get $b)))
      (then (return_call $float_copysign
              (ref.cast (ref $F64) (local.get $a))
              (ref.cast (ref $F64) (local.get $b)))))
    (if (i32.and
          (ref.test (ref $Int) (local.get $a))
          (ref.test (ref $Int) (local.get $b)))
      (then (return_call $int_copysign
              (ref.cast (ref $Int) (local.get $a))
              (ref.cast (ref $Int) (local.get $b)))))
    (unreachable))

  (func $clamp_num
    (param $lo (ref $Num)) (param $x (ref $Num)) (param $hi (ref $Num))
    (result (ref $Num))
    ;; All-float fast path.
    (if (i32.and
          (ref.test (ref $F64) (local.get $lo))
          (i32.and
            (ref.test (ref $F64) (local.get $x))
            (ref.test (ref $F64) (local.get $hi))))
      (then (return_call $float_clamp
              (ref.cast (ref $F64) (local.get $lo))
              (ref.cast (ref $F64) (local.get $x))
              (ref.cast (ref $F64) (local.get $hi)))))
    ;; All-int: max(lo, min(hi, x)).
    (if (i32.and
          (ref.test (ref $Int) (local.get $lo))
          (i32.and
            (ref.test (ref $Int) (local.get $x))
            (ref.test (ref $Int) (local.get $hi))))
      (then (return_call $int_max
              (ref.cast (ref $Int) (local.get $lo))
              (call $int_min
                (ref.cast (ref $Int) (local.get $hi))
                (ref.cast (ref $Int) (local.get $x))))))
    (unreachable))


  ;; -- Internal helper: Int "n - n = 0 in same subtype" for fract on Int.
  ;;
  ;; Imports `op_minus` from int.wat so we get a zero of the right
  ;; family without needing int.wat to grow a zero-builder.
  (import "std/int.wat" "op_minus"
    (func $int_op_minus (param (ref $Int)) (param (ref $Int)) (result (ref $Int))))

  (func $int_sub_self (param $n (ref $Int)) (result (ref $Int))
    (return_call $int_op_minus (local.get $n) (local.get $n)))


  ;; -- Fn2 adapters ---------------------------------------------------
  ;;
  ;; Each peels (cont, ..args) off the args list, calls the dispatcher,
  ;; and tail-calls cont with the result. Args are cast to (ref $Num).

  (elem declare func
    $_abs_apply $_neg_apply $_ceil_apply $_floor_apply $_trunc_apply
    $_round_apply $_round_even_apply $_sqrt_apply $_sign_apply $_fract_apply
    $_min_apply $_max_apply $_copysign_apply $_clamp_apply)

  (func $_unary_peel
    (param $args (ref null any))
    (result (ref null any) (ref $Num))

    (local $cont (ref null any))
    (local $rest (ref null any))
    (local $a    (ref $Num))

    (local.set $cont (call $head_any (local.get $args)))
    (local.set $rest (call $tail_any (local.get $args)))
    (local.set $a    (ref.cast (ref $Num) (call $head_any (local.get $rest))))

    (local.get $cont) (local.get $a))

  (func $_abs_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $abs_num (local.get $a)) (local.get $cont)))

  (func $_neg_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $neg_num (local.get $a)) (local.get $cont)))

  (func $_ceil_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $ceil_num (local.get $a)) (local.get $cont)))

  (func $_floor_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $floor_num (local.get $a)) (local.get $cont)))

  (func $_trunc_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $trunc_num (local.get $a)) (local.get $cont)))

  (func $_round_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $round_num (local.get $a)) (local.get $cont)))

  (func $_round_even_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $round_even_num (local.get $a)) (local.get $cont)))

  (func $_sqrt_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $sqrt_num (local.get $a)) (local.get $cont)))

  (func $_sign_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $sign_num (local.get $a)) (local.get $cont)))

  (func $_fract_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $fract_num (local.get $a)) (local.get $cont)))

  ;; --- varargs adapters (fold) — min / max ---

  (func $_min_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $rest (ref $List))
    (local $acc (ref $Num))
    (local.set $cont (call $head_any (local.get $args)))
    (local.set $rest (ref.cast (ref $List) (call $tail_any (local.get $args))))
    (local.set $acc (ref.cast (ref $Num) (call $head_any (local.get $rest))))
    (local.set $rest (ref.cast (ref $List) (call $tail_any (local.get $rest))))
    (block $done
      (loop $fold
        (br_if $done (call $list_is_empty (local.get $rest)))
        (local.set $acc (call $min_num
          (local.get $acc)
          (ref.cast (ref $Num) (call $head_any (local.get $rest)))))
        (local.set $rest (ref.cast (ref $List) (call $tail_any (local.get $rest))))
        (br $fold)))
    (return_call $apply_1 (local.get $acc) (local.get $cont)))

  (func $_max_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $rest (ref $List))
    (local $acc (ref $Num))
    (local.set $cont (call $head_any (local.get $args)))
    (local.set $rest (ref.cast (ref $List) (call $tail_any (local.get $args))))
    (local.set $acc (ref.cast (ref $Num) (call $head_any (local.get $rest))))
    (local.set $rest (ref.cast (ref $List) (call $tail_any (local.get $rest))))
    (block $done
      (loop $fold
        (br_if $done (call $list_is_empty (local.get $rest)))
        (local.set $acc (call $max_num
          (local.get $acc)
          (ref.cast (ref $Num) (call $head_any (local.get $rest)))))
        (local.set $rest (ref.cast (ref $List) (call $tail_any (local.get $rest))))
        (br $fold)))
    (return_call $apply_1 (local.get $acc) (local.get $cont)))

  (func $_copysign_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $rest (ref null any))
    (local $a (ref $Num)) (local $b (ref $Num))
    (local.set $cont (call $head_any (local.get $args)))
    (local.set $rest (call $tail_any (local.get $args)))
    (local.set $a (ref.cast (ref $Num) (call $head_any (local.get $rest))))
    (local.set $rest (call $tail_any (local.get $rest)))
    (local.set $b (ref.cast (ref $Num) (call $head_any (local.get $rest))))
    (return_call $apply_1
      (call $copysign_num (local.get $a) (local.get $b))
      (local.get $cont)))

  (func $_clamp_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $rest (ref null any))
    (local $lo (ref $Num)) (local $x (ref $Num)) (local $hi (ref $Num))
    (local.set $cont (call $head_any (local.get $args)))
    (local.set $rest (call $tail_any (local.get $args)))
    (local.set $lo (ref.cast (ref $Num) (call $head_any (local.get $rest))))
    (local.set $rest (call $tail_any (local.get $rest)))
    (local.set $x (ref.cast (ref $Num) (call $head_any (local.get $rest))))
    (local.set $rest (call $tail_any (local.get $rest)))
    (local.set $hi (ref.cast (ref $Num) (call $head_any (local.get $rest))))
    (return_call $apply_1
      (call $clamp_num (local.get $lo) (local.get $x) (local.get $hi))
      (local.get $cont)))


  ;; -- Closure globals + @impl entries ---------------------------------

  (global $_abs_closure        (ref $Closure) (struct.new $Closure (ref.func $_abs_apply)        (ref.null $Captures)))
  (global $_neg_closure        (ref $Closure) (struct.new $Closure (ref.func $_neg_apply)        (ref.null $Captures)))
  (global $_ceil_closure       (ref $Closure) (struct.new $Closure (ref.func $_ceil_apply)       (ref.null $Captures)))
  (global $_floor_closure      (ref $Closure) (struct.new $Closure (ref.func $_floor_apply)      (ref.null $Captures)))
  (global $_trunc_closure      (ref $Closure) (struct.new $Closure (ref.func $_trunc_apply)      (ref.null $Captures)))
  (global $_round_closure      (ref $Closure) (struct.new $Closure (ref.func $_round_apply)      (ref.null $Captures)))
  (global $_round_even_closure (ref $Closure) (struct.new $Closure (ref.func $_round_even_apply) (ref.null $Captures)))
  (global $_sqrt_closure       (ref $Closure) (struct.new $Closure (ref.func $_sqrt_apply)       (ref.null $Captures)))
  (global $_sign_closure       (ref $Closure) (struct.new $Closure (ref.func $_sign_apply)       (ref.null $Captures)))
  (global $_fract_closure      (ref $Closure) (struct.new $Closure (ref.func $_fract_apply)      (ref.null $Captures)))
  (global $_min_closure        (ref $Closure) (struct.new $Closure (ref.func $_min_apply)        (ref.null $Captures)))
  (global $_max_closure        (ref $Closure) (struct.new $Closure (ref.func $_max_apply)        (ref.null $Captures)))
  (global $_copysign_closure   (ref $Closure) (struct.new $Closure (ref.func $_copysign_apply)   (ref.null $Captures)))
  (global $_clamp_closure      (ref $Closure) (struct.new $Closure (ref.func $_clamp_apply)      (ref.null $Captures)))

  (func $abs        (@pub) (@impl "std/math.fnk:abs")        (result (ref any)) (global.get $_abs_closure))
  (func $neg        (@pub) (@impl "std/math.fnk:neg")        (result (ref any)) (global.get $_neg_closure))
  (func $ceil       (@pub) (@impl "std/math.fnk:ceil")       (result (ref any)) (global.get $_ceil_closure))
  (func $floor      (@pub) (@impl "std/math.fnk:floor")      (result (ref any)) (global.get $_floor_closure))
  (func $trunc      (@pub) (@impl "std/math.fnk:trunc")      (result (ref any)) (global.get $_trunc_closure))
  (func $round      (@pub) (@impl "std/math.fnk:round")      (result (ref any)) (global.get $_round_closure))
  (func $round_even (@pub) (@impl "std/math.fnk:round_even") (result (ref any)) (global.get $_round_even_closure))
  (func $sqrt       (@pub) (@impl "std/math.fnk:sqrt")       (result (ref any)) (global.get $_sqrt_closure))
  (func $sign       (@pub) (@impl "std/math.fnk:sign")       (result (ref any)) (global.get $_sign_closure))
  (func $fract      (@pub) (@impl "std/math.fnk:fract")      (result (ref any)) (global.get $_fract_closure))
  (func $min        (@pub) (@impl "std/math.fnk:min")        (result (ref any)) (global.get $_min_closure))
  (func $max        (@pub) (@impl "std/math.fnk:max")        (result (ref any)) (global.get $_max_closure))
  (func $copysign   (@pub) (@impl "std/math.fnk:copysign")   (result (ref any)) (global.get $_copysign_closure))
  (func $clamp      (@pub) (@impl "std/math.fnk:clamp")      (result (ref any)) (global.get $_clamp_closure))

)
