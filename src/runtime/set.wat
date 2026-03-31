;; Set — immutable hash set using HAMT trie structure
;;
;; WASM GC implementation using struct and array types.
;; Same trie structure as the HAMT (hamt.wat) but leaves store only
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
;;     to direct-style deep_eq supporting: i31ref, $Num, $StrRaw,
;;     $StrRendered. User-defined Eq via std-lib (CPS, future).
;;   - Hash: imported from hashing.wat (hash_i31). Dispatches on i31ref,
;;     $Num, $Str via br_on_cast.
;;
;; Type hierarchy (types.wat defines the opaque base type):
;;
;;   $Set              ← opaque base (from types.wat)
;;   └── $SetImpl      ← wrapper: single $SetNode field
;;
;; Exported functions:
;;   $set_empty      : () -> (ref $SetNode)
;;   $set_set        : (ref $SetNode), (ref eq) -> (ref $SetNode)
;;   $set_has        : (ref $SetNode), (ref eq) -> i32
;;   $set_remove     : (ref $SetNode), (ref eq) -> (ref $SetNode)
;;   $set_size       : (ref $SetNode) -> i32
;;   $set_union      : (ref $SetNode), (ref $SetNode) -> (ref $SetNode)
;;                     a + b — all entries from both
;;   $set_intersect  : (ref $SetNode), (ref $SetNode) -> (ref $SetNode)
;;                     a & b — entries in both
;;   $set_difference : (ref $SetNode), (ref $SetNode) -> (ref $SetNode)
;;                     a - b — entries in a not in b
;;   $set_sym_diff   : (ref $SetNode), (ref $SetNode) -> (ref $SetNode)
;;                     a ^ b — entries in one but not both
;;   $set_subset     : (ref $SetNode), (ref $SetNode) -> i32
;;                     a <= b — 1 if every element of a is in b (short-circuits)
;;   $set_disjoint   : (ref $SetNode), (ref $SetNode) -> i32
;;                     a >< b — 1 if no common elements (short-circuits)
;;   $set_eq         : (ref $SetNode), (ref $SetNode) -> i32
;;                     a == b — same size and a <= b

(import "@fink/runtime/types" "*" (func (param anyref)))


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

  (import "@fink/runtime/hashing" "hash_i31"
    (func $hash_i31 (param (ref eq)) (result i32)))


  ;; -- Helpers --------------------------------------------------------

  (func $_hash_fragment (param $hash i32) (param $depth i32) (result i32)
    local.get $hash
    local.get $depth
    i32.const 5
    i32.mul
    i32.shr_u
    i32.const 0x1f
    i32.and
  )

  (func $_bit_index (param $bitmap i32) (param $fragment i32) (result i32)
    local.get $bitmap
    i32.const 1
    local.get $fragment
    i32.shl
    i32.const 1
    i32.sub
    i32.and
    i32.popcnt
  )

  (func $_collision_find
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
        (if (ref.eq
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

  (global $empty_node (ref $SetNode)
    (struct.new $SetNode
      (i32.const 0)
      (array.new_fixed $SetChildren 0)
    )
  )

  (func $set_empty (export "set_empty") (result (ref $SetNode))
    global.get $empty_node
  )


  ;; -- Has ------------------------------------------------------------

  (func $set_has (export "set_has")
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

    (local.set $h (call $hash_i31(local.get $key)))
    (local.set $depth (i32.const 0))

    (block $not_found
      (loop $descend
        (local.set $fragment
          (call $_hash_fragment (local.get $h) (local.get $depth)))
        (local.set $bitmap
          (struct.get $SetNode $bitmap (local.get $current)))
        (local.set $bit
          (i32.shl (i32.const 1) (local.get $fragment)))

        (br_if $not_found
          (i32.eqz (i32.and (local.get $bitmap) (local.get $bit))))

        (local.set $idx
          (call $_bit_index (local.get $bitmap) (local.get $fragment)))
        (local.set $child
          (array.get $SetChildren
            (struct.get $SetNode $children (local.get $current))
            (local.get $idx)))

        ;; entry — check key
        (if (ref.test (ref $SetEntry) (local.get $child))
          (then
            (if (ref.eq
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
                (call $_collision_find
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

  (func $set_set (export "set_set")
    (param $current (ref $SetNode))
    (param $key (ref eq))
    (result (ref $SetNode))

    (call $_set_set_inner
      (local.get $current)
      (local.get $key)
      (call $hash_i31(local.get $key))
      (i32.const 0))
  )

  (func $_set_set_inner
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
      (call $_hash_fragment (local.get $h) (local.get $depth)))
    (local.set $bit
      (i32.shl (i32.const 1) (local.get $fragment)))
    (local.set $bitmap
      (struct.get $SetNode $bitmap (local.get $current)))
    (local.set $idx
      (call $_bit_index (local.get $bitmap) (local.get $fragment)))
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
          (call $_collision_find (local.get $col_entries) (local.get $key)))

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
        (if (ref.eq
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
                  (call $_set_set_inner
                    (call $_set_set_inner
                      (call $set_empty)
                      (struct.get $SetEntry $key
                        (ref.cast (ref $SetEntry) (local.get $child)))
                      (call $hash_i31
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
      (call $_set_set_inner
        (ref.cast (ref $SetNode) (local.get $child))
        (local.get $key)
        (local.get $h)
        (i32.add (local.get $depth) (i32.const 1))))
    (struct.new $SetNode
      (local.get $bitmap)
      (local.get $new_children))
  )


  ;; -- Remove ---------------------------------------------------------

  (func $set_remove (export "set_remove")
    (param $current (ref $SetNode))
    (param $key (ref eq))
    (result (ref $SetNode))

    (call $_set_remove_inner
      (local.get $current)
      (local.get $key)
      (call $hash_i31(local.get $key))
      (i32.const 0))
  )

  (func $_set_remove_inner
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
      (call $_hash_fragment (local.get $h) (local.get $depth)))
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
      (call $_bit_index (local.get $bitmap) (local.get $fragment)))
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
          (call $_collision_find (local.get $col_entries) (local.get $key)))

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
          (then (return (call $set_empty))))

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
      (call $_set_remove_inner
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

  (func $set_size (export "set_size")
    (param $node (ref $SetNode))
    (result i32)

    (call $_set_size_node (local.get $node))
  )

  (func $_set_size_node
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
                (call $_set_size_node
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
  (func $set_union (export "set_union")
    (param $dest (ref $SetNode))
    (param $src (ref $SetNode))
    (result (ref $SetNode))

    (call $_set_union_node (local.get $dest) (local.get $src))
  )

  (func $_set_union_node
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
              (call $set_set (local.get $dest)
                (struct.get $SetEntry $key
                  (ref.cast (ref $SetEntry) (local.get $child)))))))

        (if (ref.test (ref $SetNode) (local.get $child))
          (then
            (local.set $dest
              (call $_set_union_node (local.get $dest)
                (ref.cast (ref $SetNode) (local.get $child))))))

        (if (ref.test (ref $SetCollision) (local.get $child))
          (then
            (local.set $dest
              (call $_set_union_collision (local.get $dest)
                (struct.get $SetCollision $col_entries
                  (ref.cast (ref $SetCollision) (local.get $child)))))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $dest)
  )

  (func $_set_union_collision
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
          (call $set_set (local.get $dest)
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
  (func $set_intersect (export "set_intersect")
    (param $a (ref $SetNode))
    (param $b (ref $SetNode))
    (result (ref $SetNode))

    (call $_set_intersect_node
      (call $set_empty)
      (local.get $a)
      (local.get $b))
  )

  (func $_set_intersect_node
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
            (if (call $set_has (local.get $other)
                  (struct.get $SetEntry $key
                    (ref.cast (ref $SetEntry) (local.get $child))))
              (then
                (local.set $result
                  (call $set_set (local.get $result)
                    (struct.get $SetEntry $key
                      (ref.cast (ref $SetEntry) (local.get $child)))))))))

        (if (ref.test (ref $SetNode) (local.get $child))
          (then
            (local.set $result
              (call $_set_intersect_node
                (local.get $result)
                (ref.cast (ref $SetNode) (local.get $child))
                (local.get $other)))))

        (if (ref.test (ref $SetCollision) (local.get $child))
          (then
            (local.set $result
              (call $_set_intersect_collision
                (local.get $result)
                (struct.get $SetCollision $col_entries
                  (ref.cast (ref $SetCollision) (local.get $child)))
                (local.get $other)))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $result)
  )

  (func $_set_intersect_collision
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
        (if (call $set_has (local.get $other)
              (struct.get $SetEntry $key
                (ref.cast (ref $SetEntry)
                  (array.get $SetChildren
                    (local.get $entries)
                    (local.get $i)))))
          (then
            (local.set $result
              (call $set_set (local.get $result)
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
  (func $set_difference (export "set_difference")
    (param $a (ref $SetNode))
    (param $b (ref $SetNode))
    (result (ref $SetNode))

    (call $_set_difference_node
      (call $set_empty)
      (local.get $a)
      (local.get $b))
  )

  (func $_set_difference_node
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
                  (call $set_has (local.get $other)
                    (struct.get $SetEntry $key
                      (ref.cast (ref $SetEntry) (local.get $child)))))
              (then
                (local.set $result
                  (call $set_set (local.get $result)
                    (struct.get $SetEntry $key
                      (ref.cast (ref $SetEntry) (local.get $child)))))))))

        (if (ref.test (ref $SetNode) (local.get $child))
          (then
            (local.set $result
              (call $_set_difference_node
                (local.get $result)
                (ref.cast (ref $SetNode) (local.get $child))
                (local.get $other)))))

        (if (ref.test (ref $SetCollision) (local.get $child))
          (then
            (local.set $result
              (call $_set_difference_collision
                (local.get $result)
                (struct.get $SetCollision $col_entries
                  (ref.cast (ref $SetCollision) (local.get $child)))
                (local.get $other)))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $result)
  )

  (func $_set_difference_collision
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
              (call $set_has (local.get $other)
                (struct.get $SetEntry $key
                  (ref.cast (ref $SetEntry)
                    (array.get $SetChildren
                      (local.get $entries)
                      (local.get $i))))))
          (then
            (local.set $result
              (call $set_set (local.get $result)
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
  (func $set_sym_diff (export "set_sym_diff")
    (param $a (ref $SetNode))
    (param $b (ref $SetNode))
    (result (ref $SetNode))

    (call $set_union
      (call $set_difference (local.get $a) (local.get $b))
      (call $set_difference (local.get $b) (local.get $a)))
  )


  ;; -- Subset ---------------------------------------------------------

  ;; a <= b — 1 if every element of a is in b.
  ;; Short-circuits on first element of a not found in b.
  (func $set_subset (export "set_subset")
    (param $a (ref $SetNode))
    (param $b (ref $SetNode))
    (result i32)

    (call $_set_subset_node (local.get $a) (local.get $b))
  )

  ;; Returns 1 if all entries in src are in other, 0 on first miss.
  (func $_set_subset_node
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
                  (call $set_has (local.get $other)
                    (struct.get $SetEntry $key
                      (ref.cast (ref $SetEntry) (local.get $child)))))
              (then (return (i32.const 0))))))

        (if (ref.test (ref $SetNode) (local.get $child))
          (then
            (if (i32.eqz
                  (call $_set_subset_node
                    (ref.cast (ref $SetNode) (local.get $child))
                    (local.get $other)))
              (then (return (i32.const 0))))))

        (if (ref.test (ref $SetCollision) (local.get $child))
          (then
            (if (i32.eqz
                  (call $_set_subset_collision
                    (struct.get $SetCollision $col_entries
                      (ref.cast (ref $SetCollision) (local.get $child)))
                    (local.get $other)))
              (then (return (i32.const 0))))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (i32.const 1)
  )

  (func $_set_subset_collision
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
              (call $set_has (local.get $other)
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
  (func $set_disjoint (export "set_disjoint")
    (param $a (ref $SetNode))
    (param $b (ref $SetNode))
    (result i32)

    (call $_set_disjoint_node (local.get $a) (local.get $b))
  )

  ;; Returns 1 if no entry in src is in other, 0 on first hit.
  (func $_set_disjoint_node
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
            (if (call $set_has (local.get $other)
                  (struct.get $SetEntry $key
                    (ref.cast (ref $SetEntry) (local.get $child))))
              (then (return (i32.const 0))))))

        (if (ref.test (ref $SetNode) (local.get $child))
          (then
            (if (i32.eqz
                  (call $_set_disjoint_node
                    (ref.cast (ref $SetNode) (local.get $child))
                    (local.get $other)))
              (then (return (i32.const 0))))))

        (if (ref.test (ref $SetCollision) (local.get $child))
          (then
            (if (i32.eqz
                  (call $_set_disjoint_collision
                    (struct.get $SetCollision $col_entries
                      (ref.cast (ref $SetCollision) (local.get $child)))
                    (local.get $other)))
              (then (return (i32.const 0))))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (i32.const 1)
  )

  (func $_set_disjoint_collision
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
        (if (call $set_has (local.get $other)
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
  (func $set_eq (export "set_eq")
    (param $a (ref $SetNode))
    (param $b (ref $SetNode))
    (result i32)

    (if (i32.ne
          (call $set_size (local.get $a))
          (call $set_size (local.get $b)))
      (then (return (i32.const 0))))

    (call $set_subset (local.get $a) (local.get $b))
  )


  ;; -- Set wrappers (user-visible API) -----------------------------------
  ;; Wrap/unwrap $SetImpl ↔ $SetNode at the boundary.

  (func $set_impl_empty (export "set_impl_empty") (result (ref $SetImpl))
    (struct.new $SetImpl (global.get $empty_node))
  )

  (func $set_impl_has (export "set_impl_has")
    (param $s (ref $SetImpl)) (param $key (ref eq))
    (result i32)
    (call $set_has (struct.get $SetImpl $node (local.get $s)) (local.get $key))
  )

  (func $set_impl_set (export "set_impl_set")
    (param $s (ref $SetImpl)) (param $key (ref eq))
    (result (ref $SetImpl))
    (struct.new $SetImpl
      (call $set_set (struct.get $SetImpl $node (local.get $s)) (local.get $key)))
  )

  (func $set_impl_remove (export "set_impl_remove")
    (param $s (ref $SetImpl)) (param $key (ref eq))
    (result (ref $SetImpl))
    (struct.new $SetImpl
      (call $set_remove (struct.get $SetImpl $node (local.get $s)) (local.get $key)))
  )

  (func $set_impl_size (export "set_impl_size")
    (param $s (ref $SetImpl)) (result i32)
    (call $set_size (struct.get $SetImpl $node (local.get $s)))
  )

  (func $set_impl_union (export "set_impl_union")
    (param $a (ref $SetImpl)) (param $b (ref $SetImpl))
    (result (ref $SetImpl))
    (struct.new $SetImpl
      (call $set_union
        (struct.get $SetImpl $node (local.get $a))
        (struct.get $SetImpl $node (local.get $b))))
  )

  (func $set_impl_intersect (export "set_impl_intersect")
    (param $a (ref $SetImpl)) (param $b (ref $SetImpl))
    (result (ref $SetImpl))
    (struct.new $SetImpl
      (call $set_intersect
        (struct.get $SetImpl $node (local.get $a))
        (struct.get $SetImpl $node (local.get $b))))
  )

  (func $set_impl_difference (export "set_impl_difference")
    (param $a (ref $SetImpl)) (param $b (ref $SetImpl))
    (result (ref $SetImpl))
    (struct.new $SetImpl
      (call $set_difference
        (struct.get $SetImpl $node (local.get $a))
        (struct.get $SetImpl $node (local.get $b))))
  )

  (func $set_impl_sym_diff (export "set_impl_sym_diff")
    (param $a (ref $SetImpl)) (param $b (ref $SetImpl))
    (result (ref $SetImpl))
    (struct.new $SetImpl
      (call $set_sym_diff
        (struct.get $SetImpl $node (local.get $a))
        (struct.get $SetImpl $node (local.get $b))))
  )

  (func $set_impl_subset (export "set_impl_subset")
    (param $a (ref $SetImpl)) (param $b (ref $SetImpl))
    (result i32)
    (call $set_subset
      (struct.get $SetImpl $node (local.get $a))
      (struct.get $SetImpl $node (local.get $b)))
  )

  (func $set_impl_disjoint (export "set_impl_disjoint")
    (param $a (ref $SetImpl)) (param $b (ref $SetImpl))
    (result i32)
    (call $set_disjoint
      (struct.get $SetImpl $node (local.get $a))
      (struct.get $SetImpl $node (local.get $b)))
  )

  (func $set_impl_eq (export "set_impl_eq")
    (param $a (ref $SetImpl)) (param $b (ref $SetImpl))
    (result i32)
    (call $set_eq
      (struct.get $SetImpl $node (local.get $a))
      (struct.get $SetImpl $node (local.get $b)))
  )

)
