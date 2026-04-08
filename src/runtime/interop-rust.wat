;; Rust host interop — self-contained main runner.
;;
;; Exports _run_main (direct-style) which sets up IO channels, runs the
;; user's main to completion, drains the scheduler, and calls sys_exit.
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
  (elem declare func $_io_receiver_fn $_receive_thunk_fn $_done_cont_fn)


  ;; -- Host imports (provided by Rust runner) --------------------------------

  (import "env" "host_exit" (func $host_exit (param i32)))
  (import "env" "host_write_stdout" (func $host_write_stdout (param i32 i32)))
  (import "env" "host_write_stderr" (func $host_write_stderr (param i32 i32)))

  ;; -- User code import ------------------------------------------------------

  (import "@fink/user" "main" (func $main (type $Fn2)))


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
    (call $run_next)

    ;; Terminate.
    (return_call $host_exit (local.get $code))
  )


  ;; -- IO receiver ------------------------------------------------------------
  ;;
  ;; Single CPS receiver (type $Fn2) for both stdout and stderr.
  ;; Dispatches to the correct host write based on the channel's $tag field.
  ;; Called by process_msg_q via _apply([msg], receiver).
  ;;
  ;; Captures: [ch]. Args: [msg].

  (func $_io_receiver_fn (type $Fn2)
    (param $caps (ref null any))
    (param $args (ref null any))

    (local $ch (ref null any))
    (local $msg (ref null any))
    (local $tag i32)
    (local $offset i32)
    (local $length i32)

    ;; Extract channel from captures[0].
    (local.set $ch
      (array.get $Captures
        (ref.cast (ref $Captures) (local.get $caps))
        (i32.const 0)))

    ;; Extract message from args list head.
    (local.set $msg
      (call $list_head_any (local.get $args)))

    ;; Write string bytes to scratch memory.
    (call $str_write_to_mem (ref.cast (ref $Str) (local.get $msg)))
    (local.set $length)
    (local.set $offset)

    ;; Read channel tag (i31ref) to dispatch.
    (local.set $tag
      (i31.get_s (ref.cast (ref i31)
        (struct.get $Channel $tag
          (ref.cast (ref $Channel) (local.get $ch))))))

    ;; Dispatch: tag 1 = stdout, tag 2 = stderr.
    (if (i32.eq (local.get $tag) (i32.const 1))
      (then (call $host_write_stdout (local.get $offset) (local.get $length)))
      (else (call $host_write_stderr (local.get $offset) (local.get $length))))

    ;; Re-register: receive(ch, self_closure).
    (return_call $receive
      (local.get $ch)
      (struct.new $Closure
        (ref.func $_io_receiver_fn)
        (ref.cast (ref null $Captures) (local.get $caps))))
  )


  ;; -- Receive thunk ---------------------------------------------------------
  ;;
  ;; Scheduler task that calls receive(ch, receiver_closure).
  ;; Captures: [ch, receiver_closure]. Called via _apply from task queue.

  (func $_receive_thunk_fn (type $Fn2)
    (param $caps (ref null any))
    (param $args (ref null any))

    (local $captures (ref null $Captures))
    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))

    ;; receive(ch, receiver_closure)
    (return_call $receive
      (array.get $Captures (ref.as_non_null (local.get $captures)) (i32.const 0))
      (array.get $Captures (ref.as_non_null (local.get $captures)) (i32.const 1)))
  )


  ;; -- _run_main -------------------------------------------------------------
  ;;
  ;; Direct-style export. The single entry point for the host.
  ;;
  ;; 1. Creates stdin/stdout/stderr channels
  ;; 2. Queues receive thunks for stdout/stderr
  ;; 3. Creates done continuation (captures exit code, drains, calls sys_exit)
  ;; 4. Builds args list [done_cont, stdin, stdout, stderr]
  ;; 5. Calls main — enters CPS, scheduler takes over
  ;; 6. When scheduler drains, returns here (but sys_exit already called)

  (func $_run_main (export "_run_main")

    (local $stdin  (ref null any))
    (local $stdout (ref null any))
    (local $stderr (ref null any))
    (local $done   (ref null any))
    (local $args   (ref null any))

    ;; Create channels with i31ref tags (0=stdin, 1=stdout, 2=stderr).
    (local.set $stdin
      (call $_channel_new (ref.i31 (i32.const 0))))
    (local.set $stdout
      (call $_channel_new (ref.i31 (i32.const 1))))
    (local.set $stderr
      (call $_channel_new (ref.i31 (i32.const 2))))

    ;; Queue receive thunks for stdout and stderr.
    ;; Both use $_io_receiver_fn which dispatches by channel tag.
    (call $queue_push
      (struct.new $Closure
        (ref.func $_receive_thunk_fn)
        (array.new_fixed $Captures 2
          (ref.as_non_null (local.get $stdout))
          (struct.new $Closure
            (ref.func $_io_receiver_fn)
            (array.new_fixed $Captures 1
              (ref.as_non_null (local.get $stdout)))))))

    (call $queue_push
      (struct.new $Closure
        (ref.func $_receive_thunk_fn)
        (array.new_fixed $Captures 2
          (ref.as_non_null (local.get $stderr))
          (struct.new $Closure
            (ref.func $_io_receiver_fn)
            (array.new_fixed $Captures 1
              (ref.as_non_null (local.get $stderr)))))))

    ;; Create done continuation.
    (local.set $done
      (struct.new $Closure
        (ref.func $_done_cont_fn)
        (ref.null $Captures)))

    ;; Build args list: [done, stdin, stdout, stderr] (prepend in reverse).
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
        (local.get $done) (local.get $args)))

    ;; Enter CPS world. Never returns — sys_exit terminates.
    (return_call $main (ref.null any) (local.get $args))
  )

)
