;; Closure dispatch — unified $Fn2 calling convention.
;;
;; All functions are $Fn2(captures, args). Conts are in captures or in
;; the args list (conts-first ordering ensures this after lifting).

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

  ;; Function signature for the unified calling convention.
  ;; $Fn2(captures, args) — all functions (conts are in captures or args).
  (type $Fn2 (@pub) (func (param (ref null any) (ref null any))))


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
  ;; continuation site.
  (func $apply (@pub)
    (param $args (ref null any))
    (param $callee (ref null any))

    (local $clos (ref $Closure))
    (local.set $clos (ref.cast (ref $Closure) (local.get $callee)))

    (return_call_ref $Fn2
      (struct.get $Closure $captures (local.get $clos))
      (local.get $args)
      (ref.cast (ref $Fn2) (struct.get $Closure $func (local.get $clos))))
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
  ;; $apply. Used by every CPS continuation site that returns N values
  ;; to its continuation.

  (func $apply_0 (@pub) (param $cont (ref null any))
    (return_call $apply (call $args_empty) (local.get $cont)))

  (func $apply_1 (@pub) (param $result (ref null any)) (param $cont (ref null any))
    (return_call $apply
      (call $args_prepend (ref.as_non_null (local.get $result)) (call $args_empty))
      (local.get $cont)))

  (func $apply_2_vals (@pub) (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply
      (call $args_prepend (ref.as_non_null (local.get $a))
        (call $args_prepend (ref.as_non_null (local.get $b)) (call $args_empty)))
      (local.get $cont)))


  ;; -- Thunks ----------------------------------------------------------
  ;;
  ;; A thunk is a zero-arg $Closure that, when applied, calls a saved
  ;; continuation with a saved value: thunk() = cont(value). Used by the
  ;; async scheduler (queued tasks) and by channel/host-cont resumption.

  ;; Declarative element segment — required by WASM spec for ref.func.
  (elem declare func $_thunk_fn)

  ;; Thunk body. Captures: [cont, value]. When applied: apply([value], cont).
  (func $_thunk_fn (type $Fn2) (param $caps (ref null any)) (param $args (ref null any))
    (local $captures (ref $Captures))
    (local $cont (ref any))
    (local $value (ref any))
    (local.set $captures (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $cont (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 0))))
    (local.set $value (ref.as_non_null (array.get $Captures (local.get $captures) (i32.const 1))))
    (return_call $apply
      (call $args_prepend (local.get $value) (call $args_empty))
      (local.get $cont))
  )

  (func $make_thunk (@pub) (param $cont (ref any)) (param $value (ref any)) (result (ref $Closure))
    (struct.new $Closure
      (ref.func $_thunk_fn)
      (array.new_fixed $Captures 2 (local.get $cont) (local.get $value)))
  )

  ;; Make a thunk that calls cont with unit (i31 0).
  (func $make_unit_thunk (@pub) (param $cont (ref any)) (result (ref $Closure))
    (call $make_thunk (local.get $cont) (ref.i31 (i32.const 0)))
  )
)
