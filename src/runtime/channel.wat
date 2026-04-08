;; Multi-message async channels (point-to-point).
;;
;; Primitives (direct-param calling convention, like other builtins):
;;   channel(ignored, cont) — create new channel; cont receives channel value
;;   send(ch, msg, cont)    — buffer message, trigger matching; cont receives unit
;;   receive(ch, cont)      — park receiver; cont receives message when matched
;;
;; Internal:
;;   process_msg_q(ch) — drain one (msg, receiver) pair per tick; self-requeues
;;
;; Both send and receive may push a process_msg_q task to the scheduler's
;; task queue. process_msg_q pops one message and one receiver, creates a
;; thunk(receiver, msg), and if more pairs remain, requeues itself.
;; This keeps matching cooperative — one pair per scheduler tick.

(module

  ;; Declarative element segment — required by WASM spec for ref.func.
  (elem declare func $_process_msg_q_fn)


  ;; -- Helpers --------------------------------------------------------------

  ;; Create a process_msg_q closure capturing [ch].
  (func $make_process_msg_q (param $ch (ref $Channel)) (result (ref $Closure))
    (struct.new $Closure
      (ref.func $_process_msg_q_fn)
      (array.new_fixed $Captures 1 (local.get $ch)))
  )


  ;; -- channel --------------------------------------------------------------
  ;;
  ;; channel(tag, cont):
  ;;   1. allocate $Channel with empty lists and user-supplied tag
  ;;   2. push thunk(cont, channel) to task queue
  ;;   3. run_next

  (func $channel (export "channel")
    (param $tag (ref null any))
    (param $cont (ref null any))

    (local $ch (ref $Channel))
    (local.set $ch (struct.new $Channel
      (struct.new $Nil)
      (struct.new $Nil)
      (ref.as_non_null (local.get $tag))))

    (call $queue_push
      (call $make_thunk
        (ref.as_non_null (local.get $cont))
        (local.get $ch)))
    (return_call $run_next)
  )


  ;; -- send -----------------------------------------------------------------
  ;;
  ;; send(ch, msg, cont):
  ;;   1. append msg to ch.$messages
  ;;   2. push process_msg_q(ch) to task queue
  ;;   3. push unit_thunk(cont) to task queue
  ;;   4. run_next

  (func $send (export "send")
    (param $ch_val (ref null any))
    (param $msg    (ref null any))
    (param $cont   (ref null any))

    (local $ch (ref $Channel))
    (local.set $ch (ref.cast (ref $Channel) (local.get $ch_val)))

    ;; Append msg to messages (FIFO).
    (struct.set $Channel $messages (local.get $ch)
      (call $list_concat
        (struct.get $Channel $messages (local.get $ch))
        (struct.new $Cons
          (ref.as_non_null (local.get $msg))
          (struct.new $Nil))))

    ;; Push process_msg_q to drain one pair.
    (call $queue_push (call $make_process_msg_q (local.get $ch)))

    ;; Sender continues with unit (always suspends).
    (call $queue_push
      (call $make_unit_thunk (ref.as_non_null (local.get $cont))))

    (return_call $run_next)
  )


  ;; -- receive --------------------------------------------------------------
  ;;
  ;; receive(ch, cont):
  ;;   1. append cont to ch.$receivers
  ;;   2. if ch.$messages non-empty, push process_msg_q(ch) to task queue
  ;;   3. run_next

  (func $receive (export "receive")
    (param $ch_val (ref null any))
    (param $cont   (ref null any))

    (local $ch (ref $Channel))
    (local.set $ch (ref.cast (ref $Channel) (local.get $ch_val)))

    ;; Append cont to receivers (FIFO).
    (struct.set $Channel $receivers (local.get $ch)
      (call $list_concat
        (struct.get $Channel $receivers (local.get $ch))
        (struct.new $Cons
          (ref.as_non_null (local.get $cont))
          (struct.new $Nil))))

    ;; If messages are buffered, trigger matching.
    (if (ref.test (ref $Cons) (struct.get $Channel $messages (local.get $ch)))
      (then
        (call $queue_push (call $make_process_msg_q (local.get $ch)))))

    (return_call $run_next)
  )


  ;; -- process_msg_q (internal) ---------------------------------------------
  ;;
  ;; Drains one (msg, receiver) pair per tick. Self-requeues if more remain.
  ;; Captures: [ch]. Called via _apply from task queue.
  ;;
  ;; process_msg_q(ch):
  ;;   1. if $messages empty OR $receivers empty → run_next (done)
  ;;   2. pop one msg, pop one receiver
  ;;   3. push thunk(receiver, msg) to task queue
  ;;   4. if both lists still non-empty → push self to task queue
  ;;   5. run_next

  (func $_process_msg_q_fn (type $Fn2)
    (param $caps (ref null any))
    (param $args (ref null any))

    (local $ch (ref $Channel))
    (local $messages (ref $List))
    (local $receivers (ref $List))
    (local $msg_cons (ref $Cons))
    (local $recv_cons (ref $Cons))

    (local.set $ch (ref.cast (ref $Channel)
      (ref.as_non_null (array.get $Captures
        (ref.cast (ref $Captures) (local.get $caps))
        (i32.const 0)))))

    (local.set $messages (struct.get $Channel $messages (local.get $ch)))
    (local.set $receivers (struct.get $Channel $receivers (local.get $ch)))

    ;; If either list is empty, nothing to match.
    (if (i32.or
          (ref.test (ref $Nil) (local.get $messages))
          (ref.test (ref $Nil) (local.get $receivers)))
      (then (return_call $run_next)))

    ;; Pop one message.
    (local.set $msg_cons (ref.cast (ref $Cons) (local.get $messages)))
    (struct.set $Channel $messages (local.get $ch)
      (struct.get $Cons $tail (local.get $msg_cons)))

    ;; Pop one receiver.
    (local.set $recv_cons (ref.cast (ref $Cons) (local.get $receivers)))
    (struct.set $Channel $receivers (local.get $ch)
      (struct.get $Cons $tail (local.get $recv_cons)))

    ;; Push thunk(receiver, msg) to task queue.
    (call $queue_push
      (call $make_thunk
        (struct.get $Cons $head (local.get $recv_cons))
        (struct.get $Cons $head (local.get $msg_cons))))

    ;; If more pairs remain, self-requeue.
    (if (i32.and
          (ref.test (ref $Cons) (struct.get $Channel $messages (local.get $ch)))
          (ref.test (ref $Cons) (struct.get $Channel $receivers (local.get $ch))))
      (then
        (call $queue_push (call $make_process_msg_q (local.get $ch)))))

    (return_call $run_next)
  )

)
