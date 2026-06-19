;; Symbols -- interned, package-wide source identities.
;;
;; A $Symbol is the runtime identity of a source NAME: a record field, and
;; (later) type / module / function names. The compiler interns each distinct
;; name to one canonical $Symbol per package (dedup-by-name at link time), so
;; `bar` in module A and `bar` in module B are the SAME $Symbol -- identity by
;; ref.eq. This makes structural field access work cross-type (`{foo} = Foo
;; {bar, foo}` maps the anonymous rec's `foo` to Foo's `foo`) without runtime
;; string compares.
;;
;; The $id is the package-wide interned id (assigned by the linker after merging
;; per-module field tables). It IS the hash -- ids are dense and distinct, so
;; they distribute across hamt buckets with no string hashing. The source name
;; is debug/repr metadata, resolved host-side and strippable; the runtime holds
;; only the id.
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
    (ref.eq (local.get $a) (local.get $b)))

  (func $op_neq (@pub)
    (param $a (ref $Symbol)) (param $b (ref $Symbol)) (result i32)
    (i32.eqz (ref.eq (local.get $a) (local.get $b))))

  (func $hash_i31 (@pub)
    (param $s (ref $Symbol)) (result i32)
    (struct.get $Symbol $id (local.get $s)))
)
