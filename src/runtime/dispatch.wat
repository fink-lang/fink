;; Closure dispatch — unified $Fn2 calling convention.
;;
;; All functions are $Fn2(captures, args). Conts are in captures or in
;; the args list (conts-first ordering ensures this after lifting).

(module

  (func $_apply (export "_apply")
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
