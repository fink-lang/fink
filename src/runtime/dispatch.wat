;; Closure dispatch — call-ref-or-closure helpers.
;;
;; _croc_N (call-ref-or-closure, arity N) dispatches a call through a
;; $Closure value: extracts the funcref and captures, then tail-calls
;; the underlying function with captures prepended to the call args.
;;
;; These are shim implementations for runtime-only testing. The compiler
;; emits its own _croc_N that covers all capture counts in the module;
;; the linker replaces these shims with the compiler's versions.
;;
;; Convention:
;;   _croc_N takes N call args + 1 callee (ref null any), tail-calls
;;   the function inside the $Closure with captures + args.

(module

  ;; Shim _croc_1 — used by runtime operators to dispatch continuations.
  ;; Replaced by compiler's full implementation at link time.
  (func $_croc_1 (export "_croc_1")
    (param $a0 (ref null any))
    (param $callee (ref null any))
    (unreachable)
  )

)
