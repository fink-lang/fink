;; HAMT — Hash Array Mapped Trie for fink records/dicts
;;
;; WASM GC implementation using struct and array types.
;;
;; Design:
;;   - Branching factor 32 (5 bits per level, max 7 levels for 32-bit hash)
;;   - Each node has a 32-bit bitmap and a dense children array
;;   - Children are either leaf entries (key + value) or sub-nodes
;;   - At max depth, hash collisions are stored in a flat collision node
;;   - Structural sharing: insert/delete return new nodes, unchanged
;;     subtrees are shared with the original
;;
;; Value representation:
;;   - Type hierarchy (types.wat defines opaque base types):
;;       $Rec  (from types.wat)      — opaque record type
;;       └── $RecImpl (sub $Rec)     — wrapper: single $HamtNode field
;;       $Dict (from types.wat)      — opaque dict type
;;       └── $DictImpl (sub $Dict)   — wrapper: single $HamtNode field
;;       $HamtLeaf                   — key-value pair (internal)
;;       $HamtNode                   — bitmap + children array (internal)
;;       $HamtCollision              — hash + flat leaf array (internal)
;;   - Keys and values are (ref eq) — non-nullable. This allows i31ref
;;     (for interned symbol ids) and any GC struct/array ref.
;;   - Return values are (ref null eq) where null signals "not found".
;;   - Key equality uses deep_eq (from operators.wat): i31ref → ref.eq,
;;     $Num → f64.eq, $Str → str_eq. General dict keys with
;;     user-defined Eq protocol will live in the std-lib (CPS).
;;
;; Hashing:
;;   - Imported from hashing.wat (hash_i31). Dispatches on i31ref, $Num,
;;     $Str via br_on_cast. General hashing for user-defined types via
;;     Hash protocol (future, std-lib, CPS).
;;
;; Exported functions:
;;   TODO HamtNode is internal, public interfaces should use Rec/Dict.
;;     Or these functions hould be made private _* .
;;   $hamt_empty   : () -> (ref $HamtNode)
;;   $hamt_get     : (ref $HamtNode), (ref eq) -> (ref null eq)
;;   $hamt_set     : (ref $HamtNode), (ref eq), (ref eq) -> (ref $HamtNode)
;;   $hamt_delete  : (ref $HamtNode), (ref eq) -> (ref $HamtNode)
;;   $hamt_pop     : (ref $HamtNode), (ref eq) -> (ref null eq), (ref $HamtNode)
;;   $hamt_merge   : (ref $HamtNode), (ref $HamtNode) -> (ref $HamtNode)
;;   $hamt_size    : (ref $HamtNode) -> i32
;;                   Merge src into dest. Src entries win on key conflict.
;;                   Single-traversal get+delete. Returns (value, rest).
;;                   Value is null if key absent; rest is unchanged in that case.

(module

  ;; Continuation dispatch — provided by the compiler's emitted module.
  (import "@fink/user" "_croc_0" (func $croc_0 (param (ref null any))))
  (import "@fink/user" "_croc_1" (func $croc_1 (param (ref null any)) (param (ref null any))))
  (import "@fink/user" "_croc_2" (func $croc_2 (param (ref null any)) (param (ref null any)) (param (ref null any))))

  ;; -- Type definitions -----------------------------------------------

  ;; Internal HAMT types. These are implementation details — user code
  ;; sees $Rec / $Dict (from types.wat) via the wrapper types below.
  ;; Children array uses structref as the common base for leaves, nodes,
  ;; and collision nodes.

  ;; $HamtLeaf — key-value pair.
  ;; Key and val are (ref eq) — non-nullable.
  (type $HamtLeaf (struct
    (field $key (ref eq))
    (field $val (ref eq))
  ))

  ;; $HamtChildren — dense array of struct refs (leaves, nodes, or collisions).
  (type $HamtChildren (array (mut (ref null struct))))

  (rec
    ;; $HamtNode — bitmap + dense children array.
    ;; bitmap bit i is set iff hash fragment i is occupied.
    ;; children array length = popcount(bitmap).
    (type $HamtNode (struct
      (field $bitmap (mut i32))
      (field $children (ref $HamtChildren))
    ))

    ;; $HamtCollision — flat array of leaves that share the same hash.
    ;; Used at max trie depth when multiple keys hash identically.
    (type $HamtCollision (struct
      (field $col_hash i32)
      (field $col_leaves (ref $HamtChildren))
    ))

    ;; -- Wrapper types (user-visible) ------------------------------------
    ;; Single-field wrappers that participate in the canonical type hierarchy.
    ;; Casting happens only at the runtime API boundary.

    ;; $RecImpl — wraps $HamtNode as a $Rec (from types.wat).
    (type $RecImpl (sub $Rec (struct
      (field $hamt (ref $HamtNode))
    )))

    ;; $DictImpl — wraps $HamtNode as a $Dict (from types.wat).
    (type $DictImpl (sub $Dict (struct
      (field $hamt (ref $HamtNode))
    )))
  )

  ;; Max depth: 7 levels (0..6) consume 35 bits. For a 32-bit hash,
  ;; level 6 uses only the top 2 bits. Beyond level 6 we must use
  ;; collision nodes.
  ;; We use depth >= 7 as the trigger for collision handling, but in
  ;; practice collisions happen when two keys have identical hashes
  ;; and we exhaust all 7 trie levels.


  ;; -- Imports ----------------------------------------------------------

  ;; -- Helpers --------------------------------------------------------

  ;; hash_fragment — extract 5-bit fragment at given depth (0-6)
  ;; fragment = (hash >> (depth * 5)) & 0x1f
  (func $_hamt_hash_fragment (param $hash i32) (param $depth i32) (result i32)
    local.get $hash
    local.get $depth
    i32.const 5
    i32.mul
    i32.shr_u
    i32.const 0x1f
    i32.and
  )

  ;; bit_index — index into the dense children array for a given
  ;; bitmap and hash fragment.
  ;; = popcount(bitmap & ((1 << fragment) - 1))
  (func $_hamt_bit_index (param $bitmap i32) (param $fragment i32) (result i32)
    local.get $bitmap
    i32.const 1
    local.get $fragment
    i32.shl
    i32.const 1
    i32.sub
    i32.and
    i32.popcnt
  )


  ;; -- Collision helpers ----------------------------------------------

  ;; Scan a collision node's leaves for a key. Returns the index,
  ;; or -1 if not found.
  (func $_hamt_collision_find
    (param $leaves (ref $HamtChildren))
    (param $key (ref eq))
    (result i32)

    (local $i i32)
    (local $len i32)
    (local.set $len (array.len (local.get $leaves)))
    (local.set $i (i32.const 0))
    (block $not_found
      (loop $scan
        (br_if $not_found
          (i32.ge_u (local.get $i) (local.get $len)))
        (if (call $deep_eq
              (struct.get $HamtLeaf $key
                (ref.cast (ref $HamtLeaf)
                  (array.get $HamtChildren
                    (local.get $leaves)
                    (local.get $i))))
              (local.get $key))
          (then (return (local.get $i))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $scan)))
    (i32.const -1)
  )


  ;; -- Empty node -----------------------------------------------------

  ;; The canonical empty node — bitmap 0, zero-length children array.
  (global $empty_node (ref $HamtNode)
    (struct.new $HamtNode
      (i32.const 0)
      (array.new_fixed $HamtChildren 0)
    )
  )

  (func $hamt_empty (export "hamt_empty") (result (ref $HamtNode))
    global.get $empty_node
  )


  ;; -- Get ------------------------------------------------------------

  ;; Look up a key. Returns null if not found.
  (func $hamt_get (export "hamt_get")
    (param $current (ref $HamtNode))
    (param $key (ref eq))
    (result (ref null eq))

    (local $h i32)
    (local $depth i32)
    (local $fragment i32)
    (local $bit i32)
    (local $bitmap i32)
    (local $idx i32)
    (local $child (ref null struct))
    (local $col_idx i32)

    ;; compute hash once
    (local.set $h (call $hash_i31(local.get $key)))
    (local.set $depth (i32.const 0))

    (block $not_found
      (loop $descend
        ;; extract fragment for this depth
        (local.set $fragment
          (call $_hamt_hash_fragment (local.get $h) (local.get $depth)))

        ;; check bitmap
        (local.set $bitmap
          (struct.get $HamtNode $bitmap (local.get $current)))
        (local.set $bit
          (i32.shl (i32.const 1) (local.get $fragment)))

        ;; if bit not set, key is absent
        (br_if $not_found
          (i32.eqz (i32.and (local.get $bitmap) (local.get $bit))))

        ;; index into dense array
        (local.set $idx
          (call $_hamt_bit_index (local.get $bitmap) (local.get $fragment)))

        ;; get child
        (local.set $child
          (array.get $HamtChildren
            (struct.get $HamtNode $children (local.get $current))
            (local.get $idx)))

        ;; if child is a leaf, check key equality
        (if (ref.test (ref $HamtLeaf) (local.get $child))
          (then
            (if (call $deep_eq
                  (struct.get $HamtLeaf $key
                    (ref.cast (ref $HamtLeaf) (local.get $child)))
                  (local.get $key))
              (then
                (return
                  (struct.get $HamtLeaf $val
                    (ref.cast (ref $HamtLeaf) (local.get $child)))))
              (else
                (br $not_found)))))

        ;; if child is a collision node, scan it
        (if (ref.test (ref $HamtCollision) (local.get $child))
          (then
            (local.set $col_idx
              (call $_hamt_collision_find
                (struct.get $HamtCollision $col_leaves
                  (ref.cast (ref $HamtCollision) (local.get $child)))
                (local.get $key)))
            (if (i32.ge_s (local.get $col_idx) (i32.const 0))
              (then
                (return
                  (struct.get $HamtLeaf $val
                    (ref.cast (ref $HamtLeaf)
                      (array.get $HamtChildren
                        (struct.get $HamtCollision $col_leaves
                          (ref.cast (ref $HamtCollision) (local.get $child)))
                        (local.get $col_idx)))))))
            (br $not_found)))

        ;; child is a sub-node — descend
        (local.set $current
          (ref.cast (ref $HamtNode) (local.get $child)))
        (local.set $depth
          (i32.add (local.get $depth) (i32.const 1)))
        (br $descend)
      )
    )

    ;; not found
    (ref.null eq)
  )


  ;; -- Set ------------------------------------------------------------

  ;; Insert or update a key-value pair. Returns a new node (structural
  ;; sharing with the original for unchanged subtrees).
  (func $hamt_set (export "hamt_set")
    (param $current (ref $HamtNode))
    (param $key (ref eq))
    (param $val (ref eq))
    (result (ref $HamtNode))

    (local $h i32)

    (local.set $h (call $hash_i31(local.get $key)))
    (call $_hamt_set_inner
      (local.get $current)
      (local.get $key)
      (local.get $val)
      (local.get $h)
      (i32.const 0))
  )

  (func $_hamt_set_inner
    (param $current (ref $HamtNode))
    (param $key (ref eq))
    (param $val (ref eq))
    (param $h i32)
    (param $depth i32)
    (result (ref $HamtNode))

    (local $fragment i32)
    (local $bit i32)
    (local $bitmap i32)
    (local $idx i32)
    (local $old_children (ref $HamtChildren))
    (local $new_children (ref $HamtChildren))
    (local $old_len i32)
    (local $child (ref null struct))
    (local $new_leaf (ref $HamtLeaf))
    (local $i i32)
    (local $col_leaves (ref $HamtChildren))
    (local $col_idx i32)
    (local $new_col_leaves (ref $HamtChildren))

    (local.set $fragment
      (call $_hamt_hash_fragment (local.get $h) (local.get $depth)))
    (local.set $bit
      (i32.shl (i32.const 1) (local.get $fragment)))
    (local.set $bitmap
      (struct.get $HamtNode $bitmap (local.get $current)))
    (local.set $idx
      (call $_hamt_bit_index (local.get $bitmap) (local.get $fragment)))
    (local.set $old_children
      (struct.get $HamtNode $children (local.get $current)))
    (local.set $old_len
      (array.len (local.get $old_children)))

    ;; new leaf to insert
    (local.set $new_leaf
      (struct.new $HamtLeaf (local.get $key) (local.get $val)))

    ;; bit not set — insert new leaf at idx
    (if (i32.eqz (i32.and (local.get $bitmap) (local.get $bit)))
      (then
        ;; create new children array with one more slot
        (local.set $new_children
          (array.new $HamtChildren
            (local.get $new_leaf)  ;; placeholder fill
            (i32.add (local.get $old_len) (i32.const 1))))

        ;; copy elements before idx
        (local.set $i (i32.const 0))
        (block $done_before
          (loop $copy_before
            (br_if $done_before
              (i32.ge_u (local.get $i) (local.get $idx)))
            (array.set $HamtChildren
              (local.get $new_children)
              (local.get $i)
              (array.get $HamtChildren
                (local.get $old_children)
                (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $copy_before)))

        ;; copy elements after idx
        (local.set $i (local.get $idx))
        (block $done_after
          (loop $copy_after
            (br_if $done_after
              (i32.ge_u (local.get $i) (local.get $old_len)))
            (array.set $HamtChildren
              (local.get $new_children)
              (i32.add (local.get $i) (i32.const 1))
              (array.get $HamtChildren
                (local.get $old_children)
                (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $copy_after)))

        ;; new_children[idx] = new_leaf
        (array.set $HamtChildren
          (local.get $new_children)
          (local.get $idx)
          (local.get $new_leaf))

        (return
          (struct.new $HamtNode
            (i32.or (local.get $bitmap) (local.get $bit))
            (local.get $new_children))))
    )

    ;; bit is set — slot exists, get existing child
    (local.set $child
      (array.get $HamtChildren
        (local.get $old_children)
        (local.get $idx)))

    ;; if existing child is a collision node
    (if (ref.test (ref $HamtCollision) (local.get $child))
      (then
        (local.set $col_leaves
          (struct.get $HamtCollision $col_leaves
            (ref.cast (ref $HamtCollision) (local.get $child))))
        (local.set $col_idx
          (call $_hamt_collision_find (local.get $col_leaves) (local.get $key)))

        (if (i32.ge_s (local.get $col_idx) (i32.const 0))
          (then
            ;; key exists in collision — replace leaf at col_idx
            (local.set $new_col_leaves
              (array.new $HamtChildren
                (local.get $new_leaf)
                (array.len (local.get $col_leaves))))
            (array.copy $HamtChildren $HamtChildren
              (local.get $new_col_leaves) (i32.const 0)
              (local.get $col_leaves) (i32.const 0)
              (array.len (local.get $col_leaves)))
            (array.set $HamtChildren
              (local.get $new_col_leaves)
              (local.get $col_idx)
              (local.get $new_leaf))

            ;; build new parent with updated collision node
            (local.set $new_children
              (array.new $HamtChildren
                (local.get $new_leaf) ;; placeholder
                (local.get $old_len)))
            (array.copy $HamtChildren $HamtChildren
              (local.get $new_children) (i32.const 0)
              (local.get $old_children) (i32.const 0)
              (local.get $old_len))
            (array.set $HamtChildren
              (local.get $new_children)
              (local.get $idx)
              (struct.new $HamtCollision
                (struct.get $HamtCollision $col_hash
                  (ref.cast (ref $HamtCollision) (local.get $child)))
                (local.get $new_col_leaves)))
            (return
              (struct.new $HamtNode
                (local.get $bitmap)
                (local.get $new_children))))
          (else
            ;; key not in collision — append new leaf
            (local.set $new_col_leaves
              (array.new $HamtChildren
                (local.get $new_leaf)
                (i32.add (array.len (local.get $col_leaves)) (i32.const 1))))
            (array.copy $HamtChildren $HamtChildren
              (local.get $new_col_leaves) (i32.const 0)
              (local.get $col_leaves) (i32.const 0)
              (array.len (local.get $col_leaves)))

            ;; build new parent with extended collision node
            (local.set $new_children
              (array.new $HamtChildren
                (local.get $new_leaf) ;; placeholder
                (local.get $old_len)))
            (array.copy $HamtChildren $HamtChildren
              (local.get $new_children) (i32.const 0)
              (local.get $old_children) (i32.const 0)
              (local.get $old_len))
            (array.set $HamtChildren
              (local.get $new_children)
              (local.get $idx)
              (struct.new $HamtCollision
                (struct.get $HamtCollision $col_hash
                  (ref.cast (ref $HamtCollision) (local.get $child)))
                (local.get $new_col_leaves)))
            (return
              (struct.new $HamtNode
                (local.get $bitmap)
                (local.get $new_children)))))))

    ;; if existing child is a leaf
    (if (ref.test (ref $HamtLeaf) (local.get $child))
      (then
        ;; same key — replace value
        (if (call $deep_eq
              (struct.get $HamtLeaf $key
                (ref.cast (ref $HamtLeaf) (local.get $child)))
              (local.get $key))
          (then
            ;; clone children array, replace leaf at idx
            (local.set $new_children
              (array.new $HamtChildren
                (local.get $new_leaf) ;; placeholder
                (local.get $old_len)))
            (array.copy $HamtChildren $HamtChildren
              (local.get $new_children) (i32.const 0)
              (local.get $old_children) (i32.const 0)
              (local.get $old_len))
            (array.set $HamtChildren
              (local.get $new_children)
              (local.get $idx)
              (local.get $new_leaf))
            (return
              (struct.new $HamtNode
                (local.get $bitmap)
                (local.get $new_children))))
          (else
            ;; different key at this slot — need to push deeper
            ;; if at max depth, create a collision node
            (if (i32.ge_u (local.get $depth) (i32.const 6))
              (then
                ;; create collision node with both leaves
                (local.set $new_children
                  (array.new $HamtChildren
                    (local.get $new_leaf) ;; placeholder
                    (local.get $old_len)))
                (array.copy $HamtChildren $HamtChildren
                  (local.get $new_children) (i32.const 0)
                  (local.get $old_children) (i32.const 0)
                  (local.get $old_len))
                (array.set $HamtChildren
                  (local.get $new_children)
                  (local.get $idx)
                  (struct.new $HamtCollision
                    (local.get $h)
                    (array.new_fixed $HamtChildren 2
                      (local.get $child)    ;; existing leaf
                      (local.get $new_leaf) ;; new leaf
                    )))
                (return
                  (struct.new $HamtNode
                    (local.get $bitmap)
                    (local.get $new_children))))
              (else
                ;; not at max depth — push both into a sub-node
                (local.set $new_children
                  (array.new $HamtChildren
                    (local.get $new_leaf) ;; placeholder
                    (local.get $old_len)))
                (array.copy $HamtChildren $HamtChildren
                  (local.get $new_children) (i32.const 0)
                  (local.get $old_children) (i32.const 0)
                  (local.get $old_len))
                (array.set $HamtChildren
                  (local.get $new_children)
                  (local.get $idx)
                  (call $_hamt_set_inner
                    (call $_hamt_set_inner
                      (call $hamt_empty)
                      (struct.get $HamtLeaf $key
                        (ref.cast (ref $HamtLeaf) (local.get $child)))
                      (struct.get $HamtLeaf $val
                        (ref.cast (ref $HamtLeaf) (local.get $child)))
                      (call $hash_i31
                        (struct.get $HamtLeaf $key
                          (ref.cast (ref $HamtLeaf) (local.get $child))))
                      (i32.add (local.get $depth) (i32.const 1)))
                    (local.get $key)
                    (local.get $val)
                    (local.get $h)
                    (i32.add (local.get $depth) (i32.const 1))))
                (return
                  (struct.new $HamtNode
                    (local.get $bitmap)
                    (local.get $new_children)))))))))

    ;; existing child is a sub-node — recurse
    (local.set $new_children
      (array.new $HamtChildren
        (local.get $new_leaf) ;; placeholder
        (local.get $old_len)))
    (array.copy $HamtChildren $HamtChildren
      (local.get $new_children) (i32.const 0)
      (local.get $old_children) (i32.const 0)
      (local.get $old_len))
    (array.set $HamtChildren
      (local.get $new_children)
      (local.get $idx)
      (call $_hamt_set_inner
        (ref.cast (ref $HamtNode) (local.get $child))
        (local.get $key)
        (local.get $val)
        (local.get $h)
        (i32.add (local.get $depth) (i32.const 1))))
    (struct.new $HamtNode
      (local.get $bitmap)
      (local.get $new_children))
  )


  ;; -- Delete ---------------------------------------------------------

  ;; Remove a key. Returns a new node (structural sharing).
  ;; If the key is not present, returns the original node unchanged.
  (func $hamt_delete (export "hamt_delete")
    (param $current (ref $HamtNode))
    (param $key (ref eq))
    (result (ref $HamtNode))

    (local $h i32)
    (local.set $h (call $hash_i31(local.get $key)))
    (call $_hamt_delete_inner
      (local.get $current)
      (local.get $key)
      (local.get $h)
      (i32.const 0))
  )

  (func $_hamt_delete_inner
    (param $current (ref $HamtNode))
    (param $key (ref eq))
    (param $h i32)
    (param $depth i32)
    (result (ref $HamtNode))

    (local $fragment i32)
    (local $bit i32)
    (local $bitmap i32)
    (local $idx i32)
    (local $old_children (ref $HamtChildren))
    (local $new_children (ref $HamtChildren))
    (local $old_len i32)
    (local $child (ref null struct))
    (local $sub_result (ref $HamtNode))
    (local $i i32)
    (local $col_leaves (ref $HamtChildren))
    (local $col_idx i32)
    (local $col_len i32)
    (local $new_col_leaves (ref $HamtChildren))

    (local.set $fragment
      (call $_hamt_hash_fragment (local.get $h) (local.get $depth)))
    (local.set $bit
      (i32.shl (i32.const 1) (local.get $fragment)))
    (local.set $bitmap
      (struct.get $HamtNode $bitmap (local.get $current)))
    (local.set $old_children
      (struct.get $HamtNode $children (local.get $current)))
    (local.set $old_len
      (array.len (local.get $old_children)))

    ;; bit not set — key absent, return unchanged
    (if (i32.eqz (i32.and (local.get $bitmap) (local.get $bit)))
      (then (return (local.get $current))))

    (local.set $idx
      (call $_hamt_bit_index (local.get $bitmap) (local.get $fragment)))
    (local.set $child
      (array.get $HamtChildren
        (local.get $old_children)
        (local.get $idx)))

    ;; child is a collision node
    (if (ref.test (ref $HamtCollision) (local.get $child))
      (then
        (local.set $col_leaves
          (struct.get $HamtCollision $col_leaves
            (ref.cast (ref $HamtCollision) (local.get $child))))
        (local.set $col_len
          (array.len (local.get $col_leaves)))
        (local.set $col_idx
          (call $_hamt_collision_find (local.get $col_leaves) (local.get $key)))

        ;; key not in collision — unchanged
        (if (i32.lt_s (local.get $col_idx) (i32.const 0))
          (then (return (local.get $current))))

        ;; if collision has exactly 2 leaves, removing one leaves a
        ;; single leaf — replace collision with that leaf
        (if (i32.eq (local.get $col_len) (i32.const 2))
          (then
            (local.set $new_children
              (array.new $HamtChildren
                ;; the surviving leaf (the other one)
                (array.get $HamtChildren
                  (local.get $col_leaves)
                  (if (result i32)
                    (i32.eq (local.get $col_idx) (i32.const 0))
                    (then (i32.const 1))
                    (else (i32.const 0))))
                (local.get $old_len)))
            (array.copy $HamtChildren $HamtChildren
              (local.get $new_children) (i32.const 0)
              (local.get $old_children) (i32.const 0)
              (local.get $old_len))
            ;; replace collision slot with surviving leaf
            (array.set $HamtChildren
              (local.get $new_children)
              (local.get $idx)
              (array.get $HamtChildren
                (local.get $col_leaves)
                (if (result i32)
                  (i32.eq (local.get $col_idx) (i32.const 0))
                  (then (i32.const 1))
                  (else (i32.const 0)))))
            (return
              (struct.new $HamtNode
                (local.get $bitmap)
                (local.get $new_children)))))

        ;; collision has 3+ leaves — remove one, keep collision
        (local.set $new_col_leaves
          (array.new $HamtChildren
            (array.get $HamtChildren
              (local.get $col_leaves)
              (if (result i32)
                (i32.eq (local.get $col_idx) (i32.const 0))
                (then (i32.const 1))
                (else (i32.const 0))))
            (i32.sub (local.get $col_len) (i32.const 1))))

        ;; copy elements before col_idx
        (local.set $i (i32.const 0))
        (block $done_before
          (loop $copy_before
            (br_if $done_before
              (i32.ge_u (local.get $i) (local.get $col_idx)))
            (array.set $HamtChildren
              (local.get $new_col_leaves)
              (local.get $i)
              (array.get $HamtChildren
                (local.get $col_leaves)
                (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $copy_before)))

        ;; copy elements after col_idx (shifted down)
        (local.set $i (i32.add (local.get $col_idx) (i32.const 1)))
        (block $done_after
          (loop $copy_after
            (br_if $done_after
              (i32.ge_u (local.get $i) (local.get $col_len)))
            (array.set $HamtChildren
              (local.get $new_col_leaves)
              (i32.sub (local.get $i) (i32.const 1))
              (array.get $HamtChildren
                (local.get $col_leaves)
                (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $copy_after)))

        ;; build parent with updated collision
        (local.set $new_children
          (array.new $HamtChildren
            (local.get $child) ;; placeholder
            (local.get $old_len)))
        (array.copy $HamtChildren $HamtChildren
          (local.get $new_children) (i32.const 0)
          (local.get $old_children) (i32.const 0)
          (local.get $old_len))
        (array.set $HamtChildren
          (local.get $new_children)
          (local.get $idx)
          (struct.new $HamtCollision
            (struct.get $HamtCollision $col_hash
              (ref.cast (ref $HamtCollision) (local.get $child)))
            (local.get $new_col_leaves)))
        (return
          (struct.new $HamtNode
            (local.get $bitmap)
            (local.get $new_children)))))

    ;; child is a leaf
    (if (ref.test (ref $HamtLeaf) (local.get $child))
      (then
        ;; key mismatch — return unchanged
        (if (i32.eqz
              (call $deep_eq
                (struct.get $HamtLeaf $key
                  (ref.cast (ref $HamtLeaf) (local.get $child)))
                (local.get $key)))
          (then (return (local.get $current))))

        ;; key matches — remove this slot
        (if (i32.eq (local.get $old_len) (i32.const 1))
          (then
            ;; last entry — return empty node
            (return (call $hamt_empty))))

        ;; create new array with one fewer slot
        (local.set $new_children
          (array.new $HamtChildren
            ;; dummy ref to fill — use first element that isn't
            ;; the one we're removing
            (array.get $HamtChildren
              (local.get $old_children)
              (if (result i32)
                (i32.eq (local.get $idx) (i32.const 0))
                (then (i32.const 1))
                (else (i32.const 0))))
            (i32.sub (local.get $old_len) (i32.const 1))))

        ;; copy elements before idx
        (local.set $i (i32.const 0))
        (block $done_before
          (loop $copy_before
            (br_if $done_before
              (i32.ge_u (local.get $i) (local.get $idx)))
            (array.set $HamtChildren
              (local.get $new_children)
              (local.get $i)
              (array.get $HamtChildren
                (local.get $old_children)
                (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $copy_before)))

        ;; copy elements after idx (shifted down by 1)
        (local.set $i (i32.add (local.get $idx) (i32.const 1)))
        (block $done_after
          (loop $copy_after
            (br_if $done_after
              (i32.ge_u (local.get $i) (local.get $old_len)))
            (array.set $HamtChildren
              (local.get $new_children)
              (i32.sub (local.get $i) (i32.const 1))
              (array.get $HamtChildren
                (local.get $old_children)
                (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $copy_after)))

        (return
          (struct.new $HamtNode
            (i32.xor (local.get $bitmap) (local.get $bit))
            (local.get $new_children)))))

    ;; child is a sub-node — recurse
    (local.set $sub_result
      (call $_hamt_delete_inner
        (ref.cast (ref $HamtNode) (local.get $child))
        (local.get $key)
        (local.get $h)
        (i32.add (local.get $depth) (i32.const 1))))

    ;; if unchanged, return original
    (if (ref.eq (local.get $sub_result)
                (ref.cast (ref $HamtNode) (local.get $child)))
      (then (return (local.get $current))))

    ;; clone children, replace sub-node with result
    (local.set $new_children
      (array.new $HamtChildren
        (local.get $sub_result) ;; placeholder
        (local.get $old_len)))
    (array.copy $HamtChildren $HamtChildren
      (local.get $new_children) (i32.const 0)
      (local.get $old_children) (i32.const 0)
      (local.get $old_len))
    (array.set $HamtChildren
      (local.get $new_children)
      (local.get $idx)
      (local.get $sub_result))
    (struct.new $HamtNode
      (local.get $bitmap)
      (local.get $new_children))
  )


  ;; -- Pop ------------------------------------------------------------

  ;; Single-traversal get+delete for pattern matching destructure:
  ;;   {a, ...rest} = foo  →  (a_val, rest) = hamt_pop(foo, :a)
  ;;
  ;; Returns (value, rest_node) via multi-value.
  ;; If key is absent, returns (null, original_node).
  (func $hamt_pop (export "hamt_pop")
    (param $current (ref $HamtNode))
    (param $key (ref eq))
    (result (ref null eq) (ref $HamtNode))

    (local $h i32)
    (local.set $h (call $hash_i31(local.get $key)))
    (call $_hamt_pop_inner
      (local.get $current)
      (local.get $key)
      (local.get $h)
      (i32.const 0))
  )

  (func $_hamt_pop_inner
    (param $current (ref $HamtNode))
    (param $key (ref eq))
    (param $h i32)
    (param $depth i32)
    (result (ref null eq) (ref $HamtNode))

    (local $fragment i32)
    (local $bit i32)
    (local $bitmap i32)
    (local $idx i32)
    (local $old_children (ref $HamtChildren))
    (local $new_children (ref $HamtChildren))
    (local $old_len i32)
    (local $child (ref null struct))
    (local $i i32)
    (local $sub_val (ref null eq))
    (local $sub_rest (ref $HamtNode))
    (local $col_leaves (ref $HamtChildren))
    (local $col_idx i32)
    (local $col_len i32)
    (local $new_col_leaves (ref $HamtChildren))

    (local.set $fragment
      (call $_hamt_hash_fragment (local.get $h) (local.get $depth)))
    (local.set $bit
      (i32.shl (i32.const 1) (local.get $fragment)))
    (local.set $bitmap
      (struct.get $HamtNode $bitmap (local.get $current)))
    (local.set $old_children
      (struct.get $HamtNode $children (local.get $current)))
    (local.set $old_len
      (array.len (local.get $old_children)))

    ;; bit not set — key absent
    (if (i32.eqz (i32.and (local.get $bitmap) (local.get $bit)))
      (then
        (return (ref.null eq) (local.get $current))))

    (local.set $idx
      (call $_hamt_bit_index (local.get $bitmap) (local.get $fragment)))
    (local.set $child
      (array.get $HamtChildren
        (local.get $old_children)
        (local.get $idx)))

    ;; child is a collision node
    (if (ref.test (ref $HamtCollision) (local.get $child))
      (then
        (local.set $col_leaves
          (struct.get $HamtCollision $col_leaves
            (ref.cast (ref $HamtCollision) (local.get $child))))
        (local.set $col_len
          (array.len (local.get $col_leaves)))
        (local.set $col_idx
          (call $_hamt_collision_find (local.get $col_leaves) (local.get $key)))

        ;; not found in collision
        (if (i32.lt_s (local.get $col_idx) (i32.const 0))
          (then
            (return (ref.null eq) (local.get $current))))

        ;; collision has exactly 2 — removing one leaves a single leaf
        (if (i32.eq (local.get $col_len) (i32.const 2))
          (then
            (local.set $new_children
              (array.new $HamtChildren
                (array.get $HamtChildren
                  (local.get $col_leaves)
                  (if (result i32)
                    (i32.eq (local.get $col_idx) (i32.const 0))
                    (then (i32.const 1))
                    (else (i32.const 0))))
                (local.get $old_len)))
            (array.copy $HamtChildren $HamtChildren
              (local.get $new_children) (i32.const 0)
              (local.get $old_children) (i32.const 0)
              (local.get $old_len))
            (array.set $HamtChildren
              (local.get $new_children)
              (local.get $idx)
              (array.get $HamtChildren
                (local.get $col_leaves)
                (if (result i32)
                  (i32.eq (local.get $col_idx) (i32.const 0))
                  (then (i32.const 1))
                  (else (i32.const 0)))))
            (return
              (struct.get $HamtLeaf $val
                (ref.cast (ref $HamtLeaf)
                  (array.get $HamtChildren
                    (local.get $col_leaves)
                    (local.get $col_idx))))
              (struct.new $HamtNode
                (local.get $bitmap)
                (local.get $new_children)))))

        ;; collision has 3+ — remove one, keep collision
        (local.set $new_col_leaves
          (array.new $HamtChildren
            (array.get $HamtChildren
              (local.get $col_leaves)
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
            (array.set $HamtChildren
              (local.get $new_col_leaves)
              (local.get $i)
              (array.get $HamtChildren
                (local.get $col_leaves)
                (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $copy_before)))

        (local.set $i (i32.add (local.get $col_idx) (i32.const 1)))
        (block $done_after
          (loop $copy_after
            (br_if $done_after
              (i32.ge_u (local.get $i) (local.get $col_len)))
            (array.set $HamtChildren
              (local.get $new_col_leaves)
              (i32.sub (local.get $i) (i32.const 1))
              (array.get $HamtChildren
                (local.get $col_leaves)
                (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $copy_after)))

        (local.set $new_children
          (array.new $HamtChildren
            (local.get $child) ;; placeholder
            (local.get $old_len)))
        (array.copy $HamtChildren $HamtChildren
          (local.get $new_children) (i32.const 0)
          (local.get $old_children) (i32.const 0)
          (local.get $old_len))
        (array.set $HamtChildren
          (local.get $new_children)
          (local.get $idx)
          (struct.new $HamtCollision
            (struct.get $HamtCollision $col_hash
              (ref.cast (ref $HamtCollision) (local.get $child)))
            (local.get $new_col_leaves)))
        (return
          (struct.get $HamtLeaf $val
            (ref.cast (ref $HamtLeaf)
              (array.get $HamtChildren
                (local.get $col_leaves)
                (local.get $col_idx))))
          (struct.new $HamtNode
            (local.get $bitmap)
            (local.get $new_children)))))

    ;; child is a leaf
    (if (ref.test (ref $HamtLeaf) (local.get $child))
      (then
        ;; key mismatch — absent
        (if (i32.eqz
              (call $deep_eq
                (struct.get $HamtLeaf $key
                  (ref.cast (ref $HamtLeaf) (local.get $child)))
                (local.get $key)))
          (then
            (return (ref.null eq) (local.get $current))))

        ;; key matches — return value and build rest without this key

        ;; last entry — return value + empty
        (if (i32.eq (local.get $old_len) (i32.const 1))
          (then
            (return
              (struct.get $HamtLeaf $val
                (ref.cast (ref $HamtLeaf) (local.get $child)))
              (call $hamt_empty))))

        ;; create new array with one fewer slot
        (local.set $new_children
          (array.new $HamtChildren
            (array.get $HamtChildren
              (local.get $old_children)
              (if (result i32)
                (i32.eq (local.get $idx) (i32.const 0))
                (then (i32.const 1))
                (else (i32.const 0))))
            (i32.sub (local.get $old_len) (i32.const 1))))

        ;; copy elements before idx
        (local.set $i (i32.const 0))
        (block $done_before
          (loop $copy_before
            (br_if $done_before
              (i32.ge_u (local.get $i) (local.get $idx)))
            (array.set $HamtChildren
              (local.get $new_children)
              (local.get $i)
              (array.get $HamtChildren
                (local.get $old_children)
                (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $copy_before)))

        ;; copy elements after idx (shifted down by 1)
        (local.set $i (i32.add (local.get $idx) (i32.const 1)))
        (block $done_after
          (loop $copy_after
            (br_if $done_after
              (i32.ge_u (local.get $i) (local.get $old_len)))
            (array.set $HamtChildren
              (local.get $new_children)
              (i32.sub (local.get $i) (i32.const 1))
              (array.get $HamtChildren
                (local.get $old_children)
                (local.get $i)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $copy_after)))

        (return
          (struct.get $HamtLeaf $val
            (ref.cast (ref $HamtLeaf) (local.get $child)))
          (struct.new $HamtNode
            (i32.xor (local.get $bitmap) (local.get $bit))
            (local.get $new_children)))))

    ;; child is a sub-node — recurse
    (call $_hamt_pop_inner
      (ref.cast (ref $HamtNode) (local.get $child))
      (local.get $key)
      (local.get $h)
      (i32.add (local.get $depth) (i32.const 1)))
    ;; multi-value on stack: (val, sub_rest)
    (local.set $sub_rest)
    (local.set $sub_val)

    ;; if sub_rest unchanged, key was absent in subtree
    (if (ref.eq (local.get $sub_rest)
                (ref.cast (ref $HamtNode) (local.get $child)))
      (then
        (return (local.get $sub_val) (local.get $current))))

    ;; clone children, replace sub-node with sub_rest
    (local.set $new_children
      (array.new $HamtChildren
        (local.get $sub_rest) ;; placeholder
        (local.get $old_len)))
    (array.copy $HamtChildren $HamtChildren
      (local.get $new_children) (i32.const 0)
      (local.get $old_children) (i32.const 0)
      (local.get $old_len))
    (array.set $HamtChildren
      (local.get $new_children)
      (local.get $idx)
      (local.get $sub_rest))

    (local.get $sub_val)
    (struct.new $HamtNode
      (local.get $bitmap)
      (local.get $new_children))
  )


  ;; -- Merge ----------------------------------------------------------

  ;; Merge all entries from src into dest. Src wins on key conflict.
  ;;   {..dest, ..src}  →  hamt_merge(dest, src)
  ;;
  ;; Walks src's tree and calls hamt_set for each leaf found.
  (func $hamt_merge (export "hamt_merge")
    (param $dest (ref $HamtNode))
    (param $src (ref $HamtNode))
    (result (ref $HamtNode))

    (call $_hamt_merge_node (local.get $dest) (local.get $src))
  )

  ;; Walk a source node, inserting each leaf into dest.
  (func $_hamt_merge_node
    (param $dest (ref $HamtNode))
    (param $src (ref $HamtNode))
    (result (ref $HamtNode))

    (local $children (ref $HamtChildren))
    (local $len i32)
    (local $i i32)
    (local $child (ref null struct))

    (local.set $children
      (struct.get $HamtNode $children (local.get $src)))
    (local.set $len
      (array.len (local.get $children)))

    ;; empty source — return dest unchanged
    (if (i32.eqz (local.get $len))
      (then (return (local.get $dest))))

    (local.set $i (i32.const 0))
    (block $done
      (loop $walk
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $len)))

        (local.set $child
          (array.get $HamtChildren
            (local.get $children)
            (local.get $i)))

        ;; leaf — insert into dest
        (if (ref.test (ref $HamtLeaf) (local.get $child))
          (then
            (local.set $dest
              (call $hamt_set
                (local.get $dest)
                (struct.get $HamtLeaf $key
                  (ref.cast (ref $HamtLeaf) (local.get $child)))
                (struct.get $HamtLeaf $val
                  (ref.cast (ref $HamtLeaf) (local.get $child)))))))

        ;; sub-node — recurse
        (if (ref.test (ref $HamtNode) (local.get $child))
          (then
            (local.set $dest
              (call $_hamt_merge_node
                (local.get $dest)
                (ref.cast (ref $HamtNode) (local.get $child))))))

        ;; collision node — insert all its leaves
        (if (ref.test (ref $HamtCollision) (local.get $child))
          (then
            (local.set $dest
              (call $_hamt_merge_collision
                (local.get $dest)
                (struct.get $HamtCollision $col_leaves
                  (ref.cast (ref $HamtCollision) (local.get $child)))))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $dest)
  )

  ;; Insert all leaves from a collision node's array into dest.
  (func $_hamt_merge_collision
    (param $dest (ref $HamtNode))
    (param $leaves (ref $HamtChildren))
    (result (ref $HamtNode))

    (local $len i32)
    (local $i i32)

    (local.set $len (array.len (local.get $leaves)))
    (local.set $i (i32.const 0))
    (block $done
      (loop $walk
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $dest
          (call $hamt_set
            (local.get $dest)
            (struct.get $HamtLeaf $key
              (ref.cast (ref $HamtLeaf)
                (array.get $HamtChildren
                  (local.get $leaves)
                  (local.get $i))))
            (struct.get $HamtLeaf $val
              (ref.cast (ref $HamtLeaf)
                (array.get $HamtChildren
                  (local.get $leaves)
                  (local.get $i))))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $dest)
  )


  ;; -- Size -----------------------------------------------------------

  ;; Count the number of key-value entries in the HAMT.
  ;; Walks the tree, counting leaves and collision entries.
  (func $hamt_size (export "hamt_size")
    (param $node (ref $HamtNode))
    (result i32)

    (call $_hamt_size_node (local.get $node))
  )

  (func $_hamt_size_node
    (param $node (ref $HamtNode))
    (result i32)

    (local $children (ref $HamtChildren))
    (local $len i32)
    (local $i i32)
    (local $count i32)
    (local $child (ref null struct))

    (local.set $children
      (struct.get $HamtNode $children (local.get $node)))
    (local.set $len
      (array.len (local.get $children)))
    (local.set $count (i32.const 0))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $len)))

        (local.set $child
          (array.get $HamtChildren
            (local.get $children)
            (local.get $i)))

        ;; leaf — count 1
        (if (ref.test (ref $HamtLeaf) (local.get $child))
          (then
            (local.set $count
              (i32.add (local.get $count) (i32.const 1)))))

        ;; sub-node — recurse
        (if (ref.test (ref $HamtNode) (local.get $child))
          (then
            (local.set $count
              (i32.add (local.get $count)
                (call $_hamt_size_node
                  (ref.cast (ref $HamtNode) (local.get $child)))))))

        ;; collision — count its leaves
        (if (ref.test (ref $HamtCollision) (local.get $child))
          (then
            (local.set $count
              (i32.add (local.get $count)
                (array.len
                  (struct.get $HamtCollision $col_leaves
                    (ref.cast (ref $HamtCollision) (local.get $child))))))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $count)
  )


  ;; -- Record: direct-style API ------------------------------------------
  ;; Typed functions for internal/runtime use. Keys/values are (ref eq).

  (func $_hamt_rec_new (export "rec_new") (result (ref $RecImpl))
    (struct.new $RecImpl (global.get $empty_node))
  )

  (func $_hamt_rec_get (export "rec_get")
    (param $rec (ref $RecImpl)) (param $key (ref eq))
    (result (ref null eq))
    (call $hamt_get (struct.get $RecImpl $hamt (local.get $rec)) (local.get $key))
  )

  (func $_hamt_rec_set
    (param $rec (ref $RecImpl)) (param $key (ref eq)) (param $val (ref eq))
    (result (ref $RecImpl))
    (struct.new $RecImpl
      (call $hamt_set (struct.get $RecImpl $hamt (local.get $rec))
        (local.get $key) (local.get $val)))
  )

  (func $_hamt_rec_delete (export "rec_delete")
    (param $rec (ref $RecImpl)) (param $key (ref eq))
    (result (ref $RecImpl))
    (struct.new $RecImpl
      (call $hamt_delete (struct.get $RecImpl $hamt (local.get $rec))
        (local.get $key)))
  )

  (func $_hamt_rec_pop
    (param $rec (ref $RecImpl)) (param $key (ref eq))
    (result (ref null eq) (ref $RecImpl))
    (local $val (ref null eq))
    (local $rest (ref $HamtNode))
    (call $hamt_pop (struct.get $RecImpl $hamt (local.get $rec)) (local.get $key))
    (local.set $rest)
    (local.set $val)
    (local.get $val)
    (struct.new $RecImpl (local.get $rest))
  )

  (func $_hamt_rec_merge
    (param $dest (ref $RecImpl)) (param $src (ref $RecImpl))
    (result (ref $RecImpl))
    (struct.new $RecImpl
      (call $hamt_merge
        (struct.get $RecImpl $hamt (local.get $dest))
        (struct.get $RecImpl $hamt (local.get $src))))
  )

  (func $rec_size (export "rec_size")
    (param $rec (ref $RecImpl)) (result i32)
    (call $hamt_size (struct.get $RecImpl $hamt (local.get $rec)))
  )

  ;; Predicate: is this record empty?
  (func $rec_is_empty (export "rec_is_empty")
    (param $val (ref null any)) (result i32)
    (i32.eqz (call $rec_size (ref.cast (ref $RecImpl) (local.get $val))))
  )

  ;; -- Dict wrappers (user-visible API) ----------------------------------
  ;; Same as record wrappers but for $DictImpl ↔ $HamtNode.

  (func $dict_empty (export "dict_empty") (result (ref $DictImpl))
    (struct.new $DictImpl (global.get $empty_node))
  )

  (func $dict_get (export "dict_get")
    (param $dict (ref $DictImpl)) (param $key (ref eq))
    (result (ref null eq))
    (call $hamt_get (struct.get $DictImpl $hamt (local.get $dict)) (local.get $key))
  )

  (func $dict_set (export "dict_set")
    (param $dict (ref $DictImpl)) (param $key (ref eq)) (param $val (ref eq))
    (result (ref $DictImpl))
    (struct.new $DictImpl
      (call $hamt_set (struct.get $DictImpl $hamt (local.get $dict))
        (local.get $key) (local.get $val)))
  )

  (func $dict_delete (export "dict_delete")
    (param $dict (ref $DictImpl)) (param $key (ref eq))
    (result (ref $DictImpl))
    (struct.new $DictImpl
      (call $hamt_delete (struct.get $DictImpl $hamt (local.get $dict))
        (local.get $key)))
  )

  (func $dict_pop (export "dict_pop")
    (param $dict (ref $DictImpl)) (param $key (ref eq))
    (result (ref null eq) (ref $DictImpl))
    (local $val (ref null eq))
    (local $rest (ref $HamtNode))
    (call $hamt_pop (struct.get $DictImpl $hamt (local.get $dict)) (local.get $key))
    (local.set $rest)
    (local.set $val)
    (local.get $val)
    (struct.new $DictImpl (local.get $rest))
  )

  (func $dict_merge (export "dict_merge")
    (param $dest (ref $DictImpl)) (param $src (ref $DictImpl))
    (result (ref $DictImpl))
    (struct.new $DictImpl
      (call $hamt_merge
        (struct.get $DictImpl $hamt (local.get $dest))
        (struct.get $DictImpl $hamt (local.get $src))))
  )

  (func $dict_size (export "dict_size")
    (param $dict (ref $DictImpl)) (result i32)
    (call $hamt_size (struct.get $DictImpl $hamt (local.get $dict)))
  )


  ;; CPS wrappers — compiler-facing interface
  ;; All params/results are (ref null any). Continuation dispatch via _croc_N.
  ;;
  ;;   rec_set: (rec, key, val, cont) → _croc_1(new_rec, cont)
  ;;   rec_merge: (dest, src, cont) → _croc_1(merged, cont)
  ;;   rec_pop: (rec, key, fail, succ) → if missing: _croc_0(fail)
  ;;                                     else: _croc_2(val, rest, succ)

  (func $rec_set (export "rec_set")
    (param $rec (ref null any)) (param $key (ref null any))
    (param $val (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (call $_hamt_rec_set
        (ref.cast (ref $RecImpl) (local.get $rec))
        (ref.cast (ref eq) (local.get $key))
        (ref.cast (ref eq) (local.get $val)))
      (local.get $cont)))

  (func $rec_merge (export "rec_merge")
    (param $dest (ref null any)) (param $src (ref null any))
    (param $cont (ref null any))
    (return_call $croc_1
      (call $_hamt_rec_merge
        (ref.cast (ref $RecImpl) (local.get $dest))
        (ref.cast (ref $RecImpl) (local.get $src)))
      (local.get $cont)))

  (func $rec_pop (export "rec_pop")
    (param $rec (ref null any)) (param $key (ref null any))
    (param $fail (ref null any)) (param $succ (ref null any))
    (local $val (ref null eq))
    (local $rest (ref $RecImpl))
    (call $_hamt_rec_pop
      (ref.cast (ref $RecImpl) (local.get $rec))
      (ref.cast (ref eq) (local.get $key)))
    (local.set $rest)
    (local.set $val)
    ;; null value = key not found → call fail
    (if (ref.is_null (local.get $val))
      (then (return_call $croc_0 (local.get $fail))))
    ;; found → pass (value, rest) to succ
    (return_call $croc_2
      (local.get $val)
      (local.get $rest)
      (local.get $succ)))

)
