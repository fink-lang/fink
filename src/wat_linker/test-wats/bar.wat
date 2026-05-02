;; Companion to foo.wat — provides the symbols foo.wat imports.
;;
;; Inter-wat exports (marked with `(@pub)`):
;;   - type Bar      (referenced as `(import "./bar.wat" "Bar" ...)`)
;;   - func bar_make (referenced as `(import "./bar.wat" "bar_make" ...)`)

(module

  ;; --- host import: passes through unchanged into the merged binary ----

  (import "env" "host_clamp" (func $host_clamp (param i32) (result i32)))


  ;; --- locally-defined exported type ------------------------------------

  (type $Bar (@pub) (sub (struct
    (field $val i32))))


  ;; --- private helper: same-file reference exercise ---------------------

  (func $clamp (param $x i32) (result i32)
    (call $host_clamp (local.get $x)))


  ;; --- exported constructor --------------------------------------------

  (func $bar_make (@pub)
    (param $v i32) (result (ref $Bar))
    (struct.new $Bar (call $clamp (local.get $v))))
)
