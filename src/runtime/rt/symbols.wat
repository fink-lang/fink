;; Symbols -- interned, package-wide source identities.
;;
;; A symbol is the runtime identity of a source NAME: a record field, and
;; (later) type / module / function names. The compiler interns each distinct
;; name to a package-wide id (dedup-by-name at link time), so `bar` in module A
;; and `bar` in module B carry the SAME id. This makes structural field access
;; work cross-type (`{foo} = Foo {bar, foo}` maps the anonymous rec's `foo` to
;; Foo's `foo`) without runtime string compares, and the compiler can emit a
;; symbol inline at each use site -- no global instance table, no interning at
;; runtime (the interning is purely compile-time name->id assignment).
;;
;; Representation: a TAGGED i31ref, not a heap struct. The word is
;; `(id << 3) | TAG_SYMBOL` (tag 0b010). i31 is the immediate-value space shared
;; with bool (false = i31(0), true = i31(1)); the 3-bit tag discriminates symbol
;; from bool. No allocation: a symbol is a non-heap reference, and two symbols
;; with the same id are the same word -- identity is whole-word ref.eq, hash is
;; the word itself. Identity ops (deep_eq, hash) treat the word opaquely and do
;; NOT inspect the tag; only operations that must RENDER a symbol (repr, dict key
;; formatting) call is_symbol to discriminate. (Encoding is pre-1.0 internal --
;; nothing persists a symbol word; the linker re-assigns ids each build. Keep it
;; behind new_symbol/symbol_id/is_symbol so it can change freely.)
;;
;; First consumer: record field keys (std/dict keyed by symbol instead of
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

  ;; -- tag + decode / discriminate -----------------------------------
  ;;
  ;; Tagged i31: word = (id << 3) | TAG_SYMBOL. TAG_SYMBOL = 0b010 sits past the
  ;; two bool words (false = 0b000, true = 0b001) in the shared i31 space.
  ;;
  ;; A symbol is a COMPILE-TIME CONSTANT: the linker assigns the id and folds the
  ;; whole word, so there is no runtime constructor. Lowering emits the word as
  ;; an `(ref.i31 (i32.const <word>))` inline (box_symbol), and the table
  ;; population passes the already-encoded word to register_symbol. The ENCODE
  ;; lives once, at link (resolve_symbols in link.rs). symbols.wat owns only the
  ;; DECODE / DISCRIMINATE side below -- they must match link's `(id << 3) | 2`.

  ;; symbol_id(sym) -> id. Unsigned shift drops the tag.
  (func $symbol_id (@pub) (param $sym (ref i31)) (result i32)
    (i32.shr_u (i31.get_u (local.get $sym)) (i32.const 3)))

  ;; is_symbol(v): true iff v is an i31 carrying TAG_SYMBOL. The ONLY symbol
  ;; discriminator -- identity ops (deep_eq, hash) never call it (they treat the
  ;; word opaquely as a plain i31); only renderers (repr, dict key fmt) do.
  ;; Symbol equality / hashing therefore need no symbol-specific func: deep_eq
  ;; and hash_i31 handle a symbol word through their generic i31 arm.
  (func $is_symbol (@pub) (param $v (ref null any)) (result i32)
    (if (i32.eqz (ref.test (ref i31) (local.get $v)))
      (then (return (i32.const 0))))
    (i32.eq
      (i32.and (i31.get_u (ref.cast (ref i31) (local.get $v))) (i32.const 0x7))
      (i32.const 0x2)))


  ;; -- name <-> symbol tables (interop / host boundary) ----------------
  ;;
  ;; A symbol carries only its id; the source name lives here. fink code never
  ;; needs these -- key kind is fixed at compile time (idents lower to symbols,
  ;; strings stay $Str), so fink dict ops never coerce. The tables exist for the
  ;; INTEROP boundary, where the host has no interface files and can only work
  ;; with names: it resolves an export name to its symbol (forward) to index a
  ;; symbol-keyed record, and renders symbol keys back to names (reverse).
  ;;
  ;; Forward: name($Str) -> symbol (`str_to_symbol`). Reverse: symbol ->
  ;; name($Str) (`symbol_to_str`, also used by repr). Lowering prepends
  ;; `register_symbol(name, word)` calls (one per interned name) to the module
  ;; body, populating both at startup. `word` is the already-encoded symbol
  ;; (link folds it), so register_symbol stores it directly.
  (global $symbol_table (mut (ref null $Dict)) (ref.null none))
  (global $symbol_names (mut (ref null $Dict)) (ref.null none))

  (func $register_symbol (@pub) (@impl "rt/symbols.wat:register_symbol")
    (param $name (ref null any)) (param $sym (ref i31))
    (if (ref.is_null (global.get $symbol_table))
      (then
        (global.set $symbol_table (call $rec_new))
        (global.set $symbol_names (call $rec_new))))
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
  ;; A symbol reprs as its source name: bare if a valid ident (`foo`), else
  ;; quoted (`'foo bar'`) -- the same rule record keys used to special-case in
  ;; the dict formatter, now owned here via the repr protocol. repr.wat's
  ;; repr_val dispatches its symbol arm here (gated by is_symbol); the dict
  ;; formatter just calls repr_val on keys like it does on values.
  (func $repr (@pub) (@impl "std/repr.fnk:repr" $Symbol)
    (param $sym (ref i31)) (result (ref $Str))
    (local $name (ref null any))
    (local.set $name (call $symbol_to_str (local.get $sym)))
    (if (ref.is_null (local.get $name))
      (then (return (call $str_empty))))
    (if (call $str_is_key_ident (ref.cast (ref $Str) (local.get $name)))
      (then (return (ref.cast (ref $Str) (local.get $name)))))
    (return_call $str_repr (ref.cast (ref $Str) (local.get $name))))
)
