;; Closure dispatch — unified $Fn2 calling convention.
;;
;; All functions are $Fn2(captures, args). Conts are in captures or in
;; the args list (conts-first ordering ensures this after lifting).

(module

  ;; Universal closure dispatcher. Host-callable entry point for the
  ;; runner; also tail-called from every CPS continuation site.
  ;; Export name `_apply` is the host ABI contract — do not rename.
  (func $rt/apply.wat:apply (export "rt/apply.wat:apply")
    (param $args (ref null any))
    (param $callee (ref null any))

    (local $clos (ref $Closure))
    (local.set $clos (ref.cast (ref $Closure) (local.get $callee)))

    (return_call_ref $Fn2
      (struct.get $Closure $captures (local.get $clos))
      (local.get $args)
      (ref.cast (ref $Fn2) (struct.get $Closure $func (local.get $clos))))
  )

)
