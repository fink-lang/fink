;; Math primitives — Tier 1: native f64 wasm instructions wrapped as
;; user-importable fink functions and as direct-style helpers for
;; in-runtime use.
;;
;; Surface (via `import 'std/math.fnk'`):
;;   abs, neg, ceil, floor, trunc, round, sqrt, sign, fract,
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
  (import "std/float.wat" "F64"     (type $F64      (sub final $Num (struct (field $val f64)))))

  ;; Func imports
  (import "rt/apply.wat" "apply_1"
    (func $apply_1 (param $result (ref null any)) (param $cont (ref null any))))
  (import "std/list.wat" "head_any"
    (func $head_any (param (ref null any)) (result (ref null any))))
  (import "std/list.wat" "tail_any"
    (func $tail_any (param (ref null any)) (result (ref null any))))


  ;; -- Direct-style helpers --------------------------------------------
  ;;
  ;; These wrap a single wasm instruction. Other WAT modules can call
  ;; them directly when they need the underlying primitive.

  (func $abs_f64 (@pub) (param $a (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.abs (struct.get $F64 $val (local.get $a)))))

  (func $neg_f64 (@pub) (param $a (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.neg (struct.get $F64 $val (local.get $a)))))

  (func $ceil_f64 (@pub) (param $a (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.ceil (struct.get $F64 $val (local.get $a)))))

  (func $floor_f64 (@pub) (param $a (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.floor (struct.get $F64 $val (local.get $a)))))

  (func $trunc_f64 (@pub) (param $a (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.trunc (struct.get $F64 $val (local.get $a)))))

  ;; round-to-nearest-even (banker's rounding) — wasm's native f64.nearest.
  (func $round_f64 (@pub) (param $a (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.nearest (struct.get $F64 $val (local.get $a)))))

  (func $sqrt_f64 (@pub) (param $a (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.sqrt (struct.get $F64 $val (local.get $a)))))

  ;; sign(x): 1.0 if x > 0, -1.0 if x < 0, 0.0 if x == 0, NaN if x is NaN.
  (func $sign_f64 (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $v f64)
    (local.set $v (struct.get $F64 $val (local.get $a)))
    ;; NaN check
    (if (f64.ne (local.get $v) (local.get $v))
      (then (return (struct.new $F64 (local.get $v)))))
    ;; ±0 → 0.0 (exact zero)
    (if (f64.eq (local.get $v) (f64.const 0))
      (then (return (struct.new $F64 (f64.const 0)))))
    ;; non-zero, non-NaN → ±1
    (struct.new $F64 (f64.copysign (f64.const 1) (local.get $v))))

  ;; fract(x): x - trunc(x). Sign matches x.
  (func $fract_f64 (@pub) (param $a (ref $F64)) (result (ref $F64))
    (local $v f64)
    (local.set $v (struct.get $F64 $val (local.get $a)))
    (struct.new $F64
      (f64.sub (local.get $v) (f64.trunc (local.get $v)))))

  (func $min_f64 (@pub) (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.min
      (struct.get $F64 $val (local.get $a))
      (struct.get $F64 $val (local.get $b)))))

  (func $max_f64 (@pub) (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.max
      (struct.get $F64 $val (local.get $a))
      (struct.get $F64 $val (local.get $b)))))

  (func $copysign_f64 (@pub) (param $a (ref $F64)) (param $b (ref $F64)) (result (ref $F64))
    (struct.new $F64 (f64.copysign
      (struct.get $F64 $val (local.get $a))
      (struct.get $F64 $val (local.get $b)))))

  ;; clamp(x, lo, hi): max(lo, min(hi, x)).
  (func $clamp_f64 (@pub)
    (param $x (ref $F64)) (param $lo (ref $F64)) (param $hi (ref $F64))
    (result (ref $F64))
    (struct.new $F64 (f64.max
      (struct.get $F64 $val (local.get $lo))
      (f64.min
        (struct.get $F64 $val (local.get $hi))
        (struct.get $F64 $val (local.get $x))))))


  ;; -- User-importable closures ---------------------------------------
  ;;
  ;; Each `_<name>_apply` is a Fn2 adapter that peels (cont, ..args) off
  ;; the args list, calls the direct helper, and tail-calls cont with
  ;; the result. The `_<name>_closure` global wraps the adapter as a
  ;; $Closure value. The `<name>` function with `@impl "std/math.fnk:..."`
  ;; is what fink lookups resolve to — returns the closure ref.
  ;;
  ;; Cast pattern: args[1] etc. are (ref null any); cast to (ref $F64).

  (elem declare func
    $_abs_apply $_neg_apply $_ceil_apply $_floor_apply $_trunc_apply
    $_round_apply $_sqrt_apply $_sign_apply $_fract_apply
    $_min_apply $_max_apply $_copysign_apply $_clamp_apply)

  ;; --- 1-arg adapters ---

  (func $_unary_peel
    (param $args (ref null any))
    (result (ref null any) (ref $F64))

    (local $cont (ref null any))
    (local $rest (ref null any))
    (local $a    (ref $F64))

    (local.set $cont (call $head_any (local.get $args)))
    (local.set $rest (call $tail_any (local.get $args)))
    (local.set $a    (ref.cast (ref $F64) (call $head_any (local.get $rest))))

    (local.get $cont) (local.get $a))

  (func $_abs_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $abs_f64 (local.get $a)) (local.get $cont)))

  (func $_neg_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $neg_f64 (local.get $a)) (local.get $cont)))

  (func $_ceil_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $ceil_f64 (local.get $a)) (local.get $cont)))

  (func $_floor_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $floor_f64 (local.get $a)) (local.get $cont)))

  (func $_trunc_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $trunc_f64 (local.get $a)) (local.get $cont)))

  (func $_round_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $round_f64 (local.get $a)) (local.get $cont)))

  (func $_sqrt_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $sqrt_f64 (local.get $a)) (local.get $cont)))

  (func $_sign_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $sign_f64 (local.get $a)) (local.get $cont)))

  (func $_fract_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $a (ref $F64))
    (call $_unary_peel (local.get $args))
    (local.set $a) (local.set $cont)
    (return_call $apply_1 (call $fract_f64 (local.get $a)) (local.get $cont)))

  ;; --- 2-arg adapters ---

  (func $_min_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $rest (ref null any))
    (local $a (ref $F64)) (local $b (ref $F64))
    (local.set $cont (call $head_any (local.get $args)))
    (local.set $rest (call $tail_any (local.get $args)))
    (local.set $a (ref.cast (ref $F64) (call $head_any (local.get $rest))))
    (local.set $rest (call $tail_any (local.get $rest)))
    (local.set $b (ref.cast (ref $F64) (call $head_any (local.get $rest))))
    (return_call $apply_1
      (call $min_f64 (local.get $a) (local.get $b))
      (local.get $cont)))

  (func $_max_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $rest (ref null any))
    (local $a (ref $F64)) (local $b (ref $F64))
    (local.set $cont (call $head_any (local.get $args)))
    (local.set $rest (call $tail_any (local.get $args)))
    (local.set $a (ref.cast (ref $F64) (call $head_any (local.get $rest))))
    (local.set $rest (call $tail_any (local.get $rest)))
    (local.set $b (ref.cast (ref $F64) (call $head_any (local.get $rest))))
    (return_call $apply_1
      (call $max_f64 (local.get $a) (local.get $b))
      (local.get $cont)))

  (func $_copysign_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $rest (ref null any))
    (local $a (ref $F64)) (local $b (ref $F64))
    (local.set $cont (call $head_any (local.get $args)))
    (local.set $rest (call $tail_any (local.get $args)))
    (local.set $a (ref.cast (ref $F64) (call $head_any (local.get $rest))))
    (local.set $rest (call $tail_any (local.get $rest)))
    (local.set $b (ref.cast (ref $F64) (call $head_any (local.get $rest))))
    (return_call $apply_1
      (call $copysign_f64 (local.get $a) (local.get $b))
      (local.get $cont)))

  ;; --- 3-arg adapter ---

  (func $_clamp_apply (type $Fn2)
    (param $_caps (ref null any)) (param $args (ref null any))
    (local $cont (ref null any)) (local $rest (ref null any))
    (local $x (ref $F64)) (local $lo (ref $F64)) (local $hi (ref $F64))
    (local.set $cont (call $head_any (local.get $args)))
    (local.set $rest (call $tail_any (local.get $args)))
    (local.set $x (ref.cast (ref $F64) (call $head_any (local.get $rest))))
    (local.set $rest (call $tail_any (local.get $rest)))
    (local.set $lo (ref.cast (ref $F64) (call $head_any (local.get $rest))))
    (local.set $rest (call $tail_any (local.get $rest)))
    (local.set $hi (ref.cast (ref $F64) (call $head_any (local.get $rest))))
    (return_call $apply_1
      (call $clamp_f64 (local.get $x) (local.get $lo) (local.get $hi))
      (local.get $cont)))


  ;; -- Closure globals + @impl entries ---------------------------------

  (global $_abs_closure      (ref $Closure) (struct.new $Closure (ref.func $_abs_apply)      (ref.null $Captures)))
  (global $_neg_closure      (ref $Closure) (struct.new $Closure (ref.func $_neg_apply)      (ref.null $Captures)))
  (global $_ceil_closure     (ref $Closure) (struct.new $Closure (ref.func $_ceil_apply)     (ref.null $Captures)))
  (global $_floor_closure    (ref $Closure) (struct.new $Closure (ref.func $_floor_apply)    (ref.null $Captures)))
  (global $_trunc_closure    (ref $Closure) (struct.new $Closure (ref.func $_trunc_apply)    (ref.null $Captures)))
  (global $_round_closure    (ref $Closure) (struct.new $Closure (ref.func $_round_apply)    (ref.null $Captures)))
  (global $_sqrt_closure     (ref $Closure) (struct.new $Closure (ref.func $_sqrt_apply)     (ref.null $Captures)))
  (global $_sign_closure     (ref $Closure) (struct.new $Closure (ref.func $_sign_apply)     (ref.null $Captures)))
  (global $_fract_closure    (ref $Closure) (struct.new $Closure (ref.func $_fract_apply)    (ref.null $Captures)))
  (global $_min_closure      (ref $Closure) (struct.new $Closure (ref.func $_min_apply)      (ref.null $Captures)))
  (global $_max_closure      (ref $Closure) (struct.new $Closure (ref.func $_max_apply)      (ref.null $Captures)))
  (global $_copysign_closure (ref $Closure) (struct.new $Closure (ref.func $_copysign_apply) (ref.null $Captures)))
  (global $_clamp_closure    (ref $Closure) (struct.new $Closure (ref.func $_clamp_apply)    (ref.null $Captures)))

  (func $abs      (@pub) (@impl "std/math.fnk:abs")      (result (ref any)) (global.get $_abs_closure))
  (func $neg      (@pub) (@impl "std/math.fnk:neg")      (result (ref any)) (global.get $_neg_closure))
  (func $ceil     (@pub) (@impl "std/math.fnk:ceil")     (result (ref any)) (global.get $_ceil_closure))
  (func $floor    (@pub) (@impl "std/math.fnk:floor")    (result (ref any)) (global.get $_floor_closure))
  (func $trunc    (@pub) (@impl "std/math.fnk:trunc")    (result (ref any)) (global.get $_trunc_closure))
  (func $round    (@pub) (@impl "std/math.fnk:round")    (result (ref any)) (global.get $_round_closure))
  (func $sqrt     (@pub) (@impl "std/math.fnk:sqrt")     (result (ref any)) (global.get $_sqrt_closure))
  (func $sign     (@pub) (@impl "std/math.fnk:sign")     (result (ref any)) (global.get $_sign_closure))
  (func $fract    (@pub) (@impl "std/math.fnk:fract")    (result (ref any)) (global.get $_fract_closure))
  (func $min      (@pub) (@impl "std/math.fnk:min")      (result (ref any)) (global.get $_min_closure))
  (func $max      (@pub) (@impl "std/math.fnk:max")      (result (ref any)) (global.get $_max_closure))
  (func $copysign (@pub) (@impl "std/math.fnk:copysign") (result (ref any)) (global.get $_copysign_closure))
  (func $clamp    (@pub) (@impl "std/math.fnk:clamp")    (result (ref any)) (global.get $_clamp_closure))

)
