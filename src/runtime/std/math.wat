;; Math primitives — polymorphic dispatch over $Num.
;;
;; This module is thin glue. The actual primitives live in:
;;   - std/float.wat  (Tier 1 f64 arms)
;;   - std/int.wat    (Tier 1 int arms; traps on $U64 where ops are signed-only)
;;   - std/libm.wat   (Tier 2 transcendentals — pure-wasm port of rust-libm)
;;
;; Surface (via `import 'std/math.fnk'`):
;;   Tier 1: abs, neg, ceil, floor, trunc, round, round_even, sqrt, sign,
;;           fract, min, max, copysign, clamp
;;   Tier 2: exp, exp2, expm1, log, log2, log10, log1p, pow, cbrt, hypot,
;;           sin, cos, tan, asin, acos, atan, atan2,
;;           sinh, cosh, tanh, asinh, acosh, atanh
;;
;; Tier 2 functions auto-widen $Int → $F64; result is always $F64 (these
;; are irrational in general). Tier 2 stubs trap until ported — see
;; libm.wat status comments.

(module

  ;; Type imports
  (import "rt/apply.wat" "Closure"  (type $Closure  (sub any)))
  (import "rt/apply.wat" "Captures" (type $Captures (sub any)))
  (import "rt/apply.wat" "Fn2"      (type $Fn2      (sub any)))
  (import "rt/apply.wat" "Fn3"      (type $Fn3      (sub any)))
  (import "std/num.wat"  "Num"      (type $Num      (sub any) (struct)))
  (import "std/int.wat"  "Int"      (type $Int      (sub $Num (struct))))
  (import "std/float.wat" "F64"     (type $F64      (sub final $Num (struct (field $val f64)))))

  ;; Func imports — apply / list plumbing
  (import "rt/apply.wat" "apply_1" (func $apply_1 (;apply-ctx;) (param (ref null any)) (param $result (ref null any)) (param $cont (ref null any))))
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

  (func $_abs_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $abs_num (local.get $a)) (local.get $cont)))

  (func $_neg_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $neg_num (local.get $a)) (local.get $cont)))

  (func $_ceil_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $ceil_num (local.get $a)) (local.get $cont)))

  (func $_floor_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $floor_num (local.get $a)) (local.get $cont)))

  (func $_trunc_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $trunc_num (local.get $a)) (local.get $cont)))

  (func $_round_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $round_num (local.get $a)) (local.get $cont)))

  (func $_round_even_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $round_even_num (local.get $a)) (local.get $cont)))

  (func $_sqrt_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $sqrt_num (local.get $a)) (local.get $cont)))

  (func $_sign_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $sign_num (local.get $a)) (local.get $cont)))

  (func $_fract_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $Num))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $fract_num (local.get $a)) (local.get $cont)))

  ;; --- varargs adapters (fold) — min / max ---

  (func $_min_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
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
    (return_call $apply_1
      (ref.null any) (local.get $acc) (local.get $cont)))

  (func $_max_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
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
    (return_call $apply_1
      (ref.null any) (local.get $acc) (local.get $cont)))

  (func $_copysign_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $rest (ref null any))
    (local $a (ref $Num)) (local $b (ref $Num))
    (local.set $cont (call $head_any (local.get $args)))
    (local.set $rest (call $tail_any (local.get $args)))
    (local.set $a (ref.cast (ref $Num) (call $head_any (local.get $rest))))
    (local.set $rest (call $tail_any (local.get $rest)))
    (local.set $b (ref.cast (ref $Num) (call $head_any (local.get $rest))))
    (return_call $apply_1
      (ref.null any)
      (call $copysign_num (local.get $a) (local.get $b))
      (local.get $cont)))

  (func $_clamp_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
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
      (ref.null any)
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


  ;; =========================================================================
  ;; Tier 2 — transcendentals.
  ;;
  ;; All take $Num and return $F64 (irrational results). Int args widen
  ;; via num.wat:as_f64. Decimal traps inside as_f64.
  ;; =========================================================================

  ;; Coercion helper from num.wat for the Int→F64 widen path.
  (import "std/num.wat" "as_f64"
    (func $as_f64 (param (ref $Num)) (result (ref $F64))))

  ;; Func imports — libm arms.
  (import "std/libm.wat" "exp"   (func $libm_exp   (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "exp2"  (func $libm_exp2  (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "expm1" (func $libm_expm1 (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "log"   (func $libm_log   (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "log2"  (func $libm_log2  (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "log10" (func $libm_log10 (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "log1p" (func $libm_log1p (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "pow"   (func $libm_pow   (param (ref $F64)) (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "cbrt"  (func $libm_cbrt  (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "hypot" (func $libm_hypot (param (ref $F64)) (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "sin"   (func $libm_sin   (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "cos"   (func $libm_cos   (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "tan"   (func $libm_tan   (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "asin"  (func $libm_asin  (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "acos"  (func $libm_acos  (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "atan"  (func $libm_atan  (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "atan2" (func $libm_atan2 (param (ref $F64)) (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "sinh"  (func $libm_sinh  (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "cosh"  (func $libm_cosh  (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "tanh"  (func $libm_tanh  (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "asinh" (func $libm_asinh (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "acosh" (func $libm_acosh (param (ref $F64)) (result (ref $F64))))
  (import "std/libm.wat" "atanh" (func $libm_atanh (param (ref $F64)) (result (ref $F64))))


  ;; -- Fn2 adapters for Tier 2 ---------------------------------------------
  ;;
  ;; Each peels (cont, ..args) off the args list, widens Int→F64 via
  ;; $as_f64, calls the libm kernel, tail-calls cont with the result.

  (elem declare func
    $_exp_apply $_exp2_apply $_expm1_apply
    $_log_apply $_log2_apply $_log10_apply $_log1p_apply
    $_pow_apply $_cbrt_apply $_hypot_apply
    $_sin_apply $_cos_apply $_tan_apply
    $_asin_apply $_acos_apply $_atan_apply $_atan2_apply
    $_sinh_apply $_cosh_apply $_tanh_apply
    $_asinh_apply $_acosh_apply $_atanh_apply)

  ;; Helper: peel cont + first arg as $F64 (widening Int→F64).
  (func $_unary_peel_f64
    (param $args (ref null any))
    (result (ref null any) (ref $F64))
    (local $cont (ref null any))
    (local $rest (ref null any))
    (local $a    (ref $F64))
    (local.set $cont (call $head_any (local.get $args)))
    (local.set $rest (call $tail_any (local.get $args)))
    (local.set $a    (call $as_f64
                       (ref.cast (ref $Num) (call $head_any (local.get $rest)))))
    (local.get $cont) (local.get $a))

  (func $_exp_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_exp (local.get $a)) (local.get $cont)))

  (func $_exp2_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_exp2 (local.get $a)) (local.get $cont)))

  (func $_expm1_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_expm1 (local.get $a)) (local.get $cont)))

  (func $_log_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_log (local.get $a)) (local.get $cont)))

  (func $_log2_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_log2 (local.get $a)) (local.get $cont)))

  (func $_log10_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_log10 (local.get $a)) (local.get $cont)))

  (func $_log1p_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_log1p (local.get $a)) (local.get $cont)))

  (func $_cbrt_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_cbrt (local.get $a)) (local.get $cont)))

  (func $_sin_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_sin (local.get $a)) (local.get $cont)))

  (func $_cos_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_cos (local.get $a)) (local.get $cont)))

  (func $_tan_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_tan (local.get $a)) (local.get $cont)))

  (func $_asin_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_asin (local.get $a)) (local.get $cont)))

  (func $_acos_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_acos (local.get $a)) (local.get $cont)))

  (func $_atan_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_atan (local.get $a)) (local.get $cont)))

  (func $_sinh_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_sinh (local.get $a)) (local.get $cont)))

  (func $_cosh_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_cosh (local.get $a)) (local.get $cont)))

  (func $_tanh_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_tanh (local.get $a)) (local.get $cont)))

  (func $_asinh_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_asinh (local.get $a)) (local.get $cont)))

  (func $_acosh_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_acosh (local.get $a)) (local.get $cont)))

  (func $_atanh_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel_f64 (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any) (call $libm_atanh (local.get $a)) (local.get $cont)))

  ;; --- 2-arg adapters: pow, hypot, atan2 ---

  (func $_binary_peel_f64
    (param $args (ref null any))
    (result (ref null any) (ref $F64) (ref $F64))
    (local $cont (ref null any)) (local $rest (ref null any))
    (local $a (ref $F64)) (local $b (ref $F64))
    (local.set $cont (call $head_any (local.get $args)))
    (local.set $rest (call $tail_any (local.get $args)))
    (local.set $a    (call $as_f64
                       (ref.cast (ref $Num) (call $head_any (local.get $rest)))))
    (local.set $rest (call $tail_any (local.get $rest)))
    (local.set $b    (call $as_f64
                       (ref.cast (ref $Num) (call $head_any (local.get $rest)))))
    (local.get $cont) (local.get $a) (local.get $b))

  (func $_pow_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64)) (local $b (ref $F64))
    (call $_binary_peel_f64 (local.get $args))
    (local.set $b) (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any)
      (call $libm_pow (local.get $a) (local.get $b))
      (local.get $cont)))

  (func $_hypot_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64)) (local $b (ref $F64))
    (call $_binary_peel_f64 (local.get $args))
    (local.set $b) (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any)
      (call $libm_hypot (local.get $a) (local.get $b))
      (local.get $cont)))

  (func $_atan2_apply (type $Fn3)
    (param $_caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64)) (local $b (ref $F64))
    (call $_binary_peel_f64 (local.get $args))
    (local.set $b) (local.set $a) (local.set $cont)
    (return_call $apply_1
      (ref.null any)
      (call $libm_atan2 (local.get $a) (local.get $b))
      (local.get $cont)))


  ;; -- Closure globals + @impl entries (Tier 2) -----------------------------

  (global $_exp_closure   (ref $Closure) (struct.new $Closure (ref.func $_exp_apply)   (ref.null $Captures)))
  (global $_exp2_closure  (ref $Closure) (struct.new $Closure (ref.func $_exp2_apply)  (ref.null $Captures)))
  (global $_expm1_closure (ref $Closure) (struct.new $Closure (ref.func $_expm1_apply) (ref.null $Captures)))
  (global $_log_closure   (ref $Closure) (struct.new $Closure (ref.func $_log_apply)   (ref.null $Captures)))
  (global $_log2_closure  (ref $Closure) (struct.new $Closure (ref.func $_log2_apply)  (ref.null $Captures)))
  (global $_log10_closure (ref $Closure) (struct.new $Closure (ref.func $_log10_apply) (ref.null $Captures)))
  (global $_log1p_closure (ref $Closure) (struct.new $Closure (ref.func $_log1p_apply) (ref.null $Captures)))
  (global $_pow_closure   (ref $Closure) (struct.new $Closure (ref.func $_pow_apply)   (ref.null $Captures)))
  (global $_cbrt_closure  (ref $Closure) (struct.new $Closure (ref.func $_cbrt_apply)  (ref.null $Captures)))
  (global $_hypot_closure (ref $Closure) (struct.new $Closure (ref.func $_hypot_apply) (ref.null $Captures)))
  (global $_sin_closure   (ref $Closure) (struct.new $Closure (ref.func $_sin_apply)   (ref.null $Captures)))
  (global $_cos_closure   (ref $Closure) (struct.new $Closure (ref.func $_cos_apply)   (ref.null $Captures)))
  (global $_tan_closure   (ref $Closure) (struct.new $Closure (ref.func $_tan_apply)   (ref.null $Captures)))
  (global $_asin_closure  (ref $Closure) (struct.new $Closure (ref.func $_asin_apply)  (ref.null $Captures)))
  (global $_acos_closure  (ref $Closure) (struct.new $Closure (ref.func $_acos_apply)  (ref.null $Captures)))
  (global $_atan_closure  (ref $Closure) (struct.new $Closure (ref.func $_atan_apply)  (ref.null $Captures)))
  (global $_atan2_closure (ref $Closure) (struct.new $Closure (ref.func $_atan2_apply) (ref.null $Captures)))
  (global $_sinh_closure  (ref $Closure) (struct.new $Closure (ref.func $_sinh_apply)  (ref.null $Captures)))
  (global $_cosh_closure  (ref $Closure) (struct.new $Closure (ref.func $_cosh_apply)  (ref.null $Captures)))
  (global $_tanh_closure  (ref $Closure) (struct.new $Closure (ref.func $_tanh_apply)  (ref.null $Captures)))
  (global $_asinh_closure (ref $Closure) (struct.new $Closure (ref.func $_asinh_apply) (ref.null $Captures)))
  (global $_acosh_closure (ref $Closure) (struct.new $Closure (ref.func $_acosh_apply) (ref.null $Captures)))
  (global $_atanh_closure (ref $Closure) (struct.new $Closure (ref.func $_atanh_apply) (ref.null $Captures)))

  (func $exp   (@pub) (@impl "std/math.fnk:exp")   (result (ref any)) (global.get $_exp_closure))
  (func $exp2  (@pub) (@impl "std/math.fnk:exp2")  (result (ref any)) (global.get $_exp2_closure))
  (func $expm1 (@pub) (@impl "std/math.fnk:expm1") (result (ref any)) (global.get $_expm1_closure))
  (func $log   (@pub) (@impl "std/math.fnk:log")   (result (ref any)) (global.get $_log_closure))
  (func $log2  (@pub) (@impl "std/math.fnk:log2")  (result (ref any)) (global.get $_log2_closure))
  (func $log10 (@pub) (@impl "std/math.fnk:log10") (result (ref any)) (global.get $_log10_closure))
  (func $log1p (@pub) (@impl "std/math.fnk:log1p") (result (ref any)) (global.get $_log1p_closure))
  (func $pow   (@pub) (@impl "std/math.fnk:pow")   (result (ref any)) (global.get $_pow_closure))
  (func $cbrt  (@pub) (@impl "std/math.fnk:cbrt")  (result (ref any)) (global.get $_cbrt_closure))
  (func $hypot (@pub) (@impl "std/math.fnk:hypot") (result (ref any)) (global.get $_hypot_closure))
  (func $sin   (@pub) (@impl "std/math.fnk:sin")   (result (ref any)) (global.get $_sin_closure))
  (func $cos   (@pub) (@impl "std/math.fnk:cos")   (result (ref any)) (global.get $_cos_closure))
  (func $tan   (@pub) (@impl "std/math.fnk:tan")   (result (ref any)) (global.get $_tan_closure))
  (func $asin  (@pub) (@impl "std/math.fnk:asin")  (result (ref any)) (global.get $_asin_closure))
  (func $acos  (@pub) (@impl "std/math.fnk:acos")  (result (ref any)) (global.get $_acos_closure))
  (func $atan  (@pub) (@impl "std/math.fnk:atan")  (result (ref any)) (global.get $_atan_closure))
  (func $atan2 (@pub) (@impl "std/math.fnk:atan2") (result (ref any)) (global.get $_atan2_closure))
  (func $sinh  (@pub) (@impl "std/math.fnk:sinh")  (result (ref any)) (global.get $_sinh_closure))
  (func $cosh  (@pub) (@impl "std/math.fnk:cosh")  (result (ref any)) (global.get $_cosh_closure))
  (func $tanh  (@pub) (@impl "std/math.fnk:tanh")  (result (ref any)) (global.get $_tanh_closure))
  (func $asinh (@pub) (@impl "std/math.fnk:asinh") (result (ref any)) (global.get $_asinh_closure))
  (func $acosh (@pub) (@impl "std/math.fnk:acosh") (result (ref any)) (global.get $_acosh_closure))
  (func $atanh (@pub) (@impl "std/math.fnk:atanh") (result (ref any)) (global.get $_atanh_closure))

)
