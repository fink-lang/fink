;; Effects substrate -- low-level ctx primitives.
;;
;; Exposes two user-importable fns:
;;   {set_ctx, get_ctx} = import 'std/effects.fnk'
;;   old_ctx = set_ctx new_ctx
;;   cur_ctx = get_ctx _
;;
;; These are intentionally raw -- they expose ctx as a fink value so
;; threading regressions can be hunted down from the language level.
;; Higher-level helpers (with-style handlers, register/dispatch) sit
;; on top of these once threading is fully reliable.

(module

  (import "rt/apply.wat" "Closure"  (type $Closure  (sub any)))
  (import "rt/apply.wat" "Captures" (type $Captures (sub any)))
  (import "rt/apply.wat" "Fn3"      (type $Fn3      (sub any)))

  (import "rt/apply.wat" "args_head"
    (func $args_head (param (ref null any)) (result (ref null any))))
  (import "rt/apply.wat" "args_tail"
    (func $args_tail (param (ref null any)) (result (ref null any))))
  (import "rt/apply.wat" "apply_3"
    (func $apply_3
      (param $args (ref null any))
      (param $ctx (ref null any))
      (param $callee (ref null any))))
  (import "rt/apply.wat" "args_empty"
    (func $args_empty (result (ref any))))
  (import "rt/apply.wat" "args_prepend"
    (func $args_prepend (param $head (ref null any)) (param $tail (ref any)) (result (ref any))))


  ;; -- handler frame stack -------------------------------------------
  ;;
  ;; Substrate-internal data structure: a singly-linked list of frames,
  ;; one per active `with` block. Each frame holds the k_outer cont --
  ;; where `abort v` jumps to. Fink code never sees frames; the only
  ;; code that touches the stack is the `with H: B` lowering (push on
  ;; entry, pop on normal return) and the `abort` impl (pop + jump).
  ;;
  ;; Module-level global stop-gap -- expected to migrate to a ctx slot
  ;; once the substrate surface settles. Same trajectory as
  ;; $get_handler / $set_handler below.

  (type $Frame (struct
    (field $k_outer (ref any))
    (field $parent  (ref null $Frame))))

  (global $frame_stack (mut (ref null $Frame))
    (ref.null $Frame))

  ;; Push a frame holding k_outer; returns nothing.
  (func $frame_push (param $k_outer (ref any))
    (global.set $frame_stack
      (struct.new $Frame
        (local.get $k_outer)
        (global.get $frame_stack))))

  ;; Pop the top frame; returns its k_outer. Traps if stack is empty.
  (func $frame_pop (result (ref any))
    (local $top (ref $Frame))
    (local.set $top
      (ref.as_non_null (global.get $frame_stack)))
    (global.set $frame_stack
      (struct.get $Frame $parent (local.get $top)))
    (struct.get $Frame $k_outer (local.get $top)))


  ;; -- set_ctx --------------------------------------------------------
  ;;
  ;; Fink-level signature: `set_ctx new_ctx -> old_ctx`.
  ;;
  ;; CPS shape (Fn3): args = [cont, new_ctx]. Returns the caller's ctx
  ;; to cont, threads `new_ctx` as the cont's ctx -- every fink call
  ;; downstream of the cont sees `new_ctx` as their universe.

  (elem declare func $set_ctx_apply)

  (func $set_ctx_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref null any))
    (local $new_ctx (ref null any))
    (local $result_args (ref any))

    (local.set $cont (call $args_head (local.get $args)))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $new_ctx (call $args_head (local.get $args)))

    (local.set $result_args
      (call $args_prepend (local.get $ctx) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $new_ctx)
      (local.get $cont)))

  (global $set_ctx_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $set_ctx_apply)
      (ref.null $Captures)))

  (func $set_ctx (@pub) (@impl "std/effects.fnk:set_ctx") (result (ref any))
    (global.get $set_ctx_closure))


  ;; -- get_ctx --------------------------------------------------------
  ;;
  ;; Fink-level signature: `get_ctx _ -> ctx`.
  ;;
  ;; CPS shape (Fn3): args = [cont, _]. Returns the caller's ctx to
  ;; cont without mutating it -- threads ctx unchanged.

  (elem declare func $get_ctx_apply)

  (func $get_ctx_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref null any))
    (local $result_args (ref any))

    (local.set $cont (call $args_head (local.get $args)))

    (local.set $result_args
      (call $args_prepend (local.get $ctx) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $ctx)
      (local.get $cont)))

  (global $get_ctx_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $get_ctx_apply)
      (ref.null $Captures)))

  (func $get_ctx (@pub) (@impl "std/effects.fnk:get_ctx") (result (ref any))
    (global.get $get_ctx_closure))


  ;; -- abort ----------------------------------------------------------
  ;;
  ;; Substrate primitive for the effect-handler `with` machinery.
  ;; Fink-level signature: `abort v`. Pops the top frame off the
  ;; handler stack and return_calls its k_outer with v -- non-local
  ;; return to the nearest enclosing `with` block.
  ;;
  ;; Traps with "no handler frame" if abort is called with an empty
  ;; stack (no enclosing `with`).
  ;;
  ;; Importable: `{abort} = import 'std/effects.fnk'`. Fn3-shaped
  ;; closure, same path as set_ctx / get_ctx. Note: the cont arg the
  ;; compiler passes is the caller's continuation -- abort discards it
  ;; (that's the whole point of non-local return).

  (elem declare func $abort_apply)

  (func $abort_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $value (ref null any))
    (local $k_outer (ref any))
    (local $result_args (ref any))

    ;; args = [cont, value]; we only need value (cont is discarded).
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $value (call $args_head (local.get $args)))

    ;; Pop top frame -> k_outer. Traps if stack empty.
    (local.set $k_outer (call $frame_pop))

    (local.set $result_args
      (call $args_prepend (local.get $value) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $ctx)
      (local.get $k_outer)))

  (global $abort_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $abort_apply)
      (ref.null $Captures)))

  (func $abort (@pub) (@impl "std/effects.fnk:abort") (result (ref any))
    (global.get $abort_closure))


  ;; -- with_invoke ----------------------------------------------------
  ;;
  ;; Compiler-emitted target of `with H: B` lowering. NOT a user-import
  ;; -- called directly via Sym::WithInvoke as a 4-param fn:
  ;;
  ;;   with_invoke(ctx, handler, body_fn, cont)
  ;;
  ;; Invokes the handler with [k_outer, wrapped_body]. The handler is
  ;; a one-user-arg fn `fn body: ...` -- it receives wrapped_body as
  ;; `body` and k_outer as its own return cont. When the handler
  ;; returns, it goes straight to k_outer (no pop_cont indirection --
  ;; the frame lifetime is tied to body invocation, not handler
  ;; invocation).
  ;;
  ;; `wrapped_body` is a closure capturing body_fn. When the handler
  ;; calls `body args`, the wrapper:
  ;;   1. pushes a frame holding the body-call cont
  ;;   2. builds pop_cont (pops frame, then tail-calls the body-call
  ;;      cont with the body's return value)
  ;;   3. invokes body_fn with [pop_cont, ...user_args]
  ;;
  ;; Three exit paths:
  ;;   (a) Handler returns without ever calling body: no frame pushed,
  ;;       handler's return value flows direct to k_outer.
  ;;   (b) Handler calls body, body returns normally: body returns to
  ;;       pop_cont -> pop frame, tail-call body-call cont with value
  ;;       -> handler resumes at `body 0` expression with that value.
  ;;   (c) Handler calls body, body aborts: abort pops top frame and
  ;;       tail-calls the stored body-call cont with the abort value
  ;;       -> handler resumes at `body 0` expression with the value.
  ;;
  ;; In (b) and (c), the handler is *always* re-entered at the same
  ;; place with a value. It can't distinguish normal return from
  ;; abort -- that's by design (no substrate-level tag). What the
  ;; handler does next (cleanup, propagate, transform) is its
  ;; choice. Cleanup naturally runs after the `body 0` call site,
  ;; uniformly across both paths.

  (elem declare func $_pop_cont_fn)

  (func $_pop_cont_fn (type $Fn3)
    (param $caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $captures (ref $Captures))
    (local $body_call_cont (ref any))

    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $body_call_cont
      (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 0))))

    (drop (call $frame_pop))
    (return_call $apply_3
      (local.get $args)
      (local.get $ctx)
      (local.get $body_call_cont)))

  (func $make_pop_cont (param $cont (ref any)) (result (ref $Closure))
    (struct.new $Closure
      (ref.func $_pop_cont_fn)
      (array.new_fixed $Captures 1 (local.get $cont))))

  ;; wrapped_body: closure capturing body_fn. Pushes frame on entry,
  ;; redirects body's return through pop_cont so the frame pops on
  ;; normal completion. Abort also pops + jumps to the same cont.
  (elem declare func $_wrapped_body_fn)

  (func $_wrapped_body_fn (type $Fn3)
    (param $caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $captures (ref $Captures))
    (local $body_fn (ref any))
    (local $body_call_cont (ref any))
    (local $pop_cont (ref $Closure))
    (local $body_args (ref any))

    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $body_fn
      (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 0))))

    ;; args = [body_call_cont, ...user_args]
    (local.set $body_call_cont
      (ref.as_non_null (call $args_head (local.get $args))))

    ;; Push frame holding the body-call cont -- this is where abort
    ;; jumps to. The handler will resume at its `body 0` site with
    ;; the abort value.
    (call $frame_push (local.get $body_call_cont))

    ;; Build pop_cont so body's normal return also pops the frame.
    (local.set $pop_cont (call $make_pop_cont (local.get $body_call_cont)))

    ;; Replace head of args with pop_cont; body sees [pop_cont, ...user_args].
    (local.set $body_args
      (call $args_prepend (local.get $pop_cont)
        (ref.cast (ref any) (call $args_tail (local.get $args)))))

    (return_call $apply_3
      (local.get $body_args)
      (local.get $ctx)
      (local.get $body_fn)))

  (func $make_wrapped_body (param $body_fn (ref any)) (result (ref $Closure))
    (struct.new $Closure
      (ref.func $_wrapped_body_fn)
      (array.new_fixed $Captures 1 (local.get $body_fn))))

  (func $with_invoke (@pub) (@impl "std/effects.fnk:with_invoke")
    (param $ctx (ref null any))
    (param $handler (ref null any))
    (param $body_fn (ref null any))
    (param $cont (ref null any))

    (local $k_outer (ref any))
    (local $wrapped_body (ref $Closure))
    (local $h_args (ref any))

    (local.set $k_outer (ref.as_non_null (local.get $cont)))
    (local.set $wrapped_body
      (call $make_wrapped_body
        (ref.as_non_null (local.get $body_fn))))

    ;; handler args: [k_outer, wrapped_body]
    ;; Handler returns straight to k_outer when done.
    (local.set $h_args
      (call $args_prepend (local.get $k_outer)
        (call $args_prepend (local.get $wrapped_body) (call $args_empty))))

    (return_call $apply_3
      (local.get $h_args)
      (local.get $ctx)
      (local.get $handler)))
)
