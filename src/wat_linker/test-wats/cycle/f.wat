;; Other half of the cycle. Imports e_helper from e.wat.

(module

  (import "./e.wat" "e_helper"
    (func $e_helper (param i32) (result i32)))

  (func $f_helper (export "f_helper")
    (param $x i32) (result i32)
    (call $e_helper (i32.sub (local.get $x) (i32.const 1))))
)
