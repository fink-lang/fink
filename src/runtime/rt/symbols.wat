;; Symbols -- interned, package-wide source identities.
;;
;; A $Symbol is the runtime identity of a source NAME: a record field, and
;; (later) type / module / function names. The compiler interns each distinct
;; name to a package-wide $id (dedup-by-name at link time), so `bar` in module A
;; and `bar` in module B carry the SAME id. Identity is the $id -- equality is
;; i32.eq on it, NOT ref.eq -- so two `struct.new $Symbol (i32.const N)` for the
;; same N are equal regardless of allocation. This makes structural field access
;; work cross-type (`{foo} = Foo {bar, foo}` maps the anonymous rec's `foo` to
;; Foo's `foo`) without runtime string compares, and the compiler can emit a
;; symbol inline at each use site -- no global instance table, no interning at
;; runtime (the interning is purely compile-time name->id assignment).
;;
;; The $id also IS the hash -- ids are dense and distinct, so they distribute
;; across hamt buckets with no string hashing. The source name is debug/repr
;; metadata, resolved host-side and strippable; the runtime holds only the id.
;;
;; First consumer: record field keys (std/dict keyed by $Symbol instead of
;; $Str for static field names). Dynamic/computed keys keep the generic
;; (ref eq) key path.

(module

  ;; Dict + Str imports for the symbol name table. (Mutual import with
  ;; dict.wat, which imports Symbol/repr back -- the linker resolves the cycle,
  ;; as with opaque<->hashing.)
  (import "std/dict.wat" "Dict"    (type $Dict (sub any)))
  (import "std/dict.wat" "_rec_new"
    (func $rec_new (result (ref $Dict))))
  (import "std/dict.wat" "_rec_set_any"
    (func $rec_set (param (ref null any)) (param (ref eq)) (param (ref eq)) (result (ref $Dict))))
  (import "std/dict.wat" "get"
    (func $rec_get (param (ref $Dict)) (param (ref eq)) (result (ref null eq))))
  (import "std/str.wat" "Str" (type $Str (sub any)))
  (import "std/str.wat" "str_empty" (func $str_empty (result (ref $Str))))
  (import "std/str.wat" "repr" (func $str_repr (param (ref $Str)) (result (ref $Str))))
  (import "std/dict.wat" "_is_key_ident"
    (func $str_is_key_ident (param (ref $Str)) (result i32)))

  ;; -- $Symbol type ----------------------------------------------------
  ;;
  ;; (sub (struct ...)) makes it an eq-type so hamt keying works.
  ;; $id: package-wide interned id. Identity is the $id (i32.eq), NOT the
  ;; allocation -- two `struct.new $Symbol (N)` for the same N are equal. The
  ;; id doubles as the hash. Lowering emits a symbol inline at each key site.
  (type $Symbol (@pub) (sub (struct (field $id i32))))


  ;; -- Identity equality / id hash ------------------------------------
  ;;
  ;; op_eq / op_neq: identity (ref.eq on the canonical instance). hash_i31:
  ;; the id itself (dense, distinct -> well distributed, no string hashing).
  ;; protocols.wat / hashing.wat dispatch their $Symbol arms here.

  (func $op_eq (@pub)
    (param $a (ref $Symbol)) (param $b (ref $Symbol)) (result i32)
    (i32.eq
      (struct.get $Symbol $id (local.get $a))
      (struct.get $Symbol $id (local.get $b))))

  (func $op_neq (@pub)
    (param $a (ref $Symbol)) (param $b (ref $Symbol)) (result i32)
    (i32.ne
      (struct.get $Symbol $id (local.get $a))
      (struct.get $Symbol $id (local.get $b))))

  (func $hash_i31 (@pub)
    (param $s (ref $Symbol)) (result i32)
    (struct.get $Symbol $id (local.get $s)))

  ;; Construct a symbol from its id. Used by register_symbol below; the inline
  ;; form is emitted directly by lowering at key sites.
  (func $new_symbol (@pub) (param $id i32) (result (ref $Symbol))
    (struct.new $Symbol (local.get $id)))


  ;; -- name <-> symbol tables (interop / host boundary) ----------------
  ;;
  ;; A $Symbol carries only its id; the source name lives here. fink code never
  ;; needs these -- key kind is fixed at compile time (idents lower to $Symbol,
  ;; strings stay $Str), so fink dict ops never coerce. The tables exist for the
  ;; INTEROP boundary, where the host has no interface files and can only work
  ;; with names: it resolves an export name to its symbol (forward) to index a
  ;; symbol-keyed record, and renders symbol keys back to names (reverse).
  ;;
  ;; Forward: name($Str) -> $Symbol (`str_to_symbol`). Reverse: $Symbol ->
  ;; name($Str) (`symbol_to_str`, also used by repr). Lowering prepends
  ;; `register_symbol(name, id)` calls (one per interned name) to the module
  ;; body, populating both at startup.
  (global $symbol_table (mut (ref null $Dict)) (ref.null none))
  (global $symbol_names (mut (ref null $Dict)) (ref.null none))

  (func $register_symbol (@pub) (@impl "rt/symbols.wat:register_symbol")
    (param $name (ref null any)) (param $id i32)
    (local $sym (ref $Symbol))
    (if (ref.is_null (global.get $symbol_table))
      (then
        (global.set $symbol_table (call $rec_new))
        (global.set $symbol_names (call $rec_new))))
    (local.set $sym (call $new_symbol (local.get $id)))
    (global.set $symbol_table
      (call $rec_set
        (global.get $symbol_table)
        (ref.cast (ref eq) (local.get $name))
        (local.get $sym)))
    (global.set $symbol_names
      (call $rec_set
        (global.get $symbol_names)
        (local.get $sym)
        (ref.cast (ref eq) (local.get $name)))))

  ;; symbol_to_str(symbol) -> name($Str) or null. Reverse lookup.
  (func $symbol_to_str (@pub) (param $sym (ref eq)) (result (ref null any))
    (if (ref.is_null (global.get $symbol_names))
      (then (return (ref.null none))))
    (return_call $rec_get
      (ref.as_non_null (global.get $symbol_names))
      (local.get $sym)))

  ;; str_to_symbol(key): a $Str name resolves to its interned $Symbol via the
  ;; forward table (the $Str passes through unchanged if never interned / table
  ;; empty). Non-$Str values pass through. For the INTEROP boundary only -- the
  ;; host resolves an export name to the symbol a record is keyed by.
  (func $str_to_symbol (@pub) (param $key (ref eq)) (result (ref eq))
    (local $sym (ref null eq))
    (if (i32.eqz (ref.test (ref $Str) (local.get $key)))
      (then (return (local.get $key))))
    (if (ref.is_null (global.get $symbol_table))
      (then (return (local.get $key))))
    (local.set $sym
      (call $rec_get
        (ref.as_non_null (global.get $symbol_table))
        (local.get $key)))
    (if (ref.is_null (local.get $sym))
      (then (return (local.get $key))))
    (ref.as_non_null (local.get $sym)))

  ;; -- repr ------------------------------------------------------------
  ;;
  ;; A $Symbol reprs as its source name: bare if a valid ident (`foo`), else
  ;; quoted (`'foo bar'`) -- the same rule record keys used to special-case in
  ;; the dict formatter, now owned here via the repr protocol. repr.wat's
  ;; repr_val dispatches its $Symbol arm here; the dict formatter just calls
  ;; repr_val on keys like it does on values.
  (func $repr (@pub) (@impl "std/repr.fnk:repr" $Symbol)
    (param $sym (ref $Symbol)) (result (ref $Str))
    (local $name (ref null any))
    (local.set $name (call $symbol_to_str (local.get $sym)))
    (if (ref.is_null (local.get $name))
      (then (return (call $str_empty))))
    (if (call $str_is_key_ident (ref.cast (ref $Str) (local.get $name)))
      (then (return (ref.cast (ref $Str) (local.get $name)))))
    (return_call $str_repr (ref.cast (ref $Str) (local.get $name))))
)
