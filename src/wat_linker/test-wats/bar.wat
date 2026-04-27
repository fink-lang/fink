;; Companion to foo.wat — provides the symbols foo.wat imports.
;;
;; Exports:
;;   - type Bar      (referenced as `(import "./bar.wat" "Bar" ...)`)
;;   - func bar_make (referenced as `(import "./bar.wat" "bar_make" ...)`)

(module

  ;; --- locally-defined exported type ------------------------------------

  (type $Bar (sub (struct
    (field $val i32))))


  ;; --- private helper: same-file reference exercise ---------------------

  (func $clamp (param $x i32) (result i32)
    (local $hi i32)
    (local.set $hi (i32.const 1000))
    (block $done (result i32)
      (local.get $x)))


  ;; --- exported constructor --------------------------------------------

  (func $bar_make (export "bar_make")
    (param $v i32) (result (ref $Bar))
    (struct.new $Bar (call $clamp (local.get $v))))
)
