;; Start -- runtime bootstrap.
;;
;; Owns the module's single `(start)`. A wasm module has exactly one start
;; function, and all `rt/*.wat` merge into one module, so this file is the
;; sole owner: it runs once at instantiation, before the host invokes the
;; entry module's wrapper export -- the hook for runtime setup that must happen
;; before any user code.
;;
;; So far that is registering the built-in type singletons (str), so a
;; guard/match on `str` resolves against a live $Type.

(module

  (import "rt/str.wat" "register_str_type" (func $register_str_type))

  (func $:bootstrap
    (call $register_str_type))

  (start $:bootstrap)
)
