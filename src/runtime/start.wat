;; Start -- runtime bootstrap.
;;
;; Owns the module's single `(start)`. A wasm module has exactly one start
;; function, and all `rt/*.wat` merge into one module, so this file is the
;; sole owner: it runs once at instantiation, before the host invokes the
;; entry module's wrapper export. Use it for runtime setup that must happen
;; before any user code -- e.g. registering built-in type singletons so they
;; claim deterministic low arena ids.
;;
;; For now the bootstrap only flips an observable flag (proving the start
;; section survives link + emit and runs). Registration calls get added here
;; as built-in types land.

(module

  (global $:started (export "rt_started") (mut i32) (i32.const 0))

  (func $:bootstrap
    (global.set $:started (i32.const 1)))

  (start $:bootstrap)
)
