;; Left arm of the diamond. Imports D, defines B wrapping a D.

(module

  (import "./d.wat" "D" (type $D (sub any)))

  (type $B (sub (struct
    (field $payload (ref $D)))))

  (func $b_make (export "b_make")
    (param $d (ref $D)) (result (ref $B))
    (struct.new $B (local.get $d)))
)
