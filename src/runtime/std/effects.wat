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
  ;; where `abort v` / `yield2 v` jumps to. Fink code never sees frames.
  ;;
  ;; Migration target: move from a global mutable stack into a ctx slot
  ;; (see $Ctx below and `.brain/.scratch/effects-ctx-frame-chain.md`).
  ;; Step 1 (this commit): introduce $Ctx as the carrier of (user, chain)
  ;; but keep the global stack as the active store -- get_ctx/set_ctx
  ;; project through $Ctx.user, with back-compat fallback for bare-value
  ;; ctx. Later steps migrate yield2/abort/with_invoke off the global.

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


  ;; -- $Ctx: substrate-internal ctx carrier --------------------------
  ;;
  ;; ctx is what threads forward through every CPS call. We split it
  ;; into two slots:
  ;;   $user        -- the value fink code sees via get_ctx / set_ctx.
  ;;   $frame_chain -- the substrate-only handler-frame chain. Unused
  ;;                   in step 1; future yield2/abort will read this
  ;;                   directly instead of the global $frame_stack.
  ;;
  ;; All non-substrate code treats ctx opaquely as `(ref null any)`.
  ;; The substrate is the only place that pattern-matches on $Ctx.

  (type $Ctx (struct
    (field $user        (ref null any))
    (field $frame_chain (ref null $Frame))))

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

  ;; Return a fresh $Ctx with the given user payload and the
  ;; frame_chain inherited from the input ctx (null if input is not a
  ;; $Ctx).
  (func $ctx_with_user
      (param $ctx (ref null any))
      (param $new_user (ref null any))
      (result (ref $Ctx))
    (local $as_ctx (ref null $Ctx))
    (local $parent_chain (ref null $Frame))
    (block $not_ctx
      (block $is (result (ref $Ctx))
        (br $not_ctx
          (br_on_cast $is (ref null any) (ref $Ctx) (local.get $ctx))))
      (local.set $as_ctx)
      (local.set $parent_chain
        (struct.get $Ctx $frame_chain (local.get $as_ctx)))
      (return (struct.new $Ctx
        (local.get $new_user)
        (local.get $parent_chain))))
    (struct.new $Ctx
      (local.get $new_user)
      (ref.null $Frame)))


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


  ;; -- yield2 ---------------------------------------------------------
  ;;
  ;; Resumable-yield substrate primitive (step 1).
  ;;
  ;; Fink-level signature: `yield2 v -> Yield{value, resume}`. Captures
  ;; the caller's continuation as `resume` and returns a `$Yield` struct
  ;; carrying the yielded value and the captured cont. The handler reads
  ;; the struct via `get_yield_value` / `get_yield_resume` and decides
  ;; whether to call `resume r` (re-enter body at the yield2 site with
  ;; r) or discard it (zero-shot).
  ;;
  ;; Reaches the handler the same way `abort` does: pops the top handler
  ;; frame and tail-calls k_outer with the Yield struct. Traps with
  ;; "no handler frame" if there's no enclosing `with`. Step-1 limitation:
  ;; once popped, the resumed body has no frame, so a second yield2 from
  ;; inside the resumed body will trap. ctx-threaded handler discovery
  ;; (the longer-term shape) is deferred until a test forces it.
  ;;
  ;; Importable: `{yield2, get_yield_value, get_yield_resume} =
  ;;             import 'std/effects.fnk'`.

  (type $Yield (struct
    (field $value  (ref any))
    (field $resume (ref any))))

  ;; resume closure: captures k_body_rest (body's continuation after
  ;; the yield2 site). When the handler invokes `resume r`, the resume
  ;; call's own cont (`k_handler_after_resume`) becomes the *new*
  ;; frame's k_outer. So body's next yield2 (or natural return through
  ;; _pop_cont_fn) lands at the handler's after-resume code -- as if
  ;; resume were a regular fn call returning body's next value. r
  ;; becomes the value of the original yield2 expression in body.
  (elem declare func $_resume_fn)

  (func $_resume_fn (type $Fn3)
    (param $caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $captures (ref $Captures))
    (local $k_body_rest (ref any))
    (local $k_handler_after_resume (ref any))
    (local $r (ref null any))
    (local $result_args (ref any))

    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $k_body_rest
      (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 0))))

    ;; args = [cont, r]. cont is the handler's continuation after the
    ;; resume call -- the new frame's k_outer so body's next yield2 or
    ;; natural completion returns the value to the handler here.
    (local.set $k_handler_after_resume
      (ref.as_non_null (call $args_head (local.get $args))))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $r (call $args_head (local.get $args)))

    (call $frame_push (local.get $k_handler_after_resume))

    (local.set $result_args
      (call $args_prepend (local.get $r) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $ctx)
      (local.get $k_body_rest)))

  (func $make_resume
      (param $k_body_rest (ref any))
      (result (ref $Closure))
    (struct.new $Closure
      (ref.func $_resume_fn)
      (array.new_fixed $Captures 1
        (local.get $k_body_rest))))

  (elem declare func $yield2_apply)

  (func $yield2_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref any))
    (local $value (ref any))
    (local $k_outer (ref any))
    (local $resume (ref $Closure))
    (local $yielded (ref $Yield))
    (local $result_args (ref any))

    ;; args = [cont, value]. cont is k_body_rest -- the continuation of
    ;; the yield2 call site in body.
    (local.set $cont (ref.as_non_null (call $args_head (local.get $args))))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $value (ref.as_non_null (call $args_head (local.get $args))))

    ;; Pop top frame -> k_outer (body-call-cont). Traps if stack empty.
    (local.set $k_outer (call $frame_pop))

    ;; Build resume closure capturing k_body_rest. When invoked, the
    ;; resume's own caller-cont becomes the new frame's k_outer, so
    ;; body's next yield2 / natural return lands at the handler's
    ;; after-resume site.
    (local.set $resume (call $make_resume (local.get $cont)))

    (local.set $yielded
      (struct.new $Yield (local.get $value) (local.get $resume)))

    (local.set $result_args
      (call $args_prepend (local.get $yielded) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $ctx)
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

  ;; pop_cont reads the body-call-cont from the top frame's k_outer
  ;; rather than capturing it directly. That makes the frame the single
  ;; source of truth for "where body's return value goes" -- resume can
  ;; retarget natural-return by pushing a frame with a different k_outer.
  (elem declare func $_pop_cont_fn)

  (func $_pop_cont_fn (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $body_call_cont (ref any))

    (local.set $body_call_cont (call $frame_pop))
    (return_call $apply_3
      (local.get $args)
      (local.get $ctx)
      (local.get $body_call_cont)))

  (global $_pop_cont (ref $Closure)
    (struct.new $Closure
      (ref.func $_pop_cont_fn)
      (ref.null $Captures)))

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
    (local $body_args (ref any))

    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $body_fn
      (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 0))))

    ;; args = [body_call_cont, ...user_args]
    (local.set $body_call_cont
      (ref.as_non_null (call $args_head (local.get $args))))

    ;; Push frame holding the body-call cont -- single source of truth
    ;; for "where body's value goes". Both abort/yield2 and the
    ;; natural-return path (_pop_cont) read it from here.
    (call $frame_push (local.get $body_call_cont))

    ;; Replace head of args with the shared _pop_cont closure; body sees
    ;; [pop_cont, ...user_args]. Natural return -> pop_cont -> pops the
    ;; frame, tail-calls the frame's k_outer with body's value.
    (local.set $body_args
      (call $args_prepend (global.get $_pop_cont)
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
