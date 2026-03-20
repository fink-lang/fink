;; Simple add function: exports add(i32, i32) -> i32
;; fink_main calls add(2, 3) and prints the result.
(module
  (import "env" "print" (func $print (param i32)))
  (func $add (export "add") (param $a i32) (param $b i32) (result i32)
    local.get $a
    local.get $b
    i32.add
  )
  (func (export "fink_main")
    i32.const 2
    i32.const 3
    call $add
    call $print
  )
)
