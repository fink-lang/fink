;; Compiled from tests/fnk/add.fnk
;;
;; add = fn a, b:        ;; line 1
;;   a + b               ;; line 2
;;
;; fink_main = fn:       ;; line 4
;;   add 2, 3            ;; line 5
;;   | print             ;; line 6

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
