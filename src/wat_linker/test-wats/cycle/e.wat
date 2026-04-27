;; Two-way cycle: e.wat imports from f.wat, f.wat imports from e.wat.
;; Linker must tolerate this — each module appears once in output,
;; cross-module references resolve via id-rename.

(module

  (import "./f.wat" "f_helper"
    (func $f_helper (param i32) (result i32)))

  (func $e_top (export "e_top")
    (param $x i32) (result i32)
    (call $f_helper (i32.add (local.get $x) (i32.const 1))))

  (func $e_helper (export "e_helper")
    (param $x i32) (result i32)
    (i32.mul (local.get $x) (i32.const 2)))
)
