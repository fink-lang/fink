;; Start -- runtime bootstrap.
;;
;; Owns the module's single `(start)`. A wasm module has exactly one start
;; function, and all `rt/*.wat` merge into one module, so this file is the
;; sole owner: it runs once at instantiation, before the host invokes the
;; entry module's wrapper export -- the hook for runtime setup that must happen
;; before any user code.
;;
;; So far that is registering the built-in type singletons (str, int, float), so
;; a guard/match on them resolves against a live $Type.

(module

  (import "rt/str.wat"   "register_str_type"   (func $register_str_type))
  (import "rt/int.wat"   "register_int_type"   (func $register_int_type))
  (import "rt/float.wat" "register_float_type" (func $register_float_type))

  (func $:bootstrap
    (call $register_str_type)
    (call $register_int_type)
    (call $register_float_type))

  (start $:bootstrap)
)
