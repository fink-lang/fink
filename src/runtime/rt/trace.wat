;; Trace buffer -- a fixed-size ring of recent user-fn call sites.
;;
;; Every user-function call site emits a trace_push before dispatching.
;; The push records the call site into a ring in linear memory,
;; overwriting the oldest entry once full. Because Fink compiles every
;; call to a tail call, there is no native wasm call stack to walk; this
;; ring is the portable substitute -- it lives in linear memory so a host
;; can read it even after a hard trap, on any runtime, not just the
;; wasmtime debugger.
;;
;; Layout: the ring occupies the reserved region at the bottom of linear
;; memory, [0, trace_len*8). emit.rs owns the linear-memory map and keeps
;; this region clear of the literal data pool. Each slot is two i32s --
;; module_id then cps_id. Together they identify a call site package-wide
;; -- cps_id is only unique per module.
;;
;; trace_next is the slot index of the next write; it wraps at trace_len.
;; The reader derives the valid range from it. Slot count and base are
;; compile-time constants here -- clarity over micro-opt for now; an
;; optimizer can inline/specialise later.
;;
;; The user fragment brings memory 0; this module doesn't declare its
;; own, matching interop.wat.

(module

  (import "rt/apply.wat" "Fn3"      (type $Fn3      (sub any)))
  (import "rt/apply.wat" "Closure"  (type $Closure  (sub any)))
  (import "rt/apply.wat" "Captures" (type $Captures (sub any)))
  (import "rt/apply.wat" "args_empty"
    (func $args_empty (result (ref any))))
  (import "rt/apply.wat" "args_prepend"
    (func $args_prepend (param (ref null any)) (param (ref any)) (result (ref any))))
  (import "rt/apply.wat" "args_head"
    (func $args_head (param (ref null any)) (result (ref null any))))
  (import "rt/apply.wat" "args_tail"
    (func $args_tail (param (ref null any)) (result (ref null any))))
  (import "rt/apply.wat" "apply_3"
    (func $apply_3
      (param (ref null any)) (param (ref null any)) (param (ref null any))))

  (import "std/list.wat" "List" (type $List (sub any)))
  (import "std/list.wat" "empty"
    (func $list_empty (result (ref $List))))
  (import "std/list.wat" "prepend"
    (func $list_prepend (param (ref any)) (param (ref $List)) (result (ref $List))))

  (import "std/num.wat" "Num" (type $Num (sub any)))
  (import "std/int.wat" "Int" (type $Int (sub $Num (struct))))
  (import "std/int.wat" "I64" (type $I64 (sub $Int (struct (field $ival i64)))))
  (import "std/int.wat" "_box_i64"
    (func $box_i64 (param i64) (result (ref $I64))))
  (import "std/int.wat" "_int_ival"
    (func $int_ival (param (ref $Int)) (result i64)))

  ;; Ring capacity in slots. 64 frames.
  (global $trace_len i32 (i32.const 64))
  ;; Byte offset of the ring region (bottom of memory).
  (global $trace_base i32 (i32.const 0))
  ;; Slot index of the next write. Wraps at trace_len.
  (global $trace_next (mut i32) (i32.const 0))

  ;; Record a call site into the ring at trace_next, then advance
  ;; trace_next modulo trace_len. Args: module_id, cps_id.
  (func $trace_push (@pub)
      (param $module_id i32)
      (param $cps_id i32)
    (local $slot_addr i32)

    ;; slot_addr = trace_base + trace_next * 8
    (local.set $slot_addr
      (i32.add
        (global.get $trace_base)
        (i32.mul (global.get $trace_next) (i32.const 8))))

    ;; mem[slot_addr]   = module_id
    (i32.store (local.get $slot_addr) (local.get $module_id))
    ;; mem[slot_addr+4] = cps_id
    (i32.store
      (i32.add (local.get $slot_addr) (i32.const 4))
      (local.get $cps_id))

    ;; trace_next = (trace_next + 1) mod trace_len
    (global.set $trace_next
      (i32.rem_u
        (i32.add (global.get $trace_next) (i32.const 1))
        (global.get $trace_len))))

  ;; Read up to `depth` most-recent call sites as a fink list of
  ;; [module_id, cps_id] pairs. Walks back from trace_next (the most
  ;; recent write) toward older entries, stopping at the first unwritten
  ;; slot (cps_id == 0) or after `depth` entries. Each frame is prepended
  ;; as it is read, so the resulting list runs oldest-to-newest (the
  ;; newest call site is the last element). Raw i32 worker; the fink-
  ;; callable entry is the Fn3 wrapper below.
  (func $read_trace
      (param $depth i32)
      (result (ref $List))
    (local $n i32)
    (local $i i32)
    (local $slot i32)
    (local $slot_addr i32)
    (local $mid i32)
    (local $cid i32)
    (local $result (ref $List))
    (local $frame (ref $List))

    ;; n = min(depth, trace_len)
    (local.set $n
      (select
        (local.get $depth)
        (global.get $trace_len)
        (i32.le_u (local.get $depth) (global.get $trace_len))))

    (local.set $result (call $list_empty))

    ;; Walk back from the newest write. The i-th-newest slot is
    ;; (trace_next - 1 - i) mod trace_len. Add trace_len before the rem to
    ;; keep the operand non-negative.
    (local.set $i (i32.const 0))
    (block $done (loop $next
      (br_if $done (i32.ge_u (local.get $i) (local.get $n)))

      (local.set $slot
        (i32.rem_u
          (i32.add
            (i32.sub (global.get $trace_next) (i32.const 1))
            (i32.sub (global.get $trace_len) (local.get $i)))
          (global.get $trace_len)))

      (local.set $slot_addr
        (i32.add
          (global.get $trace_base)
          (i32.mul (local.get $slot) (i32.const 8))))

      (local.set $mid (i32.load (local.get $slot_addr)))
      (local.set $cid (i32.load (i32.add (local.get $slot_addr) (i32.const 4))))

      ;; Stop at the first unwritten slot (cps_id == 0).
      (br_if $done (i32.eqz (local.get $cid)))

      ;; frame = [module_id, cps_id]
      (local.set $frame
        (call $list_prepend
          (call $box_i64 (i64.extend_i32_u (local.get $mid)))
          (call $list_prepend
            (call $box_i64 (i64.extend_i32_u (local.get $cid)))
            (call $list_empty))))
      ;; prepend frame; walking newest-first means the result ends up
      ;; oldest-to-newest.
      (local.set $result
        (call $list_prepend (local.get $frame) (local.get $result)))

      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $next)))

    (local.get $result))

  ;; Fink-callable entry: `get_trace depth`.
  ;; CPS-lowered args = [k_caller, depth]. `depth` arrives as a boxed
  ;; $Int; unbox it, read the ring, tail-call k_caller with the list.
  (elem declare func $get_trace_apply)

  (func $get_trace_apply (type $Fn3)
      (param $_caps (ref null any))
      (param $ctx (ref null any))
      (param $args (ref null any))
    (local $k_caller (ref any))
    (local $depth (ref null any))
    (local $rest (ref null any))
    (local $trace (ref $List))
    (local $k_args (ref any))

    (local.set $k_caller (ref.as_non_null (call $args_head (local.get $args))))
    (local.set $rest (call $args_tail (local.get $args)))
    (local.set $depth (call $args_head (local.get $rest)))

    (local.set $trace
      (call $read_trace
        (i32.wrap_i64
          (call $int_ival (ref.cast (ref $Int) (local.get $depth))))))

    (local.set $k_args (call $args_empty))
    (local.set $k_args (call $args_prepend (local.get $trace) (local.get $k_args)))
    (return_call $apply_3
      (local.get $k_args)
      (local.get $ctx)
      (local.get $k_caller)))

  (global $get_trace_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $get_trace_apply)
      (ref.null $Captures)))

  (func $get_trace (@pub)
      (result (ref any))
    (global.get $get_trace_closure))
)
