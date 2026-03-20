(module
  (type $Any (sub (struct)))
  (type $AnyArray (array (mut anyref)))
  (type $FinkFn (func (param (ref $AnyArray)) (param anyref)))
  (type $FnClosure (sub $Any (struct (field (ref $FinkFn)) (field (ref $AnyArray)))))

  (global $result (export "result") (mut i32) (i32.const 0))

  (func $__halt (type $FinkFn)
    (param $args (ref $AnyArray))
    (param $cont anyref)
    (global.set $result (i31.get_s (ref.cast i31ref (array.get $AnyArray (local.get $args) (i32.const 0))))))

  (func $__call_closure
    (param $closure anyref)
    (param $args (ref $AnyArray))
    (param $cont anyref)
    (return_call_ref $FinkFn
      (local.get $args)
      (local.get $cont)
      (struct.get $FnClosure 0 (ref.cast (ref $FnClosure) (local.get $closure)))))

  (func $main (type $FinkFn)
    (param $args (ref $AnyArray))
    (param $cont anyref)
    (return_call $__call_closure
      (local.get $cont)
      (array.new_fixed $AnyArray 1 (ref.i31 (i32.const 42)))
      (ref.null none)))

  (func (export "fink_main")
    (call $main
      (array.new_fixed $AnyArray 0)
      (struct.new $FnClosure (ref.func $__halt) (array.new_fixed $AnyArray 0))))

  (elem declare func $__halt $main)
)
