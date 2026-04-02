;; Cons-cell linked list — immutable, O(1) prepend.
;;
;; Type hierarchy:
;;   $List        ← opaque base (from types.wat)
;;   ├── $Nil     ← empty list (zero-size struct)
;;   └── $Cons    ← head + tail cons cell
;;
;; $List is always non-null. Empty is $Nil, not null.
;; This allows list values to flow through (ref any) slots without
;; null ambiguity (null is reserved for Option/absence semantics).
;;
;; Direct-style API (used by other runtime modules + dispatch):
;;   $list_nil     : () -> (ref $Nil)
;;   $list_prepend : (ref any), (ref $List) -> (ref $Cons)
;;   $list_head    : (ref $Cons) -> (ref any)
;;   $list_tail    : (ref $Cons) -> (ref $List)
;;   $list_pop     : (ref $Cons) -> (ref any), (ref $List)
;;   $list_op_empty: (ref $List) -> i32
;;   $list_size    : (ref $List) -> i32
;;   $list_concat  : (ref $List), (ref $List) -> (ref $List)
;;
;; CPS wrappers (compiler-facing):
;;   All params/results are (ref null any). Continuation dispatch via _croc.
;;
;;   $seq_prepend: (val, list, cont) -> _croc([new_list], cont)   [O(1) cons]
;;   $seq_concat : (list_a, list_b, cont) -> _croc([merged], cont)
;;   $seq_pop    : (cursor, fail, succ) -> if empty: _croc([], fail)
;;                                         else: _croc([head, tail], succ)

(module

  ;; Helpers: wrap 0/1/2 results into a list and dispatch via $_apply
  ;; (defined in dispatch.wat — all runtime WATs are merged into one module).
  ;; These dispatch to continuations (no cont param → _apply_2).
  (func $apply_0 (param $cont (ref null any))
    (return_call $_apply (struct.new $Nil) (local.get $cont)))
  (func $apply_1 (param $result (ref null any)) (param $cont (ref null any))
    (return_call $_apply
      (struct.new $Cons (ref.as_non_null (local.get $result)) (struct.new $Nil))
      (local.get $cont)))
  (func $apply_2_vals (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $_apply
      (struct.new $Cons (ref.as_non_null (local.get $a))
        (struct.new $Cons (ref.as_non_null (local.get $b)) (struct.new $Nil)))
      (local.get $cont)))

  ;; -- Type definitions -----------------------------------------------

  ;; $Nil — empty list, subtype of $List.
  (type $Nil (sub $List (struct)))

  ;; $Cons — a list cell, subtype of $List.
  ;; head is the value, tail is the rest of the list (always non-null).
  (type $Cons (sub $List (struct
    (field $head (ref any))
    (field $tail (ref $List))
  )))


  ;; -- Empty ----------------------------------------------------------

  ;; Create an empty list.
  (func $list_nil (export "list_nil") (result (ref $Nil))
    (struct.new $Nil)
  )

  ;; Predicate: is this list empty?
  (func $list_op_empty (export "list_op_empty")
    (param $val (ref null any)) (result i32)
    (ref.test (ref $Nil) (local.get $val))
  )


  ;; -- Cons -----------------------------------------------------------

  ;; Prepend a value to a list. O(1).
  (func $list_prepend (export "list_prepend")
    (param $head (ref any))
    (param $tail (ref $List))
    (result (ref $Cons))

    (struct.new $Cons (local.get $head) (local.get $tail))
  )


  ;; -- Head / Tail ----------------------------------------------------

  ;; Get the first element. Traps on empty list.
  (func $list_head (export "list_head")
    (param $list (ref $Cons))
    (result (ref any))

    (struct.get $Cons $head (local.get $list))
  )

  ;; Unboxed wrappers for the emitter — take/return (ref null any), cast internally.
  ;; The emitter doesn't know about $Cons/$Nil, so these bridge the type gap.
  (func $list_head_any (export "list_head_any")
    (param $list (ref null any))
    (result (ref null any))

    (struct.get $Cons $head (ref.cast (ref $Cons) (local.get $list)))
  )

  (func $list_tail_any (export "list_tail_any")
    (param $list (ref null any))
    (result (ref null any))

    (struct.get $Cons $tail (ref.cast (ref $Cons) (local.get $list)))
  )

  (func $list_prepend_any (export "list_prepend_any")
    (param $head (ref null any))
    (param $tail (ref null any))
    (result (ref null any))

    (struct.new $Cons
      (ref.as_non_null (local.get $head))
      (ref.cast (ref $List) (local.get $tail)))
  )

  (func $list_concat_any (export "list_concat_any")
    (param $a (ref null any))
    (param $b (ref null any))
    (result (ref null any))

    (call $list_concat
      (ref.cast (ref $List) (local.get $a))
      (ref.cast (ref $List) (local.get $b)))
  )

  ;; Get the rest of the list.
  (func $list_tail (export "list_tail")
    (param $list (ref $Cons))
    (result (ref $List))

    (struct.get $Cons $tail (local.get $list))
  )


  ;; -- Pop ------------------------------------------------------------

  ;; Single call head+tail for destructuring:
  ;;   [a, ...rest] = xs  →  (a, rest) = list_pop(xs)
  ;;
  ;; Returns (head, tail) via multi-value. Traps on empty list.
  (func $list_pop (export "list_pop")
    (param $list (ref $Cons))
    (result (ref any) (ref $List))

    (struct.get $Cons $head (local.get $list))
    (struct.get $Cons $tail (local.get $list))
  )


  ;; -- Size -----------------------------------------------------------

  ;; Count elements. O(n) walk.
  (func $list_size (export "list_size")
    (param $list (ref $List))
    (result i32)

    (local $count i32)
    (local $cur (ref $List))
    (local.set $count (i32.const 0))
    (local.set $cur (local.get $list))

    (block $done
      (loop $walk
        (br_if $done (ref.test (ref $Nil) (local.get $cur)))
        (local.set $count
          (i32.add (local.get $count) (i32.const 1)))
        (local.set $cur
          (struct.get $Cons $tail
            (ref.cast (ref $Cons) (local.get $cur))))
        (br $walk)))

    (local.get $count)
  )


  ;; -- Get ------------------------------------------------------------

  ;; Indexed access. O(n) walk to position.
  ;; Returns null if index is out of bounds or negative.
  (func $list_get (export "list_get")
    (param $list (ref $List))
    (param $index i32)
    (result (ref null any))

    (local $cur (ref $List))

    ;; negative index — out of bounds
    (if (i32.lt_s (local.get $index) (i32.const 0))
      (then (return (ref.null eq))))

    (local.set $cur (local.get $list))

    (block $not_found
      (loop $walk
        (br_if $not_found (ref.test (ref $Nil) (local.get $cur)))
        (if (i32.eqz (local.get $index))
          (then
            (return
              (struct.get $Cons $head
                (ref.cast (ref $Cons) (local.get $cur))))))
        (local.set $cur
          (struct.get $Cons $tail
            (ref.cast (ref $Cons) (local.get $cur))))
        (local.set $index
          (i32.sub (local.get $index) (i32.const 1)))
        (br $walk)))

    (ref.null eq)
  )


  ;; -- Concat ---------------------------------------------------------

  ;; [..a, ..b] — walks a, rebuilds cells pointing to b. O(len(a)).
  ;; If a is empty, returns b. If b is empty, returns a.
  (func $list_concat (export "list_concat")
    (param $a (ref $List))
    (param $b (ref $List))
    (result (ref $List))

    ;; a is empty — return b
    (if (ref.test (ref $Nil) (local.get $a))
      (then (return (local.get $b))))

    ;; b is empty — return a (no copying needed)
    (if (ref.test (ref $Nil) (local.get $b))
      (then (return (local.get $a))))

    ;; Both non-empty — rebuild a's cells with b as the final tail.
    (call $_list_concat_inner
      (ref.cast (ref $Cons) (local.get $a))
      (local.get $b))
  )

  ;; Recursive helper: rebuild cons cells from src, ending with dest.
  (func $_list_concat_inner
    (param $src (ref $Cons))
    (param $dest (ref $List))
    (result (ref $List))

    (local $tail (ref $List))

    (local.set $tail
      (struct.get $Cons $tail (local.get $src)))

    ;; src is the last cell — point it to dest
    (if (ref.test (ref $Nil) (local.get $tail))
      (then
        (return
          (struct.new $Cons
            (struct.get $Cons $head (local.get $src))
            (local.get $dest)))))

    ;; recurse on tail, then wrap with current head
    (struct.new $Cons
      (struct.get $Cons $head (local.get $src))
      (call $_list_concat_inner
        (ref.cast (ref $Cons) (local.get $tail))
        (local.get $dest)))
  )


  ;; -- Find -----------------------------------------------------------

  ;; Index of first element matching val by ref.eq. Returns -1 if absent.
  ;; O(n) scan.
  (func $list_find (export "list_find")
    (param $list (ref $List))
    (param $val (ref any))
    (result i32)

    (local $i i32)
    (local $cur (ref $List))
    (local.set $i (i32.const 0))
    (local.set $cur (local.get $list))

    (block $not_found
      (loop $scan
        (br_if $not_found (ref.test (ref $Nil) (local.get $cur)))
        (if (ref.eq
              (ref.cast (ref eq) (struct.get $Cons $head
                (ref.cast (ref $Cons) (local.get $cur))))
              (ref.cast (ref eq) (local.get $val)))
          (then (return (local.get $i))))
        (local.set $cur
          (struct.get $Cons $tail
            (ref.cast (ref $Cons) (local.get $cur))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $scan)))

    (i32.const -1)
  )


  ;; =========================================================================
  ;; CPS wrappers — compiler-facing interface
  ;; =========================================================================
  ;;
  ;; All params are (ref null any). Direct-style functions above do the real
  ;; work; these wrappers box/unbox and dispatch through continuations.

  ;; seq_prepend(val, list, cont) — prepend val to front of list, pass result to cont.
  ;; O(1) — single cons cell allocation.
  (func $seq_prepend (export "seq_prepend")
    (param $val (ref null any)) (param $list (ref null any)) (param $cont (ref null any))

    (return_call $apply_1
      (call $list_prepend
        (ref.cast (ref any) (local.get $val))
        (ref.cast (ref $List) (local.get $list)))
      (local.get $cont))
  )

  ;; seq_concat(list_a, list_b, cont) — concatenate two lists, pass result to cont.
  (func $seq_concat (export "seq_concat")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))

    (return_call $apply_1
      (call $list_concat
        (ref.cast (ref $List) (local.get $a))
        (ref.cast (ref $List) (local.get $b)))
      (local.get $cont))
  )

  ;; seq_pop(cursor, fail, succ) — destructure [head, ..tail].
  ;; If empty ($Nil): tail-call fail continuation with 0 args.
  ;; If non-empty ($Cons): extract head + tail, tail-call succ with 2 args.
  (func $seq_pop (export "seq_pop")
    (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))

    (local $cons (ref $Cons))

    (if (ref.test (ref $Nil) (local.get $cursor))
      (then (return_call $apply_0 (local.get $fail))))

    (local.set $cons (ref.cast (ref $Cons) (local.get $cursor)))

    (return_call $apply_2_vals
      (struct.get $Cons $head (local.get $cons))
      (struct.get $Cons $tail (local.get $cons))
      (local.get $succ))
  )

)
