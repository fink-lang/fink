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
;;   - Type hierarchy:
;;       $Rec                        — opaque record type
;;       └── $RecImpl (sub $Rec)     — wrapper: single $HamtNode field
;;       $HamtLeaf                   — key-value pair (internal)
;;       $HamtNode                   — bitmap + children array (internal)
;;       $HamtCollision              — hash + flat leaf array (internal)
;;   - Keys and values are (ref eq) — non-nullable. This allows i31ref
;;     (for interned symbol ids) and any GC struct/array ref.
;;   - Return values are (ref null eq) where null signals "not found".
;;   - Key equality uses deep_eq (from operators.wat): i31ref → ref.eq,
;;     $Num → f64.eq, $Str → str_op_eq. General dict keys with
;;     user-defined Eq protocol will live in the std-lib (CPS).
;;
;; Hashing:
;;   - Imported from hashing.wat (hash_i31). Dispatches on i31ref, $Num,
;;     $Str via br_on_cast. General hashing for user-defined types via
;;     Hash protocol (future, std-lib, CPS).
;;
;; Exported functions:
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

  ;; Type imports (str.wat — needed for the rec fmt impl).
  (import "std/str.wat" "Str"       (type $Str       (sub any) (struct)))
  (import "std/str.wat" "ByteArray" (type $ByteArray (array (mut i8))))

  ;; Func imports
  (import "std/hashing.wat"  "hash_i31"
    (func $hash_i31 (param $key (ref eq)) (result i32)))
  (import "rt/protocols.wat" "deep_eq"
    (func $deep_eq (param $a (ref eq)) (param $b (ref eq)) (result i32)))
  (import "rt/apply.wat" "apply_0" (func $apply_0 (;apply-ctx;) (param (ref null any)) (param $cont (ref null any))))
  (import "rt/apply.wat" "apply_1" (func $apply_1 (;apply-ctx;) (param (ref null any)) (param $val (ref null any)) (param $cont (ref null any))))
  (import "rt/apply.wat" "apply_2_vals" (func $apply_2_vals (;apply-ctx;) (param (ref null any)) (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))))
  ;; str.wat helpers used by the rec fmt impl below.
  (import "std/str.wat" "from_bytes"
    (func $str_from_bytes (param $buf (ref $ByteArray)) (result (ref $Str))))
  (import "std/str.wat" "_str_len"
    (func $_str_len (param $str (ref $Str)) (result i32)))
  (import "std/str.wat" "_str_copy_to"
    (func $_str_copy_to (param $str (ref $Str)) (param $dst (ref $ByteArray)) (param $pos i32) (result i32)))
  (import "std/str.wat" "_str_from_ascii_2"
    (func $_str_from_ascii_2 (param $a i32) (param $b i32) (result (ref $Str))))
  (import "std/str.wat" "bytes"
    (func $str_bytes (param $str (ref $Str)) (result (ref $ByteArray))))
  (import "std/str.wat" "fmt_val"
    (func $str_fmt_val (param $val (ref any)) (result (ref $Str))))
  ;; repr_val — value formatter (per-type repr protocol dispatcher).
  (import "std/repr.wat" "repr_val"
    (func $repr_val (param $val (ref any)) (result (ref $Str))))
  ;; repr — for rec key quoting (string non-ident keys).
  (import "std/str.wat" "repr"
    (func $str_repr (param $str (ref $Str)) (result (ref $Str))))


  ;; -- $Rec public type -----------------------------------------------------

  (type $Rec  (@pub) (sub (struct)))


  ;; -- Type definitions -----------------------------------------------

  ;; Internal HAMT types. These are implementation details — user code
  ;; sees $Rec via the wrapper type below.
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

    ;; -- Wrapper type (private) ------------------------------------------
    ;; Single-field wrapper around the HAMT node. Private to this module:
    ;; cross-module APIs take/return the public $Rec and downcast to
    ;; $RecImpl internally, so the wrapper never appears in another
    ;; module's signatures. Casting happens only at the runtime API
    ;; boundary.
    (type $RecImpl (sub $Rec (struct
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

  (func $hamt_empty (result (ref $HamtNode))
    global.get $empty_node
  )


  ;; -- Get ------------------------------------------------------------

  ;; Look up a key. Returns null if not found.
  (func $hamt_get
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
  (func $hamt_set
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
  (func $hamt_delete
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
  (func $hamt_pop
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
  (func $hamt_merge
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
  (func $hamt_size
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

  ;; Returns the public $Rec type; the concrete $RecImpl wrapper stays
  ;; private to this module.
  (func $_rec_new (@impl "std/rec.fnk:new") (result (ref $Rec))
    (struct.new $RecImpl (global.get $empty_node))
  )

  ;; Takes the public $Rec and downcasts to the wrapper internally, so
  ;; cross-module callers never name $RecImpl.
  (func $get (@pub)
    (param $rec (ref $Rec)) (param $key (ref eq))
    (result (ref null eq))
    (call $hamt_get
      (struct.get $RecImpl $hamt (ref.cast (ref $RecImpl) (local.get $rec)))
      (local.get $key))
  )

  ;; Host-friendly $Rec field accessor. Takes anyref-typed args so
  ;; callers (interop, host) don't need to know about $RecImpl.
  ;; Returns null when the key is absent.
  (func $get_any (@pub)
    (param $rec (ref null any)) (param $key (ref null any))
    (result (ref null any))
    (call $get
      (ref.cast (ref $Rec) (local.get $rec))
      (ref.cast (ref eq) (local.get $key))))

  ;; -- Structural equality --------------------------------------------
  ;;
  ;; Two records are equal iff they have the same size and every entry in
  ;; `a` is present in `b` with a deep-equal value. Walks a's entries and
  ;; probes b via hamt_get; the size check rules out b having extra keys.
  ;; Values are compared through deep_eq (imported from protocols.wat), so
  ;; nesting is recursive. Direct-style — used by the == operator's $Rec
  ;; arm and by deep_eq for structural rec keys.
  (func $rec_deep_eq (@pub)
    (param $a (ref $Rec)) (param $b (ref $Rec)) (result i32)
    (local $a_node (ref $HamtNode))
    (local $b_node (ref $HamtNode))

    (local.set $a_node
      (struct.get $RecImpl $hamt (ref.cast (ref $RecImpl) (local.get $a))))
    (local.set $b_node
      (struct.get $RecImpl $hamt (ref.cast (ref $RecImpl) (local.get $b))))

    ;; Differing sizes can't be equal.
    (if (i32.ne
          (call $_hamt_size_node (local.get $a_node))
          (call $_hamt_size_node (local.get $b_node)))
      (then (return (i32.const 0))))

    ;; Every entry in a must match in b.
    (call $_rec_eq_node (local.get $a_node) (local.get $b_node)))

  ;; Walk every entry of `a_node`; for each, look it up in `b_node` and
  ;; require a deep-equal value. Returns 0 on the first mismatch, 1 if all
  ;; entries match.
  (func $_rec_eq_node
    (param $a_node (ref $HamtNode)) (param $b_node (ref $HamtNode))
    (result i32)
    (local $children (ref $HamtChildren))
    (local $len i32)
    (local $i i32)
    (local $child (ref null struct))

    (local.set $children
      (struct.get $HamtNode $children (local.get $a_node)))
    (local.set $len (array.len (local.get $children)))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))

        (local.set $child
          (array.get $HamtChildren (local.get $children) (local.get $i)))

        ;; leaf — probe b and compare value
        (if (ref.test (ref $HamtLeaf) (local.get $child))
          (then
            (if (i32.eqz
                  (call $_rec_eq_leaf
                    (ref.cast (ref $HamtLeaf) (local.get $child))
                    (local.get $b_node)))
              (then (return (i32.const 0))))))

        ;; sub-node — recurse
        (if (ref.test (ref $HamtNode) (local.get $child))
          (then
            (if (i32.eqz
                  (call $_rec_eq_node
                    (ref.cast (ref $HamtNode) (local.get $child))
                    (local.get $b_node)))
              (then (return (i32.const 0))))))

        ;; collision — check each colliding leaf
        (if (ref.test (ref $HamtCollision) (local.get $child))
          (then
            (if (i32.eqz
                  (call $_rec_eq_collision
                    (ref.cast (ref $HamtCollision) (local.get $child))
                    (local.get $b_node)))
              (then (return (i32.const 0))))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (i32.const 1))

  ;; A single leaf matches iff b has its key with a deep-equal value.
  (func $_rec_eq_leaf
    (param $leaf (ref $HamtLeaf)) (param $b_node (ref $HamtNode))
    (result i32)
    (local $found (ref null eq))

    (local.set $found
      (call $hamt_get (local.get $b_node)
        (struct.get $HamtLeaf $key (local.get $leaf))))

    (if (ref.is_null (local.get $found))
      (then (return (i32.const 0))))

    (call $deep_eq
      (struct.get $HamtLeaf $val (local.get $leaf))
      (ref.as_non_null (local.get $found))))

  ;; Every leaf in a collision node must match in b.
  (func $_rec_eq_collision
    (param $col (ref $HamtCollision)) (param $b_node (ref $HamtNode))
    (result i32)
    (local $leaves (ref $HamtChildren))
    (local $len i32)
    (local $i i32)

    (local.set $leaves
      (struct.get $HamtCollision $col_leaves (local.get $col)))
    (local.set $len (array.len (local.get $leaves)))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (if (i32.eqz
              (call $_rec_eq_leaf
                (ref.cast (ref $HamtLeaf)
                  (array.get $HamtChildren (local.get $leaves) (local.get $i)))
                (local.get $b_node)))
          (then (return (i32.const 0))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (i32.const 1))

  (func $op_in (@impl "std/operators.fnk:op_in" _ $Rec)
    (param $rec (ref $Rec)) (param $key (ref eq))
    (result i32)
    (ref.is_null
      (call $hamt_get
        (struct.get $RecImpl $hamt (ref.cast (ref $RecImpl) (local.get $rec)))
        (local.get $key)))
    (i32.const 1)
    (i32.xor)
  )

  (func $op_not_in (@impl "std/operators.fnk:op_notin" _ $Rec)
    (param $rec (ref $Rec)) (param $key (ref eq))
    (result i32)
    (i32.eqz (call $op_in (local.get $rec) (local.get $key)))
  )

  (func $_rec_set
    (param $rec (ref $RecImpl)) (param $key (ref eq)) (param $val (ref eq))
    (result (ref $RecImpl))
    (struct.new $RecImpl
      (call $hamt_set (struct.get $RecImpl $hamt (local.get $rec))
        (local.get $key) (local.get $val)))
  )

  (func $delete (@pub)
    (param $rec (ref $RecImpl)) (param $key (ref eq))
    (result (ref $RecImpl))
    (struct.new $RecImpl
      (call $hamt_delete (struct.get $RecImpl $hamt (local.get $rec))
        (local.get $key)))
  )

  (func $_rec_pop
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

  (func $_rec_merge
    (param $dest (ref $RecImpl)) (param $src (ref $RecImpl))
    (result (ref $RecImpl))
    (struct.new $RecImpl
      (call $hamt_merge
        (struct.get $RecImpl $hamt (local.get $dest))
        (struct.get $RecImpl $hamt (local.get $src))))
  )

  (func $size (@pub)
    (param $rec (ref $Rec)) (result i32)
    (call $hamt_size
      (struct.get $RecImpl $hamt (ref.cast (ref $RecImpl) (local.get $rec))))
  )

  ;; Predicate: is this record empty?
  (func $op_empty (@impl "std/operators.fnk:op_empty" $Rec)
    (param $val (ref null any)) (result i32)
    (i32.eqz (call $size (ref.cast (ref $RecImpl) (local.get $val))))
  )

  ;; CPS wrappers — compiler-facing interface
  ;; All params/results are (ref null any). Continuation dispatch via _apply_N.
  ;;
  ;;   rec_set: (ctx, rec, key, val, cont) → _apply([new_rec], cont)
  ;;   rec_merge: (ctx, dest, src, cont) → _apply([merged], cont)
  ;;   rec_pop: (ctx, rec, key, fail, succ) → if missing: _apply([], fail)
  ;;                                          else: _apply([val, rest], succ)
  ;;
  ;; ctx convention: $ctx is the first param and is forwarded to the cont
  ;; (via apply_N). Rec primitives operate on the monomorphic $RecImpl
  ;; kernel with no user-callbacks, so ctx is not consulted for dispatch.

  ;; Direct-style rec field setter — used by the emitter for module import rec construction.
  ;; Takes (rec, key, val) as (ref null any) and returns (ref null any).
  ;; Avoids CPS overhead for compile-time-known field sets.
  (func $_set_field (@impl "std/rec.fnk:_set_field")
    (param $rec (ref null any)) (param $key (ref null any)) (param $val (ref null any))
    (result (ref null any))
    (call $_rec_set
      (ref.cast (ref $RecImpl) (local.get $rec))
      (ref.cast (ref eq) (local.get $key))
      (ref.cast (ref eq) (local.get $val))))

  (func $rec_put (@pub) (@impl "std/rec.fnk:put")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $rec (ref null any)) (param $key (ref null any))
    (param $val (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (call $_rec_set
        (ref.cast (ref $RecImpl) (local.get $rec))
        (ref.cast (ref eq) (local.get $key))
        (ref.cast (ref eq) (local.get $val)))
      (local.get $cont)))

  (func $rec_merge (@pub) (@impl "std/rec.fnk:merge")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $dest (ref null any)) (param $src (ref null any))
    (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (call $_rec_merge
        (ref.cast (ref $RecImpl) (local.get $dest))
        (ref.cast (ref $RecImpl) (local.get $src)))
      (local.get $cont)))

  (func $rec_pop (@pub) (@impl "std/rec.fnk:pop")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted
    (param $rec (ref null any)) (param $key (ref null any))
    (param $fail (ref null any)) (param $succ (ref null any))
    (local $val (ref null eq))
    (local $rest (ref $RecImpl))
    (call $_rec_pop
      (ref.cast (ref $RecImpl) (local.get $rec))
      (ref.cast (ref eq) (local.get $key)))
    (local.set $rest)
    (local.set $val)
    ;; null value = key not found → call fail
    (if (ref.is_null (local.get $val))
      (then (return_call $apply_0
      (local.get $ctx) (local.get $fail))))
    ;; found → pass (value, rest) to succ
    (return_call $apply_2_vals
      (local.get $ctx)
      (local.get $val)
      (local.get $rest)
      (local.get $succ)))

  (func $op_dot (@impl "std/operators.fnk:op_dot" $Rec _)
    (param $ctx (ref null any))
    (param $rec (ref null any)) (param $key (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (call $get
        (ref.cast (ref $RecImpl) (local.get $rec))
        (ref.cast (ref eq) (local.get $key)))
      (local.get $cont)))


  ;; ---- Record formatting (direct-style) ----------------------------------
  ;;
  ;; Owns rec rendering as "{key: val, key2: val2}". Called by str.wat:fmt_val
  ;; via the per-type fmt dispatch protocol. Walks the HAMT in two passes:
  ;; first to compute total byte length, then to copy into a single buffer.

  ;; _is_key_ident : (ref $Str) -> i32
  ;; Check if a string is a valid fink identifier (used for bare key rendering).
  ;; Returns 1 if all bytes are identifier chars (a-z, A-Z, 0-9, _, -, $, or >= 0x80).
  ;; Empty string returns 0. First char must not be a digit.
  (func $_is_key_ident (param $str (ref $Str)) (result i32)
    (local $bytes (ref $ByteArray))
    (local $len i32)
    (local $i i32)
    (local $b i32)

    (local.set $bytes (call $str_bytes (local.get $str)))
    (local.set $len (array.len (local.get $bytes)))

    (if (i32.eqz (local.get $len))
      (then (return (i32.const 0))))

    (local.set $b (array.get_u $ByteArray (local.get $bytes) (i32.const 0)))
    (if (i32.and
          (i32.ge_u (local.get $b) (i32.const 0x30))
          (i32.le_u (local.get $b) (i32.const 0x39)))
      (then (return (i32.const 0))))

    (local.set $i (i32.const 0))
    (block $done
      (loop $check
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $b (array.get_u $ByteArray (local.get $bytes) (local.get $i)))

        (if (i32.ge_u (local.get $b) (i32.const 0x80))
          (then
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $check)))

        (if (i32.and
              (i32.ge_u (local.get $b) (i32.const 0x61))
              (i32.le_u (local.get $b) (i32.const 0x7A)))
          (then
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $check)))

        (if (i32.and
              (i32.ge_u (local.get $b) (i32.const 0x41))
              (i32.le_u (local.get $b) (i32.const 0x5A)))
          (then
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $check)))

        (if (i32.and
              (i32.ge_u (local.get $b) (i32.const 0x30))
              (i32.le_u (local.get $b) (i32.const 0x39)))
          (then
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $check)))

        (if (i32.or
              (i32.eq (local.get $b) (i32.const 0x5F))
              (i32.or
                (i32.eq (local.get $b) (i32.const 0x2D))
                (i32.eq (local.get $b) (i32.const 0x24))))
          (then
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $check)))

        (return (i32.const 0))))

    (i32.const 1)
  )

  ;; _fmt_key_len : (ref eq) -> i32
  ;; Byte length of a formatted record key.
  ;; String ident: bare len. String non-ident: repr len. Other: fmt len + 2 for parens.
  (func $_fmt_key_len (param $key (ref eq)) (result i32)
    (local $str (ref $Str))

    (block $not_str
      (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref eq) (ref $Str)
            (local.get $key))))
      (local.set $str)
      (return
        (if (result i32) (call $_is_key_ident (local.get $str))
          (then (call $_str_len (local.get $str)))
          (else (call $_str_len (call $str_repr (local.get $str)))))))

    (i32.add
      (call $_str_len
        (call $str_fmt_val (ref.cast (ref any) (local.get $key))))
      (i32.const 2))
  )

  ;; _fmt_size_node : (ref $HamtNode) -> i32
  ;; Compute total bytes for all entries in a HAMT node (key + ": " + val).
  ;; Does NOT include separators between entries or braces.
  (func $_fmt_size_node
    (param $node (ref $HamtNode))
    (result i32)

    (local $children (ref $HamtChildren))
    (local $len i32)
    (local $i i32)
    (local $total i32)
    (local $child (ref null struct))
    (local $leaf (ref $HamtLeaf))

    (local.set $children
      (struct.get $HamtNode $children (local.get $node)))
    (local.set $len
      (array.len (local.get $children)))
    (local.set $total (i32.const 0))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $len)))

        (local.set $child
          (array.get $HamtChildren
            (local.get $children)
            (local.get $i)))

        (if (ref.test (ref $HamtLeaf) (local.get $child))
          (then
            (local.set $leaf
              (ref.cast (ref $HamtLeaf) (local.get $child)))
            (local.set $total
              (i32.add (local.get $total)
                (i32.add
                  (i32.add
                    (call $_fmt_key_len
                      (struct.get $HamtLeaf $key (local.get $leaf)))
                    (i32.const 2))
                  (call $_str_len
                    (call $repr_val
                      (ref.cast (ref any)
                        (struct.get $HamtLeaf $val (local.get $leaf))))))))))

        (if (ref.test (ref $HamtNode) (local.get $child))
          (then
            (local.set $total
              (i32.add (local.get $total)
                (call $_fmt_size_node
                  (ref.cast (ref $HamtNode) (local.get $child)))))))

        (if (ref.test (ref $HamtCollision) (local.get $child))
          (then
            (local.set $total
              (i32.add (local.get $total)
                (call $_fmt_size_collision
                  (struct.get $HamtCollision $col_leaves
                    (ref.cast (ref $HamtCollision) (local.get $child))))))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $total)
  )

  ;; _fmt_size_collision : (ref $HamtChildren) -> i32
  (func $_fmt_size_collision
    (param $leaves (ref $HamtChildren))
    (result i32)

    (local $len i32)
    (local $i i32)
    (local $total i32)
    (local $leaf (ref $HamtLeaf))

    (local.set $len (array.len (local.get $leaves)))
    (local.set $total (i32.const 0))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $leaf
          (ref.cast (ref $HamtLeaf)
            (array.get $HamtChildren (local.get $leaves) (local.get $i))))
        (local.set $total
          (i32.add (local.get $total)
            (i32.add
              (i32.add
                (call $_fmt_key_len
                  (struct.get $HamtLeaf $key (local.get $leaf)))
                (i32.const 2))
              (call $_str_len
                (call $repr_val
                  (ref.cast (ref any)
                    (struct.get $HamtLeaf $val (local.get $leaf))))))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $total)
  )

  ;; _fmt_copy_key : (key, buf, pos) -> new_pos
  ;; Copy a formatted record key into buf.
  ;; String ident: bare. String non-ident: repr. Other: (fmt).
  (func $_fmt_copy_key
    (param $key (ref eq))
    (param $buf (ref $ByteArray))
    (param $pos i32)
    (result i32)

    (local $str (ref $Str))

    (block $not_str
      (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref eq) (ref $Str)
            (local.get $key))))
      (local.set $str)

      (if (call $_is_key_ident (local.get $str))
        (then
          (local.set $pos
            (call $_str_copy_to (local.get $str) (local.get $buf) (local.get $pos))))
        (else
          (local.set $pos
            (call $_str_copy_to
              (call $str_repr (local.get $str))
              (local.get $buf)
              (local.get $pos)))))
      (return (local.get $pos)))

    (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x28)) ;; '('
    (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
    (local.set $pos
      (call $_str_copy_to
        (call $str_fmt_val (ref.cast (ref any) (local.get $key)))
        (local.get $buf)
        (local.get $pos)))
    (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x29)) ;; ')'
    (local.set $pos (i32.add (local.get $pos) (i32.const 1)))

    (local.get $pos)
  )

  ;; _fmt_copy_node : (node, buf, pos, written) -> new_pos
  ;; Copy formatted entries into buf. written = entries written so far
  ;; (used to decide whether to prepend ", ").
  (func $_fmt_copy_node
    (param $node (ref $HamtNode))
    (param $buf (ref $ByteArray))
    (param $pos i32)
    (param $written i32)
    (result i32)

    (local $children (ref $HamtChildren))
    (local $len i32)
    (local $i i32)
    (local $child (ref null struct))
    (local $leaf (ref $HamtLeaf))

    (local.set $children
      (struct.get $HamtNode $children (local.get $node)))
    (local.set $len
      (array.len (local.get $children)))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $len)))

        (local.set $child
          (array.get $HamtChildren
            (local.get $children)
            (local.get $i)))

        (if (ref.test (ref $HamtLeaf) (local.get $child))
          (then
            (local.set $leaf
              (ref.cast (ref $HamtLeaf) (local.get $child)))

            (if (local.get $written)
              (then
                (array.set $ByteArray (local.get $buf) (local.get $pos)
                  (i32.const 0x2C))
                (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
                (array.set $ByteArray (local.get $buf) (local.get $pos)
                  (i32.const 0x20))
                (local.set $pos (i32.add (local.get $pos) (i32.const 1)))))

            (local.set $pos
              (call $_fmt_copy_key
                (struct.get $HamtLeaf $key (local.get $leaf))
                (local.get $buf) (local.get $pos)))

            (array.set $ByteArray (local.get $buf) (local.get $pos)
              (i32.const 0x3A))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (array.set $ByteArray (local.get $buf) (local.get $pos)
              (i32.const 0x20))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))

            (local.set $pos
              (call $_str_copy_to
                (call $repr_val
                  (ref.cast (ref any)
                    (struct.get $HamtLeaf $val (local.get $leaf))))
                (local.get $buf)
                (local.get $pos)))

            (local.set $written
              (i32.add (local.get $written) (i32.const 1)))))

        (if (ref.test (ref $HamtNode) (local.get $child))
          (then
            (local.set $pos
              (call $_fmt_copy_node
                (ref.cast (ref $HamtNode) (local.get $child))
                (local.get $buf) (local.get $pos) (local.get $written)))
            (local.set $written
              (i32.add (local.get $written)
                (call $_hamt_size_node
                  (ref.cast (ref $HamtNode) (local.get $child)))))))

        (if (ref.test (ref $HamtCollision) (local.get $child))
          (then
            (local.set $pos
              (call $_fmt_copy_collision
                (struct.get $HamtCollision $col_leaves
                  (ref.cast (ref $HamtCollision) (local.get $child)))
                (local.get $buf) (local.get $pos) (local.get $written)))
            (local.set $written
              (i32.add (local.get $written)
                (array.len
                  (struct.get $HamtCollision $col_leaves
                    (ref.cast (ref $HamtCollision) (local.get $child))))))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $pos)
  )

  ;; _fmt_copy_collision : (leaves, buf, pos, written) -> new_pos
  (func $_fmt_copy_collision
    (param $leaves (ref $HamtChildren))
    (param $buf (ref $ByteArray))
    (param $pos i32)
    (param $written i32)
    (result i32)

    (local $len i32)
    (local $i i32)
    (local $leaf (ref $HamtLeaf))

    (local.set $len (array.len (local.get $leaves)))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $len)))

        (local.set $leaf
          (ref.cast (ref $HamtLeaf)
            (array.get $HamtChildren (local.get $leaves) (local.get $i))))

        (if (i32.or (local.get $written) (local.get $i))
          (then
            (array.set $ByteArray (local.get $buf) (local.get $pos)
              (i32.const 0x2C))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (array.set $ByteArray (local.get $buf) (local.get $pos)
              (i32.const 0x20))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))))

        (local.set $pos
          (call $_fmt_copy_key
            (struct.get $HamtLeaf $key (local.get $leaf))
            (local.get $buf) (local.get $pos)))

        (array.set $ByteArray (local.get $buf) (local.get $pos)
          (i32.const 0x3A))
        (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
        (array.set $ByteArray (local.get $buf) (local.get $pos)
          (i32.const 0x20))
        (local.set $pos (i32.add (local.get $pos) (i32.const 1)))

        (local.set $pos
          (call $_str_copy_to
            (call $repr_val
              (ref.cast (ref any)
                (struct.get $HamtLeaf $val (local.get $leaf))))
            (local.get $buf)
            (local.get $pos)))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $pos)
  )

  ;; fmt : (ref $Rec) -> (ref $Str)
  ;; Format a record as "{key: val, key2: val2}". Empty record formats as "{}".
  ;; Two-pass: first compute total byte length, then copy into a single buffer.
  (func $fmt (@pub) (@impl "std/str.fnk:fmt" $Rec) (param $rec (ref $Rec)) (result (ref $Str))
    (local $node (ref $HamtNode))
    (local $total i32)
    (local $entry_count i32)
    (local $buf (ref $ByteArray))
    (local $pos i32)

    (local.set $node
      (struct.get $RecImpl $hamt
        (ref.cast (ref $RecImpl) (local.get $rec))))

    (local.set $entry_count
      (call $size (ref.cast (ref $Rec) (local.get $rec))))

    (if (i32.eqz (local.get $entry_count))
      (then
        (return (call $_str_from_ascii_2
          (i32.const 0x7B) ;; '{'
          (i32.const 0x7D) ;; '}'
        ))))

    (local.set $total
      (i32.add
        (i32.const 2)
        (i32.add
          (call $_fmt_size_node (local.get $node))
          (i32.mul
            (i32.sub (local.get $entry_count) (i32.const 1))
            (i32.const 2)))))

    (local.set $buf
      (array.new $ByteArray (i32.const 0) (local.get $total)))

    (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x7B))
    (local.set $pos (i32.const 1))

    (local.set $pos
      (call $_fmt_copy_node
        (local.get $node) (local.get $buf) (local.get $pos)
        (i32.const 0)))

    (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x7D))

    (call $str_from_bytes (local.get $buf))
  )

  ;; repr — same as fmt for records (their fmt already calls repr on values).
  (func $repr (@pub) (@impl "std/repr.fnk:repr" $Rec)
    (param $rec (ref $Rec)) (result (ref $Str))
    (return_call $fmt (local.get $rec)))

)
