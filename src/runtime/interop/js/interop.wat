;; JS host interop — minimal scaffold.
;;
;; First slice: link the runtime against a JS-target interop module
;; without yet implementing host-bridge behaviour. Every contract-side
;; function is `unreachable`; calling them traps the instance. The
;; runtime contract surface (HostChannel type, host imports, the named
;; @impl bindings) is preserved so the linker and validator are happy.
;;
;; Real bodies land in subsequent slices per
;; .brain/.scratch/plans/js-interop-plan.md.
;;
;; type_of is the one already-real export — JS hosts use it to
;; discriminate values.
;;
;; Type-of enum (matches fink.js):
;;   Fn    = 100
;;   Num   = 200  (parent slot, currently unreachable in this codebase)
;;   Int   = 220  (reserved for future small-int unboxing)
;;   Float = 250
;;   Bool  = 300
;;   List  = 400
;;   Rec   = 500
;;   Str   = 600
;;   Other = 0

(module
  (import "rt/apply.wat"    "Fn3"      (type $Fn3      (sub any)))
  (import "rt/apply.wat"    "Closure"  (type $Closure  (sub any)))
  (import "rt/apply.wat"    "Captures" (type $Captures (sub any)))

  ;; Anchor imports: the linker prunes modules unreachable from the
  ;; import graph. Mirror rust/interop.wat's pulls on rt/protocols.wat
  ;; and rt/modules.wat so their @impl bindings end up in the linked
  ;; output regardless of target.
  (import "rt/protocols.wat" "deep_eq"
    (func $deep_eq (param (ref eq)) (param (ref eq)) (result i32)))
  (import "rt/modules.wat"   "init"
    (func $modules_init (param (ref null any)) (result i32)))

  (import "std/num.wat"     "Num"       (type $Num       (sub any)))
  (import "std/int.wat"     "Int"       (type $Int       (sub any)))
  (import "std/int.wat"     "I64"       (type $I64       (sub $Int)))
  (import "std/int.wat"     "U64"       (type $U64       (sub $Int)))
  (import "std/float.wat"   "F64"       (type $F64       (sub $Num)))
  (import "std/decimal.wat" "Decimal"   (type $Decimal   (sub $Num)))
  (import "std/str.wat"     "Str"       (type $Str       (sub any)))
  (import "std/str.wat"     "ByteArray" (type $ByteArray (sub any)))
  (import "std/list.wat"    "List"      (type $List      (sub any)))
  (import "std/dict.wat"    "Rec"       (type $Rec       (sub any)))

  (import "std/str.wat" "_str_wrap_bytes"
    (func $str_wrap_bytes (param $bytes (ref null any)) (result (ref any))))
  (import "std/str.wat" "bytes"
    (func $str_bytes (param $s (ref $Str)) (result (ref $ByteArray))))

  (import "std/list.wat" "head_any"
    (func $list_head_any (param $list (ref null any)) (result (ref null any))))
  (import "std/list.wat" "tail_any"
    (func $list_tail_any (param $list (ref null any)) (result (ref null any))))
  (import "std/list.wat" "size"
    (func $list_size_inner (param $list (ref $List)) (result i32)))
  (import "std/list.wat" "empty"
    (func $list_empty_inner (result (ref $List))))
  (import "std/list.wat" "prepend"
    (func $list_prepend_inner
      (param $head (ref any)) (param $tail (ref $List)) (result (ref $List))))

  ;; Args list helpers — wat-level args list is its own ABI, distinct
  ;; from $List. JS-side apply needs to build one.
  (import "rt/apply.wat" "args_empty"
    (func $args_empty_inner (result (ref any))))
  (import "rt/apply.wat" "args_prepend"
    (func $args_prepend_inner
      (param $head (ref null any)) (param $tail (ref any)) (result (ref any))))
  (import "rt/apply.wat" "args_head"
    (func $args_head_inner (param $args (ref null any)) (result (ref null any))))
  (import "rt/apply.wat" "args_tail"
    (func $args_tail_inner (param $args (ref null any)) (result (ref null any))))
  (import "rt/apply.wat" "apply_3"
    (func $apply_3_inner
      (param $args (ref null any))
      (param $ctx (ref null any))
      (param $callee (ref null any))))
  (import "rt/apply.wat" "empty_ctx"
    (func $empty_ctx_inner (result (ref any))))
  (import "rt/apply.wat" "set_ctx"
    (func $set_ctx_inner (result (ref any))))
  (import "rt/apply.wat" "get_ctx"
    (func $get_ctx_inner (result (ref any))))

  ;; std/dict.wat:get is typed for the concrete $RecImpl subtype; JS only
  ;; ever holds opaque (ref any), so we wrap with a JS-friendly shim
  ;; (rec_get below) that does the cast.
  (import "std/dict.wat" "RecImpl" (type $RecImpl (sub any)))
  (import "std/dict.wat" "get"
    (func $rec_get_inner (param $rec (ref $RecImpl)) (param $key (ref eq))
      (result (ref null eq))))

  ;; Host imports — stubbed by fink.js. Signatures must match
  ;; rust/interop.wat so runtime modules importing this contract see
  ;; the same shapes regardless of target.
  ;; host_channel_send(tag, ptr, len): JS reads `len` UTF-8 bytes
  ;; starting at offset `ptr` in linear memory. Tag selects routing
  ;; (1 = stdout/console.log, 2 = stderr/console.error). Differs from
  ;; the rust-side import (which passes a $ByteArray ref) because JS
  ;; can't read GC arrays directly — copying into linear memory first
  ;; gives JS a TextDecoder-friendly window.
  (import "env" "host_read"         (func $host_read         (param (ref any) (ref any) (ref any))))
  ;; host_invoke_cont: dispatch a JS-side cont. The first arg is the
  ;; opaque externref the host originally handed to wrap_host_cont — JS
  ;; uses it directly (call it, look it up, whatever) to find the
  ;; callback. No wasm-side id table.
  (import "env" "host_invoke_cont"  (func $host_invoke_cont  (param externref (ref null any))))

  ;; Host yields control to the userland scheduler when its queue is
  ;; empty. JS host stashes the `resume` closure (rooted) and calls
  ;; `_invoke_resume` later when ready to re-enter the scheduler.
  (import "env" "host_yield" (func $host_yield (param (ref any)) (param (ref null any))))

  (import "env" "host_write" (func $host_write
    (param $fd (ref null any))
    (param $bytes (ref $ByteArray))))

  (import "env" "host_read_sync" (func $host_read_sync
    (param $fd (ref null any))
    (param $size (ref null any))
    (result (ref $ByteArray))))


  (func $interop_invoke_resume (@pub) (export "env:invoke_resume")
    (param $resume (ref any))
    (param $value (ref any))
    (param $ctx (ref null any))
    (local $args (ref any))
    (local.set $args (call $args_empty_inner))
    (local.set $args (call $args_prepend_inner (local.get $value) (local.get $args)))
    (return_call $apply_3_inner
      (local.get $args)
      (local.get $ctx)
      (local.get $resume)))


  (elem declare func $interop_yield_apply)

  (func $interop_yield_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $resume (ref any))
    (local.set $resume (ref.as_non_null
      (call $args_head_inner (local.get $args))))
    (return_call $host_yield (local.get $resume) (local.get $ctx)))

  (global $interop_yield_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $interop_yield_apply)
      (ref.null $Captures)))

  (func $interop_yield (@pub) (@impl "interop.fnk:yield")
    (result (ref any))
    (global.get $interop_yield_closure))


  (elem declare func $io_write_apply)

  (func $io_write_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $k_caller (ref any))
    (local $fd (ref null any))
    (local $msg (ref null any))
    (local $bytes (ref $ByteArray))
    (local $rest (ref null any))
    (local $k_args (ref any))

    (local.set $k_caller (ref.as_non_null (call $args_head_inner (local.get $args))))
    (local.set $rest (call $args_tail_inner (local.get $args)))
    (local.set $fd (call $args_head_inner (local.get $rest)))
    (local.set $rest (call $args_tail_inner (local.get $rest)))
    (local.set $msg (call $args_head_inner (local.get $rest)))

    (local.set $bytes
      (call $str_bytes (ref.cast (ref $Str) (local.get $msg))))

    (call $host_write
      (local.get $fd)
      (local.get $bytes))

    (local.set $k_args (call $args_empty_inner))
    (local.set $k_args (call $args_prepend_inner (ref.i31 (i32.const 0)) (local.get $k_args)))
    (return_call $apply_3_inner
      (local.get $k_args)
      (local.get $ctx)
      (local.get $k_caller)))

  (global $io_write_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $io_write_apply)
      (ref.null $Captures)))

  (func $io_write (@pub) (@impl "interop.fnk:io_write")
    (result (ref any))
    (global.get $io_write_closure))


  (elem declare func $io_read_apply)

  (func $io_read_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $k_caller (ref any))
    (local $fd (ref null any))
    (local $size (ref null any))
    (local $bytes (ref $ByteArray))
    (local $str (ref any))
    (local $rest (ref null any))
    (local $k_args (ref any))

    (local.set $k_caller (ref.as_non_null (call $args_head_inner (local.get $args))))
    (local.set $rest (call $args_tail_inner (local.get $args)))
    (local.set $fd (call $args_head_inner (local.get $rest)))
    (local.set $rest (call $args_tail_inner (local.get $rest)))
    (local.set $size (call $args_head_inner (local.get $rest)))

    (local.set $bytes
      (call $host_read_sync (local.get $fd) (local.get $size)))
    (local.set $str (call $str_wrap_bytes (local.get $bytes)))

    (local.set $k_args (call $args_empty_inner))
    (local.set $k_args (call $args_prepend_inner (local.get $str) (local.get $k_args)))
    (return_call $apply_3_inner
      (local.get $k_args)
      (local.get $ctx)
      (local.get $k_caller)))

  (global $io_read_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $io_read_apply)
      (ref.null $Captures)))

  (func $io_read (@pub) (@impl "interop.fnk:io_read")
    (result (ref any))
    (global.get $io_read_closure))


  ;; -- type_of -----------------------------------------------------------
  ;;
  ;; Discriminate a runtime value for a JS host. Bools are i31ref today
  ;; (small ints are still boxed as $Num), so the i31 branch returns
  ;; Bool, not Int. When small-int unboxing lands, this branch will need
  ;; to split on the i31 value range.
  (func $type_of (@pub) (export "env:type_of")
    (param $v (ref null any)) (result i32)
    (local $nn (ref any))

    (if (ref.is_null (local.get $v)) (then (return (i32.const 0))))
    (local.set $nn (ref.as_non_null (local.get $v)))

    (if (ref.test (ref i31)      (local.get $nn)) (then (return (i32.const 300))))
    (if (ref.test (ref $Str)     (local.get $nn)) (then (return (i32.const 600))))
    (if (ref.test (ref $Num)     (local.get $nn)) (then (return (i32.const 250))))
    (if (ref.test (ref $List)    (local.get $nn)) (then (return (i32.const 400))))
    (if (ref.test (ref $Rec)     (local.get $nn)) (then (return (i32.const 500))))
    (if (ref.test (ref $Closure) (local.get $nn)) (then (return (i32.const 100))))

    (i32.const 0)
  )


  ;; -- bytes_from_js / str_from_js ---------------------------------------
  ;;
  ;; Copy UTF-8 bytes the JS host wrote into linear memory at offset
  ;; `ptr`, length `len`, into a fresh GC $ByteArray. The linear-memory
  ;; window can be overwritten by the caller immediately after.
  ;;
  ;; The user fragment exports `memory` (memory 0) — interop.wat doesn't
  ;; declare its own. JS reads/writes `instance.exports.memory.buffer`.
  ;;
  ;; bytes_from_js: returns the raw $ByteArray ref. JS hands this to the
  ;;   per-module host wrapper (which expects a $ByteArray key).
  ;; str_from_js: bytes_from_js + _str_wrap_bytes, returning a $Str ref.

  ;; Scratch buffer offset for host<->wasm byte copying. Sits high in
  ;; the first 64KB page so it doesn't collide with the user fragment's
  ;; data segments (string literals etc., which start at offset 0). Both
  ;; wat-side helpers and fink.js use this same offset for their
  ;; transient buffers.
  ;;
  ;; If a single message is > 16KB (page_size - 0xC000 = 16KB) this will
  ;; overflow the page; future work: grow memory dynamically or stream.
  (global $SCRATCH_BASE (export "env:SCRATCH_BASE") i32 (i32.const 0xC000))

  (func $bytes_from_js (@pub) (export "env:bytes_from_js")
    (param $ptr i32) (param $len i32) (result (ref any))

    (local $bytes (ref $ByteArray))
    (local $i i32)

    (local.set $bytes (array.new $ByteArray (i32.const 0) (local.get $len)))
    (local.set $i (i32.const 0))
    (block $done (loop $copy
      (br_if $done (i32.ge_s (local.get $i) (local.get $len)))
      (array.set $ByteArray
        (local.get $bytes)
        (local.get $i)
        (i32.load8_u (i32.add (local.get $ptr) (local.get $i))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $copy)
    ))

    (local.get $bytes)
  )

  (func $str_from_js (@pub) (export "env:str_from_js")
    (param $ptr i32) (param $len i32) (result (ref any))

    (return_call $str_wrap_bytes
      (call $bytes_from_js (local.get $ptr) (local.get $len)))
  )


  ;; -- str_to_js ---------------------------------------------------------
  ;;
  ;; Copy a $Str's bytes into linear memory at offset `ptr`. Returns the
  ;; byte length written. JS reads the window via TextDecoder. Caller is
  ;; responsible for ensuring `ptr` + len fits in memory.

  (func $str_to_js (@pub) (export "env:str_to_js")
    (param $s (ref any)) (param $ptr i32) (result i32)

    (local $bytes (ref $ByteArray))
    (local $len i32)
    (local $i i32)

    (local.set $bytes (call $str_bytes (ref.cast (ref $Str) (local.get $s))))
    (local.set $len (array.len (local.get $bytes)))
    (local.set $i (i32.const 0))
    (block $done (loop $copy
      (br_if $done (i32.ge_s (local.get $i) (local.get $len)))
      (i32.store8
        (i32.add (local.get $ptr) (local.get $i))
        (array.get_u $ByteArray (local.get $bytes) (local.get $i)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $copy)
    ))
    (local.get $len)
  )


  ;; -- list / rec accessors ---------------------------------------------
  ;;
  ;; Thin (ref any)-typed wrappers around std/list.wat and std/dict.wat
  ;; helpers. JS holds opaque refs; these wrappers do the cast and
  ;; forward.

  (func $list_head (@pub) (export "env:list_head")
    (param $list (ref any)) (result (ref null any))
    (return_call $list_head_any (local.get $list)))

  (func $list_tail (@pub) (export "env:list_tail")
    (param $list (ref any)) (result (ref null any))
    (return_call $list_tail_any (local.get $list)))

  (func $list_size (@pub) (export "env:list_size")
    (param $list (ref any)) (result i32)
    (return_call $list_size_inner (ref.cast (ref $List) (local.get $list))))

  (func $rec_get (@pub) (export "env:rec_get")
    (param $rec (ref any)) (param $key (ref any)) (result (ref null any))
    (return_call $rec_get_inner
      (ref.cast (ref $RecImpl) (local.get $rec))
      (ref.cast (ref eq) (local.get $key))))


  ;; -- num_to_js / num_from_js -------------------------------------------
  ;;
  ;; Unwrap a $Num to its f64 value (num_to_js); construct a fresh $Num
  ;; from a JS number (num_from_js). i31 bools are not handled here —
  ;; they need separate i31_to_js / i31_from_js once bool wrapping lands.
  ;;
  ;; $Num is abstract since the numeric tower landed — concrete leaves
  ;; are $I64 / $U64 (under $Int), $F64, $Decimal. num_to_js dispatches
  ;; on the concrete type and returns f64; num_from_js boxes a JS
  ;; number as $F64 (the only concrete float leaf).

  (func $num_to_js (@pub) (export "env:num_to_js")
    (param $n (ref any)) (result f64)

    (local $nn (ref $Num))
    (local.set $nn (ref.cast (ref $Num) (local.get $n)))

    ;; $F64 — return the f64 field directly.
    (if (ref.test (ref $F64) (local.get $nn))
      (then (return (struct.get $F64 $val
        (ref.cast (ref $F64) (local.get $nn))))))

    ;; $I64 — convert i64 to f64 (loses precision past 2^53).
    (if (ref.test (ref $I64) (local.get $nn))
      (then (return (f64.convert_i64_s
        (struct.get $I64 $ival
          (ref.cast (ref $I64) (local.get $nn)))))))

    ;; $U64 — convert i64 to f64 (loses precision past 2^53).
    (if (ref.test (ref $U64) (local.get $nn))
      (then (return (f64.convert_i64_u
        (struct.get $U64 $ival
          (ref.cast (ref $U64) (local.get $nn)))))))

    ;; $Decimal — coeff * 10^exp via the runtime helper. Field-level
    ;; access would need importing decimal.wat:_as_f64; using struct
    ;; arithmetic keeps the fallback self-contained but lossy.
    (if (ref.test (ref $Decimal) (local.get $nn))
      (then (return (f64.mul
        (f64.convert_i64_s
          (struct.get $Decimal $coeff
            (ref.cast (ref $Decimal) (local.get $nn))))
        (call $_pow10
          (struct.get $Decimal $exp
            (ref.cast (ref $Decimal) (local.get $nn))))))))

    ;; Unknown $Num leaf — trap.
    (unreachable))

  (func $num_from_js (@pub) (export "env:num_from_js")
    (param $v f64) (result (ref any))
    (struct.new $F64 (local.get $v)))


  ;; -- i31_to_js / i31_from_js -------------------------------------------
  ;;
  ;; Bools are i31ref today: false = i31.const 0, true = i31.const 1.
  ;; JS maps the returned i32 to a boolean via !! at the call site.
  ;; When small-int unboxing lands, type_of will need to split the i31
  ;; range — these helpers stay bool-only until then.

  (func $i31_to_js (@pub) (export "env:i31_to_js")
    (param $v (ref any)) (result i32)
    (i31.get_s (ref.cast (ref i31) (local.get $v))))

  (func $i31_from_js (@pub) (export "env:i31_from_js")
    (param $v i32) (result (ref any))
    (ref.i31 (local.get $v)))

  ;; Compute 10^exp as f64 by repeated mul. Trivial helper so we don't
  ;; pull in a runtime import for one call site.
  (func $_pow10 (param $exp i32) (result f64)
    (local $r f64) (local $i i32)

    (local.set $r (f64.const 1))
    (if (i32.ge_s (local.get $exp) (i32.const 0))
      (then
        (local.set $i (i32.const 0))
        (block $done (loop $up
          (br_if $done (i32.ge_s (local.get $i) (local.get $exp)))
          (local.set $r (f64.mul (local.get $r) (f64.const 10)))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br $up))))
      (else
        (local.set $i (local.get $exp))
        (block $done (loop $down
          (br_if $done (i32.ge_s (local.get $i) (i32.const 0)))
          (local.set $r (f64.div (local.get $r) (f64.const 10)))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br $down)))))
    (local.get $r))


  ;; -- args list construction + apply -----------------------------------
  ;;
  ;; JS builds the args list via args_empty + args_prepend (mirroring
  ;; the runtime ABI), then calls apply with the function + args + a
  ;; cont_id. The cont fires with the result via host_invoke_cont.
  ;;
  ;; The runtime's apply expects the cont to be the *first* element of
  ;; the args list (CPS calling convention). JS must prepend the cont
  ;; ref (built via wrap_host_cont) before user args.

  (func $args_empty (@pub) (export "env:args_empty")
    (result (ref any))
    (return_call $args_empty_inner))

  (func $args_prepend (@pub) (export "env:args_prepend")
    (param $head (ref null any)) (param $tail (ref any)) (result (ref any))
    (return_call $args_prepend_inner (local.get $head) (local.get $tail)))

  (func $apply_3 (@pub) (export "env:apply_3")
    (param $args (ref null any)) (param $ctx (ref null any)) (param $callee (ref null any))
    (return_call $apply_3_inner (local.get $args) (local.get $ctx) (local.get $callee)))

  ;; Host-side helper: mint an empty universe ctx the JS side passes
  ;; into apply_3 at module-wrapper entry. Delegates to rt/apply.wat's
  ;; canonical $empty_ctx.
  (func (export "env:empty_ctx") (result (ref any))
    (return_call $empty_ctx_inner))


  ;; -- Runtime-contract stubs (all `unreachable`) ------------------------
  ;;
  ;; The runtime imports these by name from "interop.wat". Each must
  ;; exist as an export with the right signature; bodies trap until JS
  ;; interop is actually implemented.

  ;; -- Host callable (inbound contract) ----------------------------------
  ;;
  ;; The JS host calls `wrap_host_cont(handle)` with an opaque externref
  ;; (its own callback function, an object, whatever it wants to receive
  ;; back) and gets a `(ref null any)` continuation it can hand to wasm
  ;; anywhere a cont is expected. When fink-side code fires the cont
  ;; via `_apply`, the dispatcher casts it to $Closure, pulls
  ;; $host_cont_adapter, and tail-calls it. The adapter unboxes the
  ;; externref from captures and forwards to env.host_invoke_cont(handle,
  ;; args). JS dispatches via the externref directly — no id table.
  ;;
  ;; Why an `$ExternBox` GC struct: $Captures is `(array (ref null any))`,
  ;; which can't hold an externref directly (externref is its own top
  ;; type, not a subtype of any). Boxing in a one-field GC struct
  ;; makes it storable in captures; unboxing is one struct.get.
  ;;
  ;; Browser support: externref everywhere (~2021). WasmGC structs +
  ;; externref fields: Chrome/Firefox shipped; Safari with WasmGC
  ;; end-2024. If Safari rejects this shape, fall back to the
  ;; rust-side id-table approach (gated on target).

  (type $ExternBox (sub final (struct (field externref))))

  (elem declare func $host_cont_adapter $panic_apply)

  (func $host_cont_adapter (type $Fn3)
    (param $caps (ref null any))
    (param $_ctx (ref null any))
    (param $args (ref null any))

    (local $captures (ref $Captures))
    (local $handle externref)

    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $handle
      (struct.get $ExternBox 0
        (ref.cast (ref $ExternBox)
          (array.get $Captures (local.get $captures) (i32.const 0)))))

    (call $host_invoke_cont (local.get $handle) (local.get $args))
  )

  ;; Fn3-typed host cont — same wrap as the Rust interop. JS hosts use
  ;; the unified wrap_host_cont_3 export name so the apply shim's
  ;; cast to $Fn3 succeeds when fink fires the cont.
  (func $wrap_host_cont (export "env:wrap_host_cont")
    (param $handle externref) (result (ref null any))

    (struct.new $Closure
      (ref.func $host_cont_adapter)
      (array.new_fixed $Captures 1
        (struct.new $ExternBox (local.get $handle))))
  )

  (func $wrap_host_cont_3 (export "env:wrap_host_cont_3")
    (param $handle externref) (result (ref null any))

    (struct.new $Closure
      (ref.func $host_cont_adapter)
      (array.new_fixed $Captures 1
        (struct.new $ExternBox (local.get $handle))))
  )

  (func $panic (@pub)
    unreachable)

  (func $panic_apply (@pub) (@impl "std/interop.fnk:panic") (type $Fn3)
    (param $_caps (ref null any))
    (param $_ctx  (ref null any))
    (param $_args (ref null any))
    unreachable)

)
