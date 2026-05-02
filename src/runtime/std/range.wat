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
;; Type hierarchy (types.wat defines the opaque base type):
;;
;;   $Range             ← opaque base (from types.wat)
;;   └── $RangeImpl     ← start, end, inclusive flag
;;
;; Exported functions:
;;   $std/range.wat:excl  : (ref $Num), (ref $Num) -> (ref $Range)
;;   $std/range.wat:incl  : (ref $Num), (ref $Num) -> (ref $Range)
;;   $std/range.wat:op_in    : (ref $Num), (ref $Range) -> i32
;;   $std/range.wat:op_not_in : (ref $Num), (ref $Range) -> i32

(module

  ;; -- Type definitions -----------------------------------------------

  ;; $RangeImpl — range internals, subtype of $Range (from types.wat).
  (type $RangeImpl (sub $Range (struct
    (field $start (ref $Num))
    (field $end   (ref $Num))
    (field $incl  i32)
  )))


  ;; -- Construction ---------------------------------------------------

  ;; range_excl(start, end) → exclusive range
  (func $std/range.wat:excl (export "std/range.wat:excl")
    (param $start (ref $Num)) (param $end (ref $Num))
    (result (ref $Range))
    (struct.new $RangeImpl
      (local.get $start)
      (local.get $end)
      (i32.const 0)
    )
  )

  ;; range_incl(start, end) → inclusive range
  (func $std/range.wat:incl (export "std/range.wat:incl")
    (param $start (ref $Num)) (param $end (ref $Num))
    (result (ref $Range))
    (struct.new $RangeImpl
      (local.get $start)
      (local.get $end)
      (i32.const 1)
    )
  )


  ;; -- Membership -----------------------------------------------------

  ;; range_op_in(val, range) → 1 if val is in range, 0 otherwise
  ;;
  ;; For exclusive: start <= val < end
  ;; For inclusive: start <= val <= end
  (func $std/range.wat:op_in (export "std/range.wat:op_in")
    (param $val (ref $Num)) (param $range (ref $Range))
    (result i32)
    (local $impl (ref $RangeImpl))
    (local $v f64)
    (local $s f64)
    (local $e f64)

    ;; Downcast to $RangeImpl
    (local.set $impl
      (ref.cast (ref $RangeImpl) (local.get $range))
    )

    ;; Unbox val, start, end to f64
    (local.set $v
      (struct.get $Num $val (local.get $val))
    )
    (local.set $s
      (struct.get $Num $val (struct.get $RangeImpl $start (local.get $impl)))
    )
    (local.set $e
      (struct.get $Num $val (struct.get $RangeImpl $end (local.get $impl)))
    )

    ;; start <= val
    (if (i32.eqz (f64.le (local.get $s) (local.get $v)))
      (then (return (i32.const 0)))
    )

    ;; Exclusive: val < end.  Inclusive: val <= end.
    (if (struct.get $RangeImpl $incl (local.get $impl))
      (then
        (return (f64.le (local.get $v) (local.get $e)))
      )
    )
    (f64.lt (local.get $v) (local.get $e))
  )

  ;; range_op_not_in(val, range) → 1 if val is NOT in range, 0 otherwise
  (func $std/range.wat:op_not_in (export "std/range.wat:op_not_in")
    (param $val (ref $Num)) (param $range (ref $Range))
    (result i32)
    (i32.eqz (call $std/range.wat:op_in (local.get $val) (local.get $range)))
  )


  ;; -- Accessors --------------------------------------------------------

  ;; range_start(range) → start bound as $Num
  (func $std/range.wat:start (export "std/range.wat:start")
    (param $range (ref $Range))
    (result (ref $Num))
    (struct.get $RangeImpl $start
      (ref.cast (ref $RangeImpl) (local.get $range))))

  ;; range_end(range) → end bound as $Num
  (func $std/range.wat:end (export "std/range.wat:end")
    (param $range (ref $Range))
    (result (ref $Num))
    (struct.get $RangeImpl $end
      (ref.cast (ref $RangeImpl) (local.get $range))))

  ;; range_is_incl(range) → 1 if inclusive, 0 if exclusive
  (func $std/range.wat:is_incl (export "std/range.wat:is_incl")
    (param $range (ref $Range))
    (result i32)
    (struct.get $RangeImpl $incl
      (ref.cast (ref $RangeImpl) (local.get $range))))


  ;; CPS wrappers — stripped by unit test harness (prepare_wat).

  (func $std/range.fnk:excl (export "std/range.fnk:excl")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $std/list.wat:apply_1
      (call $std/range.wat:excl
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $std/range.fnk:incl (export "std/range.fnk:incl")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $std/list.wat:apply_1
      (call $std/range.wat:incl
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

)
