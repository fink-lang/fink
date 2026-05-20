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

  ;; Tagged-op substrate uses fink int literals (lowered to $I64) as
  ;; op ids. Importing the type lets us cast the user-supplied value
  ;; and read the i64 payload directly.
  (import "std/int.wat" "I64" (type $I64 (sub final (struct (field $ival i64)))))

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
  ;; Carries the matched OpFrame so `rethrow op v` can walk past it
  ;; to reach an outer same-id handler.
  (type $OpInvocation (struct
    (field $resume        (ref any))   ;; closure: re-enter suspension
    (field $block_rerun   (ref any))   ;; closure: re-enter body from top
    (field $block_return  (ref any))   ;; closure: skip to k_block_exit
    (field $matched_frame (ref $OpFrame)))) ;; the frame this handler is handling

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


  ;; ==================================================================
  ;; -- tagged-op substrate (new) -------------------------------------
  ;; ==================================================================
  ;;
  ;; Five primitives:
  ;;
  ;;   with_invoke2 op_id, handler, body_fn, cont
  ;;     Pushes an OpFrame onto ctx.op_frame_chain, then runs body_fn.
  ;;     The frame is keyed by op_id; the handler is invoked when an
  ;;     op with that id is performed inside body_fn's dynamic extent.
  ;;
  ;;   perform_op op_id, value
  ;;     Walks op_frame_chain for the matching id, builds the three
  ;;     control-flow closures, and tail-calls the handler with the
  ;;     value plus a fresh ctx whose current_op slot holds those
  ;;     closures and whose op_frame_chain has the matched frame
  ;;     popped (so handler's own re-performs of the same op walk
  ;;     past to an outer same-id handler).
  ;;
  ;;   get_resume _        -- closure that re-enters suspension cont
  ;;   get_block_rerun _   -- closure that re-runs body_fn from start
  ;;   get_block_return _  -- closure that jumps to k_block_exit
  ;;
  ;; The captured chain in resume retains the matched frame, so when
  ;; the resumed body re-performs the op, it routes back to this
  ;; same handler again (deep-handler semantics).


  ;; ---- $Resume closure ---------------------------------------------
  ;;
  ;; Captures: [k_suspension (ref any), captured_op_chain (ref null any)]
  ;; When called with [_k_caller, v]: tail-calls k_suspension with v
  ;; under ctx { user: firer-user, op_chain: captured_op_chain,
  ;; current_op: null, frame_chain: firer's (untouched) }.

  (elem declare func $_resume_op_fn)

  (func $_resume_op_fn (type $Fn3)
    (param $caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $captures (ref $Captures))
    (local $k_suspension (ref any))
    (local $captured_op_chain (ref null $OpFrame))
    (local $v (ref null any))
    (local $new_ctx (ref $Ctx))
    (local $result_args (ref any))

    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $k_suspension
      (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 0))))
    (local.set $captured_op_chain
      (ref.cast (ref null $OpFrame)
        (array.get $Captures (local.get $captures) (i32.const 1))))

    ;; args = [k_caller_after_resume, v]. Discard k_caller -- effects
    ;; from the resumed body route via the captured chain back to
    ;; the original handler.
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $v (call $args_head (local.get $args)))

    (local.set $new_ctx
      (call $ctx_make
        (call $ctx_user (local.get $ctx))
        (call $ctx_frame_chain (local.get $ctx))
        (local.get $captured_op_chain)
        (ref.null $OpInvocation)))

    (local.set $result_args
      (call $args_prepend (local.get $v) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $new_ctx)
      (local.get $k_suspension)))

  (func $make_resume_op
      (param $k_suspension (ref any))
      (param $captured_op_chain (ref null $OpFrame))
      (result (ref $Closure))
    (struct.new $Closure
      (ref.func $_resume_op_fn)
      (array.new_fixed $Captures 2
        (local.get $k_suspension)
        (local.get $captured_op_chain))))


  ;; ---- $BlockReturn closure ----------------------------------------
  ;;
  ;; Captures: [k_block_exit (ref any), parent_op_chain (ref null any)]
  ;; When called with [_k_caller, v]: tail-call k_block_exit with v
  ;; under ctx with op_chain restored to parent (frame popped).

  (elem declare func $_block_return_fn)

  (func $_block_return_fn (type $Fn3)
    (param $caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $captures (ref $Captures))
    (local $k_block_exit (ref any))
    (local $parent_op_chain (ref null $OpFrame))
    (local $v (ref null any))
    (local $new_ctx (ref $Ctx))
    (local $result_args (ref any))

    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $k_block_exit
      (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 0))))
    (local.set $parent_op_chain
      (ref.cast (ref null $OpFrame)
        (array.get $Captures (local.get $captures) (i32.const 1))))

    (local.set $args (call $args_tail (local.get $args)))
    (local.set $v (call $args_head (local.get $args)))

    (local.set $new_ctx
      (call $ctx_make
        (call $ctx_user (local.get $ctx))
        (call $ctx_frame_chain (local.get $ctx))
        (local.get $parent_op_chain)
        (ref.null $OpInvocation)))

    (local.set $result_args
      (call $args_prepend (local.get $v) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $new_ctx)
      (local.get $k_block_exit)))

  (func $make_block_return
      (param $k_block_exit (ref any))
      (param $parent_op_chain (ref null $OpFrame))
      (result (ref $Closure))
    (struct.new $Closure
      (ref.func $_block_return_fn)
      (array.new_fixed $Captures 2
        (local.get $k_block_exit)
        (local.get $parent_op_chain))))


  ;; ---- $BlockRerun closure -----------------------------------------
  ;;
  ;; Captures: [body_fn (ref any), k_block_exit (ref any),
  ;;            captured_op_chain (ref null any)]
  ;; When called with [_k_caller, _]: tail-call body_fn with [k_block_exit]
  ;; under ctx with the captured op_chain (the matched frame still in!)
  ;; so the with-block restarts from the top, hitting the same handler.

  (elem declare func $_block_rerun_fn)

  (func $_block_rerun_fn (type $Fn3)
    (param $caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $captures (ref $Captures))
    (local $body_fn (ref any))
    (local $k_block_exit (ref any))
    (local $captured_op_chain (ref null $OpFrame))
    (local $new_ctx (ref $Ctx))
    (local $body_args (ref any))

    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $body_fn
      (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 0))))
    (local.set $k_block_exit
      (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 1))))
    (local.set $captured_op_chain
      (ref.cast (ref null $OpFrame)
        (array.get $Captures (local.get $captures) (i32.const 2))))

    (local.set $new_ctx
      (call $ctx_make
        (call $ctx_user (local.get $ctx))
        (call $ctx_frame_chain (local.get $ctx))
        (local.get $captured_op_chain)
        (ref.null $OpInvocation)))

    (local.set $body_args
      (call $args_prepend (local.get $k_block_exit) (call $args_empty)))

    (return_call $apply_3
      (local.get $body_args)
      (local.get $new_ctx)
      (local.get $body_fn)))

  (func $make_block_rerun
      (param $body_fn (ref any))
      (param $k_block_exit (ref any))
      (param $captured_op_chain (ref null $OpFrame))
      (result (ref $Closure))
    (struct.new $Closure
      (ref.func $_block_rerun_fn)
      (array.new_fixed $Captures 3
        (local.get $body_fn)
        (local.get $k_block_exit)
        (local.get $captured_op_chain))))


  ;; ---- $KBlockExit closure -----------------------------------------
  ;;
  ;; Captures: [outer_cont (ref any), entry_op_chain (ref null any)]
  ;; When called with [_k_caller, v]: tail-call outer_cont with v
  ;; under ctx with op_chain restored to entry_op_chain (the chain
  ;; that was active when the with-block started). The frame pushed
  ;; by with_invoke2 gets exactly one pop here -- unless the handler
  ;; never ran (handler returned natural value via block_return or
  ;; the body completed and reached us directly), in which case
  ;; restoring entry_op_chain is still correct.

  (elem declare func $_k_block_exit2_fn)

  (func $_k_block_exit2_fn (type $Fn3)
    (param $caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $captures (ref $Captures))
    (local $outer_cont (ref any))
    (local $entry_op_chain (ref null $OpFrame))
    (local $exit_ctx (ref $Ctx))

    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $outer_cont
      (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 0))))
    (local.set $entry_op_chain
      (ref.cast (ref null $OpFrame)
        (array.get $Captures (local.get $captures) (i32.const 1))))

    (local.set $exit_ctx
      (call $ctx_make
        (call $ctx_user (local.get $ctx))
        (call $ctx_frame_chain (local.get $ctx))
        (local.get $entry_op_chain)
        (ref.null $OpInvocation)))

    (return_call $apply_3
      (local.get $args)
      (local.get $exit_ctx)
      (local.get $outer_cont)))

  (func $make_k_block_exit2
      (param $outer_cont (ref any))
      (param $entry_op_chain (ref null $OpFrame))
      (result (ref $Closure))
    (struct.new $Closure
      (ref.func $_k_block_exit2_fn)
      (array.new_fixed $Captures 2
        (local.get $outer_cont)
        (local.get $entry_op_chain))))


  ;; ---- with_invoke2 ------------------------------------------------
  ;;
  ;; Fink-level (Fn3): `with_invoke2 op_id handler body_fn`.
  ;; args = [cont, op_id, handler, body_fn]. cont is the with-block's
  ;; exit cont; op_id is a fink int (boxed $I64); handler is `fn v:
  ;; ...`; body_fn is `fn: BODY` (a zero-arg thunk; its natural-return
  ;; value becomes the with-block's result, unless an op-handler
  ;; uses block_return / block_rerun to redirect control flow).
  ;;
  ;; Pushes an OpFrame keyed by op_id, runs body_fn under it. The
  ;; frame stays live for the with-block's dynamic extent; perform_op
  ;; reads it; k_block_exit pops it.

  (elem declare func $with_invoke2_apply)

  (func $with_invoke2_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref any))
    (local $op_id_val (ref any))
    (local $handler (ref any))
    (local $body_fn (ref any))

    (local $op_id_i i32)
    (local $entry_op_chain (ref null $OpFrame))
    (local $k_block_exit (ref $Closure))
    (local $new_frame (ref $OpFrame))
    (local $new_ctx (ref $Ctx))
    (local $body_args (ref any))

    ;; args = [cont, op_id, handler, body_fn]
    (local.set $cont
      (ref.as_non_null (call $args_head (local.get $args))))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $op_id_val
      (ref.as_non_null (call $args_head (local.get $args))))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $handler
      (ref.as_non_null (call $args_head (local.get $args))))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $body_fn
      (ref.as_non_null (call $args_head (local.get $args))))

    ;; op_id is a fink int literal (boxed as $I64). Cast and read i64,
    ;; truncate to i32 since op_id_i is just an opaque tag.
    (local.set $op_id_i
      (i32.wrap_i64
        (struct.get $I64 $ival
          (ref.cast (ref $I64) (local.get $op_id_val)))))
    (local.set $entry_op_chain (call $ctx_op_frame_chain (local.get $ctx)))

    (local.set $k_block_exit
      (call $make_k_block_exit2
        (local.get $cont)
        (local.get $entry_op_chain)))

    (local.set $new_frame
      (struct.new $OpFrame
        (local.get $op_id_i)
        (local.get $handler)
        (local.get $body_fn)
        (local.get $k_block_exit)
        (local.get $entry_op_chain)))

    (local.set $new_ctx
      (call $ctx_make
        (call $ctx_user (local.get $ctx))
        (call $ctx_frame_chain (local.get $ctx))
        (local.get $new_frame)
        (call $ctx_current_op (local.get $ctx))))

    (local.set $body_args
      (call $args_prepend (local.get $k_block_exit) (call $args_empty)))

    (return_call $apply_3
      (local.get $body_args)
      (local.get $new_ctx)
      (local.get $body_fn)))

  (global $with_invoke2_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $with_invoke2_apply)
      (ref.null $Captures)))

  (func $with_invoke2 (@pub) (@impl "std/effects.fnk:with_invoke2") (result (ref any))
    (global.get $with_invoke2_closure))


  ;; ---- perform_op --------------------------------------------------
  ;;
  ;; Fink-level: `perform_op op_id value`. Walks op_frame_chain for a
  ;; frame whose op_id matches; invokes its handler with the value.
  ;; Traps if no frame matches.
  ;;
  ;; The handler invocation runs under a fresh ctx with:
  ;;   - op_frame_chain = matched_frame.parent (the handler doesn't
  ;;     see its own frame, so its own perform of the same op walks
  ;;     past to an outer same-id handler).
  ;;   - current_op = OpInvocation { resume, block_rerun, block_return }
  ;;
  ;; resume's captured chain includes the matched frame (so the
  ;; resumed body's re-performs of the same op still route here).
  ;;
  ;; Handlers transfer control via:
  ;;   (get_resume _) v        -- re-enter the op suspension with v
  ;;   (get_block_return _) v  -- exit the with-block with v
  ;;   (get_block_rerun _) _   -- restart the with-block body
  ;;   rethrow v               -- forward to outer same-id handler
  ;;
  ;; Each is tail-call-only and never returns to the handler. A
  ;; well-written handler ends in such a call. Falling off the end
  ;; without transferring control is an error -- the substrate
  ;; passes a trapping cont as the handler's return cont.

  ;; Noop cont for the handler's return slot. A correctly written
  ;; handler ends with an explicit terminator (resume / block_return
  ;; / block_rerun / rethrow) and never returns to this. If the
  ;; handler "falls off" -- e.g. it stored its resume somewhere for
  ;; later firing and finished doing other work -- control simply
  ;; stops here. Any remaining live continuations (stored resumes,
  ;; pending tasks) carry the program forward on their own.

  (elem declare func $_handler_noop_cont_fn)

  (func $_handler_noop_cont_fn (type $Fn3)
    (param $_caps (ref null any))
    (param $_ctx (ref null any))
    (param $_args (ref null any))
    (return))

  (global $_handler_noop_cont (ref $Closure)
    (struct.new $Closure
      (ref.func $_handler_noop_cont_fn)
      (ref.null $Captures)))


  (elem declare func $perform_op_apply)

  (func $perform_op_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $k_suspension (ref any))
    (local $op_id_val (ref any))
    (local $op_id_i i32)
    (local $value (ref null any))

    (local $cur (ref null $OpFrame))
    (local $matched (ref null $OpFrame))

    (local $resume (ref $Closure))
    (local $block_rerun (ref $Closure))
    (local $block_return (ref $Closure))
    (local $invocation (ref $OpInvocation))

    (local $handler_ctx (ref $Ctx))
    (local $handler_args (ref any))

    ;; args = [k_suspension, op_id, value]
    (local.set $k_suspension
      (ref.as_non_null (call $args_head (local.get $args))))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $op_id_val
      (ref.as_non_null (call $args_head (local.get $args))))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $value (call $args_head (local.get $args)))

    ;; op_id is a fink int literal (boxed as $I64). Cast and read i64,
    ;; truncate to i32 since op_id_i is just an opaque tag.
    (local.set $op_id_i
      (i32.wrap_i64
        (struct.get $I64 $ival
          (ref.cast (ref $I64) (local.get $op_id_val)))))

    ;; Walk op_frame_chain looking for op_id_i.
    (local.set $cur (call $ctx_op_frame_chain (local.get $ctx)))
    (block $found
      (loop $walk
        ;; If cur is null, trap (no matching frame).
        (local.set $matched (ref.as_non_null (local.get $cur)))
        ;; If matched.op_id == op_id_i, exit loop.
        (br_if $found
          (i32.eq
            (struct.get $OpFrame $op_id (local.get $matched))
            (local.get $op_id_i)))
        ;; Else advance to parent and continue.
        (local.set $cur (struct.get $OpFrame $parent (local.get $matched)))
        (br $walk)))

    ;; Build the three closures and the OpInvocation.
    (local.set $resume
      (call $make_resume_op
        (local.get $k_suspension)
        (call $ctx_op_frame_chain (local.get $ctx))))
    (local.set $block_rerun
      (call $make_block_rerun
        (struct.get $OpFrame $body_fn      (local.get $matched))
        (struct.get $OpFrame $k_block_exit (local.get $matched))
        (call $ctx_op_frame_chain (local.get $ctx))))
    (local.set $block_return
      (call $make_block_return
        (struct.get $OpFrame $k_block_exit (local.get $matched))
        (struct.get $OpFrame $parent       (local.get $matched))))

    (local.set $invocation
      (struct.new $OpInvocation
        (local.get $resume)
        (local.get $block_rerun)
        (local.get $block_return)
        (ref.as_non_null (local.get $matched))))

    ;; Handler ctx: op_frame_chain unchanged (so fresh fns called
    ;; from the handler still find this frame on perform). To
    ;; *forward* (re-yield to outer same-id), use `rethrow` which
    ;; walks past current_op.matched_frame.
    (local.set $handler_ctx
      (call $ctx_make
        (call $ctx_user (local.get $ctx))
        (call $ctx_frame_chain (local.get $ctx))
        (call $ctx_op_frame_chain (local.get $ctx))
        (local.get $invocation)))

    ;; Handler args: [noop_cont, value]. Handler's "return" path
    ;; goes nowhere; control flow is shaped by whatever explicit
    ;; terminator the handler invokes (resume / block_return /
    ;; block_rerun / rethrow) or by other live continuations the
    ;; handler captured / passed elsewhere.
    (local.set $handler_args
      (call $args_prepend
        (global.get $_handler_noop_cont)
        (call $args_prepend (local.get $value) (call $args_empty))))

    (return_call $apply_3
      (local.get $handler_args)
      (local.get $handler_ctx)
      (struct.get $OpFrame $handler (local.get $matched))))

  (global $perform_op_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $perform_op_apply)
      (ref.null $Captures)))

  (func $perform_op (@pub) (@impl "std/effects.fnk:perform_op") (result (ref any))
    (global.get $perform_op_closure))


  ;; ---- get_resume / get_block_rerun / get_block_return -------------

  (elem declare func $get_resume_apply)

  (func $get_resume_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref null any))
    (local $invocation (ref $OpInvocation))
    (local $result_args (ref any))

    (local.set $cont (call $args_head (local.get $args)))
    (local.set $invocation
      (ref.as_non_null (call $ctx_current_op (local.get $ctx))))

    (local.set $result_args
      (call $args_prepend
        (struct.get $OpInvocation $resume (local.get $invocation))
        (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $ctx)
      (local.get $cont)))

  (global $get_resume_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $get_resume_apply)
      (ref.null $Captures)))

  (func $get_resume (@pub) (@impl "std/effects.fnk:get_resume") (result (ref any))
    (global.get $get_resume_closure))


  (elem declare func $get_block_rerun_apply)

  (func $get_block_rerun_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref null any))
    (local $invocation (ref $OpInvocation))
    (local $result_args (ref any))

    (local.set $cont (call $args_head (local.get $args)))
    (local.set $invocation
      (ref.as_non_null (call $ctx_current_op (local.get $ctx))))

    (local.set $result_args
      (call $args_prepend
        (struct.get $OpInvocation $block_rerun (local.get $invocation))
        (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $ctx)
      (local.get $cont)))

  (global $get_block_rerun_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $get_block_rerun_apply)
      (ref.null $Captures)))

  (func $get_block_rerun (@pub) (@impl "std/effects.fnk:get_block_rerun") (result (ref any))
    (global.get $get_block_rerun_closure))


  (elem declare func $get_block_return_apply)

  (func $get_block_return_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref null any))
    (local $invocation (ref $OpInvocation))
    (local $result_args (ref any))

    (local.set $cont (call $args_head (local.get $args)))
    (local.set $invocation
      (ref.as_non_null (call $ctx_current_op (local.get $ctx))))

    (local.set $result_args
      (call $args_prepend
        (struct.get $OpInvocation $block_return (local.get $invocation))
        (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $ctx)
      (local.get $cont)))

  (global $get_block_return_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $get_block_return_apply)
      (ref.null $Captures)))

  (func $get_block_return (@pub) (@impl "std/effects.fnk:get_block_return") (result (ref any))
    (global.get $get_block_return_closure))


  ;; ---- rethrow -----------------------------------------------------
  ;;
  ;; Fink-level: `rethrow value`. Performs the CURRENT handler's
  ;; operation (taken from current_op.matched_frame.op_id) starting
  ;; the chain walk PAST the matched frame -- so an outer same-id
  ;; handler catches it. Used to forward an op the current handler
  ;; doesn't want to fully handle.

  (elem declare func $rethrow_apply)

  (func $rethrow_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref any))
    (local $value (ref null any))
    (local $invocation (ref $OpInvocation))
    (local $current_frame (ref $OpFrame))
    (local $op_id_i i32)
    (local $cur (ref null $OpFrame))
    (local $matched (ref null $OpFrame))

    (local $resume (ref $Closure))
    (local $block_rerun (ref $Closure))
    (local $block_return (ref $Closure))
    (local $new_invocation (ref $OpInvocation))
    (local $handler_ctx (ref $Ctx))
    (local $handler_args (ref any))

    ;; args = [cont, value]
    (local.set $cont
      (ref.as_non_null (call $args_head (local.get $args))))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $value (call $args_head (local.get $args)))

    ;; Find the current handler's frame and its op_id.
    (local.set $invocation
      (ref.as_non_null (call $ctx_current_op (local.get $ctx))))
    (local.set $current_frame
      (struct.get $OpInvocation $matched_frame (local.get $invocation)))
    (local.set $op_id_i
      (struct.get $OpFrame $op_id (local.get $current_frame)))

    ;; Walk from current_frame.parent onward (skip this frame and any
    ;; closer ones -- start strictly past it).
    (local.set $cur (struct.get $OpFrame $parent (local.get $current_frame)))
    (block $found
      (loop $walk
        (local.set $matched (ref.as_non_null (local.get $cur)))
        (br_if $found
          (i32.eq
            (struct.get $OpFrame $op_id (local.get $matched))
            (local.get $op_id_i)))
        (local.set $cur (struct.get $OpFrame $parent (local.get $matched)))
        (br $walk)))

    ;; Build closures for the OUTER handler.
    (local.set $resume
      (call $make_resume_op
        (local.get $cont)
        (call $ctx_op_frame_chain (local.get $ctx))))
    (local.set $block_rerun
      (call $make_block_rerun
        (struct.get $OpFrame $body_fn      (local.get $matched))
        (struct.get $OpFrame $k_block_exit (local.get $matched))
        (call $ctx_op_frame_chain (local.get $ctx))))
    (local.set $block_return
      (call $make_block_return
        (struct.get $OpFrame $k_block_exit (local.get $matched))
        (struct.get $OpFrame $parent       (local.get $matched))))

    (local.set $new_invocation
      (struct.new $OpInvocation
        (local.get $resume)
        (local.get $block_rerun)
        (local.get $block_return)
        (ref.as_non_null (local.get $matched))))

    (local.set $handler_ctx
      (call $ctx_make
        (call $ctx_user (local.get $ctx))
        (call $ctx_frame_chain (local.get $ctx))
        (call $ctx_op_frame_chain (local.get $ctx))
        (local.get $new_invocation)))

    (local.set $handler_args
      (call $args_prepend
        (local.get $cont)
        (call $args_prepend (local.get $value) (call $args_empty))))

    (return_call $apply_3
      (local.get $handler_args)
      (local.get $handler_ctx)
      (struct.get $OpFrame $handler (local.get $matched))))

  (global $rethrow_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $rethrow_apply)
      (ref.null $Captures)))

  (func $rethrow (@pub) (@impl "std/effects.fnk:rethrow") (result (ref any))
    (global.get $rethrow_closure))


  ;; ---- bind_chain --------------------------------------------------
  ;;
  ;; Fink-level: `bind_chain fn -> wrapped_fn`. Captures the current
  ;; op_frame_chain in a closure. When wrapped_fn is later called
  ;; (anywhere -- inside or outside any with-block), it restores the
  ;; captured chain and tail-calls `fn`. Effectively re-installs the
  ;; chain that was active at bind_chain time.
  ;;
  ;; Used by spawn-style patterns: a handler wants to enqueue a fresh
  ;; task fn so a drive loop OUTSIDE the with-block can fire it. The
  ;; task body performs ops that must route through the with-block's
  ;; handler. Calling bind_chain on the task before enqueueing pins
  ;; the chain to the task.

  (elem declare func $_bound_fn_apply)

  (func $_bound_fn_apply (type $Fn3)
    (param $caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $captures (ref $Captures))
    (local $inner_fn (ref any))
    (local $captured_chain (ref null $OpFrame))
    (local $new_ctx (ref $Ctx))

    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $inner_fn
      (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 0))))
    (local.set $captured_chain
      (ref.cast (ref null $OpFrame)
        (array.get $Captures (local.get $captures) (i32.const 1))))

    ;; Switch op_frame_chain to captured; preserve firer's user + clear current_op.
    (local.set $new_ctx
      (call $ctx_make
        (call $ctx_user (local.get $ctx))
        (call $ctx_frame_chain (local.get $ctx))
        (local.get $captured_chain)
        (ref.null $OpInvocation)))

    (return_call $apply_3
      (local.get $args)
      (local.get $new_ctx)
      (local.get $inner_fn)))

  (elem declare func $bind_chain_apply)

  (func $bind_chain_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref null any))
    (local $inner_fn (ref any))
    (local $wrapped (ref $Closure))
    (local $result_args (ref any))

    ;; args = [cont, fn]
    (local.set $cont (call $args_head (local.get $args)))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $inner_fn
      (ref.as_non_null (call $args_head (local.get $args))))

    (local.set $wrapped
      (struct.new $Closure
        (ref.func $_bound_fn_apply)
        (array.new_fixed $Captures 2
          (local.get $inner_fn)
          (call $ctx_op_frame_chain (local.get $ctx)))))

    (local.set $result_args
      (call $args_prepend (local.get $wrapped) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $ctx)
      (local.get $cont)))

  (global $bind_chain_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $bind_chain_apply)
      (ref.null $Captures)))

  (func $bind_chain (@pub) (@impl "std/effects.fnk:bind_chain") (result (ref any))
    (global.get $bind_chain_closure))


  ;; ---- suspend -----------------------------------------------------
  ;;
  ;; Fink-level: `suspend _ -> resume`.
  ;;
  ;; The lowest-level control-flow primitive. Captures the caller's
  ;; continuation as a closure and returns it. The expression
  ;; `suspend _` evaluates to that closure; when the closure is
  ;; called with `v`, control transfers to the captured cont (the
  ;; point right after the `suspend _` call) with `v` as the result
  ;; of the expression.
  ;;
  ;; Example:
  ;;   resume = suspend _    # at this point control is suspended
  ;;   ... resume is a value, can be passed around ...
  ;;   # eventually some code does: resume 42
  ;;   # control jumps back here; the `suspend _` expression was 42
  ;;
  ;; No frames, no chain walk, no handler involvement -- just raw
  ;; cont capture. Lower-level than perform_op / with_invoke2. The
  ;; effect-handler primitives are built on the same mechanism but
  ;; add structured dispatch on top.

  ;; The closure produced by `suspend _`. Captures the caller's
  ;; cont. When called with `v`, tail-calls cont with v under the
  ;; firer's ctx (firer-wins user payload, like resume in perform_op).
  (elem declare func $_suspend_resume_fn)

  (func $_suspend_resume_fn (type $Fn3)
    (param $caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $captures (ref $Captures))
    (local $captured_cont (ref any))
    (local $v (ref null any))
    (local $result_args (ref any))

    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $captured_cont
      (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 0))))

    ;; args = [k_caller, v]. Discard k_caller -- we tail-call into
    ;; the captured cont, not back to the firer.
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $v (call $args_head (local.get $args)))

    (local.set $result_args
      (call $args_prepend (local.get $v) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $ctx)
      (local.get $captured_cont)))

  (elem declare func $suspend_apply)

  (func $suspend_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref any))
    (local $resume (ref $Closure))
    (local $result_args (ref any))

    ;; args = [cont, _]. cont is what we capture.
    (local.set $cont (ref.as_non_null (call $args_head (local.get $args))))

    (local.set $resume
      (struct.new $Closure
        (ref.func $_suspend_resume_fn)
        (array.new_fixed $Captures 1 (local.get $cont))))

    ;; Return the resume closure as the value of `suspend _`. Caller
    ;; gets the closure and decides whether/when to fire it.
    (local.set $result_args
      (call $args_prepend (local.get $resume) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $ctx)
      (local.get $cont)))

  (global $suspend_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $suspend_apply)
      (ref.null $Captures)))

  (func $suspend (@pub) (@impl "std/effects.fnk:suspend") (result (ref any))
    (global.get $suspend_closure))


  ;; ---- conts -------------------------------------------------------
  ;;
  ;; Fink-level (inside a handler): `conts _ -> [resume, block_return, block_rerun]`.
  ;;
  ;; Returns a list of the three continuation closures from the
  ;; current op invocation. Destructure with positional pattern:
  ;;   [resume, block_return, block_rerun] = conts _

  (elem declare func $conts_apply)

  (func $conts_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref null any))
    (local $invocation (ref $OpInvocation))
    (local $list (ref any))
    (local $result_args (ref any))

    (local.set $cont (call $args_head (local.get $args)))
    (local.set $invocation
      (ref.as_non_null (call $ctx_current_op (local.get $ctx))))

    ;; Build [resume, block_return, block_rerun] using the args list
    ;; primitives (same impl as fink list).
    (local.set $list
      (call $args_prepend
        (struct.get $OpInvocation $resume (local.get $invocation))
        (call $args_prepend
          (struct.get $OpInvocation $block_return (local.get $invocation))
          (call $args_prepend
            (struct.get $OpInvocation $block_rerun (local.get $invocation))
            (call $args_empty)))))

    (local.set $result_args
      (call $args_prepend (local.get $list) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $ctx)
      (local.get $cont)))

  (global $conts_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $conts_apply)
      (ref.null $Captures)))

  (func $conts (@pub) (@impl "std/effects.fnk:conts") (result (ref any))
    (global.get $conts_closure))
)
