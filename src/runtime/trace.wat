;; Trace buffer -- a bounded stack of userland function activations.
;;
;; This is a real backtrace, not a recency log. Each frame is one userland
;; function activation; the live stack of frames is the current call chain.
;; Because Fink compiles every call to a tail call, there is no native wasm
;; call stack to walk; this stack is the portable substitute -- it lives in
;; linear memory so a host can read it even after a hard trap, on any
;; runtime, not just the wasmtime debugger.
;;
;; Three primitives drive it (all carry the full (mid, cid) pair; the
;; redundancy is a dev-time balance check, removable later):
;;   trace_push(mid, cid) -- enter a userland fn defined at (mid, cid):
;;                           push a frame stamped with that identity.
;;   trace_mark(mid, cid) -- a call site (mid, cid) within the current fn:
;;                           update the top frame's current-call-site fields.
;;   trace_pop(mid, cid)  -- leave fn (mid, cid): pop the top frame.
;;
;; Frame = 4 x i32 = 16 bytes: { fn_mid, fn_cid, call_mid, call_cid }.
;;   fn_*   -- the function's identity (stamped by push).
;;   call_* -- where in the function we currently are (set by mark; 0 until
;;             the first call).
;;
;; Bounded window of TRACE_CAP frames. trace_depth is the logical depth and
;; may exceed TRACE_CAP; storage is a ring of TRACE_CAP frames indexed by
;; (depth mod TRACE_CAP), so a push when full overwrites the oldest (bottom,
;; main-ward) frame and a pop at depth 0 is a no-op. The window size is all
;; that bounds how deep the backtrace goes.
;;
;; The user fragment brings memory 0; this module doesn't declare its own,
;; matching interop.wat.

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

  (import "rt/list.wat" "List" (type $List (sub any)))
  (import "rt/list.wat" "empty"
    (func $list_empty (result (ref $List))))
  (import "rt/list.wat" "prepend"
    (func $list_prepend (param (ref any)) (param (ref $List)) (result (ref $List))))

  (import "rt/num.wat" "Num" (type $Num (sub any)))
  (import "rt/int.wat" "Int" (type $Int (sub $Num (struct))))
  (import "rt/int.wat" "I64" (type $I64 (sub $Int (struct (field $ival i64)))))
  (import "rt/int.wat" "_box_i64"
    (func $box_i64 (param i64) (result (ref $I64))))
  (import "rt/int.wat" "_int_ival"
    (func $int_ival (param (ref $Int)) (result i64)))

  ;; Host resolves (module_id, cps_id) -> source line (0 if unknown), via
  ;; the compiled debug marks. Backs the fink-callable get_loc.
  (import "env" "host_resolve_loc"
    (func $host_resolve_loc (param i32) (param i32) (result i32)))

  ;; Window capacity in frames.
  (global $TRACE_CAP i32 (i32.const 64))
  ;; Bytes per frame: 4 x i32 = { fn_mid, fn_cid, call_mid, call_cid }.
  (global $FRAME_BYTES i32 (i32.const 16))
  ;; Byte offset of the frame region (bottom of memory).
  (global $trace_base i32 (i32.const 0))
  ;; Logical stack depth. May exceed TRACE_CAP; storage wraps mod TRACE_CAP.
  (global $trace_depth (mut i32) (i32.const 0))

  ;; Byte address of the frame at logical index `idx` (idx mod TRACE_CAP).
  (func $frame_addr (param $idx i32) (result i32)
    (i32.add
      (global.get $trace_base)
      (i32.mul
        (i32.rem_u (local.get $idx) (global.get $TRACE_CAP))
        (global.get $FRAME_BYTES))))

  ;; Enter a userland fn defined at (mid, cid): push a frame stamped with
  ;; that identity, call site cleared. depth++ (storage wraps when full,
  ;; dropping the oldest frame).
  (func $trace_push (@pub)
      (param $fn_mid i32)
      (param $fn_cid i32)
    (local $addr i32)
    (local.set $addr (call $frame_addr (global.get $trace_depth)))
    (i32.store         (local.get $addr)                  (local.get $fn_mid))
    (i32.store offset=4  (local.get $addr)                (local.get $fn_cid))
    (i32.store offset=8  (local.get $addr) (i32.const 0))  ;; call_mid
    (i32.store offset=12 (local.get $addr) (i32.const 0))  ;; call_cid
    (global.set $trace_depth (i32.add (global.get $trace_depth) (i32.const 1))))

  ;; A call site (mid, cid) within the current fn: update the top frame's
  ;; current-call-site fields. No-op if the stack is empty.
  (func $trace_mark (@pub)
      (param $call_mid i32)
      (param $call_cid i32)
    (local $addr i32)
    (if (i32.eqz (global.get $trace_depth))
      (then (return)))
    (local.set $addr
      (call $frame_addr (i32.sub (global.get $trace_depth) (i32.const 1))))
    (i32.store offset=8  (local.get $addr) (local.get $call_mid))
    (i32.store offset=12 (local.get $addr) (local.get $call_cid)))

  ;; Leave fn (mid, cid): pop the top frame. No-op at depth 0 (a pop of a
  ;; frame that aged out of the bounded window). The (mid, cid) args are
  ;; carried for a future balance assert; unused for now.
  (func $trace_pop (@pub)
      (param $fn_mid i32)
      (param $fn_cid i32)
    (if (i32.eqz (global.get $trace_depth))
      (then (return)))
    (global.set $trace_depth (i32.sub (global.get $trace_depth) (i32.const 1))))

  ;; Read up to `depth` innermost frames as a fink list of
  ;; [call_mid, call_cid] pairs -- the current call site of each live
  ;; function, newest (innermost) first. Walks the stack top-down. A frame
  ;; whose call site is still 0 (entered, not yet at a call) is emitted as
  ;; [fn_mid, fn_cid] instead, so the innermost frame always carries a
  ;; useful location.
  (func $read_trace
      (param $depth i32)
      (result (ref $List))
    (local $n i32)
    (local $avail i32)
    (local $i i32)
    (local $addr i32)
    (local $mid i32)
    (local $cid i32)
    (local $result (ref $List))
    (local $frame (ref $List))

    ;; avail = min(trace_depth, TRACE_CAP); n = min(depth, avail).
    (local.set $avail
      (select (global.get $trace_depth) (global.get $TRACE_CAP)
        (i32.le_u (global.get $trace_depth) (global.get $TRACE_CAP))))
    (local.set $n
      (select (local.get $depth) (local.get $avail)
        (i32.le_u (local.get $depth) (local.get $avail))))

    (local.set $result (call $list_empty))

    ;; Walk bottom-up over the window so that prepending leaves the
    ;; newest (innermost) frame at the head - conventional backtrace order
    ;; (get_trace's own call site first, callers after). The oldest
    ;; in-window frame is logical index (trace_depth - n); the i-th walked
    ;; is (trace_depth - n + i).
    (local.set $i (i32.const 0))
    (block $done (loop $next
      (br_if $done (i32.ge_u (local.get $i) (local.get $n)))

      (local.set $addr
        (call $frame_addr
          (i32.add
            (i32.sub (global.get $trace_depth) (local.get $n))
            (local.get $i))))

      ;; Prefer the current call site; fall back to fn identity if no call
      ;; has been marked yet (call_cid == 0).
      (local.set $cid (i32.load offset=12 (local.get $addr)))
      (if (i32.eqz (local.get $cid))
        (then
          (local.set $mid (i32.load        (local.get $addr)))
          (local.set $cid (i32.load offset=4 (local.get $addr))))
        (else
          (local.set $mid (i32.load offset=8 (local.get $addr)))))

      (local.set $frame
        (call $list_prepend
          (call $box_i64 (i64.extend_i32_u (local.get $mid)))
          (call $list_prepend
            (call $box_i64 (i64.extend_i32_u (local.get $cid)))
            (call $list_empty))))
      (local.set $result
        (call $list_prepend (local.get $frame) (local.get $result)))

      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $next)))

    (local.get $result))

  ;; Fink-callable entry: `get_trace depth`.
  ;; CPS-lowered args = [k_caller, depth]. `depth` arrives as a boxed $Int;
  ;; unbox it, read the stack, tail-call k_caller with the list.
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

  ;; Fink-callable entry: `get_loc mid, cid` -> source line (0 if unknown).
  ;; CPS-lowered args = [k_caller, mid, cid], both boxed $Int.
  (elem declare func $get_loc_apply)

  (func $get_loc_apply (type $Fn3)
      (param $_caps (ref null any))
      (param $ctx (ref null any))
      (param $args (ref null any))
    (local $k_caller (ref any))
    (local $rest (ref null any))
    (local $mid (ref null any))
    (local $cid (ref null any))
    (local $line i32)
    (local $k_args (ref any))

    (local.set $k_caller (ref.as_non_null (call $args_head (local.get $args))))
    (local.set $rest (call $args_tail (local.get $args)))
    (local.set $mid (call $args_head (local.get $rest)))
    (local.set $rest (call $args_tail (local.get $rest)))
    (local.set $cid (call $args_head (local.get $rest)))

    (local.set $line
      (call $host_resolve_loc
        (i32.wrap_i64 (call $int_ival (ref.cast (ref $Int) (local.get $mid))))
        (i32.wrap_i64 (call $int_ival (ref.cast (ref $Int) (local.get $cid))))))

    (local.set $k_args (call $args_empty))
    (local.set $k_args
      (call $args_prepend
        (call $box_i64 (i64.extend_i32_u (local.get $line)))
        (local.get $k_args)))
    (return_call $apply_3
      (local.get $k_args)
      (local.get $ctx)
      (local.get $k_caller)))

  (global $get_loc_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $get_loc_apply)
      (ref.null $Captures)))

  (func $get_loc (@pub)
      (result (ref any))
    (global.get $get_loc_closure))
)
