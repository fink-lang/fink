;; Rust host interop — host-bridge primitives.
;;
;; Provides:
;;   * `wrap_host_cont(id) -> anyref` — opaque WASM-side handle for a
;;     host-registered callback. Fired via `_apply`, dispatches to
;;     `env.host_invoke_cont(id, args)`.
;;   * `interop_yield` / `io_write` / `io_read` — Fn3-shaped ƒink
;;     primitives that bridge userland calls to host imports
;;     (host_yield, host_write, host_read_sync).
;;   * `invoke_resume(resume, value, ctx)` — host-callable export the
;;     driver loop uses to fire a yielded continuation.
;;   * `panic` — delegates to host_panic, then traps.
;;
;; Orchestration of `main` (build args list, apply, drive any pending
;; resumes from `invoke_resume`, exit) is the runner's responsibility,
;; not this file's. There is no `_run_main` here — the test harness
;; inlines the dispatch today; a future production runner will provide
;; its own entry point.

(module

  (import "rt/apply.wat"     "Fn3"     (type $Fn3 (sub any)))
  (import "rt/apply.wat"    "Closure"  (type $Closure  (sub any)))
  (import "rt/apply.wat"    "Captures" (type $Captures (sub any)))

  ;; TODO use a type or something smaller to force import protocols
  (import "rt/protocols.wat" "deep_eq" (func $deep_eq (param (ref eq)) (param (ref eq)) (result i32)))
  (import "rt/modules.wat"   "init"    (func $modules_init (param (ref null any)) (result i32)))


  ;; Inter-wat type imports.
  (import "std/num.wat"     "Num"       (type $Num      (sub any)))
  (import "std/str.wat"     "Str"       (type $Str      (sub any)))
  (import "std/str.wat"     "ByteArray" (type $ByteArray (sub any)))
  (import "std/list.wat"    "List"      (type $List     (sub any)))
  (import "std/int.wat"     "Int"       (type $Int      (sub $Num (struct))))
  (import "std/int.wat"     "I64"       (type $I64      (sub $Int (struct (field $ival i64)))))
  (import "std/int.wat"     "U64"       (type $U64      (sub $Int (struct (field $ival i64)))))
  (import "std/float.wat"   "F64"       (type $F64      (sub $Num (struct (field $val f64)))))

  ;; Func imports
  (import "std/list.wat"    "head_any"
    (func $list_head_any (param $list (ref null any)) (result (ref null any))))
  (import "std/list.wat"    "tail_any"
    (func $list_tail_any (param $list (ref null any)) (result (ref null any))))
  (import "std/list.wat"    "empty"
    (func $list_empty (result (ref $List))))
  (import "std/list.wat"    "prepend"
    (func $list_prepend (param $head (ref any)) (param $tail (ref $List)) (result (ref $List))))

  (import "rt/apply.wat"    "args_empty"
    (func $args_empty (result (ref any))))
  (import "rt/apply.wat"    "args_prepend"
    (func $args_prepend (param $head (ref null any)) (param $tail (ref any)) (result (ref any))))
  (import "rt/apply.wat"    "args_head"
    (func $args_head (param $args (ref null any)) (result (ref null any))))
  (import "rt/apply.wat"    "args_tail"
    (func $args_tail (param $args (ref null any)) (result (ref null any))))
  (import "rt/apply.wat"    "apply_3"
    (func $apply_3
      (param $args (ref null any))
      (param $ctx (ref null any))
      (param $callee (ref null any))))
  (import "rt/apply.wat"    "empty_ctx"
    (func $empty_ctx (result (ref any))))
  (import "rt/apply.wat" "set_ctx"
    (func $set_ctx (result (ref any))))
  (import "rt/apply.wat" "get_ctx"
    (func $get_ctx (result (ref any))))

  (import "std/int.wat"     "_box_i64"
    (func $_box_i64 (param $v i64) (result (ref $I64))))
  (import "std/str.wat"     "_str_wrap_bytes"
    (func $str_wrap_bytes (param $bytes (ref null any)) (result (ref any))))
  (import "std/dict.wat"    "get_any"
    (func $rec_get_any (param $rec (ref null any)) (param $key (ref null any)) (result (ref null any))))
  ;; TODO: rename str.wat's `bytes` export to `str_bytes` (clashes with
  ;; the `$bytes` local in this file).
  (import "std/str.wat"     "bytes"
    (func $str_bytes (param $s (ref $Str)) (result (ref $ByteArray))))


  ;; Declarative element segment — required by WASM spec for ref.func.
  (elem declare func $host_cont_adapter_3 $panic_apply $interop_yield_apply $io_write_apply $io_read_apply $interop_now_apply)


  ;; -- Host imports (provided by Rust runner) --------------------------------

  ;; Panic — traps the instance with a diagnostic. The i32 `reason` is
  ;; the wire encoding of `PanicReason` (see src/passes/cps/ir.rs):
  ;;   0 = IrrefutablePattern
  ;;   1 = MatchExhausted
  ;; The host translates the code into a user-facing message; trap.rs on
  ;; the Rust side recognises both and renders accordingly.
  (import "env" "host_panic" (func $host_panic (param i32)))
  ;; Host-side callback dispatch: invoke the Rust-registered callback
  ;; for `id` with the given args list. See `$host_cont_adapter` and
  ;; `wrap_host_cont` for how WASM-side callable refs into this.
  (import "env" "host_invoke_cont" (func $host_invoke_cont (param i32 (ref null any) (ref null any))))

  ;; Host yields control to the userland scheduler when its queue is
  ;; empty. The host stores the `resume` closure and decides when to
  ;; invoke it back via the `_invoke_resume` export (e.g. after an
  ;; epoll/poll cycle, after stdin has bytes, after a timer fires).
  ;; Returns immediately — wasm execution unwinds back to whichever
  ;; export call the host made, and the host then drives.
  (import "env" "host_yield" (func $host_yield (param (ref any)) (param (ref null any))))

  ;; Sync write: host writes the bytes and returns. The wasm-side
  ;; `io_write_apply` continues by tail-calling its k_caller — no
  ;; callback, no future. (Async variant will defer via host_yield
  ;; when needed.)
  (import "env" "host_write" (func $host_write
    (param $fd (ref null any))
    (param $bytes (ref $ByteArray))))

  ;; Sync read: host reads up to size bytes from fd, returns ByteArray.
  (import "env" "host_read_sync" (func $host_read_sync
    (param $fd (ref null any))
    (param $size (ref null any))
    (result (ref $ByteArray))))

  ;; Monotonic clock: nanoseconds since an arbitrary host epoch. For
  ;; elapsed-time measurement only (not wall-clock); fink computes
  ;; deltas. Impure -- exposed as a debug/perf primitive, not a pure
  ;; value.
  (import "env" "host_mono_ns" (func $host_mono_ns (result i64)))


  ;; Host-callable entry point: fire a previously-yielded resume
  ;; closure under the ctx that was active at yield time. Both are
  ;; stashed host-side by `host_yield`; this export re-threads ctx
  ;; back into apply_3 so the resumed code sees the same ctx that
  ;; was current at suspend.
  (func $interop_invoke_resume (@pub) (export "env:invoke_resume")
    (param $resume (ref any))
    (param $value (ref any))
    (param $ctx (ref null any))
    (local $args (ref any))
    ;; Fire the resume/callback closure with one arg = value, under ctx.
    ;; - yield case: value is unit/placeholder; resume is the post-yield cont.
    ;; - io callback case: value is the io result; resume is the userland
    ;;   callback closure (e.g. `fn bytes: settle_future fut, bytes`).
    (local.set $args (call $args_empty))
    (local.set $args (call $args_prepend (local.get $value) (local.get $args)))
    (return_call $apply_3
      (local.get $args)
      (local.get $ctx)
      (local.get $resume)))


  ;; Userland-callable yield: hands `resume` to the host and returns.
  ;; The wasm-side caller (e.g. `tasks.fnk:yield_to_host`) will see
  ;; control return immediately; the host re-fires `resume` later via
  ;; `_invoke_resume`.
  ;;
  ;; Exposed as a `$Closure` so user code applies it via `apply_3` like
  ;; any other ƒink fn. The Fn3 body pulls `resume` out of the args
  ;; list and forwards to the host import.
  (elem declare func $interop_yield_apply)

  (func $interop_yield_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    ;; CPS convention: args[0] is the caller's continuation. Hand it
    ;; to the host as the resume value — host fires it later via
    ;; `invoke_resume` under the same ctx. User-level surface is
    ;; `interop_yield _`; the `_` is a placeholder, the real resume
    ;; comes from the compiler's CPS lowering.
    (local $resume (ref any))
    (local.set $resume (ref.as_non_null
      (call $args_head (local.get $args))))
    (return_call $host_yield (local.get $resume) (local.get $ctx)))

  (global $interop_yield_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $interop_yield_apply)
      (ref.null $Captures)))

  (func $interop_yield (@pub) (@impl "interop.fnk:yield")
    (result (ref any))
    (global.get $interop_yield_closure))


  ;; -- io_write -----------------------------------------------------------
  ;;
  ;; User-level call: `io_write fd, bytes, callback`.
  ;; CPS-lowered args = [k_caller, fd, bytes, callback].
  ;; Body hands (fd, bytes, callback, ctx) to the host; the host writes
  ;; and later fires the callback via `invoke_resume`. We tail-call
  ;; k_caller with unit (the write call has no synchronous return value).
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

    (local.set $k_caller (ref.as_non_null (call $args_head (local.get $args))))
    (local.set $rest (call $args_tail (local.get $args)))
    (local.set $fd (call $args_head (local.get $rest)))
    (local.set $rest (call $args_tail (local.get $rest)))
    (local.set $msg (call $args_head (local.get $rest)))

    ;; Extract raw ByteArray from the $Str so host can read it directly.
    (local.set $bytes
      (call $str_bytes (ref.cast (ref $Str) (local.get $msg))))

    (call $host_write
      (local.get $fd)
      (local.get $bytes))

    ;; Tail-call k_caller with unit (placeholder i31).
    (local.set $k_args (call $args_empty))
    (local.set $k_args (call $args_prepend (ref.i31 (i32.const 0)) (local.get $k_args)))
    (return_call $apply_3
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


  ;; -- io_read -----------------------------------------------------------
  ;; Sync read: args = [k_caller, fd, size]. Calls host_read_sync, wraps
  ;; the returned ByteArray as a $Str, tail-calls k_caller with the Str.
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

    (local.set $k_caller (ref.as_non_null (call $args_head (local.get $args))))
    (local.set $rest (call $args_tail (local.get $args)))
    (local.set $fd (call $args_head (local.get $rest)))
    (local.set $rest (call $args_tail (local.get $rest)))
    (local.set $size (call $args_head (local.get $rest)))

    (local.set $bytes
      (call $host_read_sync (local.get $fd) (local.get $size)))
    (local.set $str (call $str_wrap_bytes (local.get $bytes)))

    (local.set $k_args (call $args_empty))
    (local.set $k_args (call $args_prepend (local.get $str) (local.get $k_args)))
    (return_call $apply_3
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


  ;; -- monotonic clock -------------------------------------------------
  ;;
  ;; args = [k_caller]. Calls host_mono_ns, boxes the i64 as $I64,
  ;; tail-calls k_caller with it. The `_` placeholder arg is ignored.
  (elem declare func $interop_now_apply)

  (func $interop_now_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $k_caller (ref any))
    (local $ns (ref $I64))
    (local $k_args (ref any))

    (local.set $k_caller (ref.as_non_null (call $args_head (local.get $args))))
    (local.set $ns (call $_box_i64 (call $host_mono_ns)))

    (local.set $k_args
      (call $args_prepend (local.get $ns) (call $args_empty)))
    (return_call $apply_3
      (local.get $k_args)
      (local.get $ctx)
      (local.get $k_caller)))

  (global $interop_now_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $interop_now_apply)
      (ref.null $Captures)))

  (func $interop_now (@pub) (@impl "interop.fnk:now")
    (result (ref any))
    (global.get $interop_now_closure))


  ;; -- Host callable (inbound contract) --------------------------------------
  ;;
  ;; The host cannot hand WASM a raw funcref and have it pass as a
  ;; fink $Fn3: a host-built funcref carries a structural function
  ;; type distinct from the runtime's nominal $Fn3, so
  ;; `ref.cast (ref $Fn3)` inside `apply_3` would always trap.
  ;;
  ;; Instead, the host registers its callback under an i32 id on its
  ;; side, calls `wrap_host_cont_3(id)` to get an opaque (ref null any),
  ;; and hands that anyref to WASM wherever a continuation is
  ;; expected (the wrapper-done cont, main-done cont, etc.).
  ;;
  ;; When fink-side code eventually fires the continuation via
  ;; `apply_3`, it casts the value to $Closure, pulls the funcref
  ;; (which is `$host_cont_adapter_3` by construction — correct nominal
  ;; type) and tail-calls it. The adapter reads `id` out of the captures
  ;; array and forwards to `env.host_invoke_cont(id, args)`.
  ;;
  ;; Net: host sees only an opaque anyref; never touches $Closure /
  ;; $Fn3 / funcref directly. Internals are interop's business.

  ;; $Fn3 adapter body — fires when WASM invokes a host-wrapped cont
  ;; via the ctx-aware `apply_3` dispatcher. Forwards the threaded ctx
  ;; to the host so the entry's wrapper-done cont can apply `main`
  ;; against the post-init universe (the seeded effect slots).
  (func $host_cont_adapter_3 (type $Fn3)
    (param $caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $captures (ref $Captures))
    (local $id_box (ref i31))

    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $id_box
      (ref.cast (ref i31)
        (array.get $Captures (local.get $captures) (i32.const 0))))

    (call $host_invoke_cont
      (i31.get_s (local.get $id_box))
      (local.get $args)
      (local.get $ctx))
  )

  ;; Fn3 variant — used by the ctx-aware pipeline (the only one now).
  ;; The closure's funcref is Fn3-typed so `apply_3` can cast it
  ;; without trapping. The runner uses this when invoking a
  ;; ctx-aware module.
  (func $wrap_host_cont_3 (export "env:wrap_host_cont_3")
    (param $id i32)
    (result (ref null any))

    (struct.new $Closure
      (ref.func $host_cont_adapter_3)
      (array.new_fixed $Captures 1
        (ref.i31 (local.get $id))))
  )


  ;; -- interop_panic ---------------------------------------------------------
  ;;
  ;; Called from runtime `panic` (operators.wat). Delegates to the host which
  ;; traps the instance with a diagnostic. Never returns.

  (func $panic (@pub) (param $reason i32)
    (call $host_panic (local.get $reason))
    unreachable
  )


  ;; -- $Fn3-shaped panic for CPS dispatch ------------------------------------
  ;;
  ;; CPS-side panic — used as a $Closure value passed as a fail continuation
  ;; to pattern matchers, and as a direct tail-call at the terminal of a
  ;; fail chain. Signature matches the ctx-aware $Fn3 calling convention so
  ;; `apply_3` can dispatch to it like any other continuation.
  ;;
  ;; When invoked via Fn3 dispatch the panic site has no static reason
  ;; available -- it's the fail closure value, not a known call site.
  ;; Reports IrrefutablePattern (reason 0); inline-emitted panics carry
  ;; their reason and call `$panic` directly.
  (func $panic_apply (@pub) (@impl "std/interop.fnk:panic") (type $Fn3)
    (param $_caps (ref null any))
    (param $_ctx  (ref null any))
    (param $_args (ref null any))
    (return_call $panic (i32.const 0)))



  ;; -- Host bootstrap delegates ---------------------------------------
  ;;
  ;; The wasmtime runner's `apply_main` reaches into the runtime to
  ;; build the args list and apply main. Only interop should be visible
  ;; to the host, so these delegates forward to the real funcs.
  ;;
  ;; TODO: move the apply_main bootstrap inside the wasm module behind
  ;; one entry point, then drop these.

  (func (export "env:apply_3")
    (param $args (ref null any)) (param $ctx (ref null any)) (param $callee (ref null any))
    (return_call $apply_3 (local.get $args) (local.get $ctx) (local.get $callee)))

  (func (export "env:empty_ctx") (result (ref any))
    (return_call $empty_ctx))

  (func (export "env:args_empty") (result (ref any))
    (return_call $args_empty))

  (func (export "env:args_prepend")
    (param $head (ref null any)) (param $tail (ref any))
    (result (ref any))
    (return_call $args_prepend (local.get $head) (local.get $tail)))

  (func (export "env:str_wrap_bytes")
    (param $bytes (ref null any))
    (result (ref any))
    (return_call $str_wrap_bytes (local.get $bytes)))

  ;; Host-callable: look up a $Rec field by raw byte-array key.
  ;; Wraps key_bytes into a $Str, then delegates to dict.wat:get_any.
  ;; Returns null when the key is absent. Used by the Rust runner to
  ;; pull named exports out of the exports rec it receives from the
  ;; per-module wrapper.
  (func (export "env:rec_get_by_bytes")
    (param $rec       (ref null any))
    (param $key_bytes (ref null any))
    (result (ref null any))
    (return_call $rec_get_any
      (local.get $rec)
      (call $str_wrap_bytes (local.get $key_bytes))))


  ;; -- type_of -----------------------------------------------------------
  ;;
  ;; Discriminate a runtime value for the Rust host. Tags only cover the
  ;; cases the test runner needs today — the bool / numeric leaves for
  ;; faithful headline rendering. Other types fall through to 0; extend
  ;; as new host needs arise.
  ;;
  ;;   0  unknown / null
  ;;   1  i31 (Bool)
  ;;   2  $I64 (signed)
  ;;   3  $U64 (unsigned)
  ;;   4  $F64
  ;;   5  $Num (other — e.g. $Decimal)
  (func (export "env:type_of")
    (param $v (ref null any)) (result i32)
    (local $nn (ref any))

    (if (ref.is_null (local.get $v)) (then (return (i32.const 0))))
    (local.set $nn (ref.as_non_null (local.get $v)))

    (if (ref.test (ref i31)  (local.get $nn)) (then (return (i32.const 1))))
    (if (ref.test (ref $I64) (local.get $nn)) (then (return (i32.const 2))))
    (if (ref.test (ref $U64) (local.get $nn)) (then (return (i32.const 3))))
    (if (ref.test (ref $F64) (local.get $nn)) (then (return (i32.const 4))))
    (if (ref.test (ref $Num) (local.get $nn)) (then (return (i32.const 5))))

    (i32.const 0))

)
