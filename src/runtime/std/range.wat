;; Range — immutable numeric range
;;
;; WASM GC implementation using struct types.
;;
;; Design:
;;   - Stores start and end bounds as $I64 (signed 64-bit int).
;;     Non-int bounds trap at construction. Float / decimal / user-typed
;;     ranges become possible once range becomes a protocol-driven shape
;;     (TODO).
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
  (import "std/int.wat"  "I64"  (type $I64  (sub any) (struct (field $ival i64))))
  (import "std/list.wat" "List" (type $List (sub any)))

  ;; Func imports
  ;; TODO: apply_1 wraps a single result and calls _apply — conceptually
  ;; an apply concern, not a list one. Move to rt/apply.wat.
  (import "rt/apply.wat" "apply_1"
    (func $list_apply_1 (param $val (ref any)) (param $cont (ref null any))))


  ;; -- $Range type ----------------------------------------------------------
  ;;
  ;; Opaque public type. Subtype $RangeImpl carries the actual fields.

  (type $Range (@pub) (sub (struct)))

  (type $RangeImpl (sub $Range (struct
    (field $start (ref $I64))
    (field $end   (ref $I64))
    (field $incl  i32)
  )))


  ;; -- Construction ---------------------------------------------------------

  ;; range_excl(start, end) → exclusive range
  (func $excl (@pub)
    (param $start (ref $I64)) (param $end (ref $I64))
    (result (ref $Range))
    (struct.new $RangeImpl
      (local.get $start)
      (local.get $end)
      (i32.const 0)
    )
  )

  ;; range_incl(start, end) → inclusive range
  (func $incl (@pub)
    (param $start (ref $I64)) (param $end (ref $I64))
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
  ;;
  ;; Both bounds and val are $I64 — non-int operands trap at the cast.
  (func $op_in (@impl "std/operators.fnk:op_in" $I64 $Range)
    (param $val (ref $I64)) (param $range (ref $Range))
    (result i32)
    (local $impl (ref $RangeImpl))
    (local $v i64)
    (local $s i64)
    (local $e i64)

    (local.set $impl (ref.cast (ref $RangeImpl) (local.get $range)))

    (local.set $v (struct.get $I64 $ival (local.get $val)))
    (local.set $s (struct.get $I64 $ival (struct.get $RangeImpl $start (local.get $impl))))
    (local.set $e (struct.get $I64 $ival (struct.get $RangeImpl $end (local.get $impl))))

    ;; start <= val
    (if (i32.eqz (i64.le_s (local.get $s) (local.get $v)))
      (then (return (i32.const 0)))
    )

    ;; Exclusive: val < end.  Inclusive: val <= end.
    (if (struct.get $RangeImpl $incl (local.get $impl))
      (then (return (i64.le_s (local.get $v) (local.get $e))))
    )
    (i64.lt_s (local.get $v) (local.get $e))
  )

  ;; op_not_in(val, range) → 1 if val is NOT in range, 0 otherwise
  (func $op_not_in (@impl "std/operators.fnk:op_notin" $I64 $Range)
    (param $val (ref $I64)) (param $range (ref $Range))
    (result i32)
    (i32.eqz (call $op_in (local.get $val) (local.get $range)))
  )


  ;; -- Accessors ------------------------------------------------------------

  ;; start(range) → start bound as $I64
  (func $start (@pub)
    (param $range (ref $Range))
    (result (ref $I64))
    (struct.get $RangeImpl $start
      (ref.cast (ref $RangeImpl) (local.get $range))))

  ;; end(range) → end bound as $I64
  (func $end (@pub)
    (param $range (ref $Range))
    (result (ref $I64))
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
        (ref.cast (ref $I64) (local.get $a))
        (ref.cast (ref $I64) (local.get $b)))
      (local.get $cont)))

  (func $cps_incl (@pub) (@impl "std/range.fnk:incl")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $list_apply_1
      (call $incl
        (ref.cast (ref $I64) (local.get $a))
        (ref.cast (ref $I64) (local.get $b)))
      (local.get $cont)))

)
