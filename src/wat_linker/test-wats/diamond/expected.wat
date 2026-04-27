(module
  (rec
    (type $test-wats/diamond/d.wat:D (sub (struct
    (field $val i32))))
    (type $test-wats/diamond/b.wat:B (sub (struct
    (field $payload (ref $test-wats/diamond/d.wat:D)))))
    (type $test-wats/diamond/c.wat:C (sub (struct
    (field $payload (ref $test-wats/diamond/d.wat:D)))))
    (type $test-wats/diamond/a.wat:A (sub (struct
    (field $left (ref $test-wats/diamond/b.wat:B))
    (field $right (ref $test-wats/diamond/c.wat:C))))))



  (func $test-wats/diamond/d.wat:d_get 
    (param $d (ref $test-wats/diamond/d.wat:D)) (result i32)
    (struct.get $test-wats/diamond/d.wat:D $val (local.get $d)))


  (func $test-wats/diamond/b.wat:b_make 
    (param $d (ref $test-wats/diamond/d.wat:D)) (result (ref $test-wats/diamond/b.wat:B))
    (struct.new $test-wats/diamond/b.wat:B (local.get $d)))


  (func $test-wats/diamond/c.wat:c_make 
    (param $d (ref $test-wats/diamond/d.wat:D)) (result (ref $test-wats/diamond/c.wat:C))
    (struct.new $test-wats/diamond/c.wat:C (local.get $d)))


  (func $test-wats/diamond/a.wat:a_make 
    (param $d (ref $test-wats/diamond/d.wat:D)) (result (ref $test-wats/diamond/a.wat:A))
    (struct.new $test-wats/diamond/a.wat:A
      (call $test-wats/diamond/b.wat:b_make (local.get $d))
      (call $test-wats/diamond/c.wat:c_make (local.get $d))))
)
