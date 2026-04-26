;; Minimal exercise of the surface forms the linker has to handle.
;;
;; Covers:
;;   - imported type    (uses bar.wat's $Bar)
;;   - imported func    (uses bar.wat's $bar_make)
;;   - locally-defined named type
;;   - locally-defined global
;;   - locally-defined func with params, locals, and a block label
;;   - exported func
;;   - cross-reference to a sibling local func

(module

  ;; --- imports from a sibling .wat ---------------------------------------

  (import "./bar.wat" "Bar" (type $Bar (sub any)))

  (import "./bar.wat" "bar_make"
    (func $bar_make (param $v i32) (result (ref $Bar))))


  ;; --- locally-defined named type ---------------------------------------

  (type $Foo (sub (struct
    (field $tag i32)
    (field $payload (ref $Bar)))))


  ;; --- locally-defined global -------------------------------------------

  (global $next_id (mut i32) (i32.const 0))


  ;; --- private helper, called from the exported function ---------------

  (func $alloc_id (result i32)
    (local $cur i32)
    (local.set $cur (global.get $next_id))
    (global.set $next_id (i32.add (local.get $cur) (i32.const 1)))
    (local.get $cur))


  ;; --- exported func: builds a $Foo wrapping a fresh $Bar ---------------

  (func $foo_make (export "foo_make")
    (param $v i32) (result (ref $Foo))

    (local $id i32)
    (local $bar (ref $Bar))

    (local.set $id (call $alloc_id))
    (local.set $bar (call $bar_make (local.get $v)))

    (block $done (result (ref $Foo))
      (struct.new $Foo
        (local.get $id)
        (local.get $bar))))


  ;; --- second exported func: peels the $Bar back out of a $Foo ----------

  (func (@implements "std/operators.fnk:op_in")
    (param $f (ref $Foo)) (result (ref $Bar))
    (struct.get $Foo $payload (local.get $f)))

  (func $op_notin (@implements "std/operators.fnk:op_notin")
    (param $f (ref $Foo)) (result (ref $Bar))
    (struct.get $Foo $payload (local.get $f)))
)
