;; Top of the diamond. Imports both arms; D arrives twice through the
;; graph but must appear in the merged output exactly once.

(module

  (import "./b.wat" "B" (type $B (sub any)))
  (import "./c.wat" "C" (type $C (sub any)))
  (import "./b.wat" "b_make"
    (func $b_make (param (ref $D)) (result (ref $B))))
  (import "./c.wat" "c_make"
    (func $c_make (param (ref $D)) (result (ref $C))))

  ;; A also reaches D directly (depending on its own field).
  (import "./d.wat" "D" (type $D (sub any)))

  (type $A (sub (struct
    (field $left (ref $B))
    (field $right (ref $C)))))

  (func $a_make (export "a_make")
    (param $d (ref $D)) (result (ref $A))
    (struct.new $A
      (call $b_make (local.get $d))
      (call $c_make (local.get $d))))
)
