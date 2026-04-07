;; Cooperative multitasking scheduler.
;;
;; Primitives:
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

  ;; Wrap a closure call into the args-list convention:
  ;; apply_1(result, cont) packs result into a $Cons and calls $_apply.
  ;; These are defined in list.wat (merged into same module).

  ;; Push a task to the back of the queue.
  (func $queue_push (param $task (ref any))
    (global.set $task_queue
      (call $list_concat
        (global.get $task_queue)
        (struct.new $Cons (local.get $task) (struct.new $Nil))))
  )

  ;; Pop a task from the front of the queue. Traps if empty.
  (func $queue_pop (result (ref any))
    (local $head (ref any))
    (local $cons (ref $Cons))
    (local.set $cons (ref.cast (ref $Cons) (global.get $task_queue)))
    (local.set $head (struct.get $Cons $head (local.get $cons)))
    (global.set $task_queue (struct.get $Cons $tail (local.get $cons)))
    (local.get $head)
  )

  ;; Run the next task from the queue. All primitives tail-call this.
  (func $run_next
    (return_call $_apply (struct.new $Nil) (call $queue_pop))
  )

  ;; Make a thunk (zero-arg task closure) that calls cont with a value.
  ;; thunk = $Closure(fn(caps, args): _apply([value], cont), [cont, value])
  ;; We need a lifted function that reads cont and value from captures.
  (func $_thunk_fn (param $caps (ref null any)) (param $args (ref null any))
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

  ;; Make a thunk that calls cont with no meaningful value (unit = i31 0).
  (func $make_unit_thunk (param $cont (ref any)) (result (ref $Closure))
    (call $make_thunk (local.get $cont) (ref.i31 (i32.const 0)))
  )


  ;; -- yield -------------------------------------------------------------
  ;;
  ;; yield(caps, args):
  ;;   args = [value, cont]   (value ignored for scheduling, cont = resume)
  ;;   1. wrap cont as unit thunk, push to back of queue
  ;;   2. run next task

  (func $yield (export "yield")
    (param $caps (ref null any))
    (param $args (ref null any))

    (local $args_cons (ref $Cons))
    (local $value (ref any))
    (local $cont (ref any))

    ;; Pop value (ignored for now) and cont from args list.
    (local.set $args_cons (ref.cast (ref $Cons) (local.get $args)))
    (local.set $cont (struct.get $Cons $head (local.get $args_cons)))
    (local.set $args_cons (ref.cast (ref $Cons)
      (struct.get $Cons $tail (local.get $args_cons))))
    (local.set $value (struct.get $Cons $head (local.get $args_cons)))

    ;; Push current continuation as a unit thunk to back of queue.
    (call $queue_push (call $make_unit_thunk (local.get $cont)))

    ;; Run next task.
    (return_call $run_next)
  )


  ;; -- spawn -------------------------------------------------------------
  ;;
  ;; spawn(caps, args):
  ;;   args = [task_fn, cont]
  ;;   1. create pending $Future
  ;;   2. create task thunk: fn(): task_fn(fn result: settle(future, result))
  ;;   3. push task to queue
  ;;   4. push thunk(cont, future) to queue (spawn suspends)
  ;;   5. run next task

  ;; The settle continuation — called when a spawned task produces a result.
  ;; Captures: [future]. Args: [result, ...].
  (func $_settle_fn (param $caps (ref null any)) (param $args (ref null any))
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
  ;; Captures: [task_fn, settle_cont].
  (func $_spawn_task_fn (param $caps (ref null any)) (param $args (ref null any))
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
    (param $caps (ref null any))
    (param $args (ref null any))

    (local $args_cons (ref $Cons))
    (local $cont (ref any))
    (local $task_fn (ref any))
    (local $future (ref $Future))
    (local $settle_cont (ref $Closure))
    (local $task (ref $Closure))

    ;; Pop cont and task_fn from args list.
    (local.set $args_cons (ref.cast (ref $Cons) (local.get $args)))
    (local.set $cont (struct.get $Cons $head (local.get $args_cons)))
    (local.set $args_cons (ref.cast (ref $Cons)
      (struct.get $Cons $tail (local.get $args_cons))))
    (local.set $task_fn (struct.get $Cons $head (local.get $args_cons)))

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
      (array.new_fixed $Captures 2 (local.get $task_fn) (local.get $settle_cont))))

    ;; Push task and current continuation (wrapped with future) to queue.
    (call $queue_push (local.get $task))
    (call $queue_push (call $make_thunk (local.get $cont) (local.get $future)))

    ;; Run next task.
    (return_call $run_next)
  )


  ;; -- await -------------------------------------------------------------
  ;;
  ;; await(caps, args):
  ;;   args = [future, cont]
  ;;   if settled: push thunk(cont, value) to queue
  ;;   if pending: push cont to future.$waiters
  ;;   run next task

  (func $await (export "await")
    (param $caps (ref null any))
    (param $args (ref null any))

    (local $args_cons (ref $Cons))
    (local $cont (ref any))
    (local $future (ref $Future))
    (local $value (ref null any))

    ;; Pop cont and future from args list.
    (local.set $args_cons (ref.cast (ref $Cons) (local.get $args)))
    (local.set $cont (struct.get $Cons $head (local.get $args_cons)))
    (local.set $args_cons (ref.cast (ref $Cons)
      (struct.get $Cons $tail (local.get $args_cons))))
    (local.set $future (ref.cast (ref $Future)
      (struct.get $Cons $head (local.get $args_cons))))

    ;; Check if future is settled.
    (local.set $value (struct.get $Future $value (local.get $future)))
    (if (ref.is_null (local.get $value))
      (then
        ;; Pending — add cont to future's waiters list.
        (struct.set $Future $waiters (local.get $future)
          (struct.new $Cons (local.get $cont)
            (struct.get $Future $waiters (local.get $future)))))
      (else
        ;; Settled — push thunk(cont, value) to task queue.
        (call $queue_push
          (call $make_thunk (local.get $cont)
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
