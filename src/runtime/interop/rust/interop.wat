;; Rust host interop — host-bridge primitives.
;;
;; Provides:
;;   * `wrap_host_cont(id) -> anyref` — opaque WASM-side handle for a
;;     host-registered callback. Fired via `_apply`, dispatches to
;;     `env.host_invoke_cont(id, args)`.
;;   * `interop_channel_send` / `interop_op_read` /
;;     `interop_panic` — host-bridge ops invoked by the runtime
;;     protocols (rt/protocols.wat) when a value is a $HostChannel
;;     or a panic is raised.
;;
;; Owns $HostChannel — a subtype of $Channel for host-managed IO.
;; send/recv on host channels delegate to host imports instead of using
;; the internal message queue.
;;
;; Orchestration of `main` (build args list, apply, drain scheduler,
;; exit) is the runner's responsibility, not this file's. There is no
;; `_run_main` here — the test harness inlines the dispatch today; a
;; future production runner will provide its own entry point.

(module

  (import "rt/apply.wat"     "Fn2"     (type $Fn2 (sub any)))
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
  (import "std/channel.wat" "Channel"   (type $Channel  (sub any)))
  (import "std/list.wat"    "List"      (type $List     (sub any)))
  (import "std/async.wat"   "Future"    (type $Future   (sub any)))
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
  (import "rt/apply.wat"    "apply"
    (func $apply (param $args (ref null any)) (param $callee (ref null any))))

  (import "std/str.wat"     "_str_wrap_bytes"
    (func $str_wrap_bytes (param $bytes (ref null any)) (result (ref any))))
  (import "std/dict.wat"    "get_any"
    (func $rec_get_any (param $rec (ref null any)) (param $key (ref null any)) (result (ref null any))))
  ;; TODO: rename str.wat's `bytes` export to `str_bytes` (clashes with
  ;; the `$bytes` local in this file).
  (import "std/str.wat"     "bytes"
    (func $str_bytes (param $s (ref $Str)) (result (ref $ByteArray))))
  (import "std/async.wat"   "queue_push"
    (func $queue_push (param $task (ref any))))
  (import "rt/apply.wat"    "make_thunk"
    (func $make_thunk (param $cont (ref any)) (param $value (ref any)) (result (ref $Closure))))
  (import "rt/apply.wat"    "make_unit_thunk"
    (func $make_unit_thunk (param $cont (ref any)) (result (ref $Closure))))
  (import "std/async.wat"   "resume"
    (func $resume))


  ;; -- $HostChannel type ----------------------------------------------------
  ;;
  ;; Host-managed IO channel (stdin, stdout, stderr). Subtype of $Channel
  ;; so `>>` and `<<` work uniformly. The runtime dispatches to host
  ;; imports for host channels instead of using the internal message queue.
  (type $HostChannel (@pub) (sub final $Channel (struct
    (field $messages  (mut (ref $List)))
    (field $receivers (mut (ref $List)))
    (field $tag       (ref any))
  )))


  ;; Declarative element segment — required by WASM spec for ref.func.
  (elem declare func $host_cont_adapter $host_cont_adapter_3 $read_apply $write_apply $panic_apply)


  ;; -- Host imports (provided by Rust runner) --------------------------------

  (import "env" "host_channel_send" (func $host_channel_send (param i32) (param (ref null any))))
  (import "env" "host_read" (func $host_read (param (ref any) (ref any) (ref any))))
  ;; Irrefutable pattern failure — traps the instance with a diagnostic.
  ;; TODO: pass reason / source location (offset+length into linear memory)
  ;; so the host can render a useful message.
  (import "env" "host_panic" (func $host_panic))
  ;; Host-side callback dispatch: invoke the Rust-registered callback
  ;; for `id` with the given args list. See `$host_cont_adapter` and
  ;; `wrap_host_cont` for how WASM-side callable refs into this.
  (import "env" "host_invoke_cont" (func $host_invoke_cont (param i32 (ref null any))))


  ;; -- Host callable (inbound contract) --------------------------------------
  ;;
  ;; The host cannot hand WASM a raw funcref and have it pass as a
  ;; fink $Fn2: a host-built funcref carries a structural function
  ;; type distinct from the runtime's nominal $Fn2, so
  ;; `ref.cast (ref $Fn2)` inside `_apply` would always trap.
  ;;
  ;; Instead, the host registers its callback under an i32 id on its
  ;; side, calls `wrap_host_cont(id)` to get an opaque (ref null any),
  ;; and hands that anyref to WASM wherever a continuation is
  ;; expected (done, await cont, scheduler trampolines, etc.).
  ;;
  ;; When fink-side code eventually fires the continuation via
  ;; `_apply`, `_apply` casts it to $Closure, pulls the funcref (which
  ;; is `$host_cont_adapter` by construction — correct nominal type)
  ;; and tail-calls it. The adapter reads `id` out of the captures
  ;; array and forwards to `env.host_invoke_cont(id, args)`.
  ;;
  ;; Net: host sees only an opaque anyref; never touches $Closure /
  ;; $Fn2 / funcref directly. Internals are interop's business.

  ;; $Fn2 adapter body — fires when WASM invokes a host-wrapped cont.
  (func $host_cont_adapter (type $Fn2)
    (param $caps (ref null any))
    (param $args (ref null any))

    (local $captures (ref $Captures))
    (local $id_box (ref i31))

    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $id_box
      (ref.cast (ref i31)
        (array.get $Captures (local.get $captures) (i32.const 0))))

    (call $host_invoke_cont
      (i31.get_s (local.get $id_box))
      (local.get $args))
  )

  ;; $Fn3 adapter body — fires when WASM invokes a host-wrapped cont
  ;; via the ctx-aware `apply_3` dispatcher. Same as $host_cont_adapter
  ;; but accepts an extra ctx native param which we ignore (the host
  ;; cont doesn't participate in the substrate).
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
      (local.get $args))
  )

  ;; Factory: host calls this with its callback id; gets back an
  ;; opaque (ref null any) fit for any CPS continuation slot.
  ;; Fn2 variant — used by the existing default Fn2 pipeline.
  (func $wrap_host_cont (export "env:wrap_host_cont")
    (param $id i32)
    (result (ref null any))

    (struct.new $Closure
      (ref.func $host_cont_adapter)
      (array.new_fixed $Captures 1
        (ref.i31 (local.get $id))))
  )

  ;; Fn3 variant — used by the ctx-aware (lower_ctx) pipeline.
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


  ;; -- host_channel_send -----------------------------------------------------
  ;;
  ;; host_channel_send(ch, msg, cont):
  ;;   1. Write msg to the host via the appropriate host_write import
  ;;   2. Queue unit_thunk(cont) to resume the sender
  ;;   3. Resume scheduler
  ;;
  ;; Dispatches stdout vs stderr by channel tag (i31ref: 1=stdout, 2=stderr).

  (func $channel_send (@pub)
    (param $ch (ref null any))
    (param $msg (ref null any))
    (param $cont (ref null any))

    (local $tag i32)
    (local $bytes (ref $ByteArray))

    ;; Extract raw bytes from the $Str (handles all subtypes).
    (local.set $bytes
      (call $str_bytes (ref.cast (ref $Str) (local.get $msg))))

    ;; Read channel tag (i31ref).
    (local.set $tag
      (i31.get_s (ref.cast (ref i31)
        (struct.get $Channel $tag
          (ref.cast (ref $Channel) (local.get $ch))))))

    ;; Send to host — host reads bytes directly from the GC array.
    (call $host_channel_send (local.get $tag) (local.get $bytes))

    ;; Sender continues with unit.
    (call $queue_push
      (call $make_unit_thunk (ref.as_non_null (local.get $cont))))

    (return_call $resume)
  )


  ;; -- interop_op_read --------------------------------------------------------
  ;;
  ;; interop_op_read(stream, size, cont):
  ;;   1. Create a pending $Future with cont as waiter
  ;;   2. Call host_read(stream, size, future) — host starts async read
  ;;   3. Resume scheduler — task is parked on the future
  ;;
  ;; The host settles the future during host_resume when data arrives.

  (func $op_read (@pub)
    (param $stream (ref null any))
    (param $size (ref null any))
    (param $cont (ref null any))

    (local $future (ref $Future))

    ;; Create pending future with cont as waiter.
    (local.set $future (struct.new $Future
      (ref.null any)
      (call $list_prepend
        (ref.as_non_null (local.get $cont))
        (call $list_empty))))

    ;; Tell host to start async read. Host captures the future ref.
    (call $host_read
      (ref.as_non_null (local.get $stream))
      (ref.as_non_null (local.get $size))
      (local.get $future))

    ;; Resume scheduler — this task is parked on the future.
    (return_call $resume)
  )


  ;; -- interop_panic ---------------------------------------------------------
  ;;
  ;; Called from runtime `panic` (operators.wat). Delegates to the host which
  ;; traps the instance with a diagnostic. Never returns.

  (func $panic (@pub)
    (call $host_panic)
    unreachable
  )


  ;; -- $Fn2-shaped panic for CPS dispatch ------------------------------------
  ;;
  ;; CPS-side panic — used as a $Closure value passed as a fail continuation
  ;; to pattern matchers, and as a direct tail-call at the terminal of a
  ;; fail chain. Signature matches the universal $Fn2 calling convention so
  ;; `_apply` can dispatch to it like any other continuation.
  ;;
  ;; Delegates to `$panic`, which traps the instance via host_panic. Today
  ;; panic carries no payload; future work will pass a reason / source
  ;; location for better diagnostics.
  (func $panic_apply (@pub) (@impl "std/interop.fnk:panic") (type $Fn2)
    (param $_caps (ref null any))
    (param $_args (ref null any))
    (return_call $panic))


  ;; -- stdio channels --------------------------------------------------------
  ;;
  ;; Constant-init `$HostChannel` globals — created once at instantiation,
  ;; never reassigned. Tags (i31ref) follow POSIX fd numbers:
  ;;   0 = stdin, 1 = stdout, 2 = stderr.
  ;;
  ;; `interop_channel_send` reads the tag to dispatch to the right host
  ;; sink. The tag is also how the test harness keys per-channel capture
  ;; buffers.
  ;;
  ;; The accessor functions are what `rt/protocols.wat` exports as the
  ;; protocol dispatchers `std/io.fnk:stdout` etc. Keeping the channel
  ;; values behind accessors preserves the layering invariant: nothing
  ;; outside `interop/*` reads these globals directly.

  ;; Lazy-init: globals start null, populated on first access.
  ;; Required because const init can't call functions ($list_empty) and
  ;; we want to avoid leaking $Nil across the channel boundary.
  (global $stdout (mut (ref null $HostChannel)) (ref.null $HostChannel))
  (global $stderr (mut (ref null $HostChannel)) (ref.null $HostChannel))
  (global $stdin  (mut (ref null $HostChannel)) (ref.null $HostChannel))

  (func $_make_host_channel (param $tag i32) (result (ref $HostChannel))
    (struct.new $HostChannel
      (call $list_empty)
      (call $list_empty)
      (ref.i31 (local.get $tag))))

  (func $get_stdout (@pub) (@impl "std/io.fnk:stdout") (result (ref any))
    (if (ref.is_null (global.get $stdout))
      (then (global.set $stdout (call $_make_host_channel (i32.const 1)))))
    (ref.as_non_null (global.get $stdout)))

  (func $get_stderr (@pub) (@impl "std/io.fnk:stderr") (result (ref any))
    (if (ref.is_null (global.get $stderr))
      (then (global.set $stderr (call $_make_host_channel (i32.const 2)))))
    (ref.as_non_null (global.get $stderr)))

  (func $get_stdin (@pub) (@impl "std/io.fnk:stdin") (result (ref any))
    (if (ref.is_null (global.get $stdin))
      (then (global.set $stdin (call $_make_host_channel (i32.const 0)))))
    (ref.as_non_null (global.get $stdin)))


  ;; -- read closure ----------------------------------------------------------
  ;;
  ;; `std/io.fnk:read` returns a $Closure value (callable via _apply),
  ;; not a bare reference. The closure construction lives here because
  ;; it bridges between two ABIs:
  ;;   * user calling convention via _apply → args list = [cont, ...user_args]
  ;;   * interop_op_read fixed-arg ABI       → (stream, size, cont)
  ;; That translation is host-bridge plumbing, hence belongs alongside
  ;; the rest of the interop_* primitives.
  ;;
  ;; Singleton — same closure instance every access; captures null
  ;; (nothing per-instance).

  (func $read_apply (type $Fn2)
    (param $_caps (ref null any))
    (param $args (ref null any))

    (local $cursor (ref null any))
    (local $cont (ref null any))
    (local $stream (ref null any))
    (local $size (ref null any))

    (local.set $cursor (local.get $args))
    ;; TODO this needs to got through args_* protocol!
    (local.set $cont (call $list_head_any (local.get $cursor)))
    (local.set $cursor (call $list_tail_any (local.get $cursor)))
    (local.set $stream (call $list_head_any (local.get $cursor)))
    (local.set $cursor (call $list_tail_any (local.get $cursor)))
    (local.set $size (call $list_head_any (local.get $cursor)))

    (return_call $op_read
      (local.get $stream)
      (local.get $size)
      (local.get $cont)))

  (global $read_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $read_apply)
      (ref.null $Captures)))

  (func $get_read (@pub) (@impl "std/io.fnk:read") (result (ref any))
    (global.get $read_closure))


  ;; -- write closure ---------------------------------------------------------
  ;;
  ;; `std/io.fnk:write` returns a $Closure that, when applied as
  ;; `write stream, value`, sends `value` to the host stream tagged by
  ;; `stream` and resumes the caller with `stream` (so `write` returns the
  ;; stream — enables chaining like `s | write ?, 'a' | write ?, 'b'`).
  ;;
  ;; Differs from `channel_send` only in that the cont is resumed with
  ;; the stream value (via `make_thunk`) instead of unit.

  (func $channel_send_stream
    (param $ch (ref null any))
    (param $msg (ref null any))
    (param $cont (ref null any))

    (local $tag i32)
    (local $bytes (ref $ByteArray))

    (local.set $bytes
      (call $str_bytes (ref.cast (ref $Str) (local.get $msg))))

    (local.set $tag
      (i31.get_s (ref.cast (ref i31)
        (struct.get $Channel $tag
          (ref.cast (ref $Channel) (local.get $ch))))))

    (call $host_channel_send (local.get $tag) (local.get $bytes))

    ;; Sender continues with the stream itself.
    (call $queue_push
      (call $make_thunk
        (ref.as_non_null (local.get $cont))
        (ref.as_non_null (local.get $ch))))

    (return_call $resume)
  )

  (func $write_apply (type $Fn2)
    (param $_caps (ref null any))
    (param $args (ref null any))

    (local $cursor (ref null any))
    (local $cont (ref null any))
    (local $stream (ref null any))
    (local $value (ref null any))

    (local.set $cursor (local.get $args))
    (local.set $cont (call $list_head_any (local.get $cursor)))
    (local.set $cursor (call $list_tail_any (local.get $cursor)))
    (local.set $stream (call $list_head_any (local.get $cursor)))
    (local.set $cursor (call $list_tail_any (local.get $cursor)))
    (local.set $value (call $list_head_any (local.get $cursor)))

    (return_call $channel_send_stream
      (local.get $stream)
      (local.get $value)
      (local.get $cont)))

  (global $write_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $write_apply)
      (ref.null $Captures)))

  (func $get_write (@pub) (@impl "std/io.fnk:write") (result (ref any))
    (global.get $write_closure))


  ;; -- Host bootstrap delegates ---------------------------------------
  ;;
  ;; The wasmtime runner's `apply_main` reaches into the runtime to
  ;; build the args list and apply main. Only interop should be visible
  ;; to the host, so these delegates forward to the real funcs.
  ;;
  ;; TODO: move the apply_main bootstrap inside the wasm module behind
  ;; one entry point, then drop these.

  (func (export "env:apply")
    (param $args (ref null any)) (param $callee (ref null any))
    (return_call $apply (local.get $args) (local.get $callee)))

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
