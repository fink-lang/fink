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

  ;; Type imports
  (import "rt/apply.wat"  "Closure"  (type $Closure  (sub any)))
  (import "rt/apply.wat"  "Captures" (type $Captures (sub any)))
  (import "rt/apply.wat"  "Fn3"      (type $Fn3      (sub any)))
  (import "std/list.wat"  "List"     (type $List     (sub any)))

  ;; Func imports
  (import "std/list.wat"  "empty"
    (func $list_empty (result (ref $List))))
  (import "std/list.wat"  "prepend"
    (func $list_prepend (param $head (ref any)) (param $tail (ref $List)) (result (ref $List))))
  (import "std/list.wat"  "concat"
    (func $list_concat (param $a (ref $List)) (param $b (ref $List)) (result (ref $List))))
  (import "std/list.wat"  "is_empty"
    (func $list_is_empty (param $list (ref $List)) (result i32)))
  (import "std/list.wat"  "head_any"
    (func $list_head_any (param $list (ref null any)) (result (ref null any))))
  (import "std/list.wat"  "tail_any"
    (func $list_tail_any (param $list (ref null any)) (result (ref null any))))

  (import "std/async.wat" "queue_push"
    (func $queue_push (param $task (ref any))))
  (import "rt/apply.wat" "make_thunk" (func $make_thunk (;apply-ctx;) (param (ref null any)) (param $cont (ref any)) (param $value (ref any)) (result (ref $Closure))))
  (import "rt/apply.wat" "make_unit_thunk" (func $make_unit_thunk (;apply-ctx;) (param (ref null any)) (param $cont (ref any)) (result (ref $Closure))))
  (import "std/async.wat" "resume"
    (func $resume))


  ;; -- $Channel type --------------------------------------------------------
  ;;
  ;; Multi-message async channel (point-to-point).
  ;; send buffers messages; an internal task drains (msg, receiver) pairs.
  ;; $tag: user-supplied metadata value (set at creation, immutable).
  ;;
  ;; $receivers stores $Waiter structs (ctx, cont) — see $Waiter below.
  ;; Receivers are parked here when they call receive() with no buffered
  ;; messages yet; when a sender shows up, _process_msg_q_fn pairs each
  ;; waiter with the next message and resumes the waiter under its own ctx.
  (type $Channel (@pub) (sub (struct
    (field $messages  (mut (ref $List)))
    (field $receivers (mut (ref $List)))
    (field $tag       (ref any))
  )))

  ;; $Waiter — a parked receiver. Pairs a continuation with the universe
  ;; ctx that was active when receive() was called, so the cont can resume
  ;; under that same ctx (not the scheduler's ctx) once a message arrives.
  (type $Waiter (sub (struct
    (field $ctx  (ref null any))
    (field $cont (ref any))
  )))


  ;; -- Helpers --------------------------------------------------------------

  ;; Declarative element segment — required by WASM spec for ref.func.
  (elem declare func $_process_msg_q_fn)

  ;; Create a process_msg_q closure capturing [ch].
  (func $make_process_msg_q (param $ch (ref $Channel)) (result (ref $Closure))
    (struct.new $Closure
      (ref.func $_process_msg_q_fn)
      (array.new_fixed $Captures 1 (local.get $ch)))
  )


  ;; -- _channel_new (host helper) -------------------------------------------
  ;;
  ;; Direct-style constructor for host use (non-CPS).
  ;; The host calls this to create channels before entering the CPS world
  ;; (e.g. for stdin/stdout/stderr injection into main).

  (func $_channel_new (@pub)
    (param $tag (ref null any))
    (result (ref any))
    (struct.new $Channel
      (call $list_empty)
      (call $list_empty)
      (ref.as_non_null (local.get $tag))))


  ;; -- channel --------------------------------------------------------------
  ;;
  ;; channel(tag, cont):
  ;;   1. allocate $Channel with empty lists and user-supplied tag
  ;;   2. push thunk(cont, channel) to task queue
  ;;   3. resume

  ;; channel(ctx, tag, cont):
  ;;   1. allocate $Channel with empty lists and user-supplied tag
  ;;   2. push thunk(cont, channel) under caller's ctx to task queue
  ;;   3. resume
  (func $channel (@pub) (@impl "std/channel.fnk:channel")
    (param $ctx (ref null any))
    (param $tag (ref null any))
    (param $cont (ref null any))

    (local $ch (ref $Channel))
    (local.set $ch (struct.new $Channel
      (call $list_empty)
      (call $list_empty)
      (ref.as_non_null (local.get $tag))))

    ;; Resume the caller under its own ctx via the captured thunk.
    (call $queue_push
      (call $make_thunk
        (local.get $ctx)
        (ref.as_non_null (local.get $cont))
        (local.get $ch)))
    (return_call $resume)
  )


  ;; -- op_shr (>> on channels) ----------------------------------------------
  ;;
  ;; op_shr(ch, msg, cont):
  ;;   1. append msg to ch.$messages
  ;;   2. push process_msg_q(ch) to task queue
  ;;   3. push unit_thunk(cont) to task queue
  ;;   4. resume

  (func $op_shr (@impl "std/operators.fnk:op_shr" $Channel)
    (param $ctx    (ref null any))
    (param $ch_val (ref null any))
    (param $msg    (ref null any))
    (param $cont   (ref null any))

    (local $ch (ref $Channel))
    (local.set $ch (ref.cast (ref $Channel) (local.get $ch_val)))

    ;; Append msg to messages (FIFO).
    (struct.set $Channel $messages (local.get $ch)
      (call $list_concat
        (struct.get $Channel $messages (local.get $ch))
        (call $list_prepend
          (ref.as_non_null (local.get $msg))
          (call $list_empty))))

    ;; Push process_msg_q to drain one pair.
    (call $queue_push (call $make_process_msg_q (local.get $ch)))

    ;; Sender continues with unit, under its captured ctx.
    (call $queue_push
      (call $make_unit_thunk
        (local.get $ctx) (ref.as_non_null (local.get $cont))))

    (return_call $resume)
  )


  ;; -- receive --------------------------------------------------------------
  ;;
  ;; receive(ch, cont):
  ;;   1. append cont to ch.$receivers
  ;;   2. if ch.$messages non-empty, push process_msg_q(ch) to task queue
  ;;   3. resume

  (func $receive (@pub) (@impl "std/channel.fnk:receive")
      (param $ctx (ref null any))
    (param $ch_val (ref null any))
    (param $cont   (ref null any))

    (local $ch (ref $Channel))
    (local.set $ch (ref.cast (ref $Channel) (local.get $ch_val)))

    ;; Park (ctx, cont) as a $Waiter on the FIFO receivers list. ctx is
    ;; captured here so the resume thunk built by _process_msg_q_fn can
    ;; restore it on the receiving side.
    (struct.set $Channel $receivers (local.get $ch)
      (call $list_concat
        (struct.get $Channel $receivers (local.get $ch))
        (call $list_prepend
          (struct.new $Waiter
            (local.get $ctx)
            (ref.as_non_null (local.get $cont)))
          (call $list_empty))))

    ;; If messages are buffered, trigger matching.
    ;; TODO: replace ref.test $Cons with a public list_is_empty op
    ;; (currently relies on list internal structural test).
    (if (i32.eqz (call $list_is_empty
          (struct.get $Channel $messages (local.get $ch))))
      (then
        (call $queue_push (call $make_process_msg_q (local.get $ch)))))

    (return_call $resume)
  )


  ;; -- process_msg_q (internal) ---------------------------------------------
  ;;
  ;; Drains one (msg, receiver) pair per tick. Self-requeues if more remain.
  ;; Captures: [ch]. Called via _apply from task queue.
  ;;
  ;; process_msg_q(ch):
  ;;   1. if $messages empty OR $receivers empty → resume (done)
  ;;   2. pop one msg, pop one receiver
  ;;   3. push thunk(receiver, msg) to task queue
  ;;   4. if both lists still non-empty → push self to task queue
  ;;   5. resume

  ;; The $_ctx param is the scheduler's ctx — intentionally unused.
  ;; The receiver's real ctx is captured per-receiver in the $Waiter
  ;; struct parked on ch.$receivers; we read it here and pass it into
  ;; make_thunk so the receiver resumes under its own ctx, not ours.
  (func $_process_msg_q_fn (type $Fn3)
    (param $caps (ref null any))
    (param $_sched_ctx (ref null any))
    (param $args (ref null any))

    (local $ch (ref $Channel))
    (local $messages (ref $List))
    (local $receivers (ref $List))
    (local $waiter (ref $Waiter))

    (local.set $ch (ref.cast (ref $Channel)
      (ref.as_non_null (array.get $Captures
        (ref.cast (ref $Captures) (local.get $caps))
        (i32.const 0)))))

    (local.set $messages (struct.get $Channel $messages (local.get $ch)))
    (local.set $receivers (struct.get $Channel $receivers (local.get $ch)))

    ;; If either list is empty, nothing to match.
    (if (i32.or
          (call $list_is_empty (local.get $messages))
          (call $list_is_empty (local.get $receivers)))
      (then (return_call $resume)))

    ;; Pop one waiter and one message.
    (local.set $waiter
      (ref.cast (ref $Waiter)
        (ref.as_non_null (call $list_head_any (local.get $receivers)))))

    (struct.set $Channel $messages (local.get $ch)
      (ref.cast (ref $List) (call $list_tail_any (local.get $messages))))

    (struct.set $Channel $receivers (local.get $ch)
      (ref.cast (ref $List) (call $list_tail_any (local.get $receivers))))

    ;; Resume the waiter's cont with the message, under the waiter's
    ;; captured ctx.
    (call $queue_push
      (call $make_thunk
        (struct.get $Waiter $ctx  (local.get $waiter))
        (struct.get $Waiter $cont (local.get $waiter))
        (ref.as_non_null (call $list_head_any (local.get $messages)))))

    ;; If more pairs remain, self-requeue.
    (if (i32.and
          (i32.eqz (call $list_is_empty
            (struct.get $Channel $messages (local.get $ch))))
          (i32.eqz (call $list_is_empty
            (struct.get $Channel $receivers (local.get $ch)))))
      (then
        (call $queue_push (call $make_process_msg_q (local.get $ch)))))

    (return_call $resume)
  )

)
