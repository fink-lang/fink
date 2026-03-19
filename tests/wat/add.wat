;; Simple add function: exports add(i32, i32) -> i32
;; The start function calls add(2, 3) so the debugger can step into WASM code.
(module
  (import "env" "print" (func $print (param i32)))
  (func $add (export "add") (param $a i32) (param $b i32) (result i32)
    local.get $a
    local.get $b
    i32.add
  )
  (func $main
    i32.const 2
    i32.const 3
    call $add
    call $print
  )
  (start $main)
)
