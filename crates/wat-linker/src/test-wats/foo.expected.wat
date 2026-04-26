(module
  (rec
    (type $test-wats/bar.wat:Bar (sub (struct
    (field $val i32))))
    (type $test-wats/foo.wat:Foo (sub (struct
    (field $tag i32)
    (field $payload (ref $test-wats/bar.wat:Bar))))))



  ;; --- private helper: same-file reference exercise ---------------------

  (func $test-wats/bar.wat:clamp (param $x i32) (result i32)
    (local $hi i32)
    (local.set $hi (i32.const 1000))
    (block $done (result i32)
      (local.get $x)))


  ;; --- exported constructor --------------------------------------------

  (func $test-wats/bar.wat:bar_make 
    (param $v i32) (result (ref $test-wats/bar.wat:Bar))
    (struct.new $test-wats/bar.wat:Bar (call $test-wats/bar.wat:clamp (local.get $v))))


  ;; --- locally-defined global -------------------------------------------

  (global $test-wats/foo.wat:next_id (mut i32) (i32.const 0))


  ;; --- private helper, called from the exported function ---------------

  (func $test-wats/foo.wat:alloc_id (result i32)
    (local $cur i32)
    (local.set $cur (global.get $test-wats/foo.wat:next_id))
    (global.set $test-wats/foo.wat:next_id (i32.add (local.get $cur) (i32.const 1)))
    (local.get $cur))


  ;; --- exported func: builds a $Foo wrapping a fresh $Bar ---------------

  (func $test-wats/foo.wat:foo_make 
    (param $v i32) (result (ref $test-wats/foo.wat:Foo))

    (local $id i32)
    (local $bar (ref $test-wats/bar.wat:Bar))

    (local.set $id (call $test-wats/foo.wat:alloc_id))
    (local.set $bar (call $test-wats/bar.wat:bar_make (local.get $v)))

    (block $done (result (ref $test-wats/foo.wat:Foo))
      (struct.new $test-wats/foo.wat:Foo
        (local.get $id)
        (local.get $bar))))


  ;; --- second exported func: peels the $Bar back out of a $Foo ----------

  (func (@implements "std/operators.fnk:op_in")
    (param $f (ref $test-wats/foo.wat:Foo)) (result (ref $test-wats/bar.wat:Bar))
    (struct.get $test-wats/foo.wat:Foo $payload (local.get $f)))

  (func $test-wats/foo.wat:op_notin (@implements "std/operators.fnk:op_notin")
    (param $f (ref $test-wats/foo.wat:Foo)) (result (ref $test-wats/bar.wat:Bar))
    (struct.get $test-wats/foo.wat:Foo $payload (local.get $f)))
)
