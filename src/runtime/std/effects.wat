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


  ;; -- substrate-internal ctx and handler-frame chain ----------------
  ;;
  ;; ctx is the value threaded forward through every CPS call. The
  ;; substrate wraps it as a $Ctx struct with two slots:
  ;;   $user        -- the value fink code sees via get_ctx / set_ctx.
  ;;   $frame_chain -- the substrate-only handler-frame chain. yield2
  ;;                   and abort read the head frame from here; their
  ;;                   k_outer takes them to the enclosing `with`
  ;;                   handler. wrapped_body pushes a frame on entry;
  ;;                   pop_cont pops on natural body return.
  ;;
  ;; Each $Frame holds the body-call-cont (where yield2 / abort /
  ;; natural-return land in the handler).
  ;;
  ;; All non-substrate code treats ctx opaquely as `(ref null any)`.
  ;; The substrate is the only place that pattern-matches on $Ctx.

  (type $Frame (struct
    (field $k_outer (ref any))
    (field $parent  (ref null $Frame))))

  ;; Tagged-op substrate (new): each frame is keyed by an op id and
  ;; carries the with-block's body fn + exit cont. See "-- tagged-op
  ;; substrate ----" further down.
  (type $OpFrame (struct
    (field $op_id        i32)
    (field $handler      (ref any))
    (field $body_fn      (ref any))
    (field $k_block_exit (ref any))
    (field $parent       (ref null $OpFrame))))

  ;; Per-handler-invocation slot: the verbs an active handler can use.
  ;; Set on the ctx that flows into the handler call; null elsewhere.
  (type $OpInvocation (struct
    (field $resume       (ref any))   ;; closure: re-enter suspension
    (field $block_rerun  (ref any))   ;; closure: re-enter body from top
    (field $block_return (ref any)))) ;; closure: skip to k_block_exit

  (type $Ctx (struct
    (field $user           (ref null any))
    (field $frame_chain    (ref null $Frame))      ;; OLD substrate
    (field $op_frame_chain (ref null $OpFrame))    ;; NEW substrate
    (field $current_op     (ref null $OpInvocation)))) ;; NEW: only set during handler call

  ;; If ctx is a $Ctx, return its user payload. Otherwise return ctx
  ;; itself -- back-compat for code paths that still pass bare-value
  ;; ctx during the migration.
  (func $ctx_user (param $ctx (ref null any)) (result (ref null any))
    (local $as_ctx (ref null $Ctx))
    (block $not_ctx
      (block $is (result (ref $Ctx))
        (br $not_ctx
          (br_on_cast $is (ref null any) (ref $Ctx) (local.get $ctx))))
      (local.set $as_ctx)
      (return (struct.get $Ctx $user (local.get $as_ctx))))
    (local.get $ctx))

  ;; Return a fresh $Ctx with the given user payload and the other
  ;; slots inherited from the input ctx (null if input is not a $Ctx).
  (func $ctx_with_user
      (param $ctx (ref null any))
      (param $new_user (ref null any))
      (result (ref $Ctx))
    (struct.new $Ctx
      (local.get $new_user)
      (call $ctx_frame_chain    (local.get $ctx))
      (call $ctx_op_frame_chain (local.get $ctx))
      (call $ctx_current_op     (local.get $ctx))))

  ;; Return ctx.frame_chain, or null if ctx is a bare value (no chain).
  (func $ctx_frame_chain
      (param $ctx (ref null any))
      (result (ref null $Frame))
    (local $as_ctx (ref null $Ctx))
    (block $not_ctx
      (block $is (result (ref $Ctx))
        (br $not_ctx
          (br_on_cast $is (ref null any) (ref $Ctx) (local.get $ctx))))
      (local.set $as_ctx)
      (return (struct.get $Ctx $frame_chain (local.get $as_ctx))))
    (ref.null $Frame))

  ;; Return ctx.op_frame_chain, or null if ctx is a bare value.
  (func $ctx_op_frame_chain
      (param $ctx (ref null any))
      (result (ref null $OpFrame))
    (local $as_ctx (ref null $Ctx))
    (block $not_ctx
      (block $is (result (ref $Ctx))
        (br $not_ctx
          (br_on_cast $is (ref null any) (ref $Ctx) (local.get $ctx))))
      (local.set $as_ctx)
      (return (struct.get $Ctx $op_frame_chain (local.get $as_ctx))))
    (ref.null $OpFrame))

  ;; Return ctx.current_op, or null if not in a handler invocation
  ;; (or ctx is a bare value).
  (func $ctx_current_op
      (param $ctx (ref null any))
      (result (ref null $OpInvocation))
    (local $as_ctx (ref null $Ctx))
    (block $not_ctx
      (block $is (result (ref $Ctx))
        (br $not_ctx
          (br_on_cast $is (ref null any) (ref $Ctx) (local.get $ctx))))
      (local.set $as_ctx)
      (return (struct.get $Ctx $current_op (local.get $as_ctx))))
    (ref.null $OpInvocation))

  ;; Build a fresh $Ctx from an input ctx with selected slots
  ;; replaced. For each replacement arg, pass the new value directly;
  ;; for "inherit", use the accessor on $ctx.
  (func $ctx_make
      (param $new_user (ref null any))
      (param $new_frame_chain (ref null $Frame))
      (param $new_op_chain (ref null $OpFrame))
      (param $new_current_op (ref null $OpInvocation))
      (result (ref $Ctx))
    (struct.new $Ctx
      (local.get $new_user)
      (local.get $new_frame_chain)
      (local.get $new_op_chain)
      (local.get $new_current_op)))

  ;; Return a fresh ctx with frame_chain replaced; other slots
  ;; inherited from input.
  (func $ctx_with_frame_chain
      (param $ctx (ref null any))
      (param $new_chain (ref null $Frame))
      (result (ref $Ctx))
    (call $ctx_make
      (call $ctx_user           (local.get $ctx))
      (local.get $new_chain)
      (call $ctx_op_frame_chain (local.get $ctx))
      (call $ctx_current_op     (local.get $ctx))))


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
    (local $new_user (ref null any))
    (local $result_args (ref any))

    (local.set $cont (call $args_head (local.get $args)))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $new_user (call $args_head (local.get $args)))

    ;; Return the user-facing OLD ctx to cont; thread a fresh $Ctx
    ;; (new user payload, preserved frame chain) as the new ctx.
    (local.set $result_args
      (call $args_prepend
        (call $ctx_user (local.get $ctx))
        (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (call $ctx_with_user (local.get $ctx) (local.get $new_user))
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

    ;; Return the user-facing ctx; thread the full $Ctx (still
    ;; carrying the frame chain) unchanged.
    (local.set $result_args
      (call $args_prepend
        (call $ctx_user (local.get $ctx))
        (call $args_empty)))

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
  ;; Fink-level signature: `abort v`. Reads the head frame from
  ;; ctx.frame_chain and tail-calls its k_outer with v under the
  ;; parent ctx (chain head popped for the handler's view) --
  ;; non-local return to the nearest enclosing `with` block.
  ;;
  ;; Traps with "no handler frame" (ref.as_non_null on null
  ;; frame_chain) if abort is called with no enclosing `with`.
  ;;
  ;; Importable: `{abort} = import 'std/effects.fnk'`. Fn3-shaped
  ;; closure. The cont arg the compiler passes is the caller's
  ;; continuation -- abort discards it.

  (elem declare func $abort_apply)

  (func $abort_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $value (ref null any))
    (local $frame (ref $Frame))
    (local $k_outer (ref any))
    (local $parent_ctx (ref $Ctx))
    (local $result_args (ref any))

    ;; args = [cont, value]; we only need value (cont is discarded).
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $value (call $args_head (local.get $args)))

    ;; Read chain head from ctx.
    (local.set $frame
      (ref.as_non_null (call $ctx_frame_chain (local.get $ctx))))
    (local.set $k_outer (struct.get $Frame $k_outer (local.get $frame)))

    ;; Build parent ctx: same user payload, chain with head popped.
    (local.set $parent_ctx
      (call $ctx_with_frame_chain
        (local.get $ctx)
        (struct.get $Frame $parent (local.get $frame))))

    (local.set $result_args
      (call $args_prepend (local.get $value) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $parent_ctx)
      (local.get $k_outer)))

  (global $abort_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $abort_apply)
      (ref.null $Captures)))

  (func $abort (@pub) (@impl "std/effects.fnk:abort") (result (ref any))
    (global.get $abort_closure))


  ;; -- yield2 ---------------------------------------------------------
  ;;
  ;; Resumable-yield substrate primitive.
  ;;
  ;; Fink-level signature: `yield2 v -> Yield{value, resume}`. Captures
  ;; (k_body_rest, ctx) and packages them as a `resume` closure inside a
  ;; `$Yield` struct. The handler reads the struct via `get_yield_value`
  ;; / `get_yield_resume` and decides whether to call `resume r`
  ;; (re-enter body at the yield2 site with r) or discard it.
  ;;
  ;; Routing: reads ctx.frame_chain head, tail-calls its k_outer with
  ;; the Yield struct under the parent ctx (chain head popped).
  ;;
  ;; The captured ctx in `resume` retains the un-popped chain -- so
  ;; firing the resume later (even outside the spawning with-block)
  ;; re-enters body under the original chain. Subsequent yields from
  ;; the resumed body find the original frame.
  ;;
  ;; Traps if there's no enclosing `with` (null chain).

  (type $Yield (struct
    (field $value  (ref any))
    (field $resume (ref any))))

  ;; resume closure: captures (k_body_rest, captured_chain). When
  ;; fired via `resume r`:
  ;;   - frame_chain is restored to captured_chain (with a new frame
  ;;     for the resume call pushed on top). This is the substrate-
  ;;     internal world: yield2 / abort from the resumed body land
  ;;     at the captured handler's body-call site (extended by the
  ;;     resume call's own cont so body's next observable lands at
  ;;     the resume caller).
  ;;   - user payload comes from the FIRER (caller of resume). This
  ;;     matches ctx's monotonic-forward semantics for user state:
  ;;     a stored resume sees set_ctx mutations that happened after
  ;;     it was captured.
  (elem declare func $_resume_fn)

  (func $_resume_fn (type $Fn3)
    (param $caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $captures (ref $Captures))
    (local $k_body_rest (ref any))
    (local $captured_chain (ref null $Frame))
    (local $k_handler_after_resume (ref any))
    (local $r (ref null any))
    (local $new_frame (ref $Frame))
    (local $new_ctx (ref $Ctx))
    (local $result_args (ref any))

    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $k_body_rest
      (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 0))))
    (local.set $captured_chain
      (ref.cast (ref null $Frame)
        (array.get $Captures (local.get $captures) (i32.const 1))))

    ;; args = [k_handler_after_resume, r].
    (local.set $k_handler_after_resume
      (ref.as_non_null (call $args_head (local.get $args))))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $r (call $args_head (local.get $args)))

    ;; Push a new frame onto the captured chain for this resume call.
    (local.set $new_frame
      (struct.new $Frame
        (local.get $k_handler_after_resume)
        (local.get $captured_chain)))
    ;; Combine firer's user payload with restored-and-extended chain.
    (local.set $new_ctx
      (call $ctx_with_frame_chain
        (local.get $ctx)
        (local.get $new_frame)))

    (local.set $result_args
      (call $args_prepend (local.get $r) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $new_ctx)
      (local.get $k_body_rest)))

  (func $make_resume
      (param $k_body_rest (ref any))
      (param $captured_chain (ref null $Frame))
      (result (ref $Closure))
    (struct.new $Closure
      (ref.func $_resume_fn)
      (array.new_fixed $Captures 2
        (local.get $k_body_rest)
        (local.get $captured_chain))))

  (elem declare func $yield2_apply)

  (func $yield2_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref any))
    (local $value (ref any))
    (local $frame (ref $Frame))
    (local $k_outer (ref any))
    (local $parent_ctx (ref $Ctx))
    (local $resume (ref $Closure))
    (local $yielded (ref $Yield))
    (local $result_args (ref any))

    ;; args = [cont, value]. cont is k_body_rest -- the continuation of
    ;; the yield2 call site in body.
    (local.set $cont (ref.as_non_null (call $args_head (local.get $args))))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $value (ref.as_non_null (call $args_head (local.get $args))))

    ;; Read chain head from ctx.
    (local.set $frame
      (ref.as_non_null (call $ctx_frame_chain (local.get $ctx))))
    (local.set $k_outer (struct.get $Frame $k_outer (local.get $frame)))

    ;; Build parent ctx for the trip up to k_outer (chain head popped
    ;; for the handler's view).
    (local.set $parent_ctx
      (call $ctx_with_frame_chain
        (local.get $ctx)
        (struct.get $Frame $parent (local.get $frame))))

    ;; Capture the body's continuation AND the un-popped chain in
    ;; resume. Firing this later re-enters body with the original
    ;; chain intact (under the firer's user payload).
    (local.set $resume
      (call $make_resume
        (local.get $cont)
        (struct.get $Frame $parent (local.get $frame))))

    (local.set $yielded
      (struct.new $Yield (local.get $value) (local.get $resume)))

    (local.set $result_args
      (call $args_prepend (local.get $yielded) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $parent_ctx)
      (local.get $k_outer)))

  (global $yield2_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $yield2_apply)
      (ref.null $Captures)))

  (func $yield2 (@pub) (@impl "std/effects.fnk:yield2") (result (ref any))
    (global.get $yield2_closure))


  ;; -- get_yield_value / get_yield_resume -----------------------------
  ;;
  ;; Accessors for the opaque $Yield struct. Fink-level signatures:
  ;;   get_yield_value  y -> v   (the value passed to yield2)
  ;;   get_yield_resume y -> k   (the captured cont; call as `k r` to
  ;;                              re-enter body at the yield2 site with r)

  (elem declare func $get_yield_value_apply)

  ;; Pass-through projection: if arg is a $Yield, return its value
  ;; field; otherwise return the arg as-is. Lets handlers treat body's
  ;; natural-return value and a yielded value uniformly.
  (func $get_yield_value_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref null any))
    (local $arg (ref any))
    (local $value (ref any))
    (local $result_args (ref any))

    ;; args = [cont, value-or-yield]
    (local.set $cont (call $args_head (local.get $args)))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $arg (ref.as_non_null (call $args_head (local.get $args))))

    ;; If $arg is a $Yield, set $value to its .value field; else
    ;; pass-through $arg unchanged.
    (block $not_yield
      (block $is_yield (result (ref $Yield))
        (br $not_yield
          (br_on_cast $is_yield (ref any) (ref $Yield) (local.get $arg))))
      ;; is_yield path: $Yield on stack as block result
      (local.set $value (struct.get $Yield $value))
      (local.set $result_args
        (call $args_prepend (local.get $value) (call $args_empty)))
      (return_call $apply_3
        (local.get $result_args)
        (local.get $ctx)
        (local.get $cont)))
    ;; not_yield path: pass $arg through
    (local.set $result_args
      (call $args_prepend (local.get $arg) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $ctx)
      (local.get $cont)))

  (global $get_yield_value_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $get_yield_value_apply)
      (ref.null $Captures)))

  (func $get_yield_value (@pub) (@impl "std/effects.fnk:get_yield_value") (result (ref any))
    (global.get $get_yield_value_closure))


  (elem declare func $get_yield_resume_apply)

  ;; Pass-through projection: if arg is a $Yield, return its resume
  ;; field; otherwise return the arg as-is. Symmetric with
  ;; $get_yield_value -- handlers can call both uniformly without
  ;; first checking whether body yielded.
  (func $get_yield_resume_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref null any))
    (local $arg (ref any))
    (local $resume (ref any))
    (local $result_args (ref any))

    (local.set $cont (call $args_head (local.get $args)))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $arg (ref.as_non_null (call $args_head (local.get $args))))

    (block $not_yield
      (block $is_yield (result (ref $Yield))
        (br $not_yield
          (br_on_cast $is_yield (ref any) (ref $Yield) (local.get $arg))))
      (local.set $resume (struct.get $Yield $resume))
      (local.set $result_args
        (call $args_prepend (local.get $resume) (call $args_empty)))
      (return_call $apply_3
        (local.get $result_args)
        (local.get $ctx)
        (local.get $cont)))
    (local.set $result_args
      (call $args_prepend (local.get $arg) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $ctx)
      (local.get $cont)))

  (global $get_yield_resume_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $get_yield_resume_apply)
      (ref.null $Captures)))

  (func $get_yield_resume (@pub) (@impl "std/effects.fnk:get_yield_resume") (result (ref any))
    (global.get $get_yield_resume_closure))


  ;; -- is_yield -------------------------------------------------------
  ;;
  ;; Fink-level: `is_yield val -> bool`. Returns i31 1 if val is a
  ;; $Yield struct, i31 0 otherwise. Booleans flow as i31ref (matches
  ;; the convention in rt/protocols.wat:10).

  (elem declare func $is_yield_apply)

  (func $is_yield_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref null any))
    (local $val (ref any))
    (local $result_args (ref any))

    ;; args = [cont, val].
    (local.set $cont (call $args_head (local.get $args)))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $val (ref.as_non_null (call $args_head (local.get $args))))

    (local.set $result_args
      (call $args_prepend
        (block $done (result (ref any))
          (block $not_yield
            (block $is (result (ref $Yield))
              (br $not_yield
                (br_on_cast $is (ref any) (ref $Yield) (local.get $val))))
            (drop)
            (br $done (ref.i31 (i32.const 1))))
          (ref.i31 (i32.const 0)))
        (call $args_empty)))
    (return_call $apply_3
      (local.get $result_args)
      (local.get $ctx)
      (local.get $cont)))

  (global $is_yield_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $is_yield_apply)
      (ref.null $Captures)))

  (func $is_yield (@pub) (@impl "std/effects.fnk:is_yield") (result (ref any))
    (global.get $is_yield_closure))


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

  ;; pop_cont: body's natural-return cont. Reads the body-call-cont
  ;; (and the parent ctx) from the top frame of ctx.frame_chain.
  ;; Pops the chain head and tail-calls the body-call-cont with body's
  ;; value under the parent ctx -- so the handler sees the same chain
  ;; depth it had when it called body.
  (elem declare func $_pop_cont_fn)

  (func $_pop_cont_fn (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $frame (ref $Frame))
    (local $body_call_cont (ref any))
    (local $parent_ctx (ref $Ctx))

    (local.set $frame
      (ref.as_non_null (call $ctx_frame_chain (local.get $ctx))))
    (local.set $body_call_cont (struct.get $Frame $k_outer (local.get $frame)))
    (local.set $parent_ctx
      (call $ctx_with_frame_chain
        (local.get $ctx)
        (struct.get $Frame $parent (local.get $frame))))
    (return_call $apply_3
      (local.get $args)
      (local.get $parent_ctx)
      (local.get $body_call_cont)))

  (global $_pop_cont (ref $Closure)
    (struct.new $Closure
      (ref.func $_pop_cont_fn)
      (ref.null $Captures)))

  ;; wrapped_body: closure capturing body_fn. Pushes a frame onto
  ;; ctx.frame_chain holding the body-call-cont, then runs body
  ;; under the extended ctx. Body's yield2 / abort find the frame at
  ;; the chain head. Natural return flows through pop_cont which
  ;; pops the chain head and restores the parent.
  (elem declare func $_wrapped_body_fn)

  (func $_wrapped_body_fn (type $Fn3)
    (param $caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $captures (ref $Captures))
    (local $body_fn (ref any))
    (local $body_call_cont (ref any))
    (local $body_args (ref any))
    (local $new_frame (ref $Frame))
    (local $new_ctx (ref $Ctx))

    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $body_fn
      (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 0))))

    ;; args = [body_call_cont, ...user_args]
    (local.set $body_call_cont
      (ref.as_non_null (call $args_head (local.get $args))))

    ;; Push frame holding the body-call cont onto the chain.
    (local.set $new_frame
      (struct.new $Frame
        (local.get $body_call_cont)
        (call $ctx_frame_chain (local.get $ctx))))
    (local.set $new_ctx
      (call $ctx_with_frame_chain
        (local.get $ctx)
        (local.get $new_frame)))

    ;; Replace head of args with the shared _pop_cont closure; body sees
    ;; [pop_cont, ...user_args]. Natural return -> pop_cont -> pops the
    ;; chain head, tail-calls the frame's k_outer with body's value
    ;; under the parent ctx.
    (local.set $body_args
      (call $args_prepend (global.get $_pop_cont)
        (ref.cast (ref any) (call $args_tail (local.get $args)))))

    (return_call $apply_3
      (local.get $body_args)
      (local.get $new_ctx)
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
