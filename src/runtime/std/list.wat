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
  (import "rt/apply.wat" "Fn3"      (type $Fn3      (sub any)))
  (import "std/str.wat"  "Str"      (type $Str      (sub any) (struct)))
  (import "std/str.wat"  "ByteArray" (type $ByteArray (array (mut i8))))

  ;; Func imports
  (import "rt/apply.wat" "apply_0" (func $apply_0 (;apply-ctx;) (param (ref null any)) (param $cont (ref null any))))
  (import "rt/apply.wat" "apply_1" (func $apply_1 (;apply-ctx;) (param (ref null any)) (param $result (ref null any)) (param $cont (ref null any))))
  (import "rt/apply.wat" "apply_2_vals" (func $apply_2_vals (;apply-ctx;) (param (ref null any)) (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))))
  (import "std/str.wat" "from_bytes"
    (func $str_from_bytes (param (ref $ByteArray)) (result (ref $Str))))
  (import "std/str.wat" "_str_len"
    (func $_str_len (param (ref $Str)) (result i32)))
  (import "std/str.wat" "_str_copy_to"
    (func $_str_copy_to (param (ref $Str)) (param (ref $ByteArray)) (param i32) (result i32)))
  (import "std/str.wat" "_str_from_ascii_2"
    (func $_str_from_ascii_2 (param i32) (param i32) (result (ref $Str))))
  ;; repr_val — element formatter (per-type repr protocol dispatcher).
  (import "std/repr.wat" "repr_val"
    (func $repr_val (param (ref any)) (result (ref $Str))))

  ;; deep_eq — structural element comparison for list equality.
  (import "rt/protocols.wat" "deep_eq"
    (func $deep_eq (param (ref eq)) (param (ref eq)) (result i32)))

  ;; Range / I64 imports for $op_dot indexing & slicing.
  (import "std/int.wat"   "I64"      (type $I64   (sub any) (struct (field $ival i64))))
  (import "std/range.wat" "Range"    (type $Range (sub any)))
  (import "std/range.wat" "start"    (func $range_start   (param (ref $Range)) (result (ref $I64))))
  (import "std/range.wat" "end"      (func $range_end     (param (ref $Range)) (result (ref null $I64))))
  (import "std/range.wat" "is_incl"  (func $range_is_incl (param (ref $Range)) (result i32)))


  ;; -- Type definitions ------------------------------------------------

  ;; $List — opaque public base type.
  (type $List (@pub) (sub (struct)))

  ;; $Nil — empty list, subtype of $List.
  ;; @pub so rt/apply.wat can construct it (args calling convention).
  (type $Nil (@pub) (sub $List (struct)))

  ;; $Cons — a list cell, subtype of $List.
  ;; head is the value, tail is the rest of the list (always non-null).
  ;; @pub so rt/apply.wat can construct it (args calling convention).
  (type $Cons (@pub) (sub $List (struct
    (field $head (ref any))
    (field $tail (ref $List))
  )))


  ;; -- Construction / empty -------------------------------------------

  ;; empty — singleton-style empty list. Cross-wat callers use this
  ;; instead of poking the private $Nil type directly.
  (func $empty (@pub) (result (ref $List))
    (struct.new $Nil))

  ;; Prepend a value to a list. O(1).
  (func $prepend (@pub)
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


  ;; -- Structural equality --------------------------------------------
  ;;
  ;; Two lists are equal iff they have the same length and equal elements
  ;; in the same positions (ordered, unlike records). Walks both lists in
  ;; lockstep; elements compared through deep_eq, so nesting is recursive.
  ;; Direct-style — used by the == operator's $List arm and by deep_eq for
  ;; structural list keys/values.
  (func $list_deep_eq (@pub)
    (param $a (ref $List)) (param $b (ref $List)) (result i32)
    (local $ca (ref $Cons))
    (local $cb (ref $Cons))

    (block $done
      (loop $walk
        ;; both empty -> equal
        (if (ref.test (ref $Nil) (local.get $a))
          (then (return (ref.test (ref $Nil) (local.get $b)))))
        ;; a non-empty, b empty -> unequal
        (if (ref.test (ref $Nil) (local.get $b))
          (then (return (i32.const 0))))

        (local.set $ca (ref.cast (ref $Cons) (local.get $a)))
        (local.set $cb (ref.cast (ref $Cons) (local.get $b)))

        ;; heads must be deep-equal
        (if (i32.eqz
              (call $deep_eq
                (ref.cast (ref eq) (struct.get $Cons $head (local.get $ca)))
                (ref.cast (ref eq) (struct.get $Cons $head (local.get $cb)))))
          (then (return (i32.const 0))))

        ;; advance both tails
        (local.set $a (struct.get $Cons $tail (local.get $ca)))
        (local.set $b (struct.get $Cons $tail (local.get $cb)))
        (br $walk)))

    (i32.const 1))


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
  (func $head_any (@pub)
    (param $list (ref null any))
    (result (ref null any))
    (struct.get $Cons $head (ref.cast (ref $Cons) (local.get $list))))

  (func $tail_any (@pub)
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
  (func $concat (@pub)
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


  ;; -- Slice -----------------------------------------------------------

  ;; Drop the first $n cells. n must be in [0, len]. Returns the suffix —
  ;; shared with the input (no allocation). Out-of-range traps.
  (func $_list_skip
    (param $list (ref $List))
    (param $n i32)
    (result (ref $List))

    (local $cur (ref $List))
    (local.set $cur (local.get $list))

    (block $done
      (loop $walk
        (br_if $done (i32.eqz (local.get $n)))
        (if (ref.test (ref $Nil) (local.get $cur))
          (then (unreachable)))
        (local.set $cur
          (struct.get $Cons $tail
            (ref.cast (ref $Cons) (local.get $cur))))
        (local.set $n (i32.sub (local.get $n) (i32.const 1)))
        (br $walk)))

    (local.get $cur))

  ;; Build a new spine of the first $n cells of $list, ending in $Nil.
  ;; n must be in [0, len($list)]. Out-of-range traps.
  ;; Recursive — mirrors $_concat_inner shape.
  (func $_list_take
    (param $list (ref $List))
    (param $n i32)
    (result (ref $List))

    (local $cons (ref $Cons))

    (if (i32.eqz (local.get $n))
      (then (return (struct.new $Nil))))

    (if (ref.test (ref $Nil) (local.get $list))
      (then (unreachable)))

    (local.set $cons (ref.cast (ref $Cons) (local.get $list)))

    (struct.new $Cons
      (struct.get $Cons $head (local.get $cons))
      (call $_list_take
        (struct.get $Cons $tail (local.get $cons))
        (i32.sub (local.get $n) (i32.const 1)))))

  ;; Resolve a possibly-negative i64 index against a length. Negative
  ;; values count from the end: -1 means len-1. Returns the resolved
  ;; non-negative i64; caller bounds-checks.
  (func $_list_resolve_idx
    (param $idx i64)
    (param $len i64)
    (result i64)

    (if (result i64) (i64.lt_s (local.get $idx) (i64.const 0))
      (then (i64.add (local.get $len) (local.get $idx)))
      (else (local.get $idx))))

  ;; Slice $list[$start_i .. $end_i_or_neg1) where end_i_or_neg1 == -1
  ;; signals open end (use len). Negative bounds resolve against len.
  ;; Out-of-bounds traps. Returns the resulting list.
  ;;
  ;; Tail-share strategy: when end resolves to len, return the suffix
  ;; from $_list_skip directly (no allocation). When start == 0 and
  ;; end == len, returns the original list unchanged. Empty slice
  ;; returns $Nil. Other slices allocate (end_r - start_r) cons cells.
  (func $_list_slice
    (param $list (ref $List))
    (param $start_i i64)
    (param $end_i i64)
    (param $is_open i32)
    (result (ref $List))

    (local $len i64)
    (local $start_r i64)
    (local $end_r i64)
    (local $suffix (ref $List))

    (local.set $len (i64.extend_i32_s (call $size (local.get $list))))

    ;; Resolve negatives.
    (local.set $start_r
      (call $_list_resolve_idx (local.get $start_i) (local.get $len)))
    (if (local.get $is_open)
      (then (local.set $end_r (local.get $len)))
      (else
        (local.set $end_r
          (call $_list_resolve_idx (local.get $end_i) (local.get $len)))))

    ;; Bounds check: 0 <= start <= end <= len.
    (if (i64.lt_s (local.get $start_r) (i64.const 0))
      (then (unreachable)))
    (if (i64.lt_s (local.get $end_r) (local.get $start_r))
      (then (unreachable)))
    (if (i64.gt_s (local.get $end_r) (local.get $len))
      (then (unreachable)))

    ;; Skip start_r cells — suffix is shared.
    (local.set $suffix
      (call $_list_skip
        (local.get $list)
        (i32.wrap_i64 (local.get $start_r))))

    ;; If end_r == len, the suffix IS the answer (no allocation).
    (if (i64.eq (local.get $end_r) (local.get $len))
      (then (return (local.get $suffix))))

    ;; Otherwise, take (end_r - start_r) cells from the suffix.
    (call $_list_take
      (local.get $suffix)
      (i32.wrap_i64 (i64.sub (local.get $end_r) (local.get $start_r)))))


  ;; op_dot(list, key, cont) — indexing & slicing.
  ;;   $I64 key   → element at index (negative counts from end).
  ;;                Out of bounds → unreachable.
  ;;   $Range key → slice (closed, inclusive, or open-end).
  ;;                Out of bounds → unreachable.
  (func $op_dot (@impl "std/operators.fnk:op_dot" $List _)
    (param $ctx (ref null any))
    (param $list (ref null any)) (param $key (ref null any)) (param $cont (ref null any))

    (local $l (ref $List))
    (local $range (ref $Range))
    (local $start_i i64)
    (local $end_i i64)
    (local $is_open i32)
    (local $idx i64)
    (local $len i64)
    (local $elem (ref null any))

    (local.set $l (ref.cast (ref $List) (local.get $list)))

    ;; Try $Range key — slice.
    (block $not_range
      (block $is_range (result (ref $Range))
        (br $not_range
          (br_on_cast $is_range (ref null any) (ref $Range)
            (local.get $key))))
      (local.set $range)

      (local.set $start_i (struct.get $I64 $ival (call $range_start (local.get $range))))

      ;; Open-end: leave end_i unset and flag is_open.
      (local.set $is_open (i32.const 0))
      (local.set $end_i (i64.const 0))
      (block $end_done
        (block $end_ref (result (ref $I64))
          (br_on_non_null $end_ref (call $range_end (local.get $range)))
          (local.set $is_open (i32.const 1))
          (br $end_done))
        (local.set $end_i (struct.get $I64 $ival))
        (if (call $range_is_incl (local.get $range))
          (then
            (local.set $end_i (i64.add (local.get $end_i) (i64.const 1))))))

      (return_call $apply_1
      (local.get $ctx)
        (call $_list_slice
          (local.get $l)
          (local.get $start_i)
          (local.get $end_i)
          (local.get $is_open))
        (local.get $cont)))

    ;; Try $I64 key — element at index.
    (block $not_i64
      (block $is_i64 (result (ref $I64))
        (br $not_i64
          (br_on_cast $is_i64 (ref null any) (ref $I64)
            (local.get $key))))
      (local.set $idx (struct.get $I64 $ival))

      (local.set $len (i64.extend_i32_s (call $size (local.get $l))))
      (local.set $idx (call $_list_resolve_idx (local.get $idx) (local.get $len)))

      (if (i64.lt_s (local.get $idx) (i64.const 0))
        (then (unreachable)))
      (if (i64.ge_s (local.get $idx) (local.get $len))
        (then (unreachable)))

      (local.set $elem
        (call $get (local.get $l) (i32.wrap_i64 (local.get $idx))))
      (if (ref.is_null (local.get $elem))
        (then (unreachable)))
      (return_call $apply_1
      (local.get $ctx)
        (ref.as_non_null (local.get $elem))
        (local.get $cont)))

    (unreachable))


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
    (param $ctx (ref null any))
    (param $val (ref null any)) (param $list (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (call $prepend
        (ref.cast (ref any) (local.get $val))
        (ref.cast (ref $List) (local.get $list)))
      (local.get $cont)))

  ;; seq_concat(list_a, list_b, cont) — concatenate two lists, pass result to cont.
  (func $seq_concat (@pub) (@impl "std/seq.fnk:concat" $List)
    (param $ctx (ref null any))
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (call $concat
        (ref.cast (ref $List) (local.get $a))
        (ref.cast (ref $List) (local.get $b)))
      (local.get $cont)))

  ;; seq_pop(cursor, fail, succ) — destructure [head, ..tail].
  ;; If empty: tail-call fail with 0 args.
  ;; If non-empty: extract head + tail, tail-call succ with 2 args.
  (func $seq_pop (@impl "std/seq.fnk:pop" $List)
    (param $ctx (ref null any))
    (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))

    (local $cons (ref $Cons))

    (if (ref.test (ref $Nil) (local.get $cursor))
      (then (return_call $apply_0
      (local.get $ctx) (local.get $fail))))

    (local.set $cons (ref.cast (ref $Cons) (local.get $cursor)))

    (return_call $apply_2_vals
      (local.get $ctx)
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
    (param $ctx (ref null any))
    (param $cursor (ref null any)) (param $fail (ref null any)) (param $succ (ref null any))

    (local $init (ref $List))
    (local $last (ref any))

    (if (ref.test (ref $Nil) (local.get $cursor))
      (then (return_call $apply_0
      (local.get $ctx) (local.get $fail))))

    (call $pop_back (ref.cast (ref $List) (local.get $cursor)))
    (local.set $last)
    (local.set $init)

    (return_call $apply_2_vals
      (local.get $ctx)
      (local.get $init)
      (local.get $last)
      (local.get $succ)))


  ;; -- std/list.fnk:list — user-importable constructor closure ------------

  (elem declare func $_list_apply)

  (func $_list_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref null any))
    (local $rest (ref $List))

    ;; Peel cont off args[0]. Remainder is the user's list.
    (local.set $cont (call $head_any (local.get $args)))
    (local.set $rest
      (ref.cast (ref $List) (call $tail_any (local.get $args))))

    ;; Tail-call cont with [list], threading caller's ctx.
    (return_call $apply_1
      (local.get $ctx)
      (local.get $rest)
      (local.get $cont)))

  (global $_list_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $_list_apply)
      (ref.null $Captures)))

  (func $list (@pub) (@impl "std/list.fnk:list") (result (ref any))
    (global.get $_list_closure))

  ;; Format a $List as "[a, b, c]". Calls back into str.wat:fmt_val
  ;; for each element. Empty list renders as "[]".
  (func $fmt (@pub) (@impl "std/str.fnk:fmt" $List) (param $list (ref $List)) (result (ref $Str))
    (local $cur (ref null any))
    (local $total i32)
    (local $count i32)
    (local $buf (ref $ByteArray))
    (local $pos i32)
    (local $is_first i32)

    (if (call $op_empty (local.get $list))
      (then
        (return_call $_str_from_ascii_2
          (i32.const 0x5B) (i32.const 0x5D)))) ;; "[]"

    ;; Pass 1: total length = "[" + sum(elem_len) + (count-1)*2 (", ") + "]"
    (local.set $cur (local.get $list))
    (local.set $total (i32.const 2))
    (local.set $count (i32.const 0))
    (block $done1
      (loop $len_loop
        (br_if $done1 (call $op_empty (local.get $cur)))
        (local.set $total
          (i32.add (local.get $total)
            (call $_str_len
              (call $repr_val
                (ref.as_non_null (call $head_any (local.get $cur)))))))
        (local.set $count (i32.add (local.get $count) (i32.const 1)))
        (local.set $cur (call $tail_any (local.get $cur)))
        (br $len_loop)))

    (local.set $total
      (i32.add (local.get $total)
        (i32.mul
          (i32.sub (local.get $count) (i32.const 1))
          (i32.const 2))))

    (local.set $buf (array.new $ByteArray (i32.const 0) (local.get $total)))

    ;; Write '['.
    (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x5B))
    (local.set $pos (i32.const 1))

    ;; Pass 2: format and copy each element.
    (local.set $cur (local.get $list))
    (local.set $is_first (i32.const 1))
    (block $done2
      (loop $copy_loop
        (br_if $done2 (call $op_empty (local.get $cur)))

        ;; Write ", " separator (except before first element).
        (if (i32.eqz (local.get $is_first))
          (then
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x2C))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x20))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))))
        (local.set $is_first (i32.const 0))

        (local.set $pos
          (call $_str_copy_to
            (call $repr_val
              (ref.as_non_null (call $head_any (local.get $cur))))
            (local.get $buf)
            (local.get $pos)))

        (local.set $cur (call $tail_any (local.get $cur)))
        (br $copy_loop)))

    ;; Write ']'.
    (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x5D))

    (return_call $str_from_bytes (local.get $buf)))

  ;; repr — same as fmt for lists (their fmt already calls repr on elements).
  (func $repr (@pub) (@impl "std/repr.fnk:repr" $List)
    (param $list (ref $List)) (result (ref $Str))
    (return_call $fmt (local.get $list)))

)
