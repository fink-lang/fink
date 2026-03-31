;; Hashing — centralised hash dispatch for fink runtime types
;;
;; WASM GC implementation using br_on_cast type dispatch.
;;
;; Design:
;;   - hash_i31 produces a 31-bit hash packed into i31ref
;;   - Dispatches on input type via br_on_cast chain:
;;       i31ref   → i31.get_s (value is its own hash)
;;       $Num     → f64 bits folded to i31
;;       $Str     → delegates to str_hash_i31 (string module owns its internals)
;;   - Future hash variants (hash_64, hash_bytes) can be added alongside
;;   - Compiler can shortcut the dispatch when the type is statically known
;;
;; Exported functions:
;;   $hash_i31  : (ref eq) -> i32

(module


  ;; -- Hash dispatch ------------------------------------------------------

  ;; hash_i31(key) → 31-bit hash as i32
  ;;
  ;; br_on_cast dispatch over known built-in types.
  ;; Unreachable for types that are not valid hash keys (closures,
  ;; collections, templates). The compiler must not emit hash_i31
  ;; calls for those types.
  (func $hash_i31 (export "hash_i31")
    (param $key (ref eq))
    (result i32)

    ;; Try i31ref — value is its own hash
    (block $not_i31
      (block $is_i31 (result (ref i31))
        (br $not_i31
          (br_on_cast $is_i31 (ref eq) (ref i31)
            (local.get $key))))
      (return (i31.get_s)))

    ;; Try $Num — fold f64 bits to i31
    (block $not_num
      (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref eq) (ref $Num)
            (local.get $key))))
      (return (call $_hash_f64
        (struct.get $Num $val))))

    ;; Try $Str — delegate to string module
    (block $not_str
      (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref eq) (ref $Str)
            (local.get $key))))
      (return (call $str_hash_i31)))

    ;; Unknown type — unreachable for valid keys
    (unreachable)
  )


  ;; -- Helpers ------------------------------------------------------------

  ;; _hash_f64 — fold 64-bit float to 31-bit hash
  ;;
  ;; XOR the upper and lower 32-bit halves, then mask to 31 bits
  ;; so the result fits in i31ref without overflow.
  (func $_hash_f64
    (param $v f64)
    (result i32)

    (local $bits i64)
    (local.set $bits (i64.reinterpret_f64 (local.get $v)))

    (i32.and
      (i32.xor
        (i32.wrap_i64 (local.get $bits))
        (i32.wrap_i64 (i64.shr_u (local.get $bits) (i64.const 32))))
      (i32.const 0x7fffffff))
  )

)
