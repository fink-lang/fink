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
;; Task queue: a $List of $Closure thunks. FIFO via concat-to-end.
;; Each thunk is a zero-arg closure: fn(): <resume some continuation>.

(module

  ;; Declarative element segment — required by WASM spec for ref.func.
  (elem declare func $_thunk_fn $_settle_fn $_spawn_task_fn)

  ;; -- Task queue global -------------------------------------------------

  (global $task_queue (mut (ref $List)) (struct.new $Nil))


  ;; -- Helpers -----------------------------------------------------------

  ;; Push a task to the back of the queue.
  (func $queue_push (param $task (ref any))
    (global.set $task_queue
      (call $list_concat
        (global.get $task_queue)
        (struct.new $Cons (local.get $task) (struct.new $Nil))))
  )

  ;; Pop a task from the front of the queue. Traps if empty.
  (func $queue_pop (result (ref any))
    (local $cons (ref $Cons))
    (local.set $cons (ref.cast (ref $Cons) (global.get $task_queue)))
    (global.set $task_queue (struct.get $Cons $tail (local.get $cons)))
    (struct.get $Cons $head (local.get $cons))
  )

  ;; Run the next task from the queue. All primitives tail-call this.
  ;; Returns when the queue is empty — control propagates back through
  ;; the tail-call chain to whoever first entered CPS.
  (func $run_next
    (if (ref.test (ref $Nil) (global.get $task_queue))
      (then (return)))
    (return_call $_apply (struct.new $Nil) (call $queue_pop))
  )

  ;; Make a thunk (zero-arg task closure) that calls cont with a value.
  ;; Captures: [cont, value]. When called: _apply([value], cont).
  (func $_thunk_fn (type $Fn2) (param $caps (ref null any)) (param $args (ref null any))
    (local $captures (ref $Captures))
    (local $cont (ref any))
    (local $value (ref any))
    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $cont (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 0))))
    (local.set $value (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 1))))
    (return_call $_apply
      (struct.new $Cons (local.get $value) (struct.new $Nil))
      (local.get $cont))
  )

  (func $make_thunk (param $cont (ref any)) (param $value (ref any)) (result (ref $Closure))
    (struct.new $Closure
      (ref.func $_thunk_fn)
      (array.new_fixed $Captures 2 (local.get $cont) (local.get $value)))
  )

  ;; Make a thunk that calls cont with unit (i31 0).
  (func $make_unit_thunk (param $cont (ref any)) (result (ref $Closure))
    (call $make_thunk (local.get $cont) (ref.i31 (i32.const 0)))
  )


  ;; -- yield -------------------------------------------------------------
  ;;
  ;; yield(value, cont):
  ;;   1. wrap cont as unit thunk, push to back of queue
  ;;   2. run next task

  (func $yield (export "yield")
    (param $value (ref null any))
    (param $cont (ref null any))

    ;; Push current continuation as a unit thunk to back of queue.
    (call $queue_push (call $make_unit_thunk (ref.as_non_null (local.get $cont))))

    ;; Run next task.
    (return_call $run_next)
  )


  ;; -- spawn -------------------------------------------------------------
  ;;
  ;; spawn(task_fn, cont):
  ;;   1. create pending $Future
  ;;   2. create task thunk: fn(): task_fn(fn result: settle(future, result))
  ;;   3. push task to queue
  ;;   4. push thunk(cont, future) to queue (spawn suspends)
  ;;   5. run next task

  ;; The settle continuation — called when a spawned task produces a result.
  ;; Captures: [future]. Called via _apply with args list [result].
  (func $_settle_fn (type $Fn2) (param $caps (ref null any)) (param $args (ref null any))
    (local $future (ref $Future))
    (local $result (ref any))
    (local.set $future (ref.cast (ref $Future)
      (ref.as_non_null (array.get $Captures
        (ref.cast (ref $Captures) (local.get $caps))
        (i32.const 0)))))
    ;; Result is first element of args list.
    (local.set $result (struct.get $Cons $head
      (ref.cast (ref $Cons) (local.get $args))))
    (call $settle (local.get $future) (local.get $result))
    (return_call $run_next)
  )

  ;; The spawned task body — calls task_fn with the settle continuation.
  ;; Captures: [task_fn, settle_cont]. Called via _apply.
  (func $_spawn_task_fn (type $Fn2) (param $caps (ref null any)) (param $args (ref null any))
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
      (struct.new $Cons (local.get $settle_cont) (struct.new $Nil))
      (local.get $task_fn))
  )

  (func $spawn (export "spawn")
    (param $task_fn (ref null any))
    (param $cont (ref null any))

    (local $future (ref $Future))
    (local $settle_cont (ref $Closure))
    (local $task (ref $Closure))

    ;; Create pending future.
    (local.set $future (struct.new $Future
      (ref.null any)    ;; value = null (pending)
      (struct.new $Nil) ;; waiters = empty
    ))

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
    (return_call $run_next)
  )


  ;; -- await -------------------------------------------------------------
  ;;
  ;; await(future, cont):
  ;;   if settled: push thunk(cont, value) to queue
  ;;   if pending: push cont to future.$waiters
  ;;   run next task

  (func $await (export "await")
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
          (struct.new $Cons (ref.as_non_null (local.get $cont))
            (struct.get $Future $waiters (local.get $future)))))
      (else
        ;; Settled — push thunk(cont, value) to task queue.
        (call $queue_push
          (call $make_thunk
            (ref.as_non_null (local.get $cont))
            (ref.as_non_null (local.get $value))))))

    ;; Run next task.
    (return_call $run_next)
  )


  ;; -- settle (internal) -------------------------------------------------
  ;;
  ;; settle(future, value):
  ;;   1. set future.$value = value
  ;;   2. for each waiter in future.$waiters: push thunk(waiter, value)
  ;;   3. clear waiters

  (func $settle (param $future (ref $Future)) (param $value (ref any))
    (local $waiters (ref $List))
    (local $cons (ref $Cons))

    ;; Set the settled value.
    (struct.set $Future $value (local.get $future) (local.get $value))

    ;; Move all waiters to the task queue.
    (local.set $waiters (struct.get $Future $waiters (local.get $future)))
    (block $done
      (loop $loop
        (br_if $done (ref.test (ref $Nil) (local.get $waiters)))
        (local.set $cons (ref.cast (ref $Cons) (local.get $waiters)))
        (call $queue_push
          (call $make_thunk
            (struct.get $Cons $head (local.get $cons))
            (local.get $value)))
        (local.set $waiters (struct.get $Cons $tail (local.get $cons)))
        (br $loop)
      )
    )

    ;; Clear waiters.
    (struct.set $Future $waiters (local.get $future) (struct.new $Nil))
  )

)
