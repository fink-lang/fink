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
  (import "std/int.wat"  "Int"  (type $Int  (sub any) (struct)))
  (import "std/int.wat"  "I64"  (type $I64  (sub $Int (struct (field $ival i64)))))
  (import "std/list.wat" "List" (type $List (sub any)))
  (import "std/str.wat"  "Str"  (type $Str  (sub any) (struct)))
  (import "std/str.wat"  "ByteArray" (type $ByteArray (array (mut i8))))

  ;; Func imports
  ;; TODO: apply_1 wraps a single result and calls _apply — conceptually
  ;; an apply concern, not a list one. Move to rt/apply.wat.
  (import "rt/apply.wat" "apply_1"
    (func $list_apply_1 (param $val (ref any)) (param $cont (ref null any))))
  (import "std/int.wat"  "fmt" (func $int_fmt (param (ref $Int)) (result (ref $Str))))
  (import "std/str.wat"  "from_bytes" (func $str_from_bytes
    (param (ref $ByteArray)) (result (ref $Str))))
  (import "std/str.wat"  "bytes" (func $str_bytes
    (param (ref $Str)) (result (ref $ByteArray))))


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

  ;; Format a $Range as "start..end" (exclusive) or "start...end"
  ;; (inclusive). Bounds rendered via int.wat:fmt; bytes are
  ;; concatenated locally and wrapped via str.wat:from_bytes.
  (func $fmt (@pub) (param $range (ref $Range)) (result (ref $Str))
    (local $start_str (ref $Str))
    (local $end_str (ref $Str))
    (local $start_bytes (ref $ByteArray))
    (local $end_bytes (ref $ByteArray))
    (local $start_len i32)
    (local $end_len i32)
    (local $dot_len i32)
    (local $total i32)
    (local $buf (ref $ByteArray))
    (local $pos i32)
    (local $i i32)

    (local.set $start_str (call $int_fmt (call $start (local.get $range))))
    (local.set $end_str   (call $int_fmt (call $end   (local.get $range))))

    (local.set $start_bytes (call $str_bytes (local.get $start_str)))
    (local.set $end_bytes   (call $str_bytes (local.get $end_str)))
    (local.set $start_len (array.len (local.get $start_bytes)))
    (local.set $end_len   (array.len (local.get $end_bytes)))

    ;; Dot count: 2 for exclusive, 3 for inclusive.
    (local.set $dot_len
      (if (result i32) (call $is_incl (local.get $range))
        (then (i32.const 3))
        (else (i32.const 2))))

    (local.set $total
      (i32.add (i32.add (local.get $start_len) (local.get $dot_len))
        (local.get $end_len)))
    (local.set $buf (array.new $ByteArray (i32.const 0) (local.get $total)))

    ;; Copy start bytes.
    (local.set $pos (i32.const 0))
    (local.set $i (i32.const 0))
    (block $s_done (loop $s_copy
      (br_if $s_done (i32.ge_u (local.get $i) (local.get $start_len)))
      (array.set $ByteArray (local.get $buf) (local.get $pos)
        (array.get_u $ByteArray (local.get $start_bytes) (local.get $i)))
      (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $s_copy)))

    ;; Write dots: 0x2E = '.'
    (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x2E))
    (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
    (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x2E))
    (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
    (if (i32.eq (local.get $dot_len) (i32.const 3))
      (then
        (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x2E))
        (local.set $pos (i32.add (local.get $pos) (i32.const 1)))))

    ;; Copy end bytes.
    (local.set $i (i32.const 0))
    (block $e_done (loop $e_copy
      (br_if $e_done (i32.ge_u (local.get $i) (local.get $end_len)))
      (array.set $ByteArray (local.get $buf) (local.get $pos)
        (array.get_u $ByteArray (local.get $end_bytes) (local.get $i)))
      (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $e_copy)))

    (return_call $str_from_bytes (local.get $buf)))

)
