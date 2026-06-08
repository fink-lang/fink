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
)
