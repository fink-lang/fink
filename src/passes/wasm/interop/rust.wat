;; Rust host interop — host-bridge primitives.
;;
;; Provides:
;;   * `wrap_host_cont(id) -> anyref` — opaque WASM-side handle for a
;;     host-registered callback. Fired via `_apply`, dispatches to
;;     `env.host_invoke_cont(id, args)`.
;;   * `interop_channel_send` / `interop_channel_recv` / `interop_op_read` /
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

  ;; Declarative element segment — required by WASM spec for ref.func.
  (elem declare func $interop/rust.wat:host_cont_adapter $interop/rust.wat:read_apply)


  ;; -- Host imports (provided by Rust runner) --------------------------------

  (import "env" "host_channel_send" (func $interop/rust.wat:host_channel_send (param i32) (param (ref null any))))
  (import "env" "host_read" (func $interop/rust.wat:host_read (param (ref any) (ref any) (ref any))))
  ;; Irrefutable pattern failure — traps the instance with a diagnostic.
  ;; TODO: pass reason / source location (offset+length into linear memory)
  ;; so the host can render a useful message.
  (import "env" "host_panic" (func $interop/rust.wat:host_panic))
  ;; Host-side callback dispatch: invoke the Rust-registered callback
  ;; for `id` with the given args list. See `$interop/rust.wat:host_cont_adapter` and
  ;; `wrap_host_cont` for how WASM-side callable refs into this.
  (import "env" "host_invoke_cont" (func $interop/rust.wat:host_invoke_cont (param i32 (ref null any))))


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
  ;; is `$interop/rust.wat:host_cont_adapter` by construction — correct nominal type)
  ;; and tail-calls it. The adapter reads `id` out of the captures
  ;; array and forwards to `env.host_invoke_cont(id, args)`.
  ;;
  ;; Net: host sees only an opaque anyref; never touches $Closure /
  ;; $Fn2 / funcref directly. Internals are interop's business.

  ;; $Fn2 adapter body — fires when WASM invokes a host-wrapped cont.
  (func $interop/rust.wat:host_cont_adapter (type $Fn2)
    (param $caps (ref null any))
    (param $args (ref null any))

    (local $captures (ref $Captures))
    (local $id_box (ref i31))

    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $id_box
      (ref.cast (ref i31)
        (array.get $Captures (local.get $captures) (i32.const 0))))

    (call $interop/rust.wat:host_invoke_cont
      (i31.get_s (local.get $id_box))
      (local.get $args))
  )

  ;; Factory: host calls this with its callback id; gets back an
  ;; opaque (ref null any) fit for any CPS continuation slot.
  (func $interop/rust.wat:wrap_host_cont (export "wrap_host_cont")
    (param $id i32)
    (result (ref null any))

    (struct.new $Closure
      (ref.func $interop/rust.wat:host_cont_adapter)
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

  (func $interop/rust.wat:channel_send
    (param $ch (ref null any))
    (param $msg (ref null any))
    (param $cont (ref null any))

    (local $tag i32)
    (local $bytes (ref $ByteArray))

    ;; Extract raw bytes from the $Str (handles all subtypes).
    (local.set $bytes
      (call $std/str.wat:bytes (ref.cast (ref $Str) (local.get $msg))))

    ;; Read channel tag (i31ref).
    (local.set $tag
      (i31.get_s (ref.cast (ref i31)
        (struct.get $Channel $tag
          (ref.cast (ref $Channel) (local.get $ch))))))

    ;; Send to host — host reads bytes directly from the GC array.
    (call $interop/rust.wat:host_channel_send (local.get $tag) (local.get $bytes))

    ;; Sender continues with unit.
    (call $std/async.wat:queue_push
      (call $std/async.wat:make_unit_thunk (ref.as_non_null (local.get $cont))))

    (return_call $std/async.wat:resume)
  )


  ;; -- host_channel_recv -----------------------------------------------------
  ;;
  ;; host_channel_recv(ch, cont):
  ;;   Parks cont on the channel's receivers list and resumes.
  ;;   The host will deliver data during host_resume by calling
  ;;   channel_deliver, which wakes parked receivers.
  ;;
  ;; TODO: signal host to start async read for this channel.

  (func $interop/rust.wat:channel_recv
    (param $ch (ref null any))
    (param $cont (ref null any))

    (local $host_ch (ref $HostChannel))
    (local.set $host_ch (ref.cast (ref $HostChannel) (local.get $ch)))

    ;; Park cont on the channel's receivers list (FIFO).
    (struct.set $Channel $receivers (local.get $host_ch)
      (call $std/list.wat:concat
        (struct.get $Channel $receivers (local.get $host_ch))
        (struct.new $Cons
          (ref.as_non_null (local.get $cont))
          (struct.new $Nil))))

    (return_call $std/async.wat:resume)
  )


  ;; -- interop_op_read --------------------------------------------------------
  ;;
  ;; interop_op_read(stream, size, cont):
  ;;   1. Create a pending $Future with cont as waiter
  ;;   2. Call host_read(stream, size, future) — host starts async read
  ;;   3. Resume scheduler — task is parked on the future
  ;;
  ;; The host settles the future during host_resume when data arrives.

  (func $interop/rust.wat:op_read
    (param $stream (ref null any))
    (param $size (ref null any))
    (param $cont (ref null any))

    (local $future (ref $Future))

    ;; Create pending future with cont as waiter.
    (local.set $future (struct.new $Future
      (ref.null any)
      (struct.new $Cons
        (ref.as_non_null (local.get $cont))
        (struct.new $Nil))))

    ;; Tell host to start async read. Host captures the future ref.
    (call $interop/rust.wat:host_read
      (ref.as_non_null (local.get $stream))
      (ref.as_non_null (local.get $size))
      (local.get $future))

    ;; Resume scheduler — this task is parked on the future.
    (return_call $std/async.wat:resume)
  )


  ;; -- interop_panic ---------------------------------------------------------
  ;;
  ;; Called from runtime `panic` (operators.wat). Delegates to the host which
  ;; traps the instance with a diagnostic. Never returns.

  (func $interop/rust.wat:panic
    (call $interop/rust.wat:host_panic)
    unreachable
  )


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

  (global $interop/rust.wat:stdout (ref $HostChannel)
    (struct.new $HostChannel
      (struct.new $Nil)
      (struct.new $Nil)
      (ref.i31 (i32.const 1))))

  (global $interop/rust.wat:stderr (ref $HostChannel)
    (struct.new $HostChannel
      (struct.new $Nil)
      (struct.new $Nil)
      (ref.i31 (i32.const 2))))

  (global $interop/rust.wat:stdin (ref $HostChannel)
    (struct.new $HostChannel
      (struct.new $Nil)
      (struct.new $Nil)
      (ref.i31 (i32.const 0))))

  (func $interop/io:get_stdout (export "interop/io:get_stdout") (result (ref any))
    (global.get $interop/rust.wat:stdout))

  (func $interop/io:get_stderr (export "interop/io:get_stderr") (result (ref any))
    (global.get $interop/rust.wat:stderr))

  (func $interop/io:get_stdin (export "interop/io:get_stdin") (result (ref any))
    (global.get $interop/rust.wat:stdin))


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

  (func $interop/rust.wat:read_apply (type $Fn2)
    (param $_caps (ref null any))
    (param $args (ref null any))

    (local $cursor (ref null any))
    (local $cont (ref null any))
    (local $stream (ref null any))
    (local $size (ref null any))

    (local.set $cursor (local.get $args))
    ;; TODO this needs to got through args_* protocol!
    (local.set $cont (call $std/list.wat:head_any (local.get $cursor)))
    (local.set $cursor (call $std/list.wat:tail_any (local.get $cursor)))
    (local.set $stream (call $std/list.wat:head_any (local.get $cursor)))
    (local.set $cursor (call $std/list.wat:tail_any (local.get $cursor)))
    (local.set $size (call $std/list.wat:head_any (local.get $cursor)))

    (return_call $interop/rust.wat:op_read
      (local.get $stream)
      (local.get $size)
      (local.get $cont)))

  (global $interop/rust.wat:read_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $interop/rust.wat:read_apply)
      (ref.null $Captures)))

  (func $interop/io:get_read (export "interop/io:get_read") (result (ref any))
    (global.get $interop/rust.wat:read_closure))

)
