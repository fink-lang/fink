;; Cons-cell linked list — immutable, O(1) prepend.
;;
;; Type hierarchy:
;;   $List        ← opaque public base type
;;   ├── $Nil     ← empty list (zero-size struct, private)
;;   └── $Cons    ← head + tail cons cell (private)
;;
;; $List is always non-null. Empty is $Nil, not null. This allows list
;; values to flow through (ref any) slots without null ambiguity (null
;; is reserved for Option/absence semantics).

(module

  ;; Type imports
  (import "rt/apply.wat" "Closure"  (type $Closure  (sub any)))
  (import "rt/apply.wat" "Captures" (type $Captures (sub any)))
  (import "rt/apply.wat" "Fn2"      (type $Fn2      (sub any)))

  ;; Func imports
  (import "rt/apply.wat" "apply"
    (func $_apply (param $args (ref null any)) (param $callee (ref null any))))


  ;; -- Type definitions ------------------------------------------------

  ;; $List — opaque public base type.
  (type $List (@pub) (sub (struct)))

  ;; $Nil — empty list, subtype of $List. Private.
  (type $Nil (sub $List (struct)))

  ;; $Cons — a list cell, subtype of $List. Private.
  ;; head is the value, tail is the rest of the list (always non-null).
  (type $Cons (sub $List (struct
    (field $head (ref any))
    (field $tail (ref $List))
  )))


  ;; -- Apply helpers ---------------------------------------------------
  ;;
  ;; TODO: apply_0/apply_1/apply_2_vals wrap N values into an args list
  ;; and tail-call _apply — they are apply concerns, not list concerns.
  ;; Move to rt/apply.wat once list public API is stable.

  (func $apply_0 (@pub) (param $cont (ref null any))
    (return_call $_apply (struct.new $Nil) (local.get $cont)))

  (func $apply_1 (@pub) (param $result (ref null any)) (param $cont (ref null any))
    (return_call $_apply
      (struct.new $Cons (ref.as_non_null (local.get $result)) (struct.new $Nil))
      (local.get $cont)))

  (func $apply_2_vals (@pub) (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $_apply
      (struct.new $Cons (ref.as_non_null (local.get $a))
        (struct.new $Cons (ref.as_non_null (local.get $b)) (struct.new $Nil)))
      (local.get $cont)))


  ;; -- Construction / empty -------------------------------------------

  ;; empty — singleton-style empty list. Cross-wat callers use this
  ;; instead of poking the private $Nil type directly.
  (func $empty (@pub) (@impl "std/fn.fnk:args_empty") (result (ref $List))
    (struct.new $Nil))

  ;; Prepend a value to a list. O(1).
  (func $prepend (@pub) (@impl "std/fn.fnk:args_prepend")
    (param $head (ref any))
    (param $tail (ref $List))
    (result (ref $List))
    (struct.new $Cons (local.get $head) (local.get $tail)))


  ;; -- Predicates ------------------------------------------------------

  ;; Is this list empty?
  (func $is_empty (@pub)
    (param $list (ref $List))
    (result i32)
    (ref.test (ref $Nil) (local.get $list)))

  ;; Polymorphic op_empty impl: dispatched from the operators protocol.
  (func $op_empty (@impl "std/operators.fnk:op_empty" $List)
    (param $val (ref null any)) (result i32)
    (ref.test (ref $Nil) (local.get $val)))


  ;; -- Head / Tail -----------------------------------------------------

  ;; Get the first element. Traps on empty list.
  (func $head (@pub)
    (param $list (ref $List))
    (result (ref any))
    (struct.get $Cons $head (ref.cast (ref $Cons) (local.get $list))))

  ;; Get the rest of the list. Traps on empty list.
  (func $tail (@pub)
    (param $list (ref $List))
    (result (ref $List))
    (struct.get $Cons $tail (ref.cast (ref $Cons) (local.get $list))))

  ;; Args-protocol primitives (calling convention) — take/return
  ;; (ref null any), cast internally. Used by emitter where args lists
  ;; flow through anyref slots.
  (func $head_any (@pub) (@impl "std/fn.fnk:args_head")
    (param $list (ref null any))
    (result (ref null any))
    (struct.get $Cons $head (ref.cast (ref $Cons) (local.get $list))))

  (func $tail_any (@pub) (@impl "std/fn.fnk:args_tail")
    (param $list (ref null any))
    (result (ref null any))
    (struct.get $Cons $tail (ref.cast (ref $Cons) (local.get $list))))


  ;; -- Pop -------------------------------------------------------------

  ;; Single call head+tail for destructuring:
  ;;   [a, ...rest] = xs  →  (a, rest) = list_pop(xs)
  ;; Returns (head, tail) via multi-value. Traps on empty list.
  (func $pop (@pub)
    (param $list (ref $List))
    (result (ref any) (ref $List))
    (struct.get $Cons $head (ref.cast (ref $Cons) (local.get $list)))
    (struct.get $Cons $tail (ref.cast (ref $Cons) (local.get $list))))


  ;; -- Size ------------------------------------------------------------

  ;; Count elements. O(n) walk.
  (func $size (@pub) (@impl "std/seq.fnk:size" $List)
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

    (local.get $count))


  ;; -- Get -------------------------------------------------------------

  ;; Indexed access. O(n) walk to position.
  ;; Returns null if index is out of bounds or negative.
  (func $get (@pub)
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

    (ref.null eq))


  ;; -- Concat ----------------------------------------------------------

  ;; [..a, ..b] — walks a, rebuilds cells pointing to b. O(len(a)).
  ;; If a is empty, returns b. If b is empty, returns a.
  (func $concat (@pub) (@impl "std/fn.fnk:args_concat")
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
    (call $_concat_inner
      (ref.cast (ref $Cons) (local.get $a))
      (local.get $b)))

  ;; Recursive helper: rebuild cons cells from src, ending with dest.
  (func $_concat_inner
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
      (call $_concat_inner
        (ref.cast (ref $Cons) (local.get $tail))
        (local.get $dest))))


  ;; -- Find ------------------------------------------------------------

  ;; Index of first element matching val by ref.eq. Returns -1 if absent.
  ;; O(n) scan.
  (func $find (@pub)
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

    (i32.const -1))


  ;; =====================================================================
  ;; CPS wrappers — compiler-facing protocol impls
  ;; =====================================================================

  ;; seq_prepend(val, list, cont) — prepend val to front of list, pass result to cont.
  ;; O(1) — single cons cell allocation.
  (func $seq_prepend (@pub) (@impl "std/seq.fnk:prepend" $List)
    (param $val (ref null any)) (param $list (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (call $prepend
        (ref.cast (ref any) (local.get $val))
        (ref.cast (ref $List) (local.get $list)))
      (local.get $cont)))

  ;; seq_concat(list_a, list_b, cont) — concatenate two lists, pass result to cont.
  (func $seq_concat (@impl "std/seq.fnk:concat" $List)
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (call $concat
        (ref.cast (ref $List) (local.get $a))
        (ref.cast (ref $List) (local.get $b)))
      (local.get $cont)))

  ;; seq_pop(cursor, fail, succ) — destructure [head, ..tail].
  ;; If empty: tail-call fail with 0 args.
  ;; If non-empty: extract head + tail, tail-call succ with 2 args.
  (func $seq_pop (@impl "std/seq.fnk:pop" $List)
    (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))

    (local $cons (ref $Cons))

    (if (ref.test (ref $Nil) (local.get $cursor))
      (then (return_call $apply_0 (local.get $fail))))

    (local.set $cons (ref.cast (ref $Cons) (local.get $cursor)))

    (return_call $apply_2_vals
      (struct.get $Cons $head (local.get $cons))
      (struct.get $Cons $tail (local.get $cons))
      (local.get $succ)))


  ;; -- Pop-back -------------------------------------------------------

  ;; pop_back direct-style: peel one element off the END of a list.
  ;; Returns (init, last) via multi-value. Traps on empty list.
  ;; O(n) — rebuilds the spine.
  (func $pop_back (@pub)
    (param $list (ref $List))
    (result (ref $List) (ref any))

    (local $tail (ref $List))
    (local $rest_init (ref $List))
    (local $rest_last (ref any))
    (local $cons (ref $Cons))

    (local.set $cons (ref.cast (ref $Cons) (local.get $list)))
    (local.set $tail
      (struct.get $Cons $tail (local.get $cons)))

    ;; If $list is the last cell, init is empty and we are done.
    (if (ref.test (ref $Nil) (local.get $tail))
      (then
        (return
          (struct.new $Nil)
          (struct.get $Cons $head (local.get $cons)))))

    ;; Recurse on the tail; prepend head onto the resulting init.
    (call $pop_back (local.get $tail))
    (local.set $rest_last)
    (local.set $rest_init)

    (struct.new $Cons
      (struct.get $Cons $head (local.get $cons))
      (local.get $rest_init))
    (local.get $rest_last))

  ;; seq_pop_back(cursor, fail, succ) — destructure [..init, last].
  (func $seq_pop_back (@impl "std/seq.fnk:pop_back" $List)
    (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))

    (local $init (ref $List))
    (local $last (ref any))

    (if (ref.test (ref $Nil) (local.get $cursor))
      (then (return_call $apply_0 (local.get $fail))))

    (call $pop_back (ref.cast (ref $List) (local.get $cursor)))
    (local.set $last)
    (local.set $init)

    (return_call $apply_2_vals
      (local.get $init)
      (local.get $last)
      (local.get $succ)))


  ;; -- std/list.fnk:list — user-importable constructor closure ------------

  (elem declare func $_list_apply)

  (func $_list_apply (type $Fn2)
    (param $_caps (ref null any))
    (param $args (ref null any))

    (local $cont (ref null any))
    (local $rest (ref $List))

    ;; Peel cont off args[0]. Remainder is the user's list.
    (local.set $cont (call $head_any (local.get $args)))
    (local.set $rest
      (ref.cast (ref $List) (call $tail_any (local.get $args))))

    ;; Tail-call cont with [list].
    (return_call $apply_1
      (local.get $rest)
      (local.get $cont)))

  (global $_list_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $_list_apply)
      (ref.null $Captures)))

  (func $list (@pub) (@impl "std/list.fnk:list") (result (ref any))
    (global.get $_list_closure))

)
