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

  (import "std/num.wat" "hash_i31"
    (func $num_hash_i31 (param (ref $Num)) (result i32)))
  (import "std/str.wat" "hash_i31"
    (func $str_hash_i31 (param (ref $Str)) (result i32)))


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

    ;; Try $Closure -- hash to a constant. Allows using closures as
    ;; dict/rec keys (identity-based equality). All closures collide
    ;; into the same hash bucket; the hamt's eq check (ref.eq via
    ;; deep_eq) distinguishes them. O(n) lookup on bucket size; fine
    ;; for the small dispatch tables typical of effect handlers.
    (block $not_clos
      (block $is_clos (result (ref $Closure))
        (br $not_clos
          (br_on_cast $is_clos (ref eq) (ref $Closure)
            (local.get $key))))
      (drop)
      (return (i32.const 0)))

    ;; Unknown type — unreachable for valid keys
    (unreachable)
  )
)
