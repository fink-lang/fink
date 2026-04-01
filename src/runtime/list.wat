;; List — immutable cons-cell linked list for fink sequences
;;
;; WASM GC implementation using struct types.
;;
;; Design:
;;   - Classic cons cells: each node holds a head value and a tail ref
;;   - Empty list is null (no separate nil type needed)
;;   - Structural sharing: [a, ...rest] is O(1) — just take the tail
;;   - Prepend (cons) is O(1) — new cell pointing to existing list
;;
;; Type hierarchy (types.wat defines the opaque base type):
;;
;;   $List             ← opaque base (from types.wat)
;;   └── $Cons         ← internal: head + tail cons cell
;;
;; Value representation:
;;   - Head values are (ref any) — non-nullable
;;   - Tail is (ref null $Cons) — null means end of list
;;
;; Exported functions:
;;  TODO: Cons is internal, public interfaces should use List. Or these functions
;;     hould be made private _* .
;;   $list_empty   : () -> (ref null $Cons)
;;   $list_prepend : (ref any), (ref null $Cons) -> (ref $Cons)
;;   $list_head    : (ref $Cons) -> (ref any)
;;   $list_tail    : (ref $Cons) -> (ref null $Cons)
;;   $list_pop     : (ref $Cons) -> (ref any), (ref null $Cons)
;;                   Single call head+tail for [a, ...rest] = xs
;;   $list_size    : (ref null $Cons) -> i32
;;   $list_concat  : (ref null $Cons), (ref null $Cons) -> (ref null $Cons)
;;                   [..a, ..b] — walks a, rebuilds pointing to b. O(n).
;;   $list_get     : (ref null $Cons), i32 -> (ref null any)
;;                   Indexed access. Returns null if out of bounds.
;;   $list_find    : (ref null $Cons), (ref any) -> i32
;;                   Index of first element matching by ref.eq, or -1.
;;                   Will be extended to direct-style deep_eq supporting:
;;                   i31ref, $Num, $Str.
;;                   Finding by user-defined Eq will live in std-lib (CPS).
;;
;; CPS wrappers (compiler-facing):
;;   All params/results are (ref null any). Continuation dispatch via _croc_N.
;;
;;   $seq_prepend: (val, list, cont) -> _croc_1(new_list, cont)   [O(1) cons]
;;   $seq_concat : (list_a, list_b, cont) -> _croc_1(merged, cont)
;;   $seq_pop    : (cursor, fail, succ) -> if empty: _croc_0(fail)
;;                                         else: _croc_2(head, tail, succ)

(module

  ;; Continuation dispatch — provided by the compiler's emitted module.
  (import "@fink/user" "_croc_0" (func $croc_0 (param (ref null any))))
  (import "@fink/user" "_croc_1" (func $croc_1 (param (ref null any)) (param (ref null any))))
  (import "@fink/user" "_croc_2" (func $croc_2 (param (ref null any)) (param (ref null any)) (param (ref null any))))

  ;; -- Type definitions -----------------------------------------------

  ;; $Cons — a list cell, subtype of $List (from types.wat).
  ;; head is the value, tail is the rest of the list (null = end).
  (type $Cons (sub $List (struct
    (field $head (ref any))
    (field $tail (ref null $Cons))
  )))


  ;; -- Empty ----------------------------------------------------------

  ;; Empty list is just null.
  (func $list_empty (export "list_empty") (result (ref null $Cons))
    (ref.null $Cons)
  )

  ;; Predicate: is this list empty? (null = empty, $Cons = non-empty)
  (func $list_is_empty (export "list_is_empty")
    (param $val (ref null any)) (result i32)
    (ref.is_null (local.get $val))
  )


  ;; -- Cons -----------------------------------------------------------

  ;; Prepend a value to a list. O(1).
  (func $list_prepend (export "list_prepend")
    (param $head (ref any))
    (param $tail (ref null $Cons))
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

  ;; Get the rest of the list. Returns null for single-element list.
  (func $list_tail (export "list_tail")
    (param $list (ref $Cons))
    (result (ref null $Cons))

    (struct.get $Cons $tail (local.get $list))
  )


  ;; -- Pop ------------------------------------------------------------

  ;; Single call head+tail for destructuring:
  ;;   [a, ...rest] = xs  →  (a, rest) = list_pop(xs)
  ;;
  ;; Returns (head, tail) via multi-value. Traps on empty list.
  (func $list_pop (export "list_pop")
    (param $list (ref $Cons))
    (result (ref any) (ref null $Cons))

    (struct.get $Cons $head (local.get $list))
    (struct.get $Cons $tail (local.get $list))
  )


  ;; -- Size -----------------------------------------------------------

  ;; Count elements. O(n) walk.
  (func $list_size (export "list_size")
    (param $list (ref null $Cons))
    (result i32)

    (local $count i32)
    (local.set $count (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done (ref.is_null (local.get $list)))
        (local.set $count
          (i32.add (local.get $count) (i32.const 1)))
        (local.set $list
          (struct.get $Cons $tail
            (ref.cast (ref $Cons) (local.get $list))))
        (br $walk)))

    (local.get $count)
  )


  ;; -- Get ------------------------------------------------------------

  ;; Indexed access. O(n) walk to position.
  ;; Returns null if index is out of bounds or negative.
  (func $list_get (export "list_get")
    (param $list (ref null $Cons))
    (param $index i32)
    (result (ref null any))

    ;; negative index — out of bounds
    (if (i32.lt_s (local.get $index) (i32.const 0))
      (then (return (ref.null eq))))

    (block $not_found
      (loop $walk
        (br_if $not_found (ref.is_null (local.get $list)))
        (if (i32.eqz (local.get $index))
          (then
            (return
              (struct.get $Cons $head
                (ref.cast (ref $Cons) (local.get $list))))))
        (local.set $list
          (struct.get $Cons $tail
            (ref.cast (ref $Cons) (local.get $list))))
        (local.set $index
          (i32.sub (local.get $index) (i32.const 1)))
        (br $walk)))

    (ref.null eq)
  )


  ;; -- Concat ---------------------------------------------------------

  ;; [..a, ..b] — walks a, rebuilds cells pointing to b. O(len(a)).
  ;; If a is empty, returns b. If b is empty, returns a.
  (func $list_concat (export "list_concat")
    (param $a (ref null $Cons))
    (param $b (ref null $Cons))
    (result (ref null $Cons))

    ;; a is empty — return b
    (if (ref.is_null (local.get $a))
      (then (return (local.get $b))))

    ;; b is empty — return a (no copying needed)
    (if (ref.is_null (local.get $b))
      (then (return (local.get $a))))

    ;; Both non-empty — rebuild a's cells with b as the final tail.
    (call $_list_concat_inner
      (ref.cast (ref $Cons) (local.get $a))
      (local.get $b))
  )

  ;; Recursive helper: rebuild cons cells from src, ending with dest.
  (func $_list_concat_inner
    (param $src (ref $Cons))
    (param $dest (ref null $Cons))
    (result (ref null $Cons))

    (local $tail (ref null $Cons))

    (local.set $tail
      (struct.get $Cons $tail (local.get $src)))

    ;; src is the last cell — point it to dest
    (if (ref.is_null (local.get $tail))
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
    (param $list (ref null $Cons))
    (param $val (ref any))
    (result i32)

    (local $i i32)
    (local.set $i (i32.const 0))

    (block $not_found
      (loop $scan
        (br_if $not_found (ref.is_null (local.get $list)))
        (if (ref.eq
              (ref.cast (ref eq) (struct.get $Cons $head
                (ref.cast (ref $Cons) (local.get $list))))
              (ref.cast (ref eq) (local.get $val)))
          (then (return (local.get $i))))
        (local.set $list
          (struct.get $Cons $tail
            (ref.cast (ref $Cons) (local.get $list))))
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

    (return_call $croc_1
      (call $list_prepend
        (ref.cast (ref any) (local.get $val))
        (ref.cast (ref null $Cons) (local.get $list)))
      (local.get $cont))
  )

  ;; seq_concat(list_a, list_b, cont) — concatenate two lists, pass result to cont.
  (func $seq_concat (export "seq_concat")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))

    (return_call $croc_1
      (call $list_concat
        (ref.cast (ref null $Cons) (local.get $a))
        (ref.cast (ref null $Cons) (local.get $b)))
      (local.get $cont))
  )

  ;; seq_pop(cursor, fail, succ) — destructure [head, ..tail].
  ;; If cursor is null (empty list): tail-call fail continuation with 0 args.
  ;; If non-null: extract head + tail, tail-call succ continuation with 2 args.
  (func $seq_pop (export "seq_pop")
    (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))

    (local $cons (ref $Cons))

    (if (ref.is_null (local.get $cursor))
      (then (return_call $croc_0 (local.get $fail))))

    (local.set $cons (ref.cast (ref $Cons) (local.get $cursor)))

    (return_call $croc_2
      (struct.get $Cons $head (local.get $cons))
      (struct.get $Cons $tail (local.get $cons))
      (local.get $succ))
  )

)
