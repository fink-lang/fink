;; Closure dispatch — apply_2 / apply_3 helpers.
;;
;; Universal calling convention:
;;   $Fn2(captures, args)        — continuations, match arms
;;   $Fn3(captures, args, cont)  — user functions
;;
;; _apply_2: dispatch a closure call without cont.
;; _apply_3: dispatch a closure call with cont.
;; Both are trivial pass-through — extract funcref + captures from
;; the $Closure struct, tail-call with the appropriate signature.

(module

  ;; $Fn2 and $Fn3 are defined in types.wat (canonical rec group).

  ;; _apply: dispatch without cont (continuations, match arms).
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

  ;; _apply_cont: dispatch with cont.
  ;; Tries $Fn3 first; if the callee is $Fn2 (continuation), prepends cont
  ;; onto the args list and dispatches as $Fn2.
  (func $_apply_cont (export "_apply_cont")
    (param $args (ref null any))
    (param $cont (ref null any))
    (param $callee (ref null any))

    (local $clos (ref $Closure))
    (local $fref funcref)
    (local.set $clos (ref.cast (ref $Closure) (local.get $callee)))
    (local.set $fref (struct.get $Closure $func (local.get $clos)))

    ;; If funcref is $Fn3, call directly with cont as separate param.
    (if (ref.test (ref $Fn3) (local.get $fref))
      (then
        (return_call_ref $Fn3
          (struct.get $Closure $captures (local.get $clos))
          (local.get $args)
          (local.get $cont)
          (ref.cast (ref $Fn3) (local.get $fref)))))

    ;; Fallback: $Fn2 — prepend cont onto args list.
    (return_call_ref $Fn2
      (struct.get $Closure $captures (local.get $clos))
      (call $list_prepend
        (ref.as_non_null (local.get $cont))
        (ref.cast (ref $List) (local.get $args)))
      (ref.cast (ref $Fn2) (local.get $fref)))
  )

)
