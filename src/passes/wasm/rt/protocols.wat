;; Operator implementations — CPS functions for arithmetic, comparison, and logic.
;;
;; Each operator follows the CPS calling convention:
;;   (func $op_plus (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
;;     ;; unbox args, compute, box result, tail-call _apply([result], cont)
;;   )
;;
;; Type conventions:
;;   - Numbers: $Num struct (f64 field)
;;   - Booleans: i31ref (0 = false, 1 = true)
;;   - Continuation dispatch via _apply (imported from dispatch module)
;;
;; These are the phase-0 implementations operating on concrete types.
;; Protocol-based overloading (future) will replace these with dispatch
;; through user-defined protocol implementations.

(module

  ;; Continuation dispatch: $apply_1 (defined in list.wat) wraps a single
  ;; result in a list and tail-calls $_apply (defined in dispatch.wat).

  ;; =========================================================================
  ;; Arithmetic: unbox two $Num, f64 op, box result → _apply([result], cont)
  ;; =========================================================================

  (func $rt/protocols.wat:op_plus (export "rt/protocols.wat:op_plus")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (struct.new $Num (f64.add
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $rt/protocols.wat:op_minus (export "rt/protocols.wat:op_minus")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (struct.new $Num (f64.sub
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $rt/protocols.wat:op_mul (export "rt/protocols.wat:op_mul")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (struct.new $Num (f64.mul
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $rt/protocols.wat:op_div (export "rt/protocols.wat:op_div")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (struct.new $Num (f64.div
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  ;; =========================================================================
  ;; Integer arithmetic: unbox $Num → f64 → i64, op, i64 → f64 → box
  ;; =========================================================================

  (func $rt/protocols.wat:op_intdiv (export "rt/protocols.wat:op_intdiv")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (call $int_op_div
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $rt/protocols.wat:op_rem (export "rt/protocols.wat:op_rem")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (call $int_op_rem
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  (func $rt/protocols.wat:op_intmod (export "rt/protocols.wat:op_intmod")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (call $int_op_mod
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  ;; =========================================================================
  ;; Comparison: unbox two $Num, f64 compare → i31ref (0/1)
  ;; =========================================================================

  ;; Direct-style deep equality. Used by HAMT for key comparison.
  ;;   i31ref  → ref.eq (identity — fine for small ints and booleans)
  ;;   $Num    → f64.eq
  ;;   $Str → str_op_eq
  (func $rt/protocols.wat:deep_eq
    (param $a (ref eq)) (param $b (ref eq)) (result i32)

    ;; Try $Num
    (block $not_num
      (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref eq) (ref $Num)
            (local.get $a))))
      (return (f64.eq
        (struct.get $Num $val)
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b))))))

    ;; Try $Str
    (block $not_str
      (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref eq) (ref $Str)
            (local.get $a))))
      (return (call $str_op_eq
        (ref.cast (ref $Str) (local.get $b)))))

    ;; Fallback: ref.eq (i31ref, other GC types)
    (ref.eq (local.get $a) (local.get $b)))

  ;; Polymorphic ==: dispatch on $a's type.
  ;;   $Num    → f64.eq
  ;;   $Str → str_op_eq
  (func $rt/protocols.wat:op_eq (export "rt/protocols.wat:op_eq")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))

    ;; Try $Num
    (block $not_num
      (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref null any) (ref $Num)
            (local.get $a))))
      ;; $a is $Num — cast $b and compare
      (return_call $apply_1
        (ref.i31 (f64.eq
          (struct.get $Num $val)
          (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
        (local.get $cont)))

    ;; Try $Str
    (block $not_str
      (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref null any) (ref $Str)
            (local.get $a))))
      ;; $a is $Str — cast $b and call str_op_eq
      (return_call $apply_1
        (ref.i31 (call $str_op_eq
          (ref.cast (ref $Str) (local.get $b))))
        (local.get $cont)))

    (unreachable))

  ;; Polymorphic !=: dispatch on $a's type.
  ;;   $Num    → f64.ne
  ;;   $Str → !str_op_eq
  (func $rt/protocols.wat:op_neq (export "rt/protocols.wat:op_neq")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))

    ;; Try $Num
    (block $not_num
      (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref null any) (ref $Num)
            (local.get $a))))
      ;; $a is $Num — cast $b and compare
      (return_call $apply_1
        (ref.i31 (f64.ne
          (struct.get $Num $val)
          (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
        (local.get $cont)))

    ;; Try $Str
    (block $not_str
      (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref null any) (ref $Str)
            (local.get $a))))
      ;; $a is $Str — cast $b, call str_op_eq, invert
      (return_call $apply_1
        (ref.i31 (i32.eqz (call $str_op_eq
          (ref.cast (ref $Str) (local.get $b)))))
        (local.get $cont)))

    (unreachable))

  (func $rt/protocols.wat:op_lt (export "rt/protocols.wat:op_lt")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (ref.i31 (f64.lt
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $rt/protocols.wat:op_lte (export "rt/protocols.wat:op_lte")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (ref.i31 (f64.le
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $rt/protocols.wat:op_gt (export "rt/protocols.wat:op_gt")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (ref.i31 (f64.gt
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $rt/protocols.wat:op_gte (export "rt/protocols.wat:op_gte")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (ref.i31 (f64.ge
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  ;; =========================================================================
  ;; Logic / bitwise: polymorphic — $Num → integer bitwise, i31ref → boolean
  ;; =========================================================================

  (func $rt/protocols.wat:op_not (export "rt/protocols.wat:op_not")
    (param $a (ref null any)) (param $cont (ref null any))

    ;; Try $Num → delegate to int_op_not
    (block $not_num
      (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref null any) (ref $Num)
            (local.get $a))))
      (return_call $apply_1
        (call $int_op_not)
        (local.get $cont)))

    ;; Fallback: i31ref boolean not
    (return_call $apply_1
      (ref.i31 (i32.eqz (i31.get_s (ref.cast (ref i31) (local.get $a)))))
      (local.get $cont)))

  (func $rt/protocols.wat:op_and (export "rt/protocols.wat:op_and")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))

    ;; Try $Num → delegate to int_op_and
    (block $not_num
      (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref null any) (ref $Num)
            (local.get $a))))
      (return_call $apply_1
        (call $int_op_and (ref.cast (ref $Num) (local.get $b)))
        (local.get $cont)))

    ;; Fallback: i31ref boolean and
    (return_call $apply_1
      (ref.i31 (i32.and
        (i31.get_s (ref.cast (ref i31) (local.get $a)))
        (i31.get_s (ref.cast (ref i31) (local.get $b)))))
      (local.get $cont)))

  (func $rt/protocols.wat:op_or (export "rt/protocols.wat:op_or")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))

    ;; Try $Num → delegate to int_op_or
    (block $not_num
      (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref null any) (ref $Num)
            (local.get $a))))
      (return_call $apply_1
        (call $int_op_or (ref.cast (ref $Num) (local.get $b)))
        (local.get $cont)))

    ;; Fallback: i31ref boolean or
    (return_call $apply_1
      (ref.i31 (i32.or
        (i31.get_s (ref.cast (ref i31) (local.get $a)))
        (i31.get_s (ref.cast (ref i31) (local.get $b)))))
      (local.get $cont)))

  (func $rt/protocols.wat:op_xor (export "rt/protocols.wat:op_xor")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))

    ;; Try $Num → delegate to int_op_xor
    (block $not_num
      (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref null any) (ref $Num)
            (local.get $a))))
      (return_call $apply_1
        (call $int_op_xor (ref.cast (ref $Num) (local.get $b)))
        (local.get $cont)))

    ;; Fallback: i31ref boolean xor
    (return_call $apply_1
      (ref.i31 (i32.xor
        (i31.get_s (ref.cast (ref i31) (local.get $a)))
        (i31.get_s (ref.cast (ref i31) (local.get $b)))))
      (local.get $cont)))

  ;; =========================================================================
  ;; Type guards — is_seq_like / is_rec_like
  ;; =========================================================================

  ;; is_seq_like(val, succ, fail): succ(val) if $List, else fail()
  (func $rt/protocols.wat:is_seq_like (export "rt/protocols.wat:is_seq_like")
    (param $val (ref null any)) (param $succ (ref null any)) (param $fail (ref null any))
    (block $not_seq
      (block $is_seq (result (ref $List))
        (br $not_seq
          (br_on_cast $is_seq (ref null any) (ref $List)
            (local.get $val))))
      (drop)
      (return_call $apply_1 (local.get $val) (local.get $succ)))
    (return_call $apply_0 (local.get $fail)))

  ;; is_rec_like(val, succ, fail): succ(val) if $Rec, else fail()
  (func $rt/protocols.wat:is_rec_like (export "rt/protocols.wat:is_rec_like")
    (param $val (ref null any)) (param $succ (ref null any)) (param $fail (ref null any))
    (block $not_rec
      (block $is_rec (result (ref $Rec))
        (br $not_rec
          (br_on_cast $is_rec (ref null any) (ref $Rec)
            (local.get $val))))
      (drop)
      (return_call $apply_1 (local.get $val) (local.get $succ)))
    (return_call $apply_0 (local.get $fail)))

  ;; =========================================================================
  ;; Collection predicates (polymorphic — dispatch on type tag)
  ;; =========================================================================

  ;; Polymorphic empty: dispatch on value type to module predicates.
  ;;   null     → true (always empty)
  ;;   $List    → list_op_empty
  ;;   $Rec     → rec_op_empty
  (func $rt/protocols.wat:op_empty (export "rt/protocols.wat:op_empty")
    (param $val (ref null any)) (param $cont (ref null any))

    ;; null = empty
    (if (ref.is_null (local.get $val))
      (then
        (return_call $apply_1
          (ref.i31 (i32.const 1))
          (local.get $cont))))

    ;; $List → list_op_empty
    (block $not_list
      (block $is_list (result (ref $List))
        (br $not_list
          (br_on_cast $is_list (ref null any) (ref $List)
            (local.get $val))))
      (drop)
      (return_call $apply_1
        (ref.i31 (call $list_op_empty (local.get $val)))
        (local.get $cont)))

    ;; $Rec → rec_op_empty
    (block $not_rec
      (block $is_rec (result (ref $Rec))
        (br $not_rec
          (br_on_cast $is_rec (ref null any) (ref $Rec)
            (local.get $val))))
      (drop)
      (return_call $apply_1
        (ref.i31 (call $rec_op_empty (local.get $val)))
        (local.get $cont)))

    (unreachable))

  ;; =========================================================================
  ;; Membership: `in` / `not in` — dispatch on container type
  ;; =========================================================================

  ;; op_in(val, container, cont) → bool
  (func $rt/protocols.wat:op_in (export "rt/protocols.wat:op_in")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (local $range (ref $Range))
    (local $rec (ref $RecImpl))

    ;; Try $Range
    (block $not_range
      (block $is_range (result (ref $Range))
        (br $not_range
          (br_on_cast $is_range (ref null any) (ref $Range)
            (local.get $b))))
      (local.set $range)
      (return_call $apply_1
        (ref.i31 (call $range_op_in
          (ref.cast (ref $Num) (local.get $a))
          (local.get $range)))
        (local.get $cont)))

    ;; Try $Rec
    (block $not_rec
      (block $is_rec (result (ref $RecImpl))
        (br $not_rec
          (br_on_cast $is_rec (ref null any) (ref $RecImpl)
            (local.get $b))))
      (local.set $rec)
      (return_call $apply_1
        (ref.i31 (call $rec_op_in
          (local.get $rec)
          (ref.cast (ref eq) (local.get $a))))
        (local.get $cont)))

    (unreachable))

  ;; op_notin(val, container, cont) → bool
  (func $rt/protocols.wat:op_notin (export "rt/protocols.wat:op_notin")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (local $range (ref $Range))
    (local $rec (ref $RecImpl))

    ;; Try $Range
    (block $not_range
      (block $is_range (result (ref $Range))
        (br $not_range
          (br_on_cast $is_range (ref null any) (ref $Range)
            (local.get $b))))
      (local.set $range)
      (return_call $apply_1
        (ref.i31 (call $range_op_not_in
          (ref.cast (ref $Num) (local.get $a))
          (local.get $range)))
        (local.get $cont)))

    ;; Try $Rec
    (block $not_rec
      (block $is_rec (result (ref $RecImpl))
        (br $not_rec
          (br_on_cast $is_rec (ref null any) (ref $RecImpl)
            (local.get $b))))
      (local.set $rec)
      (return_call $apply_1
        (ref.i31 (call $rec_op_not_in
          (local.get $rec)
          (ref.cast (ref eq) (local.get $a))))
        (local.get $cont)))

    (unreachable))

  ;; =========================================================================
  ;; Member access: `.` — dispatch on container type
  ;; =========================================================================

  ;; op_dot(container, key, cont) → val
  ;;   $Str → str_op_dot
  ;;   $Rec → rec_op_dot
  (func $rt/protocols.wat:op_dot (export "rt/protocols.wat:op_dot")
    (param $container (ref null any)) (param $key (ref null any)) (param $cont (ref null any))

    ;; Try $Str
    (block $not_str
      (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref null any) (ref $Str)
            (local.get $container))))
      (drop)
      (return_call $str_op_dot
        (local.get $container)
        (local.get $key)
        (local.get $cont)))

    ;; Try $Rec
    (block $not_rec
      (block $is_rec (result (ref $RecImpl))
        (br $not_rec
          (br_on_cast $is_rec (ref null any) (ref $RecImpl)
            (local.get $container))))
      (drop)
      (return_call $rec_op_dot
        (local.get $container)
        (local.get $key)
        (local.get $cont)))

    (unreachable))

  ;; =========================================================================
  ;; Shift left: `<<` — polymorphic ($Num → bitwise, $Channel → send)
  ;; =========================================================================

  ;; op_shl(a, b, cont):
  ;;   $HostChannel on a → interop_channel_send(a, b, cont)
  ;;   $Channel on a     → channel_op_shr(a, b, cont)  [ch << msg]
  ;;   otherwise         → int_op_shl(a, b)  [numeric shift]
  ;; NB: $HostChannel check must come before $Channel (subtype).
  (func $rt/protocols.wat:op_shl (export "rt/protocols.wat:op_shl")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))

    ;; Try $HostChannel on a → host channel send
    (block $not_host_channel
      (block $is_host_channel (result (ref $HostChannel))
        (br $not_host_channel
          (br_on_cast $is_host_channel (ref null any) (ref $HostChannel)
            (local.get $a))))
      (drop)
      (return_call $interop_channel_send
        (local.get $a)
        (local.get $b)
        (local.get $cont)))

    ;; Try $Channel on a → channel send
    (block $not_channel
      (block $is_channel (result (ref $Channel))
        (br $not_channel
          (br_on_cast $is_channel (ref null any) (ref $Channel)
            (local.get $a))))
      (drop)
      (return_call $channel_op_shr
        (local.get $a)
        (local.get $b)
        (local.get $cont)))

    ;; Fallback: numeric shift left
    (return_call $apply_1
      (call $int_op_shl
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))

  ;; =========================================================================
  ;; Shift right: `>>` — polymorphic ($Num → bitwise, $Channel → send)
  ;; =========================================================================

  ;; op_shr(a, b, cont):
  ;;   $HostChannel on b → interop_channel_send(b, a, cont)
  ;;   $Channel on b     → channel_op_shr(b, a, cont)  [msg >> ch]
  ;;   otherwise         → int_op_shr(a, b)  [numeric shift]
  ;; NB: $HostChannel check must come before $Channel (subtype).
  (func $rt/protocols.wat:op_shr (export "rt/protocols.wat:op_shr")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))

    ;; Try $HostChannel on b → host channel send
    (block $not_host_channel
      (block $is_host_channel (result (ref $HostChannel))
        (br $not_host_channel
          (br_on_cast $is_host_channel (ref null any) (ref $HostChannel)
            (local.get $b))))
      (drop)
      (return_call $interop_channel_send
        (local.get $b)
        (local.get $a)
        (local.get $cont)))

    ;; Try $Channel on b → channel send
    (block $not_channel
      (block $is_channel (result (ref $Channel))
        (br $not_channel
          (br_on_cast $is_channel (ref null any) (ref $Channel)
            (local.get $b))))
      (drop)
      (return_call $channel_op_shr
        (local.get $b)
        (local.get $a)
        (local.get $cont)))

    ;; Fallback: numeric shift right
    (return_call $apply_1
      (call $int_op_shr
        (ref.cast (ref $Num) (local.get $a))
        (ref.cast (ref $Num) (local.get $b)))
      (local.get $cont)))


  ;; =========================================================================
  ;; receive — polymorphic ($HostChannel → host recv, $Channel → channel recv)
  ;; =========================================================================

  ;; receive(ch, cont):
  ;;   $HostChannel → interop_channel_recv(ch, cont)
  ;;   $Channel     → channel_receive(ch, cont)
  ;; NB: $HostChannel check must come before $Channel (subtype).
  (func $rt/protocols.wat:receive (export "rt/protocols.wat:receive")
    (param $ch (ref null any)) (param $cont (ref null any))

    ;; Try $HostChannel → host channel receive
    (block $not_host_channel
      (block $is_host_channel (result (ref $HostChannel))
        (br $not_host_channel
          (br_on_cast $is_host_channel (ref null any) (ref $HostChannel)
            (local.get $ch))))
      (drop)
      (return_call $interop_channel_recv
        (local.get $ch)
        (local.get $cont)))

    ;; Fallback: regular channel receive
    (return_call $channel_receive
      (local.get $ch)
      (local.get $cont)))


  ;; =========================================================================
  ;; read — async read from a stream
  ;; =========================================================================

  ;; op_read(stream, size, cont):
  ;;   Dispatches to interop_op_read for host channels.
  (func $rt/protocols.wat:op_read (export "rt/protocols.wat:op_read")
    (param $stream (ref null any))
    (param $size (ref null any))
    (param $cont (ref null any))

    (return_call $interop_op_read
      (local.get $stream)
      (local.get $size)
      (local.get $cont)))


  ;; =========================================================================
  ;; panic — irrefutable pattern failure
  ;; =========================================================================
  ;;
  ;; Signature matches the universal closure calling convention so `_apply`
  ;; can dispatch to it like any other continuation: panic is used both as
  ;; a direct tail-call (terminal of a fail chain) and as a $Closure value
  ;; passed as a fail continuation to pattern matchers.
  ;;
  ;; Delegates to $interop_panic, which calls into the host to trap the
  ;; instance with a diagnostic message. Today panic carries no payload —
  ;; future work will pass a reason / source location for better diagnostics.
  (func $rt/protocols.wat:panic (export "rt/protocols.wat:panic")
    (param $_caps (ref null any))
    (param $_args (ref null any))
    (return_call $interop_panic))


  ;; =========================================================================
  ;; stdio protocols — exposed to user code via `import 'std/io.fnk'`
  ;; =========================================================================
  ;;
  ;; The `std/io.fnk` virtual namespace is resolved at compile time to
  ;; these qualified exports. Each is a no-arg function returning the
  ;; protocol value; `ir_lower::lower_import` imports them under the
  ;; qualified names and binds the result of calling each into the
  ;; user's destructure rec.
  ;;
  ;; The trampolines below are the **per-target dispatch table**: each
  ;; routes a user-facing protocol (`std/io.fnk:foo`) to a stable
  ;; cross-target ABI slot (`interop_io_get_foo`). Whichever
  ;; `interop/<target>.wat` is linked fills those slots — today only
  ;; `interop/rust.wat`; a future `interop/wasi.wat` would provide the
  ;; same slot names with a different impl. ir_lower never sees the
  ;; target choice — it always emits `std/io.fnk:foo` imports.
  ;;
  ;; The pattern generalises beyond stdio. See
  ;; [project_protocol_dispatch_pattern.md] in the brain memory.

  (func (export "std/io.fnk:stdout") (result (ref any))
    (return_call $interop/io:get_stdout))

  (func (export "std/io.fnk:stderr") (result (ref any))
    (return_call $interop/io:get_stderr))

  (func (export "std/io.fnk:stdin") (result (ref any))
    (return_call $interop/io:get_stdin))

  ;; std/io.fnk:read — host-coupled async read. Returns a `$Closure`
  ;; (callable via `_apply`) wrapping the host's read primitive. The
  ;; closure construction + Fn2 adapter live in interop/rust.wat
  ;; (with the rest of the host-bridge plumbing); this file just
  ;; routes the protocol export name.
  (func (export "std/io.fnk:read") (result (ref any))
    (return_call $interop/io:get_read))

)
