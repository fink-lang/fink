;; Calls imported env.print(i32).
;; The host provides the print implementation.
(module
  (import "env" "print" (func $print (param i32)))

  (func (export "fink_main")
    i32.const 42
    call $print
  )
)
