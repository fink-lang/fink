;; Operator implementations — CPS functions for arithmetic, comparison, and logic.
;;
;; Each operator follows the CPS calling convention:
;;   (func $op_plus (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
;;     ;; unbox args, compute, box result, tail-call _croc_1(result, cont)
;;   )
;;
;; Type conventions:
;;   - Numbers: $Num struct (f64 field)
;;   - Booleans: i31ref (0 = false, 1 = true)
;;   - Continuation dispatch via _croc_1 (imported from dispatch module)
;;
;; These are the phase-0 implementations operating on concrete types.
;; Protocol-based overloading (future) will replace these with dispatch
;; through user-defined protocol implementations.

(import "@fink/runtime/types" "*" (func (param anyref)))


(module

  ;; _croc_1 is provided by the compiler's emitted module (user code fragment).
  ;; The emitter always generates _croc_N dispatch helpers that handle all
  ;; closure capture counts in the module. The linker resolves this import.
  (import "@fink/user" "_croc_1" (func $croc_1 (param (ref null any)) (param (ref null any))))

  ;; =========================================================================
  ;; Arithmetic: unbox two $Num, f64 op, box result → _croc_1(result, cont)
  ;; =========================================================================

  (func $op_plus (export "op_plus")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (struct.new $Num (f64.add
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $op_minus (export "op_minus")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (struct.new $Num (f64.sub
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $op_mul (export "op_mul")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (struct.new $Num (f64.mul
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $op_div (export "op_div")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (struct.new $Num (f64.div
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  ;; =========================================================================
  ;; Integer arithmetic: unbox $Num → f64 → i64, op, i64 → f64 → box
  ;; =========================================================================

  (func $op_intdiv (export "op_intdiv")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (struct.new $Num (f64.convert_i64_s (i64.div_s
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $a))))
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))))
      (local.get $cont)))

  (func $op_rem (export "op_rem")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (struct.new $Num (f64.convert_i64_s (i64.rem_s
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $a))))
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))))
      (local.get $cont)))

  (func $op_intmod (export "op_intmod")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (struct.new $Num (f64.convert_i64_s (i64.rem_s
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $a))))
        (i64.trunc_f64_s (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))))
      (local.get $cont)))

  ;; =========================================================================
  ;; Comparison: unbox two $Num, f64 compare → i31ref (0/1)
  ;; =========================================================================

  (func $op_eq (export "op_eq")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (f64.eq
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $op_neq (export "op_neq")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (f64.ne
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $op_lt (export "op_lt")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (f64.lt
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $op_lte (export "op_lte")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (f64.le
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $op_gt (export "op_gt")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (f64.gt
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  (func $op_gte (export "op_gte")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (f64.ge
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $a)))
        (struct.get $Num $val (ref.cast (ref $Num) (local.get $b)))))
      (local.get $cont)))

  ;; =========================================================================
  ;; Logic: i31ref bool ops
  ;; =========================================================================

  (func $op_not (export "op_not")
    (param $a (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (i32.eqz (i31.get_s (ref.cast (ref i31) (local.get $a)))))
      (local.get $cont)))

  (func $op_and (export "op_and")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (i32.and
        (i31.get_s (ref.cast (ref i31) (local.get $a)))
        (i31.get_s (ref.cast (ref i31) (local.get $b)))))
      (local.get $cont)))

  (func $op_or (export "op_or")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (i32.or
        (i31.get_s (ref.cast (ref i31) (local.get $a)))
        (i31.get_s (ref.cast (ref i31) (local.get $b)))))
      (local.get $cont)))

  (func $op_xor (export "op_xor")
    (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
    (return_call $croc_1
      (ref.i31 (i32.xor
        (i31.get_s (ref.cast (ref i31) (local.get $a)))
        (i31.get_s (ref.cast (ref i31) (local.get $b)))))
      (local.get $cont)))

)
