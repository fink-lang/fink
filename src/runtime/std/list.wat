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
;;   $std/list.wat:nil     : () -> (ref $Nil)
;;   $std/list.wat:prepend : (ref any), (ref $List) -> (ref $Cons)
;;   $std/list.wat:head    : (ref $Cons) -> (ref any)
;;   $std/list.wat:tail    : (ref $Cons) -> (ref $List)
;;   $std/list.wat:pop     : (ref $Cons) -> (ref any), (ref $List)
;;   $std/list.wat:op_empty: (ref $List) -> i32
;;   $std/list.wat:size    : (ref $List) -> i32
;;   $std/list.wat:concat  : (ref $List), (ref $List) -> (ref $List)
;;
;; CPS wrappers (compiler-facing):
;;   All params/results are (ref null any). Continuation dispatch via _apply.
;;
;;   $std/list.wat:seq_prepend: (val, list, cont) -> _apply([new_list], cont)   [O(1) cons]
;;   $std/list.wat:seq_concat : (list_a, list_b, cont) -> _apply([merged], cont)
;;   $std/list.wat:seq_pop    : (cursor, fail, succ) -> if empty: _apply([], fail)
;;                                         else: _apply([head, tail], succ)

(module

  ;; Helpers: wrap 0/1/2 results into a list and dispatch via $_apply
  ;; (defined in dispatch.wat — all runtime WATs are merged into one module).
  ;; These dispatch to continuations (no cont param → _apply_2).
  (func $std/list.wat:apply_0 (param $cont (ref null any))
    (return_call $rt/apply.wat:apply (struct.new $Nil) (local.get $cont)))
  (func $std/list.wat:apply_1 (param $result (ref null any)) (param $cont (ref null any))
    (return_call $rt/apply.wat:apply
      (struct.new $Cons (ref.as_non_null (local.get $result)) (struct.new $Nil))
      (local.get $cont)))
  (func $std/list.wat:apply_2_vals (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $rt/apply.wat:apply
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

  ;; Create an empty list — args-list constructor (calling convention).
  ;; Semantic home: rt/apply.wat as args_empty. Lives here for now
  ;; because the args-list realization is cons-cells; to be moved.
  (func $std/list.wat:nil (export "std/fn.fnk:args_empty") (result (ref $Nil))
    (struct.new $Nil)
  )

  ;; Predicate: is this list empty? ($List impl of op_empty protocol.)
  (func $std/list.wat:op_empty (export "std/list.wat:op_empty")
    (param $val (ref null any)) (result i32)
    (ref.test (ref $Nil) (local.get $val))
  )


  ;; -- Cons -----------------------------------------------------------

  ;; Prepend a value to a list. O(1). Typed-internal.
  (func $std/list.wat:prepend
    (param $head (ref any))
    (param $tail (ref $List))
    (result (ref $Cons))

    (struct.new $Cons (local.get $head) (local.get $tail))
  )


  ;; -- Head / Tail ----------------------------------------------------

  ;; Get the first element. Traps on empty list. Typed-internal.
  (func $std/list.wat:head
    (param $list (ref $Cons))
    (result (ref any))

    (struct.get $Cons $head (local.get $list))
  )

  ;; Args-protocol primitives (calling convention) — unboxed, take/return
  ;; (ref null any), cast internally. Semantic home: rt/apply.wat.
  (func $std/list.wat:head_any (export "std/fn.fnk:args_head")
    (param $list (ref null any))
    (result (ref null any))

    (struct.get $Cons $head (ref.cast (ref $Cons) (local.get $list)))
  )

  (func $std/list.wat:tail_any (export "std/fn.fnk:args_tail")
    (param $list (ref null any))
    (result (ref null any))

    (struct.get $Cons $tail (ref.cast (ref $Cons) (local.get $list)))
  )

  (func $std/list.wat:prepend_any (export "std/fn.fnk:args_prepend")
    (param $head (ref null any))
    (param $tail (ref null any))
    (result (ref null any))

    (struct.new $Cons
      (ref.as_non_null (local.get $head))
      (ref.cast (ref $List) (local.get $tail)))
  )

  (func $std/list.wat:concat_any (export "std/fn.fnk:args_concat")
    (param $a (ref null any))
    (param $b (ref null any))
    (result (ref null any))

    (call $std/list.wat:concat
      (ref.cast (ref $List) (local.get $a))
      (ref.cast (ref $List) (local.get $b)))
  )

  ;; Get the rest of the list. Typed-internal.
  (func $std/list.wat:tail
    (param $list (ref $Cons))
    (result (ref $List))

    (struct.get $Cons $tail (local.get $list))
  )


  ;; -- Pop ------------------------------------------------------------

  ;; Single call head+tail for destructuring:
  ;;   [a, ...rest] = xs  →  (a, rest) = list_pop(xs)
  ;;
  ;; Returns (head, tail) via multi-value. Traps on empty list.
  (func $std/list.wat:pop
    (param $list (ref $Cons))
    (result (ref any) (ref $List))

    (struct.get $Cons $head (local.get $list))
    (struct.get $Cons $tail (local.get $list))
  )


  ;; -- Size -----------------------------------------------------------

  ;; Count elements. O(n) walk.
  (func $std/list.wat:size
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
  (func $std/list.wat:get
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
  (func $std/list.wat:concat
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
    (call $std/list.wat:_list_concat_inner
      (ref.cast (ref $Cons) (local.get $a))
      (local.get $b))
  )

  ;; Recursive helper: rebuild cons cells from src, ending with dest.
  (func $std/list.wat:_list_concat_inner
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
      (call $std/list.wat:_list_concat_inner
        (ref.cast (ref $Cons) (local.get $tail))
        (local.get $dest)))
  )


  ;; -- Find -----------------------------------------------------------

  ;; Index of first element matching val by ref.eq. Returns -1 if absent.
  ;; O(n) scan.
  (func $std/list.wat:find
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
  (func $std/list.wat:seq_prepend (export "std/seq.fnk:prepend")
    (param $val (ref null any)) (param $list (ref null any)) (param $cont (ref null any))

    (return_call $std/list.wat:apply_1
      (call $std/list.wat:prepend
        (ref.cast (ref any) (local.get $val))
        (ref.cast (ref $List) (local.get $list)))
      (local.get $cont))
  )

  ;; seq_concat(list_a, list_b, cont) — concatenate two lists, pass result to cont.
  (func $std/list.wat:seq_concat (export "std/seq.fnk:concat")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))

    (return_call $std/list.wat:apply_1
      (call $std/list.wat:concat
        (ref.cast (ref $List) (local.get $a))
        (ref.cast (ref $List) (local.get $b)))
      (local.get $cont))
  )

  ;; seq_pop(cursor, fail, succ) — destructure [head, ..tail].
  ;; If empty ($Nil): tail-call fail continuation with 0 args.
  ;; If non-empty ($Cons): extract head + tail, tail-call succ with 2 args.
  (func $std/list.wat:seq_pop (export "std/list.wat:seq_pop")
    (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))

    (local $cons (ref $Cons))

    (if (ref.test (ref $Nil) (local.get $cursor))
      (then (return_call $std/list.wat:apply_0 (local.get $fail))))

    (local.set $cons (ref.cast (ref $Cons) (local.get $cursor)))

    (return_call $std/list.wat:apply_2_vals
      (struct.get $Cons $head (local.get $cons))
      (struct.get $Cons $tail (local.get $cons))
      (local.get $succ))
  )


  ;; -- Pop-back -------------------------------------------------------

  ;; pop_back direct-style: peel one element off the END of a list.
  ;; Returns (init, last) via multi-value. Traps on empty list.
  ;; init is a freshly built list of length n-1 (no spine sharing — cons
  ;; cells are forward-linked). last is the original last head.
  ;; O(n).
  (func $std/list.wat:pop_back
    (param $list (ref $Cons))
    (result (ref $List) (ref any))

    (local $tail (ref $List))
    (local $rest_init (ref $List))
    (local $rest_last (ref any))

    (local.set $tail
      (struct.get $Cons $tail (local.get $list)))

    ;; If $list is the last cell, init is empty and we are done.
    (if (ref.test (ref $Nil) (local.get $tail))
      (then
        (return
          (struct.new $Nil)
          (struct.get $Cons $head (local.get $list)))))

    ;; Recurse on the tail; prepend head onto the resulting init.
    (call $std/list.wat:pop_back
      (ref.cast (ref $Cons) (local.get $tail)))
    (local.set $rest_last)
    (local.set $rest_init)

    (struct.new $Cons
      (struct.get $Cons $head (local.get $list))
      (local.get $rest_init))
    (local.get $rest_last)
  )

  ;; seq_pop_back(cursor, fail, succ) — destructure [..init, last].
  ;; If empty ($Nil): tail-call fail continuation with 0 args.
  ;; If non-empty ($Cons): walk to end, extract (init, last), tail-call
  ;; succ with 2 args. O(n).
  (func $std/list.wat:seq_pop_back (export "std/list.wat:seq_pop_back")
    (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))

    (local $init (ref $List))
    (local $last (ref any))

    (if (ref.test (ref $Nil) (local.get $cursor))
      (then (return_call $std/list.wat:apply_0 (local.get $fail))))

    (call $std/list.wat:pop_back
      (ref.cast (ref $Cons) (local.get $cursor)))
    (local.set $last)
    (local.set $init)

    (return_call $std/list.wat:apply_2_vals
      (local.get $init)
      (local.get $last)
      (local.get $succ))
  )


  ;; -- std/list.fnk:list — user-importable constructor closure ----------------
  ;;
  ;; `{list} = import 'std/list.fnk'` resolves to the accessor below; the
  ;; user's `list 1, 2, 3` then calls the closure via `_apply` with
  ;; args = [cont, 1, 2, 3]. The Fn2 adapter peels cont off the head;
  ;; the remaining args list is itself a $List so we hand it back to
  ;; the cont directly via apply_1 (no walking needed).
  ;;
  ;; Same shape as $std/set.wat:_set_apply, but trivial because the
  ;; runtime args list and the user-facing $List type coincide.

  (elem declare func $std/list.wat:_list_apply)

  (func $std/list.wat:_list_apply (type $Fn2)
    (param $_caps (ref null any))
    (param $args (ref null any))

    (local $cont (ref null any))
    (local $rest (ref $List))

    ;; Peel cont off args[0]. Remainder is the user's list.
    (local.set $cont (call $std/list.wat:head_any (local.get $args)))
    (local.set $rest
      (ref.cast (ref $List) (call $std/list.wat:tail_any (local.get $args))))

    ;; Tail-call cont with [list].
    (return_call $std/list.wat:apply_1
      (local.get $rest)
      (local.get $cont))
  )

  (global $std/list.wat:_list_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $std/list.wat:_list_apply)
      (ref.null $Captures)))

  (func (export "std/list.fnk:list") (result (ref any))
    (global.get $std/list.wat:_list_closure))

)
