;; Types -- runtime representation of user-declared types.
;;
;; A `type:`/`enum:`/`union:` declaration mints a `$Type` value at runtime
;; (types are first-class, runtime-first; comptime erasure is the end-state, not
;; iteration 1). `$Type` is the reified type-value the design calls for: the same
;; nominal-wrapper shape as $Opaque (rt/opaque.wat), extended with an
;; introspection key.
;;
;; Introspection key: a `$Type` carries (mod_id, cps_id) -- the SAME pair the
;; tracer carries per frame (rt/trace.wat). The host resolves it to a name and
;; source location on demand (repr / errors / debug); the runtime holds only the
;; opaque pair, so a fully-resolved type stays erasable. Two i32s, mirroring
;; trace_push/trace_mark, rather than a packed i64 -- consistent with the one
;; existing mechanism.

(module

  (import "rt/apply.wat" "apply_1"
    (func $apply_1 (param (ref null any)) (param $result (ref null any)) (param $cont (ref null any))))
  (import "rt/apply.wat" "args_head"
    (func $args_head (param (ref null any)) (result (ref null any))))
  (import "rt/apply.wat" "args_tail"
    (func $args_tail (param (ref null any)) (result (ref null any))))
  (import "std/dict.wat" "Dict" (type $Dict (sub any)))
  ;; Direct-style dict helpers: build an empty fields dict and add entries.
  (import "std/dict.wat" "_rec_new"
    (func $dict_new (result (ref $Dict))))
  (import "std/dict.wat" "_set_field"
    (func $dict_set_field (param (ref null any)) (param (ref null any)) (param (ref null any)) (result (ref null any))))
  (import "std/dict.wat" "size" (func $dict_size (param (ref $Dict)) (result i32)))
  (import "std/dict.wat" "rec_deep_eq"
    (func $dict_deep_eq (param (ref $Dict)) (param (ref $Dict)) (result i32)))
  (import "std/list.wat" "List" (type $List (sub any)))
  (import "std/list.wat" "empty" (func $list_empty (result (ref $List))))
  (import "std/list.wat" "prepend"
    (func $list_prepend (param (ref any)) (param (ref $List)) (result (ref $List))))
  (import "std/list.wat" "concat"
    (func $list_concat (param (ref $List)) (param (ref $List)) (result (ref $List))))
  (import "std/list.wat" "list_deep_eq"
    (func $list_deep_eq (param (ref $List)) (param (ref $List)) (result i32)))
  ;; Union members are a $Set ("union is just a set"); eq delegates to set eq.
  (import "std/set.wat" "Set" (type $Set (sub any)))
  (import "std/set.wat" "impl_empty" (func $set_empty (result (ref $Set))))
  (import "std/set.wat" "impl_set"
    (func $set_add (param (ref $Set)) (param (ref eq)) (result (ref $Set))))
  (import "std/set.wat" "op_eq"
    (func $set_eq (param (ref $Set)) (param (ref $Set)) (result i32)))


  ;; -- $Type -----------------------------------------------------------
  ;;
  ;; The reified type-value. $mod_id/$cps_id are the introspection key (see
  ;; header). $fields is the name -> field-type map (a $Dict), accreted by
  ;; type_set_field; mutable because a type is built incrementally then frozen
  ;; (not shared mid-construction). Tuple/base fields land in later slices.
  (type $Type (@pub) (sub (struct
    (field $mod_id i32)
    (field $cps_id i32)
    (field $fields (mut (ref null $Dict)))
    ;; Positional (tuple) field-types, in REVERSE declaration order (cons-list
    ;; prepend); readers reverse. $base is the `..Super` link (null if none).
    (field $positionals (mut (ref null $List)))
    (field $base (mut (ref null $Type)))
  )))


  ;; -- Instances -------------------------------------------------------
  ;;
  ;; A typed instance is its nominal $type ref + a payload. Flavour is a
  ;; SUBTYPE (so the payload is statically typed): $Rec wraps a $Dict, $Tuple
  ;; wraps a $List. Apply on a $Type builds one of these (see type_apply).
  ;; Iteration 1: field values stored as-is (no per-field constructor yet).
  (type $Inst (@pub) (sub (struct
    (field $type (ref $Type))
  )))
  (type $Rec (@pub) (sub $Inst (struct
    (field $type (ref $Type))
    (field $rec_payload (ref $Dict))
  )))
  (type $Tuple (@pub) (sub $Inst (struct
    (field $type (ref $Type))
    (field $tup_payload (ref $List))
  )))


  ;; -- new_type --------------------------------------------------------
  ;;
  ;; Mint a fresh `$Type`. Called directly by codegen (not a first-class fink
  ;; value): the lowering supplies mod_id/cps_id as constants (like trace_push).
  ;; Op calling convention: (ctx, mid, cid, cont) -- tail-applies cont with the
  ;; new type value. Starts with an empty fields dict.
  (func $new_type (@pub) (@impl "rt/types.wat:new_type")
    (param $ctx (ref null any))
    (param $mid i32) (param $cid i32)
    (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (struct.new $Type
        (local.get $mid) (local.get $cid)
        (call $dict_new) (call $list_empty) (ref.null none))
      (local.get $cont)))


  ;; -- type_set_field --------------------------------------------------
  ;;
  ;; Add a named field (key -> field-type) to a type under construction.
  ;; Cont-taking accretion op (mirrors rec_put): mutates the type's fields dict
  ;; and tail-applies cont with the same type. (key, val) are the field name
  ;; and its type value.
  (func $type_set_field (@pub) (@impl "rt/types.wat:type_set_field")
    (param $ctx (ref null any))
    (param $type (ref null any)) (param $key (ref null any)) (param $val (ref null any))
    (param $cont (ref null any))
    (local $t (ref $Type))
    (local.set $t (ref.cast (ref $Type) (local.get $type)))
    (struct.set $Type $fields (local.get $t)
      (ref.cast (ref $Dict)
        (call $dict_set_field
          (struct.get $Type $fields (local.get $t))
          (local.get $key)
          (local.get $val))))
    (return_call $apply_1
      (local.get $ctx)
      (local.get $t)
      (local.get $cont)))


  ;; -- type_push -------------------------------------------------------
  ;;
  ;; Append a positional (tuple) field-type. Cont-taking accretion (mirrors
  ;; seq_prepend). Stored via cons-prepend, so $positionals ends up in reverse
  ;; declaration order; readers reverse. (ctx, type, val, cont).
  (func $type_push (@pub) (@impl "rt/types.wat:type_push")
    (param $ctx (ref null any))
    (param $type (ref null any)) (param $val (ref null any))
    (param $cont (ref null any))
    (local $t (ref $Type))
    (local.set $t (ref.cast (ref $Type) (local.get $type)))
    (struct.set $Type $positionals (local.get $t)
      (call $list_prepend
        (ref.cast (ref any) (local.get $val))
        (ref.cast (ref $List) (struct.get $Type $positionals (local.get $t)))))
    (return_call $apply_1
      (local.get $ctx)
      (local.get $t)
      (local.get $cont)))


  ;; -- type_inherit ----------------------------------------------------
  ;;
  ;; `..Base` spread. For a fixed tuple, this SPLICES the base's positionals
  ;; into this type's positionals at the accretion point -- so `u8, ..Foo` and
  ;; `..Foo, u8` differ by WHERE the splice lands (the accretion order encodes
  ;; the layout). Also records $base (for record-flavour name inheritance /
  ;; future supertype queries). $positionals is reverse-stored (cons-prepend),
  ;; so concat(base, current) places base's items deeper = earlier-declared.
  ;; (ctx, type, base, cont).
  (func $type_inherit (@pub) (@impl "rt/types.wat:type_inherit")
    (param $ctx (ref null any))
    (param $type (ref null any)) (param $base (ref null any))
    (param $cont (ref null any))
    (local $t (ref $Type))
    (local $b (ref $Type))
    (local.set $t (ref.cast (ref $Type) (local.get $type)))
    (local.set $b (ref.cast (ref $Type) (local.get $base)))
    (struct.set $Type $base (local.get $t) (local.get $b))
    ;; Splice the base's positionals in at this point (fixed-tuple layout).
    (struct.set $Type $positionals (local.get $t)
      (call $list_concat
        (ref.cast (ref $List) (struct.get $Type $positionals (local.get $b)))
        (ref.cast (ref $List) (struct.get $Type $positionals (local.get $t)))))
    (return_call $apply_1
      (local.get $ctx)
      (local.get $t)
      (local.get $cont)))


  ;; -- $Union ----------------------------------------------------------
  ;;
  ;; An open union IS a type (has a name, usable as a field type), so it
  ;; subtypes $Type and adds $members: a $Set of member type-refs ("union is
  ;; just a set"). Two unions are equal iff their member SETS are equal --
  ;; structural, order-independent -- so union eq delegates to set eq.
  ;; Inherits $Type's fields as the struct prefix (WasmGC rule).
  (type $Union (@pub) (sub $Type (struct
    (field $mod_id i32)
    (field $cps_id i32)
    (field $fields (mut (ref null $Dict)))
    (field $positionals (mut (ref null $List)))
    (field $base (mut (ref null $Type)))
    (field $members (mut (ref null $Set)))
  )))


  ;; -- new_union -------------------------------------------------------
  ;;
  ;; Mint a fresh `$Union` with an empty member set. Seed constructor, same
  ;; introspection-key convention as new_type. (ctx, mid, cid, cont).
  (func $new_union (@pub) (@impl "rt/types.wat:new_union")
    (param $ctx (ref null any))
    (param $mid i32) (param $cid i32)
    (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (struct.new $Union
        (local.get $mid) (local.get $cid)
        (call $dict_new) (call $list_empty) (ref.null none)
        (call $set_empty))
      (local.get $cont)))


  ;; -- union_add -------------------------------------------------------
  ;;
  ;; Add a member type-ref to the union's member set. Cont-taking accretion.
  ;; (ctx, union, member, cont).
  (func $union_add (@pub) (@impl "rt/types.wat:union_add")
    (param $ctx (ref null any))
    (param $union (ref null any)) (param $member (ref null any))
    (param $cont (ref null any))
    (local $u (ref $Union))
    (local.set $u (ref.cast (ref $Union) (local.get $union)))
    (struct.set $Union $members (local.get $u)
      (call $set_add
        (ref.cast (ref $Set) (struct.get $Union $members (local.get $u)))
        (ref.cast (ref eq) (local.get $member))))
    (return_call $apply_1
      (local.get $ctx)
      (local.get $u)
      (local.get $cont)))


  ;; -- union_eq --------------------------------------------------------
  ;;
  ;; Two unions are equal iff their member sets are equal. Delegates to set eq
  ;; (structural, order-independent). Direct-style: (a, b) -> i32.
  (func $union_eq (@pub)
    (param $a (ref $Union)) (param $b (ref $Union)) (result i32)
    (call $set_eq
      (ref.cast (ref $Set) (struct.get $Union $members (local.get $a)))
      (ref.cast (ref $Set) (struct.get $Union $members (local.get $b)))))


  ;; -- hash_i31 --------------------------------------------------------
  ;;
  ;; $Type identity hash. Returns 0 for now (runtime-first): the set HAMT
  ;; degrades to one bucket, ref.eq disambiguates within it -- correct, slow,
  ;; optimize later (cf. closure-hash). Needed so $Type values can be $Set
  ;; members (union members). Covers all $Type subtypes.
  (func $hash_i31 (@pub) (param $t (ref $Type)) (result i32)
    (i32.const 0))


  ;; -- $Enum -----------------------------------------------------------
  ;;
  ;; A closed enum IS a type (a namespace value, usable as a field type), so it
  ;; subtypes $Type and adds $cases: a name -> member-type $Dict (an enum is "a
  ;; namespace = a record"). Inherits $Type's fields as the struct prefix.
  (type $Enum (@pub) (sub $Type (struct
    (field $mod_id i32)
    (field $cps_id i32)
    (field $fields (mut (ref null $Dict)))
    (field $positionals (mut (ref null $List)))
    (field $base (mut (ref null $Type)))
    (field $cases (mut (ref null $Dict)))
  )))


  ;; -- new_enum --------------------------------------------------------
  ;;
  ;; Mint a fresh `$Enum` with an empty cases dict. Seed constructor.
  ;; (ctx, mid, cid, cont).
  (func $new_enum (@pub) (@impl "rt/types.wat:new_enum")
    (param $ctx (ref null any))
    (param $mid i32) (param $cid i32)
    (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (struct.new $Enum
        (local.get $mid) (local.get $cid)
        (call $dict_new) (call $list_empty) (ref.null none)
        (call $dict_new))
      (local.get $cont)))


  ;; -- enum_add --------------------------------------------------------
  ;;
  ;; Add a case (name -> member-type) to an enum. Cont-taking accretion
  ;; (mirrors type_set_field). (ctx, enum, name, member_type, cont).
  (func $enum_add (@pub) (@impl "rt/types.wat:enum_add")
    (param $ctx (ref null any))
    (param $enum (ref null any)) (param $name (ref null any)) (param $member (ref null any))
    (param $cont (ref null any))
    (local $e (ref $Enum))
    (local.set $e (ref.cast (ref $Enum) (local.get $enum)))
    (struct.set $Enum $cases (local.get $e)
      (ref.cast (ref $Dict)
        (call $dict_set_field
          (struct.get $Enum $cases (local.get $e))
          (local.get $name)
          (local.get $member))))
    (return_call $apply_1
      (local.get $ctx)
      (local.get $e)
      (local.get $cont)))


  ;; -- type_apply ------------------------------------------------------
  ;;
  ;; Construct an instance: applying a $Type builds an $Inst. apply.wat
  ;; dispatches here when the callee is a $Type (it stays dumb -- delegates
  ;; the whole flavour decision to us). Same Fn3 args convention as the closure
  ;; path: cont is the HEAD of $args, the real args follow in the tail.
  ;; Discriminate by the TYPE's structure:
  ;;   fields non-empty -> $Rec (payload = the single $Dict real-arg)
  ;;   else             -> $Tuple (payload = the real-args list)
  ;; Iteration 1: field values stored as-is (no per-field constructor).
  ;; (args, ctx, type).
  (func $type_apply (@pub)
    (param $args (ref null any))
    (param $ctx (ref null any))
    (param $type (ref null any))
    (local $t (ref $Type))
    (local $cont (ref null any))
    (local $real_args (ref null any))
    (local.set $t (ref.cast (ref $Type) (local.get $type)))
    ;; Fn3 args: cont = head, real args = tail.
    (local.set $cont (call $args_head (local.get $args)))
    (local.set $real_args (call $args_tail (local.get $args)))
    (if (i32.gt_u
          (call $dict_size
            (ref.cast (ref $Dict) (struct.get $Type $fields (local.get $t))))
          (i32.const 0))
      (then
        ;; Record instance: the single real-arg is the $Dict payload.
        (return_call $apply_1
          (local.get $ctx)
          (struct.new $Rec
            (local.get $t)
            (ref.cast (ref $Dict) (call $args_head (local.get $real_args))))
          (local.get $cont))))
    ;; Tuple instance: the real-args list is the positional payload.
    (return_call $apply_1
      (local.get $ctx)
      (struct.new $Tuple
        (local.get $t)
        (ref.cast (ref $List) (local.get $real_args)))
      (local.get $cont)))


  ;; -- is_instance -----------------------------------------------------
  ;;
  ;; Direct-style predicate: is `val` an instance of `type`? True when `val` is
  ;; a typed instance ($Inst) whose nominal $type is `type`. Walks the $base
  ;; chain so a subtype (`FooBar` with `..Foo`) satisfies a `Foo` guard.
  ;; Reuses the nominal half of inst_eq's identity model. (val, type) -> i32.
  (func $is_instance (@pub)
    (param $val (ref null any)) (param $type (ref null any)) (result i32)
    (local $t (ref null $Type))
    ;; Non-instances are never an instance of any type.
    (if (i32.eqz (ref.test (ref $Inst) (local.get $val)))
      (then (return (i32.const 0))))
    ;; Walk the instance's type and its $base chain; ref.eq against `type`.
    (local.set $t (struct.get $Inst $type (ref.cast (ref $Inst) (local.get $val))))
    (block $done
      (loop $walk
        (br_if $done (ref.is_null (local.get $t)))
        (if (ref.eq (local.get $t) (ref.cast (ref eq) (local.get $type)))
          (then (return (i32.const 1))))
        (local.set $t (struct.get $Type $base (local.get $t)))
        (br $walk)))
    (i32.const 0))


  ;; -- inst_payload ----------------------------------------------------
  ;;
  ;; Unwrap an instance to its bare structural payload ($Dict for $Rec, $List
  ;; for $Tuple). Reads (field access, destructure, spread) delegate through
  ;; this -- they are nominal-blind and strip the type. Identity is conferred
  ;; ONLY by a constructor, never recovered from a read.
  (func $inst_payload (@pub)
    (param $inst (ref null any)) (result (ref null any))
    (if (ref.test (ref $Rec) (local.get $inst))
      (then (return
        (struct.get $Rec $rec_payload (ref.cast (ref $Rec) (local.get $inst))))))
    (struct.get $Tuple $tup_payload (ref.cast (ref $Tuple) (local.get $inst))))


  ;; -- inst_eq ---------------------------------------------------------
  ;;
  ;; Instance equality: NOMINAL (same $type, ref.eq) AND STRUCTURAL (payload
  ;; eq via the payload's own deep-eq). $Rec delegates to dict deep-eq, $Tuple
  ;; to list deep-eq. Different flavour or different $type -> not equal. (Since
  ;; types aren't enforced yet, this is the whole story: tag + delegate.)
  ;; Direct-style: (a, b) -> i32.
  (func $inst_eq (@pub)
    (param $a (ref $Inst)) (param $b (ref $Inst)) (result i32)
    ;; Nominal: same type.
    (if (i32.eqz
          (ref.eq (struct.get $Inst $type (local.get $a))
                  (struct.get $Inst $type (local.get $b))))
      (then (return (i32.const 0))))
    ;; Structural: delegate to the payload's deep-eq, by flavour.
    (if (ref.test (ref $Rec) (local.get $a))
      (then
        (if (i32.eqz (ref.test (ref $Rec) (local.get $b)))
          (then (return (i32.const 0))))
        (return (call $dict_deep_eq
          (struct.get $Rec $rec_payload (ref.cast (ref $Rec) (local.get $a)))
          (struct.get $Rec $rec_payload (ref.cast (ref $Rec) (local.get $b)))))))
    ;; Tuple.
    (if (i32.eqz (ref.test (ref $Tuple) (local.get $b)))
      (then (return (i32.const 0))))
    (return_call $list_deep_eq
      (struct.get $Tuple $tup_payload (ref.cast (ref $Tuple) (local.get $a)))
      (struct.get $Tuple $tup_payload (ref.cast (ref $Tuple) (local.get $b)))))
)
