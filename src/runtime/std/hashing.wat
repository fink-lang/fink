;; Hashing — centralised hash dispatch for fink runtime types
;;
;; WASM GC implementation using br_on_cast type dispatch.
;;
;; Design:
;;   - hash_i31 produces a 31-bit hash packed into i31ref
;;   - Dispatches on input type via br_on_cast chain:
;;       i31ref   → i31.get_s (value is its own hash)
;;       $Num     → f64 bits folded to i31
;;       $Str     → delegates to str's hash_i31 (string module owns its internals)
;;   - Future hash variants (hash_64, hash_bytes) can be added alongside
;;   - Compiler can shortcut the dispatch when the type is statically known

(module

  (import "std/num.wat" "Num" (type $Num (sub any)))
  (import "std/str.wat" "Str" (type $Str (sub any)))
  (import "rt/apply.wat" "Closure" (type $Closure (sub any)))
  (import "rt/opaque.wat" "Opaque" (type $Opaque (sub any)))
  (import "std/dict.wat" "Dict" (type $Dict (sub any)))

  (import "std/num.wat" "hash_i31"
    (func $num_hash_i31 (param (ref $Num)) (result i32)))
  (import "std/str.wat" "hash_i31"
    (func $str_hash_i31 (param (ref $Str)) (result i32)))
  (import "rt/apply.wat" "hash_i31"
    (func $clos_hash_i31 (param (ref $Closure)) (result i32)))
  (import "rt/opaque.wat" "hash_i31"
    (func $opaque_hash_i31 (param (ref $Opaque)) (result i32)))
  (import "rt/types.wat" "Type" (type $Type (sub any)))
  (import "rt/types.wat" "hash_i31"
    (func $type_hash_i31 (param (ref $Type)) (result i32)))


  ;; -- Hash dispatch ------------------------------------------------------

  ;; hash_i31(key) → 31-bit hash as i32
  ;;
  ;; br_on_cast dispatch over known built-in types.
  ;; Unreachable for types that are not valid hash keys (closures,
  ;; collections, templates). The compiler must not emit hash_i31
  ;; calls for those types.
  (func $hash_i31 (@pub) (@impl "std/hashing.fnk:hash_i31")
    (param $key (ref eq))
    (result i32)

    ;; Try i31ref — value is its own hash
    (block $not_i31
      (block $is_i31 (result (ref i31))
        (br $not_i31
          (br_on_cast $is_i31 (ref eq) (ref i31)
            (local.get $key))))
      (return (i31.get_s)))

    ;; Try $Num — delegate to num module
    (block $not_num
      (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref eq) (ref $Num)
            (local.get $key))))
      (return (call $num_hash_i31)))

    ;; Try $Str — delegate to string module
    (block $not_str
      (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref eq) (ref $Str)
            (local.get $key))))
      (return (call $str_hash_i31)))

    ;; Try $Closure -- delegate to apply.wat which owns closure hashing.
    (block $not_clos
      (block $is_clos (result (ref $Closure))
        (br $not_clos
          (br_on_cast $is_clos (ref eq) (ref $Closure)
            (local.get $key))))
      (return (call $clos_hash_i31)))

    ;; Try $Opaque -- delegate to opaque.wat (identity hash, constant 0).
    (block $not_opaque
      (block $is_opaque (result (ref $Opaque))
        (br $not_opaque
          (br_on_cast $is_opaque (ref eq) (ref $Opaque)
            (local.get $key))))
      (return (call $opaque_hash_i31)))

    ;; Symbols are tagged i31 words -- handled by the i31 arm above (the word
    ;; is its own hash), no symbol-specific arm needed.

    ;; Try $Dict -- structural content hash. Stubbed to 0 for now: all
    ;; records share one bucket and deep_eq disambiguates. Correct but
    ;; unoptimized -- records-as-keys degrade to a linear bucket scan.
    ;;
    ;; TODO: real content hash. Prefer lazy-memoized (compute the full
    ;; content hash on first request, cache it on the wrapper) over
    ;; eager-on-write -- the lazy form is the common approach (Clojure,
    ;; Scala, Java) and avoids taxing every record build/update for a
    ;; hash most records never need. Must satisfy a == b => hash(a) ==
    ;; hash(b): order-independent for records (commutative combine of
    ;; per-entry hashes). Only worth doing once records-as-keys are a
    ;; measured hot path. See list.wat / set.wat for the sibling TODOs.
    (block $not_rec
      (block $is_rec (result (ref $Dict))
        (br $not_rec
          (br_on_cast $is_rec (ref eq) (ref $Dict)
            (local.get $key))))
      (drop)
      (return (i32.const 0)))

    ;; Try $Type (and subtypes) -- delegate to types.wat (identity hash,
    ;; constant 0 for now; ref.eq disambiguates within the bucket). Needed so
    ;; type-values can be $Set members (union members).
    (block $not_type
      (block $is_type (result (ref $Type))
        (br $not_type
          (br_on_cast $is_type (ref eq) (ref $Type)
            (local.get $key))))
      (return (call $type_hash_i31)))

    ;; Unknown type — unreachable for valid keys
    (unreachable)
  )
)
