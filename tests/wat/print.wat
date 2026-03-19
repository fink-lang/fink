;; Calls imported env.print(i32) from a start function.
;; The host provides the print implementation.
(module
  (import "env" "print" (func $print (param i32)))

  (func $main
    i32.const 42
    call $print
  )

  (start $main)
)
