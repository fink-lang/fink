;; Types -- runtime representation of user-declared types.
;;
;; A `type:`/`enum:`/`union:` declaration mints a `$Type` value at runtime
;; (types are first-class, runtime-first; comptime erasure is the end-state, not
;; iteration 1). `$Type` is the reified type-value the design calls for: the same
;; nominal-wrapper shape as $Opaque (rt/opaque.wat), extended with an
;; introspection key.
;;
;; Name: a `$Type` carries a `$name` symbol (a tagged i31, rt/symbols.wat) --
;; the interned id of its declared ident (`Foo` in `Foo = type: ...`). The
;; renderer resolves it to a source name in-band via `symbol_to_str` (repr /
;; errors), no host round-trip. An anonymous declaration (`type _` used inline
;; as a field type, never bound) carries the null-name symbol (id 0). A symbol
;; cannot carry a source span; type declarations have no diagnostic that points
;; at the declaration site, so no source location is held.

(module

  (import "rt/apply.wat" "apply_1"
    (func $apply_1 (param (ref null any)) (param $result (ref null any)) (param $cont (ref null any))))
  ;; Universal closure dispatcher -- used to invoke a generic type's `$new`
  ;; builder (a closure over the type-params) when applying a GENERIC type.
  (import "rt/apply.wat" "apply_3"
    (func $apply_3 (param (ref null any)) (param (ref null any)) (param (ref null any))))
  (import "rt/apply.wat" "args_head"
    (func $args_head (param (ref null any)) (result (ref null any))))
  (import "rt/apply.wat" "args_tail"
    (func $args_tail (param (ref null any)) (result (ref null any))))
  (import "rt/apply.wat" "args_prepend"
    (func $args_prepend (param (ref null any)) (param (ref any)) (result (ref any))))
  ;; Build the cont that stamps a generic-built type's `$type` back-link.
  (import "rt/apply.wat" "make_type_stamp_cont"
    (func $make_type_stamp_cont
      (param (ref null any)) (param (ref null any)) (param (ref null any))
      (result (ref null any))))
  (import "std/dict.wat" "Dict" (type $Dict (sub any)))
  ;; Direct-style dict helpers: build an empty fields dict and add entries.
  (import "std/dict.wat" "_rec_new"
    (func $dict_new (result (ref $Dict))))
  (import "std/dict.wat" "_set_field"
    (func $dict_set_field (param (ref null any)) (param (ref null any)) (param (ref null any)) (result (ref null any))))
  (import "std/dict.wat" "rec_deep_eq"
    (func $dict_deep_eq (param (ref $Dict)) (param (ref $Dict)) (result i32)))
  (import "std/dict.wat" "copy_by_keys"
    (func $dict_copy_by_keys (param (ref null any)) (param (ref null any)) (result (ref $Dict))))
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
  (import "std/set.wat" "op_in"
    (func $set_in (param (ref $Set)) (param (ref eq)) (result i32)))


  ;; -- $Type and flavour subtypes -------------------------------------
  ;;
  ;; A type is a `$base`-chain of nodes. `$Type` is the shared CORE every flavour
  ;; carries: the `$name` symbol and the `$base` link (`..Super`, or the previous
  ;; node in a multi-level decl; null at the chain root). Flavour is a SUBTYPE,
  ;; discriminated by `br_on_cast` -- not a $kind tag and not inferred from
  ;; collection emptiness:
  ;;   $RecType   -- named fields. `$fields` is the FULL name->field-type $Dict
  ;;                 (base's fields + own, built by copying base.fields then
  ;;                 set'ing own; HAMT structural sharing keeps the copy cheap).
  ;;   $TupleType -- positional. `$positionals` is the field-type $List.
  ;;   $FnType    -- a callable shape (`fn A, B: R`). SIBLING of TupleType, not a
  ;;                 subtype: a fn type is not a tuple. `$params` is the arg-type
  ;;                 $List (reverse-stored, readers reverse); `$result` the result
  ;;                 type. Describes a shape; never applied to construct a value.
  ;; A bare `type _` / chain root is a plain `$Type` (unit); it is a marker/base,
  ;; never applied to construct an instance (no current caller).
  ;; `$type`  -- the DESCRIPTOR: for a type produced by applying a generic, the
  ;;             generic it was made from (its classifier). Null for directly-
  ;;             declared types (root/self for now). Drives generic guards.
  ;; `$new`   -- the type-CONSTRUCTOR: present (non-null) iff this is a GENERIC
  ;;             type. Applying a $Type with `$new` set calls `$new` with the
  ;;             type-args to BUILD a type; with `$new` null it builds an
  ;;             instance (the data constructor). So `$new != null` IS the
  ;;             genericity tag. A field (not a subtype): a generic `type T:`
  ;;             body is still a record/tuple, so genericity lives ON the
  ;;             structure subtype, not as a sibling of it.
  (type $Type (@pub) (sub (struct
    (field $name (ref i31))
    (field $type (mut (ref null $Type)))
    (field $new (mut (ref null any)))
    (field $base (mut (ref null $Type)))
  )))
  (type $RecType (@pub) (sub $Type (struct
    (field $name (ref i31))
    (field $type (mut (ref null $Type)))
    (field $new (mut (ref null any)))
    (field $base (mut (ref null $Type)))
    (field $fields (mut (ref null $Dict)))
  )))
  (type $TupleType (@pub) (sub $Type (struct
    (field $name (ref i31))
    (field $type (mut (ref null $Type)))
    (field $new (mut (ref null any)))
    (field $base (mut (ref null $Type)))
    ;; Positional field-types, in REVERSE declaration order (cons-prepend);
    ;; readers reverse.
    (field $positionals (mut (ref null $List)))
  )))
  (type $FnType (@pub) (sub $Type (struct
    (field $name (ref i31))
    (field $type (mut (ref null $Type)))
    (field $new (mut (ref null any)))
    (field $base (mut (ref null $Type)))
    ;; Argument types, in REVERSE declaration order (cons-prepend); readers
    ;; reverse. Null until the first param is accreted.
    (field $params (mut (ref null $List)))
    ;; Result type. Null until set by fn_type_result.
    (field $result (mut (ref null $Type)))
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


  ;; -- type_set_new ----------------------------------------------------
  ;;
  ;; Record a type-constructor (`builder`, a closure over the type-params) on a
  ;; type, marking it GENERIC. Applying a type with `$new` set runs the builder
  ;; (type_apply's generic branch) to PRODUCE A TYPE, rather than constructing an
  ;; instance. Cont-taking; tail-applies cont with the (now generic) type.
  ;; (ctx, type, builder, cont).
  (func $type_set_new (@pub) (@impl "rt/types.wat:type_set_new")
    (param $ctx (ref null any))
    (param $type (ref null any)) (param $builder (ref null any))
    (param $cont (ref null any))
    (local $t (ref $Type))
    (local.set $t (ref.cast (ref $Type) (local.get $type)))
    (struct.set $Type $new (local.get $t) (local.get $builder))
    (return_call $apply_1 (local.get $ctx) (local.get $t) (local.get $cont)))


  ;; -- type_set_type_field ---------------------------------------------
  ;;
  ;; Stamp a type's `$type` descriptor (the generic that produced it) and return
  ;; the type. Used by the generic-application back-link: apply.wat's stamp cont
  ;; calls this on the type a generic's `$new` builder just produced, recording
  ;; `result.$type = the generic` so a `Foo` guard recognizes `Foo u8` instances.
  ;; apply.wat owns the cont/closure mechanics ($Closure is concrete there);
  ;; types.wat owns the $Type struct mutation. (t, generic) -> t.
  (func $type_set_type_field (@pub)
      (param $t (ref null any)) (param $generic (ref null any))
      (result (ref null any))
    (struct.set $Type $type
      (ref.cast (ref $Type) (local.get $t))
      (ref.cast (ref $Type) (local.get $generic)))
    (local.get $t))


  ;; -- new_type --------------------------------------------------------
  ;;
  ;; Mint a fresh `$Type`. Called directly by codegen (not a first-class fink
  ;; value): the lowering supplies the `$name` symbol (the declared ident's
  ;; interned id, or the null-name symbol for an anonymous `type _`).
  ;; Op calling convention: (ctx, name, cont) -- tail-applies cont with the
  ;; new type value. Starts with an empty fields dict.
  (func $new_type (@pub) (@impl "rt/types.wat:new_type")
    (param $ctx (ref null any))
    (param $name (ref null any))
    (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (struct.new $Type
        (ref.cast (ref i31) (local.get $name))
        (ref.null none) (ref.null none)  ;; $type, $new
        (ref.null none))                 ;; $base
      (local.get $cont)))


  ;; -- type_set_field --------------------------------------------------
  ;;
  ;; Add a named field to the type under construction. CONSTRUCT-OR-ACCRETE:
  ;;   - current is a $RecType -> set the field on its (own) $fields dict.
  ;;   - else -> construct a NEW $RecType based on the current node, seeded with
  ;;     the FULL field set (current's fields if it is a $RecType, else empty),
  ;;     then set this field. HAMT structural sharing keeps the seed copy cheap.
  ;; Cont-taking; tail-applies cont with the (possibly new) node. (key, val) are
  ;; the field name and its type value.
  (func $type_set_field (@pub) (@impl "rt/types.wat:type_set_field")
    (param $ctx (ref null any))
    (param $type (ref null any)) (param $key (ref null any)) (param $val (ref null any))
    (param $cont (ref null any))
    (local $t (ref $Type))
    (local $rt (ref $RecType))
    (local $base_fields (ref null $Dict))
    (local.set $t (ref.cast (ref $Type) (local.get $type)))
    ;; Already a $RecType: set the field in place.
    (if (ref.test (ref $RecType) (local.get $t))
      (then
        (local.set $rt (ref.cast (ref $RecType) (local.get $t)))
        (struct.set $RecType $fields (local.get $rt)
          (ref.cast (ref $Dict)
            (call $dict_set_field
              (struct.get $RecType $fields (local.get $rt))
              (local.get $key) (local.get $val))))
        (return_call $apply_1 (local.get $ctx) (local.get $rt) (local.get $cont))))
    ;; Otherwise wrap: new $RecType based on current. Seed fields from current if
    ;; it is a $RecType (it is not, here), else empty.
    (return_call $apply_1
      (local.get $ctx)
      (struct.new $RecType
        (struct.get $Type $name (local.get $t))
        (ref.null none) (ref.null none)  ;; $type, $new
        (local.get $t)                   ;; $base
        (ref.cast (ref $Dict)
          (call $dict_set_field (call $dict_new) (local.get $key) (local.get $val))))
      (local.get $cont)))


  ;; -- type_push -------------------------------------------------------
  ;;
  ;; Append a positional (tuple) field-type. CONSTRUCT-OR-ACCRETE:
  ;;   - current is a $TupleType -> cons-prepend onto its $positionals.
  ;;   - else -> construct a new $TupleType based on the current node, seeded with
  ;;     [val]. (Tuple inheritance/splice handled in type_inherit; here the wrap
  ;;     starts a fresh positional run on the current node as base.)
  ;; $positionals is reverse-stored (cons-prepend); readers reverse.
  ;; Cont-taking; tail-applies cont with the (possibly new) node. (ctx, type, val, cont).
  (func $type_push (@pub) (@impl "rt/types.wat:type_push")
    (param $ctx (ref null any))
    (param $type (ref null any)) (param $val (ref null any))
    (param $cont (ref null any))
    (local $t (ref $Type))
    (local $tt (ref $TupleType))
    (local.set $t (ref.cast (ref $Type) (local.get $type)))
    (if (ref.test (ref $TupleType) (local.get $t))
      (then
        (local.set $tt (ref.cast (ref $TupleType) (local.get $t)))
        (struct.set $TupleType $positionals (local.get $tt)
          (call $list_prepend
            (ref.cast (ref any) (local.get $val))
            (ref.cast (ref $List) (struct.get $TupleType $positionals (local.get $tt)))))
        (return_call $apply_1 (local.get $ctx) (local.get $tt) (local.get $cont))))
    (return_call $apply_1
      (local.get $ctx)
      (struct.new $TupleType
        (struct.get $Type $name (local.get $t))
        (ref.null none) (ref.null none)  ;; $type, $new
        (local.get $t)                   ;; $base
        (call $list_prepend (ref.cast (ref any) (local.get $val)) (call $list_empty)))
      (local.get $cont)))


  ;; -- new_fn_type / fn_type_param / fn_type_result --------------------
  ;;
  ;; A function type (`fn A, B: R`). Minted by accretion, mirroring the tuple
  ;; family: new_fn_type seeds an empty `$FnType` (carrying its name); each
  ;; fn_type_param cons-prepends an arg type onto `$params` (reverse-stored,
  ;; readers reverse); fn_type_result sets `$result`. All cont-taking.

  ;; new_fn_type(ctx, name, cont) -- mint an empty `$FnType` (null params/result).
  (func $new_fn_type (@pub) (@impl "rt/types.wat:new_fn_type")
    (param $ctx (ref null any))
    (param $name (ref null any))
    (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (struct.new $FnType
        (ref.cast (ref i31) (local.get $name))
        (ref.null none) (ref.null none)  ;; $type, $new
        (ref.null none)                  ;; $base
        (ref.null none) (ref.null none)) ;; $params, $result
      (local.get $cont)))

  ;; fn_type_param(ctx, fntype, param_type, cont) -- cons-prepend an arg type.
  (func $fn_type_param (@pub) (@impl "rt/types.wat:fn_type_param")
    (param $ctx (ref null any))
    (param $fntype (ref null any)) (param $param (ref null any))
    (param $cont (ref null any))
    (local $ft (ref $FnType))
    (local $params (ref $List))
    (local.set $ft (ref.cast (ref $FnType) (local.get $fntype)))
    (local.set $params
      (if (result (ref $List)) (ref.is_null (struct.get $FnType $params (local.get $ft)))
        (then (call $list_empty))
        (else (ref.cast (ref $List) (struct.get $FnType $params (local.get $ft))))))
    (struct.set $FnType $params (local.get $ft)
      (call $list_prepend (ref.cast (ref any) (local.get $param)) (local.get $params)))
    (return_call $apply_1 (local.get $ctx) (local.get $ft) (local.get $cont)))

  ;; fn_type_result(ctx, fntype, result_type, cont) -- set the result type.
  (func $fn_type_result (@pub) (@impl "rt/types.wat:fn_type_result")
    (param $ctx (ref null any))
    (param $fntype (ref null any)) (param $result (ref null any))
    (param $cont (ref null any))
    (local $ft (ref $FnType))
    (local.set $ft (ref.cast (ref $FnType) (local.get $fntype)))
    (struct.set $FnType $result (local.get $ft)
      (ref.cast (ref $Type) (local.get $result)))
    (return_call $apply_1 (local.get $ctx) (local.get $ft) (local.get $cont)))


  ;; -- type_inherit ----------------------------------------------------
  ;;
  ;; `..Base` spread. Construct a NEW node based on `base`, with `base`'s flavour
  ;; (RUNTIME br_cast of base) and members copied in (full-set):
  ;;   - base is $TupleType -> new $TupleType base:base, positionals = base's
  ;;     (cons-list; structural share).
  ;;   - base is $RecType   -> new $RecType   base:base, fields = base's $Dict
  ;;     (HAMT structural share -- the new type carries the full inherited set).
  ;;   - base is a unit $Type -> remain unit-based (new $Type base:base); a later
  ;;     field/positional will wrap it via type_set_field/type_push.
  ;; The prior `type` node (a fresh unit seed at chain start) is replaced by this
  ;; base-derived node. The new node keeps the DERIVED type's own `$name` (read
  ;; off `$type`, the prior seed -- e.g. `FooBar`), NOT base's: a derived type
  ;; reprs under its own name. Cont-taking. (ctx, type, base, cont).
  (func $type_inherit (@pub) (@impl "rt/types.wat:type_inherit")
    (param $ctx (ref null any))
    (param $type (ref null any)) (param $base (ref null any))
    (param $cont (ref null any))
    (local $b (ref $Type))
    (local $bt (ref $TupleType))
    (local $br (ref $RecType))
    (local $name (ref i31))
    (local.set $b (ref.cast (ref $Type) (local.get $base)))
    (local.set $name
      (struct.get $Type $name (ref.cast (ref $Type) (local.get $type))))
    ;; Tuple base.
    (if (ref.test (ref $TupleType) (local.get $b))
      (then
        (local.set $bt (ref.cast (ref $TupleType) (local.get $b)))
        (return_call $apply_1
          (local.get $ctx)
          (struct.new $TupleType
            (local.get $name)
            (ref.null none) (ref.null none)  ;; $type, $new
            (local.get $b)                   ;; $base
            (struct.get $TupleType $positionals (local.get $bt)))
          (local.get $cont))))
    ;; Rec base.
    (if (ref.test (ref $RecType) (local.get $b))
      (then
        (local.set $br (ref.cast (ref $RecType) (local.get $b)))
        (return_call $apply_1
          (local.get $ctx)
          (struct.new $RecType
            (local.get $name)
            (ref.null none) (ref.null none)  ;; $type, $new
            (local.get $b)                   ;; $base
            (struct.get $RecType $fields (local.get $br)))
          (local.get $cont))))
    ;; Unit base: new unit node based on it.
    (return_call $apply_1
      (local.get $ctx)
      (struct.new $Type
        (local.get $name)
        (ref.null none) (ref.null none)  ;; $type, $new
        (local.get $b))                  ;; $base
      (local.get $cont)))


  ;; -- $Union ----------------------------------------------------------
  ;;
  ;; An open union IS a type (has a name, usable as a field type), so it
  ;; subtypes $Type and adds $members: a $Set of member type-refs ("union is
  ;; just a set"). Two unions are equal iff their member SETS are equal --
  ;; structural, order-independent -- so union eq delegates to set eq.
  ;; Inherits $Type's fields as the struct prefix (WasmGC rule).
  (type $Union (@pub) (sub $Type (struct
    (field $name (ref i31))
    (field $type (mut (ref null $Type)))
    (field $new (mut (ref null any)))
    (field $base (mut (ref null $Type)))
    (field $members (mut (ref null $Set)))
  )))


  ;; -- new_union -------------------------------------------------------
  ;;
  ;; Mint a fresh `$Union` with an empty member set. Seed constructor, same
  ;; `$name`-symbol convention as new_type. (ctx, name, cont).
  (func $new_union (@pub) (@impl "rt/types.wat:new_union")
    (param $ctx (ref null any))
    (param $name (ref null any))
    (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (struct.new $Union
        (ref.cast (ref i31) (local.get $name))
        (ref.null none) (ref.null none)  ;; $type, $new
        (ref.null none)                  ;; $base
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


  ;; -- is_union_member -------------------------------------------------
  ;;
  ;; Membership test for a union guard: is `val` an instance of any member
  ;; type? An instance belongs to the union iff its type -- or any type in
  ;; its `$base` chain -- is a member. Reuses the set's synchronous key
  ;; lookup (`op_in`, a HAMT `has`) instead of iterating members: walk the
  ;; instance's type chain and probe each link against the member set.
  ;; Non-instances are never members.
  (func $is_union_member (@pub) (@impl "rt/types.wat:is_union_member")
    (param $val (ref null any)) (param $union (ref null any)) (result i32)
    (local $members (ref $Set))
    (local $t (ref null $Type))
    (if (i32.eqz (ref.test (ref $Inst) (local.get $val)))
      (then (return (i32.const 0))))
    (local.set $members
      (ref.cast (ref $Set)
        (struct.get $Union $members (ref.cast (ref $Union) (local.get $union)))))
    (local.set $t
      (struct.get $Inst $type (ref.cast (ref $Inst) (local.get $val))))
    (block $done
      (loop $walk
        (br_if $done (ref.is_null (local.get $t)))
        (if (call $set_in (local.get $members) (ref.cast (ref eq) (local.get $t)))
          (then (return (i32.const 1))))
        (local.set $t (struct.get $Type $base (local.get $t)))
        (br $walk)))
    (i32.const 0))


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
    (field $name (ref i31))
    (field $type (mut (ref null $Type)))
    (field $new (mut (ref null any)))
    (field $base (mut (ref null $Type)))
    (field $cases (mut (ref null $Dict)))
  )))


  ;; -- new_enum --------------------------------------------------------
  ;;
  ;; Mint a fresh `$Enum` with an empty cases dict. Seed constructor.
  ;; (ctx, name, cont).
  (func $new_enum (@pub) (@impl "rt/types.wat:new_enum")
    (param $ctx (ref null any))
    (param $name (ref null any))
    (param $cont (ref null any))
    (return_call $apply_1
      (local.get $ctx)
      (struct.new $Enum
        (ref.cast (ref i31) (local.get $name))
        (ref.null none) (ref.null none)  ;; $type, $new
        (ref.null none)                  ;; $base
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
    ;; Link the case type's $base to the enum, so a case instance IS-A its enum:
    ;; is_instance walks the instance's type chain (Some -> Opt), letting the
    ;; enum itself guard any of its cases (`Opt o = Opt.Some {...}`) via the
    ;; existing $Type guard arm -- the enum analogue of union membership.
    (struct.set $Type $base
      (ref.cast (ref $Type) (local.get $member))
      (local.get $e))
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


  ;; -- enum_cases ------------------------------------------------------
  ;;
  ;; The enum's case dict (name -> case-type). `Enum.Case` member access reads
  ;; a case type out of here (op_dot delegates to dict lookup on this). Keeps
  ;; the `$cases` field internal to types.wat; callers get the $Dict.
  (func $enum_cases (@pub) (@impl "rt/types.wat:enum_cases")
    (param $enum (ref null any)) (result (ref null any))
    (struct.get $Enum $cases (ref.cast (ref $Enum) (local.get $enum))))


  ;; -- type_apply ------------------------------------------------------
  ;;
  ;; Construct an instance: applying a $Type builds an $Inst. apply.wat
  ;; dispatches here when the callee is a $Type (it stays dumb -- delegates
  ;; the whole flavour decision to us). Same Fn3 args convention as the closure
  ;; path: cont is the HEAD of $args, the real args follow in the tail.
  ;; Discriminate by the TYPE's FLAVOUR (br_on_cast the leaf node):
  ;;   $RecType   -> $Rec (payload = the single $Dict real-arg)
  ;;   $TupleType -> $Tuple (payload = the real-args list)
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
    ;; GENERIC type: `$new` is the type-constructor (a closure over the
    ;; type-params). Applying it BUILDS A TYPE, not an instance. Run the builder
    ;; with the type-args, but WRAP the cont so the built type's `$type`
    ;; back-link is stamped to this generic before continuing (so a `Foo` guard
    ;; recognizes `Foo u8`). `$new` null => fall through to instance construction
    ;; below (the data constructor).
    (if (i32.eqz (ref.is_null (struct.get $Type $new (local.get $t))))
      (then
        (return_call $apply_3
          ;; Replace the head cont with the stamping cont; keep the type-args.
          (call $args_prepend
            (call $make_type_stamp_cont
              (local.get $ctx)
              (call $args_head (local.get $args))
              (local.get $t))
            (ref.cast (ref any) (call $args_tail (local.get $args))))
          (local.get $ctx)
          (struct.get $Type $new (local.get $t)))))
    ;; Fn3 args: cont = head, real args = tail.
    (local.set $cont (call $args_head (local.get $args)))
    (local.set $real_args (call $args_tail (local.get $args)))
    ;; Record instance: the single real-arg is the $Dict payload.
    (if (ref.test (ref $RecType) (local.get $t))
      (then
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
    ;; At each node ALSO check its `$type` descriptor: a concrete type built by
    ;; applying a GENERIC carries `$type -> the generic`, so `Foo u8` matches a
    ;; `Foo` guard (the generic is the classifier, not in the $base chain).
    (local.set $t (struct.get $Inst $type (ref.cast (ref $Inst) (local.get $val))))
    (block $done
      (loop $walk
        (br_if $done (ref.is_null (local.get $t)))
        (if (ref.eq (local.get $t) (ref.cast (ref eq) (local.get $type)))
          (then (return (i32.const 1))))
        (if (i32.eqz (ref.is_null (struct.get $Type $type (local.get $t))))
          (then
            (if (ref.eq
                  (struct.get $Type $type (local.get $t))
                  (ref.cast (ref eq) (local.get $type)))
              (then (return (i32.const 1))))))
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


  ;; -- inst_type_name --------------------------------------------------
  ;;
  ;; The `$name` symbol of an instance's nominal type. The renderer (repr.wat)
  ;; reads this and feeds it to the symbol repr to source-quote the type name
  ;; (`Foo {bar: 1}`). A symbol word, so it routes through the same name
  ;; resolution any symbol uses -- types.wat owns no string machinery.
  (func $inst_type_name (@pub)
    (param $inst (ref null any)) (result (ref i31))
    (struct.get $Type $name
      (struct.get $Inst $type (ref.cast (ref $Inst) (local.get $inst)))))


  ;; -- project_inst ----------------------------------------------------
  ;;
  ;; Cast (downcast) `val` to `target` for a type-guard match: build a fresh
  ;; instance of `target` carrying exactly target's fields, copied from val's
  ;; payload. So `Foo foo = FooBar {bar,spam}` binds a real `Foo {bar}` (the
  ;; extra `spam` dropped), indistinguishable from a directly-constructed Foo.
  ;;   $RecType target -> $Rec{target, copy_by_keys(target.$fields, val_payload)}.
  ;;   else (tuple/unit -- positional projection deferred) -> val unchanged.
  ;; (val, target) -> projected instance.
  (func $project_inst (@pub)
    (param $val (ref null any)) (param $target (ref null any))
    (result (ref null any))
    (if (ref.test (ref $RecType) (local.get $target))
      (then (return
        (struct.new $Rec
          (ref.cast (ref $Type) (local.get $target))
          (call $dict_copy_by_keys
            (struct.get $RecType $fields (ref.cast (ref $RecType) (local.get $target)))
            (call $inst_payload (local.get $val)))))))
    (local.get $val))


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
