;; Set — immutable hash set using HAMT trie structure
;;
;; WASM GC implementation using struct and array types.
;; Same trie structure as the HAMT (rec.wat) but leaves store only
;; keys, no values — halving per-entry memory.
;;
;; Design:
;;   - Branching factor 32 (5 bits per level, max 7 levels for 32-bit hash)
;;   - Each node has a 32-bit bitmap and a dense children array
;;   - Leaves store a single key (no value)
;;   - At max depth, hash collisions stored in flat collision nodes
;;   - Structural sharing on add/remove
;;
;; Value representation:
;;   - Keys are (ref eq) — non-nullable
;;   - Key equality uses ref.eq (identity) in phase 0. Will be extended
;;     to direct-style deep_eq supporting: i31ref, $Num, $Str.
;;     User-defined Eq via std-lib (CPS, future).
;;   - Hash: imported from hashing.wat (hash_i31). Dispatches on i31ref,
;;     $Num, $Str via br_on_cast.
;;
;; Type hierarchy (types.wat defines the opaque base type):
;;
;;   $Set              ← opaque base (from types.wat)
;;   └── $SetImpl      ← wrapper: single $SetNode field
;;
;; Exported functions:
;;    TODO: SetNode/SetImpl are internal, public interfaces should use Set.
;;      Or these functions should be made private _* .
;;   $std/set.wat:new        : () -> (ref $SetNode)
;;   $std/set.wat:set        : (ref $SetNode), (ref eq) -> (ref $SetNode)
;;   $std/set.wat:has        : (ref $SetNode), (ref eq) -> i32
;;   $std/set.wat:remove     : (ref $SetNode), (ref eq) -> (ref $SetNode)
;;   $std/set.wat:size       : (ref $SetNode) -> i32
;;   $std/set.wat:union      : (ref $SetNode), (ref $SetNode) -> (ref $SetNode)
;;                     a + b — all entries from both
;;   $std/set.wat:intersect  : (ref $SetNode), (ref $SetNode) -> (ref $SetNode)
;;                     a & b — entries in both
;;   $std/set.wat:difference : (ref $SetNode), (ref $SetNode) -> (ref $SetNode)
;;                     a - b — entries in a not in b
;;   $std/set.wat:sym_diff   : (ref $SetNode), (ref $SetNode) -> (ref $SetNode)
;;                     a ^ b — entries in one but not both
;;   $std/set.wat:subset     : (ref $SetNode), (ref $SetNode) -> i32
;;                     a <= b — 1 if every element of a is in b (short-circuits)
;;   $std/set.wat:disjoint   : (ref $SetNode), (ref $SetNode) -> i32
;;                     a >< b — 1 if no common elements (short-circuits)
;;   $std/set.wat:eq         : (ref $SetNode), (ref $SetNode) -> i32
;;                     a == b — same size and a <= b

(module

  ;; -- Type definitions -----------------------------------------------

  ;; Internal set types. Implementation details — user code sees $Set
  ;; (from types.wat) via the $SetImpl wrapper below.

  ;; $SetEntry — a single key.
  (type $SetEntry (struct
    (field $key (ref eq))
  ))

  ;; $SetChildren — dense array of struct refs.
  (type $SetChildren (array (mut (ref null struct))))

  (rec
    ;; $SetNode — bitmap + dense children array.
    (type $SetNode (struct
      (field $bitmap (mut i32))
      (field $children (ref $SetChildren))
    ))

    ;; $SetCollision — flat array of entries sharing the same hash.
    (type $SetCollision (struct
      (field $col_hash i32)
      (field $col_entries (ref $SetChildren))
    ))

    ;; $SetImpl — wraps $SetNode as a $Set (from types.wat).
    (type $SetImpl (sub $Set (struct
      (field $node (ref $SetNode))
    )))
  )


  ;; -- Imports ----------------------------------------------------------

  ;; -- Helpers --------------------------------------------------------

  (func $std/set.wat:_set_hash_fragment (param $hash i32) (param $depth i32) (result i32)
    local.get $hash
    local.get $depth
    i32.const 5
    i32.mul
    i32.shr_u
    i32.const 0x1f
    i32.and
  )

  (func $std/set.wat:_set_bit_index (param $bitmap i32) (param $fragment i32) (result i32)
    local.get $bitmap
    i32.const 1
    local.get $fragment
    i32.shl
    i32.const 1
    i32.sub
    i32.and
    i32.popcnt
  )

  (func $std/set.wat:_set_collision_find
    (param $entries (ref $SetChildren))
    (param $key (ref eq))
    (result i32)

    (local $i i32)
    (local $len i32)
    (local.set $len (array.len (local.get $entries)))
    (local.set $i (i32.const 0))
    (block $not_found
      (loop $scan
        (br_if $not_found
          (i32.ge_u (local.get $i) (local.get $len)))
        (if (call $rt/protocols.wat:deep_eq
              (struct.get $SetEntry $key
                (ref.cast (ref $SetEntry)
                  (array.get $SetChildren
                    (local.get $entries)
                    (local.get $i))))
              (local.get $key))
          (then (return (local.get $i))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $scan)))
    (i32.const -1)
  )


  ;; -- Empty ----------------------------------------------------------

  (global $std/set.wat:empty_node (ref $SetNode)
    (struct.new $SetNode
      (i32.const 0)
      (array.new_fixed $SetChildren 0)
    )
  )

  (func $std/set.wat:new (export "std/set.wat:new") (result (ref $SetNode))
    global.get $std/set.wat:empty_node
  )


  ;; -- Has ------------------------------------------------------------

  (func $std/set.wat:has (export "std/set.wat:has")
    (param $current (ref $SetNode))
    (param $key (ref eq))
    (result i32)

    (local $h i32)
    (local $depth i32)
    (local $fragment i32)
    (local $bit i32)
    (local $bitmap i32)
    (local $idx i32)
    (local $child (ref null struct))

    (local.set $h (call $std/hashing.wat:hash_i31(local.get $key)))
    (local.set $depth (i32.const 0))

    (block $not_found
      (loop $descend
        (local.set $fragment
          (call $std/set.wat:_set_hash_fragment (local.get $h) (local.get $depth)))
        (local.set $bitmap
          (struct.get $SetNode $bitmap (local.get $current)))
        (local.set $bit
          (i32.shl (i32.const 1) (local.get $fragment)))

        (br_if $not_found
          (i32.eqz (i32.and (local.get $bitmap) (local.get $bit))))

        (local.set $idx
          (call $std/set.wat:_set_bit_index (local.get $bitmap) (local.get $fragment)))
        (local.set $child
          (array.get $SetChildren
            (struct.get $SetNode $children (local.get $current))
            (local.get $idx)))

        ;; entry — check key
        (if (ref.test (ref $SetEntry) (local.get $child))
          (then
            (if (call $rt/protocols.wat:deep_eq
                  (struct.get $SetEntry $key
                    (ref.cast (ref $SetEntry) (local.get $child)))
                  (local.get $key))
              (then (return (i32.const 1)))
              (else (br $not_found)))))

        ;; collision — scan
        (if (ref.test (ref $SetCollision) (local.get $child))
          (then
            (return
              (i32.ge_s
                (call $std/set.wat:_set_collision_find
                  (struct.get $SetCollision $col_entries
                    (ref.cast (ref $SetCollision) (local.get $child)))
                  (local.get $key))
                (i32.const 0)))))

        ;; sub-node
        (local.set $current
          (ref.cast (ref $SetNode) (local.get $child)))
        (local.set $depth
          (i32.add (local.get $depth) (i32.const 1)))
        (br $descend)))

    (i32.const 0)
  )


  ;; -- Add ------------------------------------------------------------

  (func $std/set.wat:set (export "std/set.wat:set")
    (param $current (ref $SetNode))
    (param $key (ref eq))
    (result (ref $SetNode))

    (call $std/set.wat:_set_set_inner
      (local.get $current)
      (local.get $key)
      (call $std/hashing.wat:hash_i31(local.get $key))
      (i32.const 0))
  )

  (func $std/set.wat:_set_set_inner
    (param $current (ref $SetNode))
    (param $key (ref eq))
    (param $h i32)
    (param $depth i32)
    (result (ref $SetNode))

    (local $fragment i32)
    (local $bit i32)
    (local $bitmap i32)
    (local $idx i32)
    (local $old_children (ref $SetChildren))
    (local $new_children (ref $SetChildren))
    (local $old_len i32)
    (local $child (ref null struct))
    (local $new_entry (ref $SetEntry))
    (local $i i32)
    (local $col_entries (ref $SetChildren))
    (local $col_idx i32)
    (local $new_col_entries (ref $SetChildren))

    (local.set $fragment
      (call $std/set.wat:_set_hash_fragment (local.get $h) (local.get $depth)))
    (local.set $bit
      (i32.shl (i32.const 1) (local.get $fragment)))
    (local.set $bitmap
      (struct.get $SetNode $bitmap (local.get $current)))
    (local.set $idx
      (call $std/set.wat:_set_bit_index (local.get $bitmap) (local.get $fragment)))
    (local.set $old_children
      (struct.get $SetNode $children (local.get $current)))
    (local.set $old_len
      (array.len (local.get $old_children)))

    (local.set $new_entry
      (struct.new $SetEntry (local.get $key)))

    ;; bit not set — insert new entry at idx
    (if (i32.eqz (i32.and (local.get $bitmap) (local.get $bit)))
      (then
        (local.set $new_children
          (array.new $SetChildren
            (local.get $new_entry)
            (i32.add (local.get $old_len) (i32.const 1))))

        (local.set $i (i32.const 0))
        (block $done_before
          (loop $copy_before
            (br_if $done_before
              (i32.ge_u (local.get $i) (local.get $idx)))
            (array.set $SetChildren
              (local.get $new_children)
              (local.get $i)
              (array.get $SetChildren
                (local.get $old_children)
                (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $copy_before)))

        (local.set $i (local.get $idx))
        (block $done_after
          (loop $copy_after
            (br_if $done_after
              (i32.ge_u (local.get $i) (local.get $old_len)))
            (array.set $SetChildren
              (local.get $new_children)
              (i32.add (local.get $i) (i32.const 1))
              (array.get $SetChildren
                (local.get $old_children)
                (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $copy_after)))

        (array.set $SetChildren
          (local.get $new_children)
          (local.get $idx)
          (local.get $new_entry))

        (return
          (struct.new $SetNode
            (i32.or (local.get $bitmap) (local.get $bit))
            (local.get $new_children)))))

    ;; bit is set — slot exists
    (local.set $child
      (array.get $SetChildren
        (local.get $old_children)
        (local.get $idx)))

    ;; collision node
    (if (ref.test (ref $SetCollision) (local.get $child))
      (then
        (local.set $col_entries
          (struct.get $SetCollision $col_entries
            (ref.cast (ref $SetCollision) (local.get $child))))
        (local.set $col_idx
          (call $std/set.wat:_set_collision_find (local.get $col_entries) (local.get $key)))

        ;; already present — return unchanged
        (if (i32.ge_s (local.get $col_idx) (i32.const 0))
          (then (return (local.get $current))))

        ;; append new entry to collision
        (local.set $new_col_entries
          (array.new $SetChildren
            (local.get $new_entry)
            (i32.add (array.len (local.get $col_entries)) (i32.const 1))))
        (array.copy $SetChildren $SetChildren
          (local.get $new_col_entries) (i32.const 0)
          (local.get $col_entries) (i32.const 0)
          (array.len (local.get $col_entries)))

        (local.set $new_children
          (array.new $SetChildren
            (local.get $new_entry)
            (local.get $old_len)))
        (array.copy $SetChildren $SetChildren
          (local.get $new_children) (i32.const 0)
          (local.get $old_children) (i32.const 0)
          (local.get $old_len))
        (array.set $SetChildren
          (local.get $new_children)
          (local.get $idx)
          (struct.new $SetCollision
            (struct.get $SetCollision $col_hash
              (ref.cast (ref $SetCollision) (local.get $child)))
            (local.get $new_col_entries)))
        (return
          (struct.new $SetNode
            (local.get $bitmap)
            (local.get $new_children)))))

    ;; entry
    (if (ref.test (ref $SetEntry) (local.get $child))
      (then
        ;; same key — already present, return unchanged
        (if (call $rt/protocols.wat:deep_eq
              (struct.get $SetEntry $key
                (ref.cast (ref $SetEntry) (local.get $child)))
              (local.get $key))
          (then (return (local.get $current)))
          (else
            ;; different key — push deeper or create collision
            (if (i32.ge_u (local.get $depth) (i32.const 6))
              (then
                ;; max depth — collision node
                (local.set $new_children
                  (array.new $SetChildren
                    (local.get $new_entry)
                    (local.get $old_len)))
                (array.copy $SetChildren $SetChildren
                  (local.get $new_children) (i32.const 0)
                  (local.get $old_children) (i32.const 0)
                  (local.get $old_len))
                (array.set $SetChildren
                  (local.get $new_children)
                  (local.get $idx)
                  (struct.new $SetCollision
                    (local.get $h)
                    (array.new_fixed $SetChildren 2
                      (local.get $child)
                      (local.get $new_entry))))
                (return
                  (struct.new $SetNode
                    (local.get $bitmap)
                    (local.get $new_children))))
              (else
                ;; push both into sub-node
                (local.set $new_children
                  (array.new $SetChildren
                    (local.get $new_entry)
                    (local.get $old_len)))
                (array.copy $SetChildren $SetChildren
                  (local.get $new_children) (i32.const 0)
                  (local.get $old_children) (i32.const 0)
                  (local.get $old_len))
                (array.set $SetChildren
                  (local.get $new_children)
                  (local.get $idx)
                  (call $std/set.wat:_set_set_inner
                    (call $std/set.wat:_set_set_inner
                      (call $std/set.wat:new)
                      (struct.get $SetEntry $key
                        (ref.cast (ref $SetEntry) (local.get $child)))
                      (call $std/hashing.wat:hash_i31
                        (struct.get $SetEntry $key
                          (ref.cast (ref $SetEntry) (local.get $child))))
                      (i32.add (local.get $depth) (i32.const 1)))
                    (local.get $key)
                    (local.get $h)
                    (i32.add (local.get $depth) (i32.const 1))))
                (return
                  (struct.new $SetNode
                    (local.get $bitmap)
                    (local.get $new_children)))))))))

    ;; sub-node — recurse
    (local.set $new_children
      (array.new $SetChildren
        (local.get $new_entry)
        (local.get $old_len)))
    (array.copy $SetChildren $SetChildren
      (local.get $new_children) (i32.const 0)
      (local.get $old_children) (i32.const 0)
      (local.get $old_len))
    (array.set $SetChildren
      (local.get $new_children)
      (local.get $idx)
      (call $std/set.wat:_set_set_inner
        (ref.cast (ref $SetNode) (local.get $child))
        (local.get $key)
        (local.get $h)
        (i32.add (local.get $depth) (i32.const 1))))
    (struct.new $SetNode
      (local.get $bitmap)
      (local.get $new_children))
  )


  ;; -- Remove ---------------------------------------------------------

  (func $std/set.wat:remove (export "std/set.wat:remove")
    (param $current (ref $SetNode))
    (param $key (ref eq))
    (result (ref $SetNode))

    (call $std/set.wat:_set_remove_inner
      (local.get $current)
      (local.get $key)
      (call $std/hashing.wat:hash_i31(local.get $key))
      (i32.const 0))
  )

  (func $std/set.wat:_set_remove_inner
    (param $current (ref $SetNode))
    (param $key (ref eq))
    (param $h i32)
    (param $depth i32)
    (result (ref $SetNode))

    (local $fragment i32)
    (local $bit i32)
    (local $bitmap i32)
    (local $idx i32)
    (local $old_children (ref $SetChildren))
    (local $new_children (ref $SetChildren))
    (local $old_len i32)
    (local $child (ref null struct))
    (local $sub_result (ref $SetNode))
    (local $i i32)
    (local $col_entries (ref $SetChildren))
    (local $col_idx i32)
    (local $col_len i32)
    (local $new_col_entries (ref $SetChildren))

    (local.set $fragment
      (call $std/set.wat:_set_hash_fragment (local.get $h) (local.get $depth)))
    (local.set $bit
      (i32.shl (i32.const 1) (local.get $fragment)))
    (local.set $bitmap
      (struct.get $SetNode $bitmap (local.get $current)))
    (local.set $old_children
      (struct.get $SetNode $children (local.get $current)))
    (local.set $old_len
      (array.len (local.get $old_children)))

    ;; bit not set — absent
    (if (i32.eqz (i32.and (local.get $bitmap) (local.get $bit)))
      (then (return (local.get $current))))

    (local.set $idx
      (call $std/set.wat:_set_bit_index (local.get $bitmap) (local.get $fragment)))
    (local.set $child
      (array.get $SetChildren
        (local.get $old_children)
        (local.get $idx)))

    ;; collision node
    (if (ref.test (ref $SetCollision) (local.get $child))
      (then
        (local.set $col_entries
          (struct.get $SetCollision $col_entries
            (ref.cast (ref $SetCollision) (local.get $child))))
        (local.set $col_len
          (array.len (local.get $col_entries)))
        (local.set $col_idx
          (call $std/set.wat:_set_collision_find (local.get $col_entries) (local.get $key)))

        (if (i32.lt_s (local.get $col_idx) (i32.const 0))
          (then (return (local.get $current))))

        ;; 2 entries — collapse to single entry
        (if (i32.eq (local.get $col_len) (i32.const 2))
          (then
            (local.set $new_children
              (array.new $SetChildren
                (array.get $SetChildren
                  (local.get $col_entries)
                  (if (result i32)
                    (i32.eq (local.get $col_idx) (i32.const 0))
                    (then (i32.const 1))
                    (else (i32.const 0))))
                (local.get $old_len)))
            (array.copy $SetChildren $SetChildren
              (local.get $new_children) (i32.const 0)
              (local.get $old_children) (i32.const 0)
              (local.get $old_len))
            (array.set $SetChildren
              (local.get $new_children)
              (local.get $idx)
              (array.get $SetChildren
                (local.get $col_entries)
                (if (result i32)
                  (i32.eq (local.get $col_idx) (i32.const 0))
                  (then (i32.const 1))
                  (else (i32.const 0)))))
            (return
              (struct.new $SetNode
                (local.get $bitmap)
                (local.get $new_children)))))

        ;; 3+ entries — shrink collision
        (local.set $new_col_entries
          (array.new $SetChildren
            (array.get $SetChildren
              (local.get $col_entries)
              (if (result i32)
                (i32.eq (local.get $col_idx) (i32.const 0))
                (then (i32.const 1))
                (else (i32.const 0))))
            (i32.sub (local.get $col_len) (i32.const 1))))

        (local.set $i (i32.const 0))
        (block $done_before
          (loop $copy_before
            (br_if $done_before
              (i32.ge_u (local.get $i) (local.get $col_idx)))
            (array.set $SetChildren
              (local.get $new_col_entries)
              (local.get $i)
              (array.get $SetChildren
                (local.get $col_entries)
                (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $copy_before)))

        (local.set $i (i32.add (local.get $col_idx) (i32.const 1)))
        (block $done_after
          (loop $copy_after
            (br_if $done_after
              (i32.ge_u (local.get $i) (local.get $col_len)))
            (array.set $SetChildren
              (local.get $new_col_entries)
              (i32.sub (local.get $i) (i32.const 1))
              (array.get $SetChildren
                (local.get $col_entries)
                (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $copy_after)))

        (local.set $new_children
          (array.new $SetChildren
            (local.get $child)
            (local.get $old_len)))
        (array.copy $SetChildren $SetChildren
          (local.get $new_children) (i32.const 0)
          (local.get $old_children) (i32.const 0)
          (local.get $old_len))
        (array.set $SetChildren
          (local.get $new_children)
          (local.get $idx)
          (struct.new $SetCollision
            (struct.get $SetCollision $col_hash
              (ref.cast (ref $SetCollision) (local.get $child)))
            (local.get $new_col_entries)))
        (return
          (struct.new $SetNode
            (local.get $bitmap)
            (local.get $new_children)))))

    ;; entry
    (if (ref.test (ref $SetEntry) (local.get $child))
      (then
        (if (i32.eqz
              (ref.eq
                (struct.get $SetEntry $key
                  (ref.cast (ref $SetEntry) (local.get $child)))
                (local.get $key)))
          (then (return (local.get $current))))

        (if (i32.eq (local.get $old_len) (i32.const 1))
          (then (return (call $std/set.wat:new))))

        (local.set $new_children
          (array.new $SetChildren
            (array.get $SetChildren
              (local.get $old_children)
              (if (result i32)
                (i32.eq (local.get $idx) (i32.const 0))
                (then (i32.const 1))
                (else (i32.const 0))))
            (i32.sub (local.get $old_len) (i32.const 1))))

        (local.set $i (i32.const 0))
        (block $done_before
          (loop $copy_before
            (br_if $done_before
              (i32.ge_u (local.get $i) (local.get $idx)))
            (array.set $SetChildren
              (local.get $new_children)
              (local.get $i)
              (array.get $SetChildren
                (local.get $old_children)
                (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $copy_before)))

        (local.set $i (i32.add (local.get $idx) (i32.const 1)))
        (block $done_after
          (loop $copy_after
            (br_if $done_after
              (i32.ge_u (local.get $i) (local.get $old_len)))
            (array.set $SetChildren
              (local.get $new_children)
              (i32.sub (local.get $i) (i32.const 1))
              (array.get $SetChildren
                (local.get $old_children)
                (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $copy_after)))

        (return
          (struct.new $SetNode
            (i32.xor (local.get $bitmap) (local.get $bit))
            (local.get $new_children)))))

    ;; sub-node — recurse
    (local.set $sub_result
      (call $std/set.wat:_set_remove_inner
        (ref.cast (ref $SetNode) (local.get $child))
        (local.get $key)
        (local.get $h)
        (i32.add (local.get $depth) (i32.const 1))))

    (if (ref.eq (local.get $sub_result)
                (ref.cast (ref $SetNode) (local.get $child)))
      (then (return (local.get $current))))

    (local.set $new_children
      (array.new $SetChildren
        (local.get $sub_result)
        (local.get $old_len)))
    (array.copy $SetChildren $SetChildren
      (local.get $new_children) (i32.const 0)
      (local.get $old_children) (i32.const 0)
      (local.get $old_len))
    (array.set $SetChildren
      (local.get $new_children)
      (local.get $idx)
      (local.get $sub_result))
    (struct.new $SetNode
      (local.get $bitmap)
      (local.get $new_children))
  )


  ;; -- Size -----------------------------------------------------------

  (func $std/set.wat:size (export "std/set.wat:size")
    (param $node (ref $SetNode))
    (result i32)

    (call $std/set.wat:_set_size_node (local.get $node))
  )

  (func $std/set.wat:_set_size_node
    (param $node (ref $SetNode))
    (result i32)

    (local $children (ref $SetChildren))
    (local $len i32)
    (local $i i32)
    (local $count i32)
    (local $child (ref null struct))

    (local.set $children
      (struct.get $SetNode $children (local.get $node)))
    (local.set $len (array.len (local.get $children)))
    (local.set $count (i32.const 0))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $child
          (array.get $SetChildren (local.get $children) (local.get $i)))

        (if (ref.test (ref $SetEntry) (local.get $child))
          (then
            (local.set $count (i32.add (local.get $count) (i32.const 1)))))

        (if (ref.test (ref $SetNode) (local.get $child))
          (then
            (local.set $count
              (i32.add (local.get $count)
                (call $std/set.wat:_set_size_node
                  (ref.cast (ref $SetNode) (local.get $child)))))))

        (if (ref.test (ref $SetCollision) (local.get $child))
          (then
            (local.set $count
              (i32.add (local.get $count)
                (array.len
                  (struct.get $SetCollision $col_entries
                    (ref.cast (ref $SetCollision) (local.get $child))))))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $count)
  )


  ;; -- Union ----------------------------------------------------------

  ;; a + b — all entries from both.
  (func $std/set.wat:union (export "std/set.wat:union")
    (param $dest (ref $SetNode))
    (param $src (ref $SetNode))
    (result (ref $SetNode))

    (call $std/set.wat:_set_union_node (local.get $dest) (local.get $src))
  )

  (func $std/set.wat:_set_union_node
    (param $dest (ref $SetNode))
    (param $src (ref $SetNode))
    (result (ref $SetNode))

    (local $children (ref $SetChildren))
    (local $len i32)
    (local $i i32)
    (local $child (ref null struct))

    (local.set $children
      (struct.get $SetNode $children (local.get $src)))
    (local.set $len (array.len (local.get $children)))

    (if (i32.eqz (local.get $len))
      (then (return (local.get $dest))))

    (local.set $i (i32.const 0))
    (block $done
      (loop $walk
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $child
          (array.get $SetChildren (local.get $children) (local.get $i)))

        (if (ref.test (ref $SetEntry) (local.get $child))
          (then
            (local.set $dest
              (call $std/set.wat:set (local.get $dest)
                (struct.get $SetEntry $key
                  (ref.cast (ref $SetEntry) (local.get $child)))))))

        (if (ref.test (ref $SetNode) (local.get $child))
          (then
            (local.set $dest
              (call $std/set.wat:_set_union_node (local.get $dest)
                (ref.cast (ref $SetNode) (local.get $child))))))

        (if (ref.test (ref $SetCollision) (local.get $child))
          (then
            (local.set $dest
              (call $std/set.wat:_set_union_collision (local.get $dest)
                (struct.get $SetCollision $col_entries
                  (ref.cast (ref $SetCollision) (local.get $child)))))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $dest)
  )

  (func $std/set.wat:_set_union_collision
    (param $dest (ref $SetNode))
    (param $entries (ref $SetChildren))
    (result (ref $SetNode))

    (local $len i32)
    (local $i i32)

    (local.set $len (array.len (local.get $entries)))
    (local.set $i (i32.const 0))
    (block $done
      (loop $walk
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $dest
          (call $std/set.wat:set (local.get $dest)
            (struct.get $SetEntry $key
              (ref.cast (ref $SetEntry)
                (array.get $SetChildren
                  (local.get $entries)
                  (local.get $i))))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $dest)
  )


  ;; -- Intersection ---------------------------------------------------

  ;; a & b — entries in both.
  ;; Walks a, keeps entries that are also in b.
  (func $std/set.wat:intersect (export "std/set.wat:intersect")
    (param $a (ref $SetNode))
    (param $b (ref $SetNode))
    (result (ref $SetNode))

    (call $std/set.wat:_set_intersect_node
      (call $std/set.wat:new)
      (local.get $a)
      (local.get $b))
  )

  (func $std/set.wat:_set_intersect_node
    (param $result (ref $SetNode))
    (param $src (ref $SetNode))
    (param $other (ref $SetNode))
    (result (ref $SetNode))

    (local $children (ref $SetChildren))
    (local $len i32)
    (local $i i32)
    (local $child (ref null struct))

    (local.set $children
      (struct.get $SetNode $children (local.get $src)))
    (local.set $len (array.len (local.get $children)))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $child
          (array.get $SetChildren (local.get $children) (local.get $i)))

        (if (ref.test (ref $SetEntry) (local.get $child))
          (then
            (if (call $std/set.wat:has (local.get $other)
                  (struct.get $SetEntry $key
                    (ref.cast (ref $SetEntry) (local.get $child))))
              (then
                (local.set $result
                  (call $std/set.wat:set (local.get $result)
                    (struct.get $SetEntry $key
                      (ref.cast (ref $SetEntry) (local.get $child)))))))))

        (if (ref.test (ref $SetNode) (local.get $child))
          (then
            (local.set $result
              (call $std/set.wat:_set_intersect_node
                (local.get $result)
                (ref.cast (ref $SetNode) (local.get $child))
                (local.get $other)))))

        (if (ref.test (ref $SetCollision) (local.get $child))
          (then
            (local.set $result
              (call $std/set.wat:_set_intersect_collision
                (local.get $result)
                (struct.get $SetCollision $col_entries
                  (ref.cast (ref $SetCollision) (local.get $child)))
                (local.get $other)))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $result)
  )

  (func $std/set.wat:_set_intersect_collision
    (param $result (ref $SetNode))
    (param $entries (ref $SetChildren))
    (param $other (ref $SetNode))
    (result (ref $SetNode))

    (local $len i32)
    (local $i i32)

    (local.set $len (array.len (local.get $entries)))
    (local.set $i (i32.const 0))
    (block $done
      (loop $walk
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (if (call $std/set.wat:has (local.get $other)
              (struct.get $SetEntry $key
                (ref.cast (ref $SetEntry)
                  (array.get $SetChildren
                    (local.get $entries)
                    (local.get $i)))))
          (then
            (local.set $result
              (call $std/set.wat:set (local.get $result)
                (struct.get $SetEntry $key
                  (ref.cast (ref $SetEntry)
                    (array.get $SetChildren
                      (local.get $entries)
                      (local.get $i))))))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $result)
  )


  ;; -- Difference -----------------------------------------------------

  ;; a - b — entries in a not in b.
  (func $std/set.wat:difference (export "std/set.wat:difference")
    (param $a (ref $SetNode))
    (param $b (ref $SetNode))
    (result (ref $SetNode))

    (call $std/set.wat:_set_difference_node
      (call $std/set.wat:new)
      (local.get $a)
      (local.get $b))
  )

  (func $std/set.wat:_set_difference_node
    (param $result (ref $SetNode))
    (param $src (ref $SetNode))
    (param $other (ref $SetNode))
    (result (ref $SetNode))

    (local $children (ref $SetChildren))
    (local $len i32)
    (local $i i32)
    (local $child (ref null struct))

    (local.set $children
      (struct.get $SetNode $children (local.get $src)))
    (local.set $len (array.len (local.get $children)))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $child
          (array.get $SetChildren (local.get $children) (local.get $i)))

        (if (ref.test (ref $SetEntry) (local.get $child))
          (then
            (if (i32.eqz
                  (call $std/set.wat:has (local.get $other)
                    (struct.get $SetEntry $key
                      (ref.cast (ref $SetEntry) (local.get $child)))))
              (then
                (local.set $result
                  (call $std/set.wat:set (local.get $result)
                    (struct.get $SetEntry $key
                      (ref.cast (ref $SetEntry) (local.get $child)))))))))

        (if (ref.test (ref $SetNode) (local.get $child))
          (then
            (local.set $result
              (call $std/set.wat:_set_difference_node
                (local.get $result)
                (ref.cast (ref $SetNode) (local.get $child))
                (local.get $other)))))

        (if (ref.test (ref $SetCollision) (local.get $child))
          (then
            (local.set $result
              (call $std/set.wat:_set_difference_collision
                (local.get $result)
                (struct.get $SetCollision $col_entries
                  (ref.cast (ref $SetCollision) (local.get $child)))
                (local.get $other)))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $result)
  )

  (func $std/set.wat:_set_difference_collision
    (param $result (ref $SetNode))
    (param $entries (ref $SetChildren))
    (param $other (ref $SetNode))
    (result (ref $SetNode))

    (local $len i32)
    (local $i i32)

    (local.set $len (array.len (local.get $entries)))
    (local.set $i (i32.const 0))
    (block $done
      (loop $walk
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (if (i32.eqz
              (call $std/set.wat:has (local.get $other)
                (struct.get $SetEntry $key
                  (ref.cast (ref $SetEntry)
                    (array.get $SetChildren
                      (local.get $entries)
                      (local.get $i))))))
          (then
            (local.set $result
              (call $std/set.wat:set (local.get $result)
                (struct.get $SetEntry $key
                  (ref.cast (ref $SetEntry)
                    (array.get $SetChildren
                      (local.get $entries)
                      (local.get $i))))))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $result)
  )


  ;; -- Symmetric difference -------------------------------------------

  ;; a ^ b — entries in one but not both.
  ;; Thin wrapper: union(difference(a, b), difference(b, a))
  (func $std/set.wat:sym_diff (export "std/set.wat:sym_diff")
    (param $a (ref $SetNode))
    (param $b (ref $SetNode))
    (result (ref $SetNode))

    (call $std/set.wat:union
      (call $std/set.wat:difference (local.get $a) (local.get $b))
      (call $std/set.wat:difference (local.get $b) (local.get $a)))
  )


  ;; -- Subset ---------------------------------------------------------

  ;; a <= b — 1 if every element of a is in b.
  ;; Short-circuits on first element of a not found in b.
  (func $std/set.wat:subset (export "std/set.wat:subset")
    (param $a (ref $SetNode))
    (param $b (ref $SetNode))
    (result i32)

    (call $std/set.wat:_set_subset_node (local.get $a) (local.get $b))
  )

  ;; Returns 1 if all entries in src are in other, 0 on first miss.
  (func $std/set.wat:_set_subset_node
    (param $src (ref $SetNode))
    (param $other (ref $SetNode))
    (result i32)

    (local $children (ref $SetChildren))
    (local $len i32)
    (local $i i32)
    (local $child (ref null struct))

    (local.set $children
      (struct.get $SetNode $children (local.get $src)))
    (local.set $len (array.len (local.get $children)))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $child
          (array.get $SetChildren (local.get $children) (local.get $i)))

        (if (ref.test (ref $SetEntry) (local.get $child))
          (then
            (if (i32.eqz
                  (call $std/set.wat:has (local.get $other)
                    (struct.get $SetEntry $key
                      (ref.cast (ref $SetEntry) (local.get $child)))))
              (then (return (i32.const 0))))))

        (if (ref.test (ref $SetNode) (local.get $child))
          (then
            (if (i32.eqz
                  (call $std/set.wat:_set_subset_node
                    (ref.cast (ref $SetNode) (local.get $child))
                    (local.get $other)))
              (then (return (i32.const 0))))))

        (if (ref.test (ref $SetCollision) (local.get $child))
          (then
            (if (i32.eqz
                  (call $std/set.wat:_set_subset_collision
                    (struct.get $SetCollision $col_entries
                      (ref.cast (ref $SetCollision) (local.get $child)))
                    (local.get $other)))
              (then (return (i32.const 0))))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (i32.const 1)
  )

  (func $std/set.wat:_set_subset_collision
    (param $entries (ref $SetChildren))
    (param $other (ref $SetNode))
    (result i32)

    (local $len i32)
    (local $i i32)

    (local.set $len (array.len (local.get $entries)))
    (local.set $i (i32.const 0))
    (block $done
      (loop $walk
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (if (i32.eqz
              (call $std/set.wat:has (local.get $other)
                (struct.get $SetEntry $key
                  (ref.cast (ref $SetEntry)
                    (array.get $SetChildren
                      (local.get $entries)
                      (local.get $i))))))
          (then (return (i32.const 0))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (i32.const 1)
  )


  ;; -- Disjoint -------------------------------------------------------

  ;; a >< b — 1 if no common elements.
  ;; Short-circuits on first element of a found in b.
  (func $std/set.wat:disjoint (export "std/set.wat:disjoint")
    (param $a (ref $SetNode))
    (param $b (ref $SetNode))
    (result i32)

    (call $std/set.wat:_set_disjoint_node (local.get $a) (local.get $b))
  )

  ;; Returns 1 if no entry in src is in other, 0 on first hit.
  (func $std/set.wat:_set_disjoint_node
    (param $src (ref $SetNode))
    (param $other (ref $SetNode))
    (result i32)

    (local $children (ref $SetChildren))
    (local $len i32)
    (local $i i32)
    (local $child (ref null struct))

    (local.set $children
      (struct.get $SetNode $children (local.get $src)))
    (local.set $len (array.len (local.get $children)))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $child
          (array.get $SetChildren (local.get $children) (local.get $i)))

        (if (ref.test (ref $SetEntry) (local.get $child))
          (then
            (if (call $std/set.wat:has (local.get $other)
                  (struct.get $SetEntry $key
                    (ref.cast (ref $SetEntry) (local.get $child))))
              (then (return (i32.const 0))))))

        (if (ref.test (ref $SetNode) (local.get $child))
          (then
            (if (i32.eqz
                  (call $std/set.wat:_set_disjoint_node
                    (ref.cast (ref $SetNode) (local.get $child))
                    (local.get $other)))
              (then (return (i32.const 0))))))

        (if (ref.test (ref $SetCollision) (local.get $child))
          (then
            (if (i32.eqz
                  (call $std/set.wat:_set_disjoint_collision
                    (struct.get $SetCollision $col_entries
                      (ref.cast (ref $SetCollision) (local.get $child)))
                    (local.get $other)))
              (then (return (i32.const 0))))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (i32.const 1)
  )

  (func $std/set.wat:_set_disjoint_collision
    (param $entries (ref $SetChildren))
    (param $other (ref $SetNode))
    (result i32)

    (local $len i32)
    (local $i i32)

    (local.set $len (array.len (local.get $entries)))
    (local.set $i (i32.const 0))
    (block $done
      (loop $walk
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (if (call $std/set.wat:has (local.get $other)
              (struct.get $SetEntry $key
                (ref.cast (ref $SetEntry)
                  (array.get $SetChildren
                    (local.get $entries)
                    (local.get $i)))))
          (then (return (i32.const 0))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (i32.const 1)
  )


  ;; -- Equality -------------------------------------------------------

  ;; a == b — same size and a is subset of b.
  (func $std/set.wat:eq (export "std/set.wat:eq")
    (param $a (ref $SetNode))
    (param $b (ref $SetNode))
    (result i32)

    (if (i32.ne
          (call $std/set.wat:size (local.get $a))
          (call $std/set.wat:size (local.get $b)))
      (then (return (i32.const 0))))

    (call $std/set.wat:subset (local.get $a) (local.get $b))
  )


  ;; -- Set wrappers (user-visible API) -----------------------------------
  ;; Wrap/unwrap $SetImpl ↔ $SetNode at the boundary.

  ;; Public surface: all wrappers below take/return (ref $Set). The
  ;; $SetImpl wrapper is purely internal — never appears in any export
  ;; signature. Each wrapper does an internal cast to $SetImpl.

  (func $std/set.wat:impl_empty (export "std/set.wat:impl_empty") (result (ref $Set))
    (struct.new $SetImpl (global.get $std/set.wat:empty_node))
  )

  (func $std/set.wat:impl_has (export "std/set.wat:impl_has")
    (param $s (ref $Set)) (param $key (ref eq))
    (result i32)
    (call $std/set.wat:has
      (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $s)))
      (local.get $key))
  )

  ;; op_in : (s, key) → 1 if key is in s, else 0.
  (func $std/set.wat:op_in (export "std/set.wat:op_in")
    (param $s (ref $Set)) (param $key (ref eq))
    (result i32)
    (call $std/set.wat:has
      (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $s)))
      (local.get $key))
  )

  (func $std/set.wat:op_notin (export "std/set.wat:op_notin")
    (param $s (ref $Set)) (param $key (ref eq))
    (result i32)
    (i32.eqz (call $std/set.wat:op_in (local.get $s) (local.get $key)))
  )

  (func $std/set.wat:impl_set (export "std/set.wat:impl_set")
    (param $s (ref $Set)) (param $key (ref eq))
    (result (ref $Set))
    (struct.new $SetImpl
      (call $std/set.wat:set
        (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $s)))
        (local.get $key)))
  )

  (func $std/set.wat:impl_remove (export "std/set.wat:impl_remove")
    (param $s (ref $Set)) (param $key (ref eq))
    (result (ref $Set))
    (struct.new $SetImpl
      (call $std/set.wat:remove
        (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $s)))
        (local.get $key)))
  )

  (func $std/set.wat:impl_size (export "std/set.wat:impl_size")
    (param $s (ref $Set)) (result i32)
    (call $std/set.wat:size
      (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $s))))
  )

  (func $std/set.wat:op_plus (export "std/set.wat:op_plus")
    (param $a (ref $Set)) (param $b (ref $Set))
    (result (ref $Set))
    (struct.new $SetImpl
      (call $std/set.wat:union
        (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $a)))
        (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $b)))))
  )

  ;; or — alias for union (same semantics as +).
  (func $std/set.wat:op_or (export "std/set.wat:op_or")
    (param $a (ref $Set)) (param $b (ref $Set))
    (result (ref $Set))
    (return_call $std/set.wat:op_plus (local.get $a) (local.get $b))
  )

  (func $std/set.wat:op_and (export "std/set.wat:op_and")
    (param $a (ref $Set)) (param $b (ref $Set))
    (result (ref $Set))
    (struct.new $SetImpl
      (call $std/set.wat:intersect
        (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $a)))
        (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $b)))))
  )

  (func $std/set.wat:op_minus (export "std/set.wat:op_minus")
    (param $a (ref $Set)) (param $b (ref $Set))
    (result (ref $Set))
    (struct.new $SetImpl
      (call $std/set.wat:difference
        (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $a)))
        (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $b)))))
  )

  (func $std/set.wat:op_xor (export "std/set.wat:op_xor")
    (param $a (ref $Set)) (param $b (ref $Set))
    (result (ref $Set))
    (struct.new $SetImpl
      (call $std/set.wat:sym_diff
        (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $a)))
        (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $b)))))
  )

  (func $std/set.wat:op_lte (export "std/set.wat:op_lte")
    (param $a (ref $Set)) (param $b (ref $Set))
    (result i32)
    (call $std/set.wat:subset
      (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $a)))
      (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $b))))
  )

  (func $std/set.wat:op_gte (export "std/set.wat:op_gte")
    (param $a (ref $Set)) (param $b (ref $Set))
    (result i32)
    (call $std/set.wat:subset
      (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $b)))
      (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $a))))
  )

  ;; Strict subset: a <= b AND |a| != |b|.
  (func $std/set.wat:op_lt (export "std/set.wat:op_lt")
    (param $a (ref $Set)) (param $b (ref $Set))
    (result i32)
    (local $a_node (ref $SetNode))
    (local $b_node (ref $SetNode))
    (local.set $a_node (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $a))))
    (local.set $b_node (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $b))))
    (i32.and
      (call $std/set.wat:subset (local.get $a_node) (local.get $b_node))
      (i32.ne
        (call $std/set.wat:_set_size_node (local.get $a_node))
        (call $std/set.wat:_set_size_node (local.get $b_node))))
  )

  ;; Strict superset: a >= b AND |a| != |b|.
  (func $std/set.wat:op_gt (export "std/set.wat:op_gt")
    (param $a (ref $Set)) (param $b (ref $Set))
    (result i32)
    (local $a_node (ref $SetNode))
    (local $b_node (ref $SetNode))
    (local.set $a_node (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $a))))
    (local.set $b_node (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $b))))
    (i32.and
      (call $std/set.wat:subset (local.get $b_node) (local.get $a_node))
      (i32.ne
        (call $std/set.wat:_set_size_node (local.get $a_node))
        (call $std/set.wat:_set_size_node (local.get $b_node))))
  )

  (func $std/set.wat:op_disjoint (export "std/set.wat:op_disjoint")
    (param $a (ref $Set)) (param $b (ref $Set))
    (result i32)
    (call $std/set.wat:disjoint
      (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $a)))
      (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $b))))
  )

  (func $std/set.wat:op_eq (export "std/set.wat:op_eq")
    (param $a (ref $Set)) (param $b (ref $Set))
    (result i32)
    (call $std/set.wat:eq
      (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $a)))
      (struct.get $SetImpl $node (ref.cast (ref $SetImpl) (local.get $b))))
  )


  ;; -- Repr -----------------------------------------------------------
  ;;
  ;; Render a $Set as a string: `set v1, v2, ...` with each element
  ;; formatted via _str_fmt_val_repr. Empty set renders as `set _`.
  ;;
  ;; Two-pass byte-buffer build, mirroring the rec formatter in str.wat
  ;; but driven from inside set.wat so HAMT internals stay encapsulated.

  (func $std/set.wat:repr (export "std/set.fnk:repr")
    (param $set (ref $Set))
    (result (ref $Str))

    (local $node (ref $SetNode))
    (local $entry_count i32)
    (local $total i32)
    (local $buf (ref $ByteArray))
    (local $pos i32)

    (local.set $node
      (struct.get $SetImpl $node
        (ref.cast (ref $SetImpl) (local.get $set))))

    (local.set $entry_count
      (call $std/set.wat:_set_size_node (local.get $node)))

    ;; Empty: "set _"
    (if (i32.eqz (local.get $entry_count))
      (then
        (local.set $buf
          (array.new $ByteArray (i32.const 0) (i32.const 5)))
        (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x73)) ;; 's'
        (array.set $ByteArray (local.get $buf) (i32.const 1) (i32.const 0x65)) ;; 'e'
        (array.set $ByteArray (local.get $buf) (i32.const 2) (i32.const 0x74)) ;; 't'
        (array.set $ByteArray (local.get $buf) (i32.const 3) (i32.const 0x20)) ;; ' '
        (array.set $ByteArray (local.get $buf) (i32.const 4) (i32.const 0x5F)) ;; '_'
        (return (struct.new $StrBytesImpl (local.get $buf)))))

    ;; Pass 1: total = "set " (4) + sum(value_len) + (count-1)*2 (", ").
    (local.set $total
      (i32.add
        (i32.const 4) ;; "set "
        (i32.add
          (call $std/set.wat:_repr_size_node (local.get $node))
          (i32.mul
            (i32.sub (local.get $entry_count) (i32.const 1))
            (i32.const 2)))))

    ;; Allocate.
    (local.set $buf
      (array.new $ByteArray (i32.const 0) (local.get $total)))

    ;; Write "set ".
    (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x73)) ;; 's'
    (array.set $ByteArray (local.get $buf) (i32.const 1) (i32.const 0x65)) ;; 'e'
    (array.set $ByteArray (local.get $buf) (i32.const 2) (i32.const 0x74)) ;; 't'
    (array.set $ByteArray (local.get $buf) (i32.const 3) (i32.const 0x20)) ;; ' '
    (local.set $pos (i32.const 4))

    ;; Pass 2: copy entries.
    (local.set $pos
      (call $std/set.wat:_repr_copy_node
        (local.get $node) (local.get $buf) (local.get $pos)
        (i32.const 0))) ;; written = 0

    (struct.new $StrBytesImpl (local.get $buf))
  )

  ;; _repr_size_node : (ref $SetNode) -> i32
  ;; Sum of formatted-value lengths across all entries (no separators).
  (func $std/set.wat:_repr_size_node
    (param $node (ref $SetNode))
    (result i32)

    (local $children (ref $SetChildren))
    (local $len i32)
    (local $i i32)
    (local $total i32)
    (local $child (ref null struct))

    (local.set $children
      (struct.get $SetNode $children (local.get $node)))
    (local.set $len (array.len (local.get $children)))
    (local.set $total (i32.const 0))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))

        (local.set $child
          (array.get $SetChildren (local.get $children) (local.get $i)))

        (if (ref.test (ref $SetEntry) (local.get $child))
          (then
            (local.set $total
              (i32.add (local.get $total)
                (call $std/str.wat:_str_len
                  (call $std/str.wat:_str_fmt_val_repr
                    (ref.cast (ref any)
                      (struct.get $SetEntry $key
                        (ref.cast (ref $SetEntry) (local.get $child))))))))))

        (if (ref.test (ref $SetNode) (local.get $child))
          (then
            (local.set $total
              (i32.add (local.get $total)
                (call $std/set.wat:_repr_size_node
                  (ref.cast (ref $SetNode) (local.get $child)))))))

        (if (ref.test (ref $SetCollision) (local.get $child))
          (then
            (local.set $total
              (i32.add (local.get $total)
                (call $std/set.wat:_repr_size_collision
                  (struct.get $SetCollision $col_entries
                    (ref.cast (ref $SetCollision) (local.get $child))))))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $total)
  )

  (func $std/set.wat:_repr_size_collision
    (param $entries (ref $SetChildren))
    (result i32)

    (local $len i32)
    (local $i i32)
    (local $total i32)

    (local.set $len (array.len (local.get $entries)))
    (local.set $total (i32.const 0))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $total
          (i32.add (local.get $total)
            (call $std/str.wat:_str_len
              (call $std/str.wat:_str_fmt_val_repr
                (ref.cast (ref any)
                  (struct.get $SetEntry $key
                    (ref.cast (ref $SetEntry)
                      (array.get $SetChildren (local.get $entries) (local.get $i)))))))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $total)
  )

  ;; _repr_copy_node : (node, buf, pos, written) -> new_pos
  ;; Copy formatted entries into buf. written counts entries already
  ;; emitted (used to decide whether to prepend ", ").
  (func $std/set.wat:_repr_copy_node
    (param $node (ref $SetNode))
    (param $buf (ref $ByteArray))
    (param $pos i32)
    (param $written i32)
    (result i32)

    (local $children (ref $SetChildren))
    (local $len i32)
    (local $i i32)
    (local $child (ref null struct))

    (local.set $children
      (struct.get $SetNode $children (local.get $node)))
    (local.set $len (array.len (local.get $children)))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))

        (local.set $child
          (array.get $SetChildren (local.get $children) (local.get $i)))

        (if (ref.test (ref $SetEntry) (local.get $child))
          (then
            ;; Separator if not first.
            (if (local.get $written)
              (then
                (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x2C)) ;; ','
                (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
                (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x20)) ;; ' '
                (local.set $pos (i32.add (local.get $pos) (i32.const 1)))))

            (local.set $pos
              (call $std/str.wat:_str_copy_to
                (call $std/str.wat:_str_fmt_val_repr
                  (ref.cast (ref any)
                    (struct.get $SetEntry $key
                      (ref.cast (ref $SetEntry) (local.get $child)))))
                (local.get $buf)
                (local.get $pos)))
            (local.set $written (i32.add (local.get $written) (i32.const 1)))))

        (if (ref.test (ref $SetNode) (local.get $child))
          (then
            (local.set $pos
              (call $std/set.wat:_repr_copy_node
                (ref.cast (ref $SetNode) (local.get $child))
                (local.get $buf)
                (local.get $pos)
                (local.get $written)))
            (local.set $written
              (i32.add (local.get $written)
                (call $std/set.wat:_set_size_node
                  (ref.cast (ref $SetNode) (local.get $child)))))))

        (if (ref.test (ref $SetCollision) (local.get $child))
          (then
            (local.set $pos
              (call $std/set.wat:_repr_copy_collision
                (struct.get $SetCollision $col_entries
                  (ref.cast (ref $SetCollision) (local.get $child)))
                (local.get $buf)
                (local.get $pos)
                (local.get $written)))
            (local.set $written
              (i32.add (local.get $written)
                (array.len
                  (struct.get $SetCollision $col_entries
                    (ref.cast (ref $SetCollision) (local.get $child))))))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $pos)
  )

  (func $std/set.wat:_repr_copy_collision
    (param $entries (ref $SetChildren))
    (param $buf (ref $ByteArray))
    (param $pos i32)
    (param $written i32)
    (result i32)

    (local $len i32)
    (local $i i32)

    (local.set $len (array.len (local.get $entries)))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))

        (if (local.get $written)
          (then
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x2C))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x20))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))))

        (local.set $pos
          (call $std/str.wat:_str_copy_to
            (call $std/str.wat:_str_fmt_val_repr
              (ref.cast (ref any)
                (struct.get $SetEntry $key
                  (ref.cast (ref $SetEntry)
                    (array.get $SetChildren (local.get $entries) (local.get $i))))))
            (local.get $buf)
            (local.get $pos)))
        (local.set $written (i32.add (local.get $written) (i32.const 1)))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $pos)
  )


  ;; -- std/set.fnk:set — user-importable constructor closure -----------------
  ;;
  ;; `{set} = import 'std/set.fnk'` resolves to the accessor below; the
  ;; user's `set 1, 2, 3` then calls the closure via `_apply` with
  ;; args = [cont, 1, 2, 3]. The Fn2 adapter peels cont off the head,
  ;; folds remaining items into a $SetImpl, and tail-calls cont with
  ;; the result.
  ;;
  ;; Same shape as interop/rust.wat:read for std/io.fnk:read.

  (elem declare func $std/set.wat:_set_apply)

  (func $std/set.wat:_set_apply (type $Fn2)
    (param $_caps (ref null any))
    (param $args (ref null any))

    (local $cursor (ref null any))
    (local $cont (ref null any))
    (local $node (ref $SetNode))
    (local $key (ref eq))

    ;; Peel cont off args[0].
    (local.set $cursor (local.get $args))
    (local.set $cont (call $std/list.wat:head_any (local.get $cursor)))
    (local.set $cursor (call $std/list.wat:tail_any (local.get $cursor)))

    ;; Start with empty SetNode.
    (local.set $node (call $std/set.wat:new))

    ;; Walk remaining args, accumulating each key into the set.
    (block $done
      (loop $walk
        (br_if $done (ref.test (ref $Nil) (local.get $cursor)))
        (local.set $key
          (ref.cast (ref eq) (call $std/list.wat:head_any (local.get $cursor))))
        (local.set $node
          (call $std/set.wat:set (local.get $node) (local.get $key)))
        (local.set $cursor (call $std/list.wat:tail_any (local.get $cursor)))
        (br $walk)))

    ;; Tail-call cont with [SetImpl(node)].
    (return_call $std/list.wat:apply_1
      (struct.new $SetImpl (local.get $node))
      (local.get $cont))
  )

  (global $std/set.wat:_set_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $std/set.wat:_set_apply)
      (ref.null $Captures)))

  (func (export "std/set.fnk:set") (result (ref any))
    (global.get $std/set.wat:_set_closure))

)
