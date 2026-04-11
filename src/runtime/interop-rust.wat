;; Rust host interop — self-contained main runner.
;;
;; Exports _run_main (direct-style) which sets up host IO channels, runs the
;; user's main to completion, drains the scheduler, and calls sys_exit.
;;
;; Owns $HostChannel — a subtype of $Channel for host-managed IO.
;; send/recv on host channels delegate to host imports instead of using
;; the internal message queue. Dispatch is in operators.wat.
;;
;; The host provides:
;;   host_exit(i32)               — terminate with exit code
;;   host_write_stdout(i32, i32) — write bytes at (offset, length) to stdout
;;   host_write_stderr(i32, i32) — write bytes at (offset, length) to stderr
;;
;; A future interop-wasi.wat can provide the same _run_main export
;; backed by WASI fd_write / proc_exit instead.

(module

  ;; Declarative element segment — required by WASM spec for ref.func.
  (elem declare func $_done_cont_fn)


  ;; -- Host imports (provided by Rust runner) --------------------------------

  (import "env" "host_exit" (func $host_exit (param i32)))
  (import "env" "host_channel_send" (func $host_channel_send (param i32 i32 i32)))
  (import "env" "host_read" (func $host_read (param (ref any) (ref any) (ref any))))



  ;; -- Host channel helpers --------------------------------------------------

  ;; Create a host channel with the given tag.
  (func $create_host_channel (param $tag (ref any)) (result (ref $HostChannel))
    (struct.new $HostChannel
      (struct.new $Nil)
      (struct.new $Nil)
      (local.get $tag))
  )


  ;; -- host_channel_send -----------------------------------------------------
  ;;
  ;; host_channel_send(ch, msg, cont):
  ;;   1. Write msg to the host via the appropriate host_write import
  ;;   2. Queue unit_thunk(cont) to resume the sender
  ;;   3. Resume scheduler
  ;;
  ;; Dispatches stdout vs stderr by channel tag (i31ref: 1=stdout, 2=stderr).

  (func $interop_channel_send
    (param $ch (ref null any))
    (param $msg (ref null any))
    (param $cont (ref null any))

    (local $tag i32)
    (local $offset i32)
    (local $length i32)

    ;; Write string bytes to scratch memory.
    (call $str_write_to_mem (ref.cast (ref $Str) (local.get $msg)))
    (local.set $length)
    (local.set $offset)

    ;; Read channel tag (i31ref).
    (local.set $tag
      (i31.get_s (ref.cast (ref i31)
        (struct.get $Channel $tag
          (ref.cast (ref $Channel) (local.get $ch))))))

    ;; Send to host — host dispatches by tag.
    (call $host_channel_send (local.get $tag) (local.get $offset) (local.get $length))

    ;; Sender continues with unit.
    (call $queue_push
      (call $make_unit_thunk (ref.as_non_null (local.get $cont))))

    (return_call $resume)
  )


  ;; -- host_channel_recv -----------------------------------------------------
  ;;
  ;; host_channel_recv(ch, cont):
  ;;   Parks cont on the channel's receivers list and resumes.
  ;;   The host will deliver data during host_resume by calling
  ;;   channel_deliver, which wakes parked receivers.
  ;;
  ;; TODO: signal host to start async read for this channel.

  (func $interop_channel_recv
    (param $ch (ref null any))
    (param $cont (ref null any))

    (local $host_ch (ref $HostChannel))
    (local.set $host_ch (ref.cast (ref $HostChannel) (local.get $ch)))

    ;; Park cont on the channel's receivers list (FIFO).
    (struct.set $Channel $receivers (local.get $host_ch)
      (call $list_concat
        (struct.get $Channel $receivers (local.get $host_ch))
        (struct.new $Cons
          (ref.as_non_null (local.get $cont))
          (struct.new $Nil))))

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

  (func $interop_op_read
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
    (call $host_read
      (ref.as_non_null (local.get $stream))
      (ref.as_non_null (local.get $size))
      (local.get $future))

    ;; Resume scheduler — this task is parked on the future.
    (return_call $resume)
  )


  ;; -- Done continuation -----------------------------------------------------
  ;;
  ;; CPS function (type $Fn2) passed to main as its continuation.
  ;; When main "returns", this fires:
  ;;   1. Extract exit code from args list head
  ;;   2. Drain remaining scheduler tasks (e.g. pending IO writes)
  ;;   3. Call sys_exit with the exit code

  (func $_done_cont_fn (type $Fn2)
    (param $caps (ref null any))
    (param $args (ref null any))

    (local $code i32)
    (local $val (ref null any))

    ;; Extract result value from args list head.
    (local.set $val (call $list_head_any (local.get $args)))

    ;; Decode to i32 exit code.
    ;; Try i31ref first (small ints / bools).
    (block $decoded
      (block $not_i31
        (block $is_i31 (result (ref i31))
          (br $not_i31
            (br_on_cast $is_i31 (ref null any) (ref i31)
              (local.get $val))))
        (local.set $code (i31.get_s))
        (br $decoded))

      ;; Try $Num (f64 field).
      (block $not_num
        (block $is_num (result (ref $Num))
          (br $not_num
            (br_on_cast $is_num (ref null any) (ref $Num)
              (local.get $val))))
        (local.set $code (i32.trunc_f64_s (struct.get $Num $val)))
        (br $decoded))

      ;; Unknown type — default to 0.
    )

    ;; Drain remaining scheduler tasks (pending IO writes etc.).
    (call $resume)

    ;; Terminate.
    (return_call $host_exit (local.get $code))
  )


  ;; -- ·module_init ----------------------------------------------------------
  ;;
  ;; The fink compiler wraps each module's root in a synthetic `fink_module`
  ;; LetFn whose outer cont is `·module_init fink_module` — handing the
  ;; defined module fn to the host bootstrap. This stub receives the closure
  ;; as its single arg and is currently a no-op; a real implementation will
  ;; invoke fink_module with a done continuation and wire up exports. Exists
  ;; so the linker resolves the call; real semantics TBD.

  (func $module_init (export "module_init")
    (param $fink_module (ref null any))
    unreachable
  )


  ;; -- _run_main -------------------------------------------------------------
  ;;
  ;; Direct-style export. The single entry point for the host.
  ;;
  ;; 1. Creates stdin/stdout/stderr host channels
  ;; 2. Creates done continuation (captures exit code, drains, calls sys_exit)
  ;; 3. Builds args list [done_cont, cli_args, stdin, stdout, stderr]
  ;; 4. Calls main — enters CPS, scheduler takes over
  ;; 5. When scheduler drains, returns here (but sys_exit already called)
  ;;
  ;; $cli_args is a fink $List of $Str (byte strings) built by the host —
  ;; argv[0] is the program name, rest are CLI arguments.

  (func $_run_main (export "_run_main")
    (param $entry (ref null any))
    (param $cli_args (ref null any))

    (local $stdin  (ref null any))
    (local $stdout (ref null any))
    (local $stderr (ref null any))
    (local $done   (ref null any))
    (local $args   (ref null any))

    ;; Create host channels with i31ref tags (0=stdin, 1=stdout, 2=stderr).
    (local.set $stdin
      (call $create_host_channel (ref.i31 (i32.const 0))))
    (local.set $stdout
      (call $create_host_channel (ref.i31 (i32.const 1))))
    (local.set $stderr
      (call $create_host_channel (ref.i31 (i32.const 2))))

    ;; Create done continuation.
    (local.set $done
      (struct.new $Closure
        (ref.func $_done_cont_fn)
        (ref.null $Captures)))

    ;; Build args list: [done, cli_args, stdin, stdout, stderr]
    ;; (prepend in reverse).
    (local.set $args (call $list_nil))
    (local.set $args
      (call $list_prepend_any
        (local.get $stderr) (local.get $args)))
    (local.set $args
      (call $list_prepend_any
        (local.get $stdout) (local.get $args)))
    (local.set $args
      (call $list_prepend_any
        (local.get $stdin) (local.get $args)))
    (local.set $args
      (call $list_prepend_any
        (local.get $cli_args) (local.get $args)))
    (local.set $args
      (call $list_prepend_any
        (local.get $done) (local.get $args)))

    ;; Enter CPS world. Never returns — sys_exit terminates.
    (call $_apply
      (local.get $args)
      (local.get $entry))
  )

)
