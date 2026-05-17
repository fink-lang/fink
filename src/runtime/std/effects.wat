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


  ;; -- register -------------------------------------------------------
  ;;
  ;; Substrate primitive for installing a handler against an operation.
  ;; Fink-level signature: `register op_fn, handler_fn`. Today this is
  ;; a STUB: no handler stack exists yet, so register is a no-op that
  ;; returns unit (its handler arg) to the cont. The real registration
  ;; (pushing a frame keyed by op_fn identity onto ctx) lands with the
  ;; `with` lowering.
  ;;
  ;; Importable: `{register} = import 'std/effects.fnk'`. Fn3-shaped
  ;; closure, two-arg.

  (elem declare func $register_apply)

  (func $register_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref null any))
    (local $handler (ref null any))
    (local $result_args (ref any))

    (local.set $cont (call $args_head (local.get $args)))
    (local.set $args (call $args_tail (local.get $args)))
    ;; skip op_fn (first user arg) -- stub doesn't use it
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $handler (call $args_head (local.get $args)))

    (local.set $result_args
      (call $args_prepend (local.get $handler) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $ctx)
      (local.get $cont)))

  (global $register_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $register_apply)
      (ref.null $Captures)))

  (func $register (@pub) (@impl "std/effects.fnk:register") (result (ref any))
    (global.get $register_closure))


  ;; -- init_effects ---------------------------------------------------
  ;;
  ;; Wires the two userland fns the substrate calls to find handlers
  ;; for a given operation:
  ;;
  ;;   init_effects get_handler, set_handler
  ;;
  ;; Both are stored in module-level mutable globals. The `with foo: B`
  ;; lowering will call `get_handler foo` to find the handler-fn to
  ;; invoke. `set_handler` is exposed so userland `register`-style
  ;; helpers can write into whatever data structure they chose.
  ;;
  ;; Module-level globals are a pragmatic stop-gap -- expected to
  ;; migrate to ctx-threaded slots once the substrate surface settles.
  ;;
  ;; Importable: `{init_effects} = import 'std/effects.fnk'`.

  (global $get_handler (mut (ref null $Closure))
    (ref.null $Closure))
  (global $set_handler (mut (ref null $Closure))
    (ref.null $Closure))

  (elem declare func $init_effects_apply)

  (func $init_effects_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref null any))
    (local $get_h (ref null any))
    (local $set_h (ref null any))
    (local $result_args (ref any))

    (local.set $cont (call $args_head (local.get $args)))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $get_h (call $args_head (local.get $args)))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $set_h (call $args_head (local.get $args)))

    (global.set $get_handler
      (ref.cast (ref null $Closure) (local.get $get_h)))
    (global.set $set_handler
      (ref.cast (ref null $Closure) (local.get $set_h)))

    ;; return ctx to cont (no meaningful return value, but args_prepend
    ;; rejects null heads; ctx is a convenient non-null stand-in)
    (local.set $result_args
      (call $args_prepend (local.get $ctx) (call $args_empty)))

    (return_call $apply_3
      (local.get $result_args)
      (local.get $ctx)
      (local.get $cont)))

  (global $init_effects_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $init_effects_apply)
      (ref.null $Captures)))

  (func $init_effects (@pub) (@impl "std/effects.fnk:init_effects") (result (ref any))
    (global.get $init_effects_closure))
)
