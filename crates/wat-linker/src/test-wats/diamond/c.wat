;; Right arm of the diamond. Imports D, defines C wrapping a D.

(module

  (import "./d.wat" "D" (type $D (sub any)))

  (type $C (sub (struct
    (field $payload (ref $D)))))

  (func $c_make (export "c_make")
    (param $d (ref $D)) (result (ref $C))
    (struct.new $C (local.get $d)))
)
