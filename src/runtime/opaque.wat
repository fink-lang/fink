;; Opaque — wrap any value as a fresh identity-bearing value.
;;
;; `opaque x` wraps `x` in a new $Opaque. Each call yields a distinct wrapper:
;; two opaques are equal iff they are the SAME allocation (wrapper ref.eq),
;; regardless of what they wrap. So `opaque x` mints a fresh identity that
;; carries `x` but does not expose or compare by it.
;;
;; Equality is wrapper-identity (not the inner value's) -- that is the whole
;; point: an identity token must be distinct per mint. But HASHING forwards to
;; the inner value, so opaques distribute across hamt buckets by their inner's
;; hash instead of all colliding into one. Wrapping a counter (e.g. `unique`)
;; therefore gives well-distributed, identity-equal tokens with O(1) lookup.
;;
;; The a == b => hash(a) == hash(b) invariant holds: equal wrappers are the
;; same allocation, hence the same inner, hence the same forwarded hash. A
;; hash collision with the bare inner value (or another opaque) is harmless --
;; the hamt's ref.eq disambiguates within a bucket.
;;
;; Used as the substrate identity token: futures, channels, and ctx keys wrap
;; a counter id in an $Opaque so identity is nominal (per mint) rather than
;; structural. Records/lists are structurally equal, so they cannot serve as
;; identity; $Opaque fills that gap. It is, in effect, the first nominal type
;; in the runtime; when user-declared types land, this is the shape they
;; lower to.

(module

  (import "rt/apply.wat" "Closure"  (type $Closure  (sub any)))
  (import "rt/apply.wat" "Captures" (type $Captures (sub any)))
  (import "rt/apply.wat" "Fn3"      (type $Fn3      (func (param (ref null any) (ref null any) (ref null any)))))

  (import "rt/apply.wat" "apply_3"
    (func $apply_3 (param (ref null any)) (param (ref null any)) (param (ref null any))))
  (import "rt/apply.wat" "args_head"
    (func $args_head (param (ref null any)) (result (ref null any))))
  (import "rt/apply.wat" "args_tail"
    (func $args_tail (param (ref null any)) (result (ref null any))))
  (import "rt/apply.wat" "args_prepend"
    (func $args_prepend (param (ref any)) (param (ref null any)) (result (ref any))))
  (import "rt/apply.wat" "args_empty"
    (func $args_empty (result (ref any))))

  ;; Hashing dispatcher -- forward the inner value's hash. Circular with
  ;; hashing.wat (which imports our hash_i31); WAT resolves it at link.
  (import "rt/hashing.wat" "hash_i31"
    (func $hash_dispatch (param (ref eq)) (result i32)))


  ;; -- $Opaque type ---------------------------------------------------
  ;;
  ;; Single-field wrapper. $inner is the wrapped value; the wrapper's own
  ;; allocation is the identity. (sub (struct)) makes it an eq-type so
  ;; ref.eq and hamt keying work.
  (type $Opaque (@pub) (sub (struct (field $inner (ref eq)))))


  ;; -- opaque x -------------------------------------------------------
  ;;
  ;; Fink-level: `id = opaque x`. CPS shape (Fn3): args = [x, cont].
  ;; Wraps x in a fresh $Opaque and tail-applies cont with it.

  (elem declare func $opaque_apply)

  (func $opaque_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $val (ref null any))
    (local $cont (ref null any))

    ;; Fn3 args convention: cont is the head, real args follow.
    (local.set $cont (call $args_head (local.get $args)))
    (local.set $val  (call $args_head (call $args_tail (local.get $args))))

    (return_call $apply_3
      (call $args_prepend
        (struct.new $Opaque (ref.cast (ref eq) (local.get $val)))
        (call $args_empty))
      (local.get $ctx)
      (local.get $cont)))

  (global $opaque_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $opaque_apply)
      (ref.null $Captures)))

  (func $opaque (@pub) (result (ref any))
    (global.get $opaque_closure))


  ;; -- Identity equality / forwarded hash -----------------------------
  ;;
  ;; op_eq / op_neq: identity (ref.eq on the wrapper). protocols.wat
  ;; dispatches its $Opaque arm here. hash_i31: forward to the inner value
  ;; via the central dispatcher, so opaques distribute by inner hash.

  (func $op_eq (@pub)
    (param $a (ref $Opaque)) (param $b (ref $Opaque)) (result i32)
    (ref.eq (local.get $a) (local.get $b)))

  (func $op_neq (@pub)
    (param $a (ref $Opaque)) (param $b (ref $Opaque)) (result i32)
    (i32.eqz (ref.eq (local.get $a) (local.get $b))))

  (func $hash_i31 (@pub)
    (param $o (ref $Opaque)) (result i32)
    (call $hash_dispatch (struct.get $Opaque $inner (local.get $o))))
)
