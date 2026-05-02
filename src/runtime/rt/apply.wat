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


  ;; $Fn_host_wrapper — host-facing per-module wrapper signature.
  ;; (key_bytes: ref null any, cont_id: i32) -> ()
  ;; Each fragment's lower-synthesised wrapper export has this type;
  ;; declared once here so all modules share it instead of emitting a
  ;; per-fragment local copy.
  (type $Fn_host_wrapper (@pub) (func (param (ref null any)) (param i32)))


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

  (func $args_empty (@pub) (result (ref $List))
    (return_call $list_empty))

  (func $args_prepend (@pub)
    (param $head (ref any)) (param $tail (ref $List))
    (result (ref $List))
    (return_call $list_prepend (local.get $head) (local.get $tail)))

  (func $args_concat (@pub)
    (param $a (ref $List)) (param $b (ref $List))
    (result (ref $List))
    (return_call $list_concat (local.get $a) (local.get $b)))


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
)
