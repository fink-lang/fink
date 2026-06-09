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
;; A single process-wide $Rec, keyed by URL ($Str), value = module
;; rec ($Rec). Module export keys are known at compile time, so the
;; registry and its values are records, not dicts. Created lazily on
;; first `init`. The presence of a key
;; in the registry IS the "module already initialised" flag — no
;; separate bool. The empty rec is created up front by `init` so `pub`
;; only ever mutates an existing rec.
;;
;; ## Calling `import_module` via apply_3
;;
;; Every `<fqn>:import_module` is a Fn3-shaped function. `import`
;; routes it through `apply_3` with `empty_ctx` as the universe seed
;; -- modules are the universe entry, no caller ctx exists yet.
;;
;; ## wrap_cont — the bridge between import_module's done and import's cont
;;
;; The producer's import_module fires its done arg when finished.
;; `import` passes a synthesised wrap_cont as that done — a $Closure
;; over $_import_wrap_step that captures (url_str, user_cont). When
;; fired, wrap_cont reads registry[url] (now populated) and tail-
;; applies user_cont with the rec.

(module

  ;; Type imports
  (import "std/dict.wat"  "Rec"      (type $Rec      (sub any)))
  (import "std/list.wat"  "List"     (type $List     (sub any)))
  (import "rt/apply.wat"  "Closure"  (type $Closure  (sub any)))
  (import "rt/apply.wat"  "Captures" (type $Captures (sub any)))
  (import "rt/apply.wat"  "Fn3"      (type $Fn3      (sub any)))

  ;; Func imports
  (import "rt/apply.wat"  "apply_3"
    (func $apply_3
      (param $args (ref null any))
      (param $ctx (ref null any))
      (param $callee (ref null any))))
  (import "rt/apply.wat"  "empty_ctx"
    (func $empty_ctx (result (ref any))))
  (import "std/dict.wat"  "_rec_new"
    (func $rec_new (result (ref $Rec))))
  (import "std/dict.wat"  "get"
    (func $rec_get (param $rec (ref $Rec)) (param $key (ref eq)) (result (ref null eq))))
  (import "std/dict.wat"  "_set_field"
    (func $put_field (param $rec (ref null any)) (param $key (ref null any)) (param $val (ref null any)) (result (ref null any))))
  (import "std/list.wat"  "head_any"
    (func $list_head_any (param $list (ref null any)) (result (ref null any))))
  (import "rt/apply.wat" "apply_1" (func $apply_1 (;apply-ctx;) (param (ref null any)) (param $val (ref null any)) (param $cont (ref null any))))
  (import "std/list.wat"  "empty"
    (func $list_empty (result (ref $List))))
  (import "std/list.wat"  "prepend"
    (func $list_prepend (param $head (ref any)) (param $tail (ref $List)) (result (ref $List))))

  ;; Args machinery + int unboxing for the fink-callable get_module_url
  ;; Fn3 wrapper.
  (import "rt/apply.wat" "args_head"
    (func $args_head (param (ref null any)) (result (ref null any))))
  (import "rt/apply.wat" "args_tail"
    (func $args_tail (param (ref null any)) (result (ref null any))))
  (import "rt/apply.wat" "args_empty"
    (func $args_empty (result (ref any))))
  (import "rt/apply.wat" "args_prepend"
    (func $args_prepend (param (ref null any)) (param (ref any)) (result (ref any))))
  (import "std/num.wat" "Num" (type $Num (sub any)))
  (import "std/int.wat" "Int" (type $Int (sub $Num (struct))))
  (import "std/int.wat" "_int_ival"
    (func $int_ival (param (ref $Int)) (result i64)))


  ;; -- Registry -------------------------------------------------------

  ;; The URL→rec map. Lazy-initialised on first `init` call.
  ;; Stored as $Rec so we can call rec_get/put_field directly.
  (global $registry (mut (ref null $Rec)) (ref.null $Rec))


  ;; -- Module-id → url array ------------------------------------------
  ;;
  ;; The trace buffer records call sites as (module_id, cps_id) i32 pairs
  ;; (rt/trace.wat). Resolving a frame back to a source location needs
  ;; module_id → url. ModuleId is a compile-time concept that does not
  ;; otherwise exist in the runtime, so each module self-registers its
  ;; own (id, url) from its fink_module via `register_module`.
  ;;
  ;; A GC array indexed directly by module_id (not a wasm table). Module
  ;; ids are small dense ints assigned in BFS order by compile_package.
  ;; It grows by 100 slots at a time (array.copy into a bigger array, the
  ;; same idiom dict.wat/set.wat use) when an id exceeds capacity.
  (type $ModuleUrls (array (mut (ref null any))))
  (global $module_urls (mut (ref null $ModuleUrls)) (ref.null $ModuleUrls))

  ;; Record module_id → url. Idempotent: re-registering the same id with
  ;; the same url is a harmless overwrite (fink_module may run more than
  ;; once). Lazy-allocates and grows the table as needed.
  (func $register_module (@pub)
    (param $id i32)
    (param $url (ref null any))
    (local $old (ref null $ModuleUrls))
    (local $new_len i32)
    (local $new (ref $ModuleUrls))

    (local.set $old (global.get $module_urls))

    ;; Grow (or allocate) if id is out of the current bounds.
    (if (i32.ge_u
          (local.get $id)
          (if (result i32) (ref.is_null (local.get $old))
            (then (i32.const 0))
            (else (array.len (ref.as_non_null (local.get $old))))))
      (then
        ;; new_len = max(old_len + 100, id + 1) so a single large id
        ;; can't outpace the growth step.
        (local.set $new_len
          (i32.add
            (if (result i32) (ref.is_null (local.get $old))
              (then (i32.const 0))
              (else (array.len (ref.as_non_null (local.get $old)))))
            (i32.const 100)))
        (if (i32.gt_u (i32.add (local.get $id) (i32.const 1)) (local.get $new_len))
          (then (local.set $new_len (i32.add (local.get $id) (i32.const 1)))))

        (local.set $new (array.new $ModuleUrls (ref.null any) (local.get $new_len)))
        (if (i32.eqz (ref.is_null (local.get $old)))
          (then
            (array.copy $ModuleUrls $ModuleUrls
              (local.get $new) (i32.const 0)
              (ref.as_non_null (local.get $old)) (i32.const 0)
              (array.len (ref.as_non_null (local.get $old))))))
        (global.set $module_urls (local.get $new))))

    (array.set $ModuleUrls
      (ref.as_non_null (global.get $module_urls))
      (local.get $id)
      (local.get $url)))

  ;; Look up the url registered for module_id, or null if unregistered
  ;; or out of bounds. Raw i32 worker; the fink-callable entry is the
  ;; Fn3 wrapper below.
  (func $_get_module_url
    (param $id i32)
    (result (ref null any))
    (if (ref.is_null (global.get $module_urls))
      (then (return (ref.null any))))
    (if (i32.ge_u
          (local.get $id)
          (array.len (ref.as_non_null (global.get $module_urls))))
      (then (return (ref.null any))))
    (array.get $ModuleUrls
      (ref.as_non_null (global.get $module_urls))
      (local.get $id)))

  ;; Fink-callable entry: `get_module_url mod_id`. CPS-lowered args =
  ;; [k_caller, mod_id]. mod_id arrives as a boxed $Int; unbox, look up,
  ;; tail-call k_caller with the url (or null).
  (elem declare func $get_module_url_apply)

  (func $get_module_url_apply (type $Fn3)
      (param $_caps (ref null any))
      (param $ctx (ref null any))
      (param $args (ref null any))
    (local $k_caller (ref any))
    (local $id (ref null any))
    (local $rest (ref null any))
    (local $url (ref null any))
    (local $k_args (ref any))

    (local.set $k_caller (ref.as_non_null (call $args_head (local.get $args))))
    (local.set $rest (call $args_tail (local.get $args)))
    (local.set $id (call $args_head (local.get $rest)))

    (local.set $url
      (call $_get_module_url
        (i32.wrap_i64
          (call $int_ival (ref.cast (ref $Int) (local.get $id))))))

    (local.set $k_args (call $args_empty))
    (local.set $k_args (call $args_prepend (local.get $url) (local.get $k_args)))
    (return_call $apply_3
      (local.get $k_args)
      (local.get $ctx)
      (local.get $k_caller)))

  (global $get_module_url_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $get_module_url_apply)
      (ref.null $Captures)))

  (func $get_module_url (@pub)
      (result (ref any))
    (global.get $get_module_url_closure))

  ;; -- init -----------------------------------------------------------
  ;;
  ;; Direct call. Returns 1 if this is the first call for `mod_url`
  ;; (and creates an empty rec at registry[mod_url]); returns 0 if the
  ;; module was already in the registry.
  (func $init (@pub) (@impl "std/modules.fnk:init")
    (param $mod_url (ref null any))
    (result i32)

    (local $reg (ref $Rec))
    (local $key (ref eq))
    (local $existing (ref null eq))

    ;; Lazy-init the registry on first call.
    (if (ref.is_null (global.get $registry))
      (then
        (global.set $registry
          (call $rec_new))))

    (local.set $reg (ref.as_non_null (global.get $registry)))
    (local.set $key (ref.cast (ref eq) (local.get $mod_url)))

    (local.set $existing
      (call $rec_get (local.get $reg) (local.get $key)))

    (if (i32.eqz (ref.is_null (local.get $existing)))
      (then
        ;; Already initialised — return 0.
        (return (i32.const 0))))

    ;; First call — create empty rec and register it.
    (global.set $registry
      (ref.cast (ref $Rec)
        (call $put_field
          (local.get $reg)
          (local.get $key)
          (call $rec_new))))
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
  (func $pub (@pub) (@impl "std/modules.fnk:pub")
    (param $mod_url (ref null any))
    (param $name    (ref null any))
    (param $val     (ref null any))

    (local $reg (ref $Rec))
    (local $key (ref eq))
    (local $rec (ref $Rec))
    (local $new_rec (ref $Rec))

    ;; Ensure registry[mod_url] exists (init is a no-op if already there).
    (drop (call $init (local.get $mod_url)))

    (local.set $reg (ref.as_non_null (global.get $registry)))
    (local.set $key (ref.cast (ref eq) (local.get $mod_url)))

    (local.set $rec
      (ref.cast (ref $Rec)
        (ref.as_non_null
          (call $rec_get (local.get $reg) (local.get $key)))))

    ;; new_rec = rec_set(rec, name, val) — _set_field is the direct
    ;; (non-CPS) shape that returns the updated rec.
    (local.set $new_rec
      (ref.cast (ref $Rec)
        (call $put_field
          (local.get $rec)
          (local.get $name)
          (local.get $val))))

    (global.set $registry
      (ref.cast (ref $Rec)
        (call $put_field
          (local.get $reg)
          (local.get $key)
          (local.get $new_rec)))))


  ;; -- import ---------------------------------------------------------
  ;;
  ;; CPS. If registry[url] exists, tail-apply cont with the rec.
  ;; Otherwise, build a wrap_cont closure capturing (url, cont) and
  ;; call_ref the producer's import_module funcref with that as its
  ;; done arg. When import_module finishes, it fires wrap_cont, which
  ;; reads the now-populated rec and tail-applies user cont.
  (func $import (@pub) (@impl "std/modules.fnk:import")
    (param $ctx     (ref null any))
    (param $url     (ref null any))
    (param $mod_ref (ref null any))
    (param $cont    (ref null any))

    (local $reg (ref null $Rec))
    (local $key (ref eq))
    (local $existing (ref null eq))
    (local $caps (ref $Captures))
    (local $wrap_clos (ref $Closure))

    (local.set $reg (global.get $registry))
    (local.set $key (ref.cast (ref eq) (local.get $url)))

    ;; Already inited? Tail-apply cont with the cached rec.
    (if (i32.eqz (ref.is_null (local.get $reg)))
      (then
        (local.set $existing
          (call $rec_get
            (ref.as_non_null (local.get $reg))
            (local.get $key)))
        (if (i32.eqz (ref.is_null (local.get $existing)))
          (then
            (return_call $apply_1
              (local.get $ctx)
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
    (drop (call $init (local.get $url)))

    ;; Synthesise wrap_cont, capturing (url, cont) so it can read
    ;; registry[url] after the producer finishes and continue with cont.
    (local.set $caps
      (array.new_fixed $Captures 2 (local.get $url) (local.get $cont)))

    (local.set $wrap_clos
      (struct.new $Closure
        (ref.func $_import_wrap_step)
        (local.get $caps)))

    ;; Fn3 calling convention: ctx is a native wasm param. The
    ;; imported module body runs under the importer's ctx so its
    ;; `get_ctx`/`set_ctx` see and mutate the same universe context.
    ;; The module body tail-calls wrap_clos with the (possibly
    ;; mutated) ctx; wrap_clos forwards it to user_cont.
    (return_call $apply_3
      (call $list_prepend (local.get $wrap_clos) (call $list_empty))
      (local.get $ctx)
      (local.get $mod_ref)))


  ;; Internal: like `std/list.wat:apply_2_vals` but allows null
  ;; values. Substitutes `$Nil` for null since `$Cons.head` is
  ;; `(ref any)` (non-null). Used to fire the host-supplied cont at
  ;; module-init completion. The caller threads the ctx the cont
  ;; should see — for the entry module's wrapper-done cont this is the
  ;; post-init ctx carrying the seeded effect slots, so the host can
  ;; apply `main` against the live universe.
  (func $_apply_2_nullable
    (param $a (ref null any))
    (param $b (ref null any))
    (param $ctx (ref null any))
    (param $cont (ref null any))

    (return_call $apply_3
      (call $list_prepend
        (call $_or_nil (local.get $a))
        (call $list_prepend
          (call $_or_nil (local.get $b))
          (call $list_empty)))
      (local.get $ctx)
      (local.get $cont)))

  ;; If `v` is null, returns a sentinel non-null (an empty list as a
  ;; placeholder). Otherwise returns `v` cast to non-null.
  ;; TODO: this used to return `(struct.new $Nil)` — now we return an
  ;; empty list as the "missing value" sentinel. Caller semantics
  ;; should be reviewed; the boundary between null/Nil/empty is murky.
  (func $_or_nil
    (param $v (ref null any))
    (result (ref any))
    (if (result (ref any))
      (ref.is_null (local.get $v))
      (then (call $list_empty))
      (else (ref.as_non_null (local.get $v)))))


  ;; -- init_module ----------------------------------------------------
  ;;
  ;; Host-facing module init. CPS — tail-applies cont with two values:
  ;; (last_expr, val) where:
  ;;   - last_expr = the value the module's block evaluated to (or null
  ;;     if the module was already initialised in a prior call).
  ;;   - val = if key is null: the full exports rec
  ;;          else: registry[mod_url][key] (a single named export).
  ;;
  ;; Each module's lower-synthesised wrapper export tail-calls this with
  ;; the canonical url, the no-capture closure over the module's
  ;; fink_module funcref, an optional key, and the host cont.
  ;;
  ;; The host calls a module's wrapper export (named by canonical URL).
  ;; The wrapper passes through to here. Result: a single API by which
  ;; any host can both run-a-module and fetch-an-export.
  (func $init_module (@pub) (@impl "std/modules.fnk:init_module")
    (param $mod_url  (ref null any))
    (param $mod_clos (ref null any))
    (param $cont     (ref null any))

    (local $reg (ref null $Rec))
    (local $key_eq (ref eq))
    (local $existing (ref null eq))
    (local $exports (ref null any))
    (local $caps (ref $Captures))
    (local $intermediate (ref $Closure))

    (local.set $reg (global.get $registry))
    (local.set $key_eq (ref.cast (ref eq) (local.get $mod_url)))

    ;; Already inited? Return null last_expr + the cached exports rec.
    (if (i32.eqz (ref.is_null (local.get $reg)))
      (then
        (local.set $existing
          (call $rec_get
            (ref.as_non_null (local.get $reg))
            (local.get $key_eq)))
        (if (i32.eqz (ref.is_null (local.get $existing)))
          (then
            (local.set $exports (local.get $existing))
            (return_call $_apply_2_nullable
              (ref.null any)
              (local.get $exports)
              (call $empty_ctx)
              (local.get $cont))))))

    ;; Not inited — ensure registry[mod_url] exists, then invoke the
    ;; module closure with an intermediate cont that captures
    ;; (mod_url, cont) and packages the result.
    (drop (call $init (local.get $mod_url)))

    (local.set $caps
      (array.new_fixed $Captures 2
        (local.get $mod_url)
        (local.get $cont)))

    (local.set $intermediate
      (struct.new $Closure
        (ref.func $_init_module_step)
        (local.get $caps)))

    ;; Fn3 calling convention: ctx is a native wasm param. The module
    ;; body is the universe entry; we seed with empty_ctx here. Args
    ;; list carries only the intermediate cont. Module body shape:
    ;; `fn :caps_param, :ctx_param, :params` with cont = args_head(:params).
    (return_call $apply_3
      (call $list_prepend (local.get $intermediate) (call $list_empty))
      (call $empty_ctx)
      (local.get $mod_clos)))


  ;; _init_module_step: backs intermediate_cont. Called when the
  ;; module's fink_module finishes evaluation. Captures hold
  ;; (mod_url, cont). Reads registry[mod_url] (now populated) and
  ;; tail-applies cont with (last_expr, exports_rec).
  (elem declare func $_init_module_step)

  (func $_init_module_step (type $Fn3)
    (param $caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cap_arr (ref $Captures))
    (local $mod_url (ref null any))
    (local $user_cont (ref null any))
    (local $last_expr (ref null any))
    (local $exports (ref null any))

    (local.set $cap_arr (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $mod_url   (array.get $Captures (local.get $cap_arr) (i32.const 0)))
    (local.set $user_cont (array.get $Captures (local.get $cap_arr) (i32.const 1)))

    ;; Pull last_expr off the args list — it's the head.
    (local.set $last_expr
      (call $list_head_any (local.get $args)))

    ;; Read the now-populated exports rec.
    (local.set $exports
      (call $rec_get
        (ref.as_non_null (global.get $registry))
        (ref.cast (ref eq) (local.get $mod_url))))

    ;; Fire the host cont with the module body's post-init ctx, not a
    ;; fresh empty one. The entry module's body seeds the universe's
    ;; effect slots (unique/tasks/future/with) into ctx via top-level
    ;; init_ctx side-effects; threading that ctx forward lets the host
    ;; apply `main` against the seeded universe instead of empty_ctx.
    (return_call $_apply_2_nullable
      (local.get $last_expr)
      (local.get $exports)
      (local.get $ctx)
      (local.get $user_cont)))


  ;; -- _import_wrap_step ---------------------------------------------
  ;;
  ;; The Fn3-shaped function that backs wrap_cont. Captures hold
  ;; (url, user_cont). When fired by the producer's done, it ignores
  ;; whatever payload arrives in $args, reads registry[url] (now
  ;; guaranteed populated), and tail-applies user_cont with the rec.
  ;;
  ;; Declared with `elem declare` so `ref.func` is valid.
  (elem declare func $_import_wrap_step)

  (func $_import_wrap_step (type $Fn3)
    (param $caps (ref null any))
    (param $ctx (ref null any))
    (param $_args (ref null any))

    (local $cap_arr (ref $Captures))
    (local $url (ref null any))
    (local $user_cont (ref null any))
    (local $rec (ref null eq))

    (local.set $cap_arr (ref.cast (ref $Captures) (local.get $caps)))
    (local.set $url       (array.get $Captures (local.get $cap_arr) (i32.const 0)))
    (local.set $user_cont (array.get $Captures (local.get $cap_arr) (i32.const 1)))

    (local.set $rec
      (call $rec_get
        (ref.as_non_null (global.get $registry))
        (ref.cast (ref eq) (local.get $url))))

    ;; ctx flows from the imported module body's tail-call (which
    ;; passes its possibly-mutated ctx forward). Thread it onward so
    ;; the importer's continuation sees ctx mutations the imported
    ;; module made.
    (return_call $apply_1
      (local.get $ctx)
      (local.get $rec)
      (local.get $user_cont)))

)
