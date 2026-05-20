;; CPS calling convention + control-flow substrate.
;;
;; All functions are $Fn3(captures, ctx, args). Conts are in captures
;; or in the args list (conts-first ordering ensures this after lifting).
;; ctx is the universe context threaded as a native wasm param so
;; callees don't need to peel it off the args list.
;;
;; This module owns the entire substrate every fink program runs on:
;;
;;   $Closure / $Captures / $Fn3 / $Ctx -- the value shapes.
;;   apply_3 / apply_N / args_* / make_thunk -- the calling-convention ABI.
;;   set_ctx / get_ctx -- thread a user-visible context through every call.
;;   suspend -- capture the current continuation as a closure.
;;
;; That's the whole runtime. Effect handlers, `with` semantics, generators,
;; coroutines, exceptions, schedulers, backtracking -- everything else --
;; are library code in ƒink (e.g. std/effects.fnk) built on top of these
;; primitives. See .brain/.scratch/userland_with.md for the design.

(module

  (import "std/list.wat" "List" (type $List (sub any)))

  ;; List operations — args calling-convention storage. apply.wat owns
  ;; the args ABI; list.wat just provides the underlying data structure.
  (import "std/list.wat" "empty"
    (func $list_empty (result (ref $List))))
  (import "std/list.wat" "prepend"
    (func $list_prepend (param $head (ref any)) (param $tail (ref $List)) (result (ref $List))))
  (import "std/list.wat" "head_any"
    (func $list_head_any (param $list (ref null any)) (result (ref null any))))
  (import "std/list.wat" "tail_any"
    (func $list_tail_any (param $list (ref null any)) (result (ref null any))))
  (import "std/list.wat" "concat"
    (func $list_concat (param $a (ref $List)) (param $b (ref $List)) (result (ref $List))))


  ;; $Captures — flat array of captured values.
  ;; Each element is (ref null any) — nullable to allow default-init
  ;; by array.new_default. Closures with zero captures use a null
  ;; $Captures ref instead of an empty array (no allocation).
  (type $Captures (@pub) (array (mut (ref null any))))


  ;; $Closure — universal closure type.
  ;; Field 0: funcref to the lifted function (arity = call_arity + capture_count).
  ;; Field 1: captured values array, or null if no captures.
  ;; Dispatch (_apply_N) reads the captures array length to determine
  ;; how many extra args to push before calling the funcref.
  (type $Closure (@pub) (struct
    (field $func funcref)
    (field $captures (ref null $Captures))
  ))

  ;; Unified calling convention. $Fn3(captures, ctx, args) — caller
  ;; passes the universe context as a native wasm value, sidestepping
  ;; the args-list head/tail dance. Every closure func is Fn3-typed.
  (type $Fn3 (@pub) (func (param (ref null any) (ref null any) (ref null any))))

  ;; $Ctx — universe context threaded through every Fn3 call. Carries a
  ;; single user-visible payload; userland reads/replaces it via the
  ;; get_ctx / set_ctx primitives below. The substrate is the only place
  ;; that pattern-matches on $Ctx; non-substrate code treats ctx opaquely
  ;; as `(ref null any)`. Hosts call $empty_ctx at module entry to seed
  ;; the ctx arg of $apply_3.
  (type $Ctx (@pub) (struct
    (field $user (ref null any))
  ))

  ;; Mint an empty $Ctx. Host runners call this once per module-wrapper
  ;; entry to seed the ctx arg of $apply_3, replacing the placeholder
  ;; ref.i31 42 that lived at the host boundary during the Fn3 flip.
  ;;
  ;; The user payload is seeded with `ref.i31 0` (fink unit), not null,
  ;; so `get_ctx _` before any `set_ctx` returns a real value rather
  ;; than tripping non-null casts downstream.
  (func $empty_ctx (@pub) (result (ref $Ctx))
    (struct.new $Ctx (ref.i31 (i32.const 0)))
  )

  ;; If ctx is a $Ctx, return its user payload. Otherwise return ctx
  ;; itself -- so callers before any set_ctx still see the bare value
  ;; the host seeded with.
  (func $ctx_user (param $ctx (ref null any)) (result (ref null any))
    (local $as_ctx (ref null $Ctx))
    (block $not_ctx
      (block $is (result (ref $Ctx))
        (br $not_ctx
          (br_on_cast $is (ref null any) (ref $Ctx) (local.get $ctx))))
      (local.set $as_ctx)
      (return (struct.get $Ctx $user (local.get $as_ctx))))
    (local.get $ctx))


  ;; $SpreadArgs — wrapper for spread arguments at call sites.
  ;; Contains a $List of the spread values. Used to distinguish a spread
  ;; call (f ..items) from a regular call passing a list value (f items).
  ;; _apply uses br_on_cast $SpreadArgs to detect the spread calling
  ;; convention at runtime.
  (type $SpreadArgs (@pub) (struct
    (field $items (ref $List))
  ))

  ;; $VarArgs — variable-length argument array.
  ;; Used by builtins that accept a variable number of arguments
  ;; (e.g. str_fmt for string templates). The emitter builds the
  ;; array inline via array.new_fixed at compile time.
  (type $VarArgs (@pub) (array (ref null any)))


  ;; Universal closure dispatcher. Tail-called from every CPS
  ;; continuation site. Ctx is threaded as a native wasm param so
  ;; Fn3-typed callees don't need to peel it off the args list.
  (func $apply_3 (@pub)
    (param $args (ref null any))
    (param $ctx (ref null any))
    (param $callee (ref null any))

    (local $clos (ref $Closure))
    (local.set $clos (ref.cast (ref $Closure) (local.get $callee)))

    (return_call_ref $Fn3
      (struct.get $Closure $captures (local.get $clos))
      (local.get $ctx)
      (local.get $args)
      (ref.cast (ref $Fn3) (struct.get $Closure $func (local.get $clos))))
  )


  ;; -- Args calling-convention primitives -----------------------------
  ;;
  ;; These are the runtime ABI for the args list — head/tail/empty/prepend/
  ;; concat — used by every CPS call site. The underlying storage today
  ;; is std/list.wat; apply.wat owns the contract.
  ;;
  ;; TODO: args impl leaks — empty/prepend/concat expose `$List` in their
  ;; signatures, forcing every caller to import `$List` from std/list.wat
  ;; just to type-check. Args should be opaque (an `$Args` type that hides
  ;; the carrier) so apply.wat stays free to swap storage without ripple.

  (func $args_head (@pub)
      (param $args (ref null any))
      (result (ref null any))
    (return_call $list_head_any (local.get $args)))

  (func $args_tail (@pub)
      (param $args (ref null any))
      (result (ref null any))
    (return_call $list_tail_any (local.get $args)))

  ;; TODO use $Args type for result
  (func $args_empty (@pub) (result (ref any))
    (return_call $list_empty))

  ;; TODO use $Args type for param and result.
  ;; TODO: $head should be (ref any) — args list elements are always
  ;; real values, never null. Currently nullable because the compiler
  ;; emits user value-locals as (ref null any) and doesn't insert a
  ;; non-null cast at this boundary. Tighten user emit and revert to
  ;; (ref any) once all call sites flow non-null.
  (func $args_prepend (@pub)
      (param $head (ref null any)) (param $tail (ref any))
      (result (ref any))
    (return_call $list_prepend
      (ref.as_non_null (local.get $head))
      (ref.cast (ref $List) (local.get $tail))))

  ;; TODO use $Args type for param and result.
  ;; TODO: $a should be (ref any) — args lists are never null.
  ;; Currently nullable because the compiler emits user value-locals
  ;; as (ref null any) and doesn't insert a non-null cast at this
  ;; boundary. Tighten user emit and revert once flow is non-null.
  (func $args_concat (@pub)
      (param $a (ref null any)) (param $b (ref any))
      (result (ref any))
    (return_call $list_concat
      (ref.cast (ref $List) (local.get $a))
      (ref.cast (ref $List) (local.get $b))))


  ;; -- Apply helpers ---------------------------------------------------
  ;;
  ;; apply_0/1/2_vals wrap N values into an args list and tail-call
  ;; $apply_3. Every CPS continuation site that returns N values to its
  ;; cont routes through here; ctx is passed explicitly so the cont
  ;; runs under the producer's universe context.

  (func $apply_0 (@pub) (param $ctx (ref null any)) (param $cont (ref null any))
    (return_call $apply_3
      (call $args_empty)
      (local.get $ctx)
      (local.get $cont)))

  (func $apply_1 (@pub) (param $ctx (ref null any)) (param $result (ref null any)) (param $cont (ref null any))
    (return_call $apply_3
      (call $args_prepend (ref.as_non_null (local.get $result)) (call $args_empty))
      (local.get $ctx)
      (local.get $cont)))

  (func $apply_2_vals (@pub) (param $ctx (ref null any)) (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_3
      (call $args_prepend (ref.as_non_null (local.get $a))
        (call $args_prepend (ref.as_non_null (local.get $b)) (call $args_empty)))
      (local.get $ctx)
      (local.get $cont)))


  ;; -- Thunks ----------------------------------------------------------
  ;;
  ;; A thunk is a zero-arg $Closure that, when applied, calls a saved
  ;; continuation with a saved value: thunk() = cont(value). Used by the
  ;; async scheduler (queued tasks) and by channel/host-cont resumption.

  ;; Declarative element segment — required by WASM spec for ref.func.
  (elem declare func $_thunk_fn)

  ;; Thunk body. Captures: [cont, value, ctx]. When applied: cont(value)
  ;; resumes under the *captured* ctx, not whatever ctx the scheduler
  ;; hands in via $_sched_ctx. The captured ctx is what was active when
  ;; this thunk was built (e.g. when the sender yielded a value into a
  ;; channel). Using the captured ctx is how ctx survives the async/
  ;; channel suspension boundary.
  (func $_thunk_fn (type $Fn3)
      (param $caps (ref null any))
      (param $_sched_ctx (ref null any))
      (param $args (ref null any))
    (local $captures (ref $Captures))
    (local $cont (ref any))
    (local $value (ref any))
    (local $ctx (ref null any))
    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $cont  (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 0))))
    (local.set $value (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 1))))
    (local.set $ctx   (array.get $Captures (local.get $captures) (i32.const 2)))
    (return_call $apply_3
      (call $args_prepend (local.get $value) (call $args_empty))
      (local.get $ctx)
      (local.get $cont))
  )

  ;; Build a thunk that captures the caller's ctx. When the scheduler
  ;; later applies this thunk, the cont resumes under THIS ctx — not the
  ;; scheduler's. That is the whole point of the extra capture slot.
  (func $make_thunk (@pub) (param $ctx (ref null any)) (param $cont (ref any)) (param $value (ref any)) (result (ref $Closure))
    (struct.new $Closure
      (ref.func $_thunk_fn)
      (array.new_fixed $Captures 3 (local.get $cont) (local.get $value) (local.get $ctx)))
  )

  ;; Make a thunk that calls cont with unit (i31 0). Same ctx-capture
  ;; semantics as $make_thunk.
  (func $make_unit_thunk (@pub) (param $ctx (ref null any)) (param $cont (ref any)) (result (ref $Closure))
    (call $make_thunk (local.get $ctx) (local.get $cont) (ref.i31 (i32.const 0)))
  )


  ;; -- set_ctx --------------------------------------------------------
  ;;
  ;; Fink-level: `set_ctx new_ctx -> old_ctx`.
  ;;
  ;; CPS shape (Fn3): args = [cont, new_ctx]. Returns the caller's user
  ;; payload to cont; threads a fresh $Ctx (new user) as the cont's ctx
  ;; so every fink call downstream sees `new_ctx` as their universe.
  ;;
  ;; Exported as `std/effects.fnk:set_ctx`; the import path is the
  ;; user-facing API and stays stable across substrate refactors.

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

    (local.set $result_args
      (call $args_prepend
        (call $ctx_user (local.get $ctx))
        (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (struct.new $Ctx (local.get $new_user))
      (local.get $cont)))

  (global $set_ctx_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $set_ctx_apply)
      (ref.null $Captures)))

  (func $set_ctx (@pub) (@impl "std/effects.fnk:set_ctx") (result (ref any))
    (global.get $set_ctx_closure))


  ;; -- get_ctx --------------------------------------------------------
  ;;
  ;; Fink-level: `get_ctx _ -> ctx`.
  ;;
  ;; CPS shape (Fn3): args = [cont, _]. Returns the caller's user
  ;; payload to cont without mutating it.

  (elem declare func $get_ctx_apply)

  (func $get_ctx_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref null any))
    (local $result_args (ref any))

    (local.set $cont (call $args_head (local.get $args)))

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


  ;; -- suspend --------------------------------------------------------
  ;;
  ;; Fink-level: `result = suspend fn resume: ...`.
  ;;
  ;; Captures the suspend expression's continuation as a closure `resume`,
  ;; then invokes the user fn with `resume` as its single argument.
  ;; `resume v` transfers control to the captured cont (the point right
  ;; after `suspend ...`) with `v` becoming the value of the suspend
  ;; expression. Multi-shot: `resume` may be called any number of times
  ;; (or zero -- if user_fn never calls resume and falls off the end,
  ;; that thread of execution ends).
  ;;
  ;; Combined with set_ctx / get_ctx, this is sufficient to build effect
  ;; handlers, generators, coroutines, schedulers, backtracking, and
  ;; exceptions as userland library code.

  ;; Internal: no-op cont. Used as the k_caller passed to user_fn so
  ;; if user_fn falls off the end without calling resume, the thread
  ;; of execution dies cleanly.
  (elem declare func $_noop_cont_fn)

  (func $_noop_cont_fn (type $Fn3)
    (param $_caps (ref null any))
    (param $_ctx (ref null any))
    (param $_args (ref null any))
    (return))

  (global $_noop_cont (ref $Closure)
    (struct.new $Closure
      (ref.func $_noop_cont_fn)
      (ref.null $Captures)))

  ;; Internal: the resume closure handed to user_fn. Captures the cont
  ;; passed to suspend. When fired with `v`, discards its own k_caller
  ;; and tail-calls the captured cont with v under the firer's ctx.
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

    ;; args = [k_caller, v]. Discard k_caller; tail-call captured cont.
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
    (local $user_fn (ref any))
    (local $args_tail (ref any))
    (local $resume (ref $Closure))
    (local $call_args (ref any))

    ;; args = [cont, user_fn]. Capture cont inside resume; tail-call
    ;; user_fn with resume as its single argument under a noop k_caller.
    (local.set $cont (ref.as_non_null (call $args_head (local.get $args))))
    (local.set $args_tail (ref.as_non_null (call $args_tail (local.get $args))))
    (local.set $user_fn (ref.as_non_null (call $args_head (local.get $args_tail))))

    (local.set $resume
      (struct.new $Closure
        (ref.func $_suspend_resume_fn)
        (array.new_fixed $Captures 1 (local.get $cont))))

    (local.set $call_args
      (call $args_prepend
        (global.get $_noop_cont)
        (call $args_prepend
          (local.get $resume)
          (call $args_empty))))

    (return_call $apply_3
      (local.get $call_args)
      (local.get $ctx)
      (local.get $user_fn)))

  (global $suspend_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $suspend_apply)
      (ref.null $Captures)))

  (func $suspend (@pub) (@impl "std/effects.fnk:suspend") (result (ref any))
    (global.get $suspend_closure))
)
