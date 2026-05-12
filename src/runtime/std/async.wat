;; Cooperative multitasking scheduler.
;;
;; Primitives (direct-param calling convention, like other builtins):
;;   yield(value, cont)  — suspend current task, switch to next
;;   spawn(task_fn, cont) — create new task, return future
;;   await(future, cont)  — wait for future to settle
;;
;; Internal:
;;   settle(future, value) — mark future settled, move waiters to queue
;;
;; All primitives are suspension points — they push work to the task
;; queue and pop the next task to run. No primitive ever calls a
;; continuation directly.
;;
;; Host interop:
;;   When the task queue empties, the scheduler calls host_resume to
;;   yield execution to the host. The host can block on IO (epoll etc.),
;;   settle host futures via direct WASM calls, and return. If the queue
;;   is still empty after host_resume, the program is done.
;;
;; Task queue: a $List of $Closure thunks. FIFO via concat-to-end.
;; Each thunk is a zero-arg closure: fn(): <resume some continuation>.

(module

  ;; Type imports
  (import "rt/apply.wat" "Closure"  (type $Closure  (sub any)))
  (import "rt/apply.wat" "Captures" (type $Captures (sub any)))
  (import "rt/apply.wat" "Fn2"      (type $Fn2      (sub any)))
  (import "rt/apply.wat" "Fn3"      (type $Fn3      (sub any)))
  (import "std/list.wat" "List"     (type $List     (sub any)))

  ;; Func imports
  (import "rt/apply.wat" "apply"
    (func $_apply (param $args (ref null any)) (param $callee (ref null any))))
  (import "rt/apply.wat" "make_thunk"
    (func $make_thunk (param $cont (ref any)) (param $value (ref any)) (result (ref $Closure))))
  (import "rt/apply.wat" "make_unit_thunk"
    (func $make_unit_thunk (param $cont (ref any)) (result (ref $Closure))))

  (import "std/list.wat" "empty"
    (func $list_empty (result (ref $List))))
  (import "std/list.wat" "prepend"
    (func $list_prepend (param $head (ref any)) (param $tail (ref $List)) (result (ref $List))))
  (import "std/list.wat" "concat"
    (func $list_concat (param $a (ref $List)) (param $b (ref $List)) (result (ref $List))))
  (import "std/list.wat" "is_empty"
    (func $list_is_empty (param $list (ref $List)) (result i32)))
  (import "std/list.wat" "head_any"
    (func $list_head_any (param $list (ref null any)) (result (ref null any))))
  (import "std/list.wat" "tail_any"
    (func $list_tail_any (param $list (ref null any)) (result (ref null any))))

  ;; TODO: route through virtual interop namespace (e.g.
  ;; std/interop.fnk:resume) so async doesn't bind directly to env.
  ;; The selected interop/<target>.wat fills the slot.
  (import "env" "host_resume" (func $host_resume))


  ;; -- $Future type ---------------------------------------------------------
  ;;
  ;; Opaque future for cooperative multitasking.
  ;; Returned by `spawn`; passed to `await`. Null value = pending;
  ;; non-null = settled (fink has no null values, so this is unambiguous).
  ;; Waiters: continuations waiting for this future to settle.
  (type $Future (@pub) (struct
    (field $value   (mut (ref null any)))
    (field $waiters (mut (ref $List)))
  ))


  ;; -- Task queue global ----------------------------------------------------

  ;; Task queue. Null = empty (lazily initialized to list_empty on first push).
  (global $task_queue (mut (ref null $List)) (ref.null $List))

  ;; Declarative element segment — required by WASM spec for ref.func.
  (elem declare func $_settle_fn $_spawn_task_fn)


  ;; -- Helpers --------------------------------------------------------------

  ;; Lazy-init: returns the queue, allocating an empty list if null.
  (func $_queue_get (result (ref $List))
    (if (ref.is_null (global.get $task_queue))
      (then (global.set $task_queue (call $list_empty))))
    (ref.as_non_null (global.get $task_queue))
  )

  ;; True iff the task queue is empty (null or list-empty).
  (func $_queue_is_empty (result i32)
    (if (result i32) (ref.is_null (global.get $task_queue))
      (then (i32.const 1))
      (else (call $list_is_empty (ref.as_non_null (global.get $task_queue)))))
  )

  ;; Push a task to the back of the queue.
  (func $queue_push (@pub) (param $task (ref any))
    (global.set $task_queue
      (call $list_concat
        (call $_queue_get)
        (call $list_prepend (local.get $task) (call $list_empty))))
  )

  ;; Pop a task from the front of the queue. Traps if empty.
  (func $queue_pop (@pub) (result (ref any))
    (local $head (ref null any))
    (local.set $head (call $list_head_any (call $_queue_get)))
    (global.set $task_queue
      (ref.cast (ref $List) (call $list_tail_any (call $_queue_get))))
    (ref.as_non_null (local.get $head))
  )

  ;; Resume the scheduler. All primitives tail-call this after
  ;; enqueuing work. When the queue empties, yields to the host
  ;; (host_resume) so it can process IO / settle host futures.
  ;; If the queue is still empty after host_resume, program is done.
  (func $resume (@pub)
    (if (call $_queue_is_empty)
      (then
        (call $host_resume)
        (if (call $_queue_is_empty)
          (then (return)))))
    (return_call $_apply (call $list_empty) (call $queue_pop))
  )


  ;; -- yield ---------------------------------------------------------------
  ;;
  ;; yield(value, cont):
  ;;   1. wrap cont as unit thunk, push to back of queue
  ;;   2. run next task

  (func $yield (@pub) (@impl "std/async.fnk:yield")
    (param $value (ref null any))
    (param $cont (ref null any))

    ;; Push current continuation as a unit thunk to back of queue.
    (call $queue_push (call $make_unit_thunk (ref.as_non_null (local.get $cont))))

    ;; Run next task.
    (return_call $resume)
  )


  ;; -- spawn ---------------------------------------------------------------
  ;;
  ;; spawn(task_fn, cont):
  ;;   1. create pending $Future
  ;;   2. create task thunk: fn(): task_fn(fn result: settle(future, result))
  ;;   3. push task to queue
  ;;   4. push thunk(cont, future) to queue (spawn suspends)
  ;;   5. run next task

  ;; The settle continuation — called when a spawned task produces a result.
  ;; Captures: [future]. Called via _apply with args list [result].
  (func $_settle_fn (type $Fn3) (param $caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $future (ref $Future))
    (local $result (ref any))
    (local.set $future (ref.cast (ref $Future)
      (ref.as_non_null (array.get $Captures
        (ref.cast (ref $Captures) (local.get $caps))
        (i32.const 0)))))
    ;; Result is first element of args list.
    (local.set $result (ref.as_non_null (call $list_head_any (local.get $args))))
    (call $settle (local.get $future) (local.get $result))
    (return_call $resume)
  )

  ;; The spawned task body — calls task_fn with the settle continuation.
  ;; Captures: [task_fn, settle_cont]. Called via _apply.
  (func $_spawn_task_fn (type $Fn3) (param $caps (ref null any)) (param $_ctx (ref null any)) (param $args (ref null any))
    (local $captures (ref $Captures))
    (local $task_fn (ref any))
    (local $settle_cont (ref any))
    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $task_fn (ref.as_non_null
      (array.get $Captures (local.get $captures) (i32.const 0))))
    (local.set $settle_cont (ref.as_non_null
      (array.get $Captures (local.get $captures) (i32.const 1))))
    ;; Call task_fn with args = [settle_cont]
    (return_call $_apply
      (call $list_prepend (local.get $settle_cont) (call $list_empty))
      (local.get $task_fn))
  )

  (func $spawn (@pub) (@impl "std/async.fnk:spawn")
    (param $task_fn (ref null any))
    (param $cont (ref null any))

    (local $future (ref $Future))
    (local $settle_cont (ref $Closure))
    (local $task (ref $Closure))

    ;; Create pending future.
    (local.set $future (struct.new $Future
      (ref.null any)        ;; value = null (pending)
      (call $list_empty)))  ;; waiters = empty

    ;; Create settle continuation: captures [future].
    (local.set $settle_cont (struct.new $Closure
      (ref.func $_settle_fn)
      (array.new_fixed $Captures 1 (local.get $future))))

    ;; Create task thunk: captures [task_fn, settle_cont].
    (local.set $task (struct.new $Closure
      (ref.func $_spawn_task_fn)
      (array.new_fixed $Captures 2
        (ref.as_non_null (local.get $task_fn))
        (local.get $settle_cont))))

    ;; Push task and current continuation (wrapped with future) to queue.
    (call $queue_push (local.get $task))
    (call $queue_push (call $make_thunk
      (ref.as_non_null (local.get $cont))
      (local.get $future)))

    ;; Run next task.
    (return_call $resume)
  )


  ;; -- await ---------------------------------------------------------------
  ;;
  ;; await(future, cont):
  ;;   if settled: push thunk(cont, value) to queue
  ;;   if pending: push cont to future.$waiters
  ;;   run next task

  (func $await (@pub) (@impl "std/async.fnk:await")
    (param $future_val (ref null any))
    (param $cont (ref null any))

    (local $future (ref $Future))
    (local $value (ref null any))

    (local.set $future (ref.cast (ref $Future) (local.get $future_val)))

    ;; Check if future is settled.
    (local.set $value (struct.get $Future $value (local.get $future)))
    (if (ref.is_null (local.get $value))
      (then
        ;; Pending — add cont to future's waiters list.
        (struct.set $Future $waiters (local.get $future)
          (call $list_prepend
            (ref.as_non_null (local.get $cont))
            (struct.get $Future $waiters (local.get $future)))))
      (else
        ;; Settled — push thunk(cont, value) to task queue.
        (call $queue_push
          (call $make_thunk
            (ref.as_non_null (local.get $cont))
            (ref.as_non_null (local.get $value))))))

    ;; Run next task.
    (return_call $resume)
  )


  ;; -- settle (internal) ---------------------------------------------------
  ;;
  ;; settle(future, value):
  ;;   1. set future.$value = value
  ;;   2. for each waiter in future.$waiters: push thunk(waiter, value)
  ;;   3. clear waiters

  (func $settle (@pub) (param $future (ref $Future)) (param $value (ref any))
    (local $waiters (ref $List))

    ;; Set the settled value.
    (struct.set $Future $value (local.get $future) (local.get $value))

    ;; Move all waiters to the task queue.
    (local.set $waiters (struct.get $Future $waiters (local.get $future)))
    (block $done
      (loop $loop
        (br_if $done (call $list_is_empty (local.get $waiters)))
        (call $queue_push
          (call $make_thunk
            (ref.as_non_null (call $list_head_any (local.get $waiters)))
            (local.get $value)))
        (local.set $waiters
          (ref.cast (ref $List) (call $list_tail_any (local.get $waiters))))
        (br $loop)
      )
    )

    ;; Clear waiters.
    (struct.set $Future $waiters (local.get $future) (call $list_empty))
  )


  ;; -- _settle_future (host-callable) --------------------------------------
  ;;
  ;; Exported for the host to settle futures during host_resume.
  ;; Takes untyped (ref any) params — casts internally.

  (func $_settle_future (export "env:_settle_future")
    (param $future_ref (ref null any))
    (param $value (ref null any))

    (call $settle
      (ref.cast (ref $Future) (local.get $future_ref))
      (ref.as_non_null (local.get $value)))
  )

)
