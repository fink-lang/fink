;; Closure dispatch — unified $Fn2 calling convention.
;;
;; All functions are $Fn2(captures, args). Conts are in captures or in
;; the args list (conts-first ordering ensures this after lifting).

(module

  (import "std/list.wat" "List" (type $List (sub any)))


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

)
