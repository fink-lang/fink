;; Range — immutable numeric range
;;
;; WASM GC implementation using struct types.
;;
;; Design:
;;   - Stores start and end bounds as $Num (f64)
;;   - Inclusive flag distinguishes exclusive (..) from inclusive (...)
;;   - Membership test: start <= val and (val < end or val <= end)
;;   - Step field to be added later
;;
;; Type hierarchy:
;;
;;   $Range             ← opaque public type
;;   └── $RangeImpl     ← start, end, inclusive flag (private)

(module

  ;; Type imports
  (import "std/num.wat"  "Num"  (type $Num  (sub any)))
  (import "std/list.wat" "List" (type $List (sub any)))

  ;; Func imports
  ;; TODO: apply_1 wraps a single result and calls _apply — conceptually
  ;; an apply concern, not a list one. Move to rt/apply.wat.
  (import "std/list.wat" "apply_1"
    (func $list_apply_1 (param $val (ref any)) (param $cont (ref null any))))


  ;; -- $Range type ----------------------------------------------------------
  ;;
  ;; Opaque public type. Subtype $RangeImpl carries the actual fields.

  (type $Range (@pub) (sub (struct)))

  (type $RangeImpl (sub $Range (struct
    (field $start (ref $Num))
    (field $end   (ref $Num))
    (field $incl  i32)
  )))


  ;; -- Construction ---------------------------------------------------------

  ;; range_excl(start, end) → exclusive range
  (func $excl (@pub)
    (param $start (ref $Num)) (param $end (ref $Num))
    (result (ref $Range))
    (struct.new $RangeImpl
      (local.get $start)
      (local.get $end)
      (i32.const 0)
    )
  )

  ;; range_incl(start, end) → inclusive range
  (func $incl (@pub)
    (param $start (ref $Num)) (param $end (ref $Num))
    (result (ref $Range))
    (struct.new $RangeImpl
      (local.get $start)
      (local.get $end)
      (i32.const 1)
    )
  )


  ;; -- Membership -----------------------------------------------------------

  ;; op_in(val, range) → 1 if val is in range, 0 otherwise.
  ;;
  ;; For exclusive: start <= val < end
  ;; For inclusive: start <= val <= end
  (func $op_in (@impl "std/operators.fnk:op_in" $Num $Range)
    (param $val (ref $Num)) (param $range (ref $Range))
    (result i32)
    (local $impl (ref $RangeImpl))
    (local $v f64)
    (local $s f64)
    (local $e f64)

    (local.set $impl (ref.cast (ref $RangeImpl) (local.get $range)))

    (local.set $v (struct.get $Num $val (local.get $val)))
    (local.set $s (struct.get $Num $val (struct.get $RangeImpl $start (local.get $impl))))
    (local.set $e (struct.get $Num $val (struct.get $RangeImpl $end (local.get $impl))))

    ;; start <= val
    (if (i32.eqz (f64.le (local.get $s) (local.get $v)))
      (then (return (i32.const 0)))
    )

    ;; Exclusive: val < end.  Inclusive: val <= end.
    (if (struct.get $RangeImpl $incl (local.get $impl))
      (then (return (f64.le (local.get $v) (local.get $e))))
    )
    (f64.lt (local.get $v) (local.get $e))
  )

  ;; op_not_in(val, range) → 1 if val is NOT in range, 0 otherwise
  (func $op_not_in (@impl "std/operators.fnk:op_notin" $Num $Range)
    (param $val (ref $Num)) (param $range (ref $Range))
    (result i32)
    (i32.eqz (call $op_in (local.get $val) (local.get $range)))
  )


  ;; -- Accessors ------------------------------------------------------------

  ;; start(range) → start bound as $Num
  (func $start (@pub)
    (param $range (ref $Range))
    (result (ref $Num))
    (struct.get $RangeImpl $start
      (ref.cast (ref $RangeImpl) (local.get $range))))

  ;; end(range) → end bound as $Num
  (func $end (@pub)
    (param $range (ref $Range))
    (result (ref $Num))
    (struct.get $RangeImpl $end
      (ref.cast (ref $RangeImpl) (local.get $range))))

  ;; is_incl(range) → 1 if inclusive, 0 if exclusive
  (func $is_incl (@pub)
    (param $range (ref $Range))
    (result i32)
    (struct.get $RangeImpl $incl
      (ref.cast (ref $RangeImpl) (local.get $range))))


  ;; -- CPS wrappers ---------------------------------------------------------
  ;;
  ;; User-imported via `import 'std/range.fnk'`. Wrap direct-style ctors
  ;; in CPS so they fit the user calling convention.

  (func $cps_excl (@pub) (@impl "std/range.fnk:excl")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $list_apply_1
      (call $excl
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $cps_incl (@pub) (@impl "std/range.fnk:incl")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $list_apply_1
      (call $incl
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

)
