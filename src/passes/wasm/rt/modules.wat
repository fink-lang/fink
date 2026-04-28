;; Multi-module support — `std/modules.fnk:` protocol.
;;
;; Three primitives back the Fink-level `pub` and `import` keywords:
;;
;;   std/modules.fnk:init   mod_url            -> i32     (direct call)
;;   std/modules.fnk:pub    mod_url, name, val           (direct call)
;;   std/modules.fnk:import url, mod_ref, cont           (CPS — tail-applies cont)
;;
;; They are emitted inline by `lower` — `pub` and `import` are not
;; runtime-dispatched; lowering knows the FQN of the importing module
;; at compile time and synthesises the call sites directly.
;;
;; ## Registry
;;
;; A single process-wide $DictImpl, keyed by URL ($Str), value = module
;; rec ($RecImpl). Created lazily on first `init`. The presence of a key
;; in the registry IS the "module already initialised" flag — no
;; separate bool. The empty rec is created up front by `init` so `pub`
;; only ever mutates an existing rec.
;;
;; ## Calling `import_module` without a closure wrapper
;;
;; Every `<fqn>:import_module` is a Fn2-shaped function with no
;; captures. `import` calls it via `return_call_ref $Fn2 (null, args,
;; mod_ref)` directly — no $Closure box, no _apply dispatch.
;;
;; ## wrap_cont — the bridge between import_module's done and import's cont
;;
;; The producer's import_module fires its done arg when finished.
;; `import` passes a synthesised wrap_cont as that done — a $Closure
;; over $_import_wrap_step that captures (url_str, user_cont). When
;; fired, wrap_cont reads registry[url] (now populated) and tail-
;; applies user_cont with the rec.

(module

  ;; -- Registry -------------------------------------------------------

  ;; The URL→rec map. Lazy-initialised on first `init` call.
  ;; Stored as $DictImpl so we can call dict_get/dict_set directly.
  (global $std/modules.fnk:registry (mut (ref null $DictImpl)) (ref.null $DictImpl))

  ;; -- init -----------------------------------------------------------
  ;;
  ;; Direct call. Returns 1 if this is the first call for `mod_url`
  ;; (and creates an empty rec at registry[mod_url]); returns 0 if the
  ;; module was already in the registry.
  (func $std/modules.fnk:init (export "std/modules.fnk:init")
    (param $mod_url (ref null any))
    (result i32)

    (local $reg (ref $DictImpl))
    (local $key (ref eq))
    (local $existing (ref null eq))

    ;; Lazy-init the registry on first call.
    (if (ref.is_null (global.get $std/modules.fnk:registry))
      (then
        (global.set $std/modules.fnk:registry
          (call $std/dict.wat:dict_empty))))

    (local.set $reg (ref.as_non_null (global.get $std/modules.fnk:registry)))
    (local.set $key (ref.cast (ref eq) (local.get $mod_url)))

    (local.set $existing
      (call $std/dict.wat:dict_get (local.get $reg) (local.get $key)))

    (if (i32.eqz (ref.is_null (local.get $existing)))
      (then
        ;; Already initialised — return 0.
        (return (i32.const 0))))

    ;; First call — create empty rec and register it.
    (global.set $std/modules.fnk:registry
      (call $std/dict.wat:dict_set
        (local.get $reg)
        (local.get $key)
        (call $std/dict.wat:_rec_new)))
    (i32.const 1))


  ;; -- pub ------------------------------------------------------------
  ;;
  ;; Direct call. Mutates registry[mod_url] in place by setting a new
  ;; rec with name → val. The old rec is dropped — HAMT-persistent
  ;; semantics, the new dict entry replaces the old binding.
  ;;
  ;; Idempotent — calls `init` first to ensure the registry slot
  ;; exists. This makes single-fragment compiles (entry never called
  ;; via `import`) safe: their body's `pub` calls auto-create the
  ;; registry slot on first hit.
  (func $std/modules.fnk:pub (export "std/modules.fnk:pub")
    (param $mod_url (ref null any))
    (param $name    (ref null any))
    (param $val     (ref null any))

    (local $reg (ref $DictImpl))
    (local $key (ref eq))
    (local $rec (ref $RecImpl))
    (local $new_rec (ref $RecImpl))

    ;; Ensure registry[mod_url] exists (init is a no-op if already there).
    (drop (call $std/modules.fnk:init (local.get $mod_url)))

    (local.set $reg (ref.as_non_null (global.get $std/modules.fnk:registry)))
    (local.set $key (ref.cast (ref eq) (local.get $mod_url)))

    (local.set $rec
      (ref.cast (ref $RecImpl)
        (ref.as_non_null
          (call $std/dict.wat:dict_get (local.get $reg) (local.get $key)))))

    ;; new_rec = rec_set(rec, name, val) — _set_field is the direct
    ;; (non-CPS) shape that returns the updated rec.
    (local.set $new_rec
      (ref.cast (ref $RecImpl)
        (call $std/rec.fnk:put_field
          (local.get $rec)
          (local.get $name)
          (local.get $val))))

    (global.set $std/modules.fnk:registry
      (call $std/dict.wat:dict_set
        (local.get $reg)
        (local.get $key)
        (local.get $new_rec))))


  ;; -- import ---------------------------------------------------------
  ;;
  ;; CPS. If registry[url] exists, tail-apply cont with the rec.
  ;; Otherwise, build a wrap_cont closure capturing (url, cont) and
  ;; call_ref the producer's import_module funcref with that as its
  ;; done arg. When import_module finishes, it fires wrap_cont, which
  ;; reads the now-populated rec and tail-applies user cont.
  (func $std/modules.fnk:import (export "std/modules.fnk:import")
    (param $url     (ref null any))
    (param $mod_ref (ref null any))
    (param $cont    (ref null any))

    (local $reg (ref null $DictImpl))
    (local $key (ref eq))
    (local $existing (ref null eq))
    (local $caps (ref $Captures))
    (local $wrap_clos (ref $Closure))

    (local.set $reg (global.get $std/modules.fnk:registry))
    (local.set $key (ref.cast (ref eq) (local.get $url)))

    ;; Already inited? Tail-apply cont with the cached rec.
    (if (i32.eqz (ref.is_null (local.get $reg)))
      (then
        (local.set $existing
          (call $std/dict.wat:dict_get
            (ref.as_non_null (local.get $reg))
            (local.get $key)))
        (if (i32.eqz (ref.is_null (local.get $existing)))
          (then
            (return_call $std/list.wat:apply_1
              (local.get $existing)
              (local.get $cont))))))

    ;; Not inited — register the empty rec for this URL, then invoke
    ;; the producer's module-closure to populate it.
    ;;
    ;; init creates `registry[url] = empty_rec`, so subsequent `pub`
    ;; calls inside the producer body have a rec to mutate. Calling
    ;; init here (rather than wrapping it around every producer body
    ;; in lowering) keeps the producer body dumb — it's a regular
    ;; CPS fn that doesn't know about the registry.
    (drop (call $std/modules.fnk:init (local.get $url)))

    ;; Synthesise wrap_cont, capturing (url, cont) so it can read
    ;; registry[url] after the producer finishes and continue with cont.
    (local.set $caps
      (array.new_fixed $Captures 2 (local.get $url) (local.get $cont)))

    (local.set $wrap_clos
      (struct.new $Closure
        (ref.func $std/modules.fnk:_import_wrap_step)
        (local.get $caps)))

    ;; Standard apply path: args = Cons(wrap_clos, Nil), callee = mod_ref.
    ;; The caller wrapped the module's `import_module` funcref in a
    ;; no-capture $Closure at the lowering site, so mod_ref is already
    ;; anyref-compatible and dispatches through _apply normally.
    (return_call $rt/apply.wat:apply
      (struct.new $Cons (local.get $wrap_clos) (struct.new $Nil))
      (local.get $mod_ref)))


  ;; -- _import_wrap_step ---------------------------------------------
  ;;
  ;; The Fn2-shaped function that backs wrap_cont. Captures hold
  ;; (url, user_cont). When fired by the producer's done, it ignores
  ;; whatever payload arrives in $args, reads registry[url] (now
  ;; guaranteed populated), and tail-applies user_cont with the rec.
  ;;
  ;; Declared with `elem declare` so `ref.func` is valid.
  (elem declare func $std/modules.fnk:_import_wrap_step)

  (func $std/modules.fnk:_import_wrap_step (type $Fn2)
    (param $caps (ref null any))
    (param $_args (ref null any))

    (local $cap_arr (ref $Captures))
    (local $url (ref null any))
    (local $user_cont (ref null any))
    (local $rec (ref null eq))

    (local.set $cap_arr (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $url       (array.get $Captures (local.get $cap_arr) (i32.const 0)))
    (local.set $user_cont (array.get $Captures (local.get $cap_arr) (i32.const 1)))

    (local.set $rec
      (call $std/dict.wat:dict_get
        (ref.as_non_null (global.get $std/modules.fnk:registry))
        (ref.cast (ref eq) (local.get $url))))

    (return_call $std/list.wat:apply_1
      (local.get $rec)
      (local.get $user_cont)))

)
