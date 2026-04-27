;; Bottom of the diamond. No imports, just a type and a getter.

(module

  (type $D (sub (struct
    (field $val i32))))

  (func $d_get (export "d_get")
    (param $d (ref $D)) (result i32)
    (struct.get $D $val (local.get $d)))
)
