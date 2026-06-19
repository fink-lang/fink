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

  ;; -- $Symbol type ----------------------------------------------------
  ;;
  ;; (sub (struct ...)) makes it an eq-type so ref.eq and hamt keying work.
  ;; $id: package-wide interned id. Identity is the allocation (one canonical
  ;; instance per id), so equality is ref.eq and the id doubles as the hash.
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
)
