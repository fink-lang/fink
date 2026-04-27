(module


  (func $test-wats/cycle/f.wat:f_helper 
    (param $x i32) (result i32)
    (call $test-wats/cycle/e.wat:e_helper (i32.sub (local.get $x) (i32.const 1))))


  (func $test-wats/cycle/e.wat:e_top 
    (param $x i32) (result i32)
    (call $test-wats/cycle/f.wat:f_helper (i32.add (local.get $x) (i32.const 1))))

  (func $test-wats/cycle/e.wat:e_helper 
    (param $x i32) (result i32)
    (i32.mul (local.get $x) (i32.const 2)))
)
