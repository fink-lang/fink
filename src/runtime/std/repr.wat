;; std/repr.wat — `repr` protocol dispatcher.
;;
;; `repr(x)` produces the source-form string for x:
;;   strings: quoted + escaped  ('hello' -> "'hello'")
;;   numbers/bools: same as fmt
;;   collections: same as fmt (their fmt already calls repr on elements)
;;
;; This file is a pure dispatcher — it owns no rendering logic. Per-type
;; `repr` impls live in each type's module (str.wat, int.wat, ...).
;;
;; Three layers:
;;   1. fink-importable `repr` (CPS)         — for user code via `repr x`
;;   2. direct-style `$repr_val` (this file) — for other WAT modules
;;   3. per-type `$repr` (each type's .wat)  — actual rendering
;;
;; Collection `fmt` impls call back into `$repr_val` for elements, which
;; creates an import cycle with list.wat / dict.wat / set.wat. The
;; wat-linker handles cycles (same pattern str.wat <-> list.wat already
;; uses for fmt_val).


(module

  ;; ---- Type imports ----------------------------------------------------

  (import "std/str.wat"   "Str"   (type $Str   (sub any)))
  (import "std/num.wat"   "Num"   (type $Num   (sub any)))
  (import "std/range.wat" "Range" (type $Range (sub any)))
  (import "std/list.wat"  "List"  (type $List  (sub any)))
  (import "std/dict.wat"  "Dict"   (type $Dict   (sub any)))
  (import "std/set.wat"   "Set"   (type $Set   (sub any)))


  ;; ---- Per-type repr impl imports --------------------------------------
  ;;
  ;; num.wat owns the inner numeric dispatch ($Int / $F64 / $Decimal).

  (import "std/str.wat"   "repr"         (func $str_repr     (param (ref $Str))     (result (ref $Str))))
  (import "std/str.wat"   "closure_repr" (func $closure_repr (param (ref $Closure)) (result (ref $Str))))
  (import "std/num.wat"   "repr" (func $num_repr   (param (ref $Num))   (result (ref $Str))))
  (import "std/range.wat" "repr" (func $range_repr (param (ref $Range)) (result (ref $Str))))
  (import "std/list.wat"  "repr" (func $list_repr  (param (ref $List))  (result (ref $Str))))
  (import "std/dict.wat"  "repr" (func $rec_repr   (param (ref $Dict))   (result (ref $Str))))
  (import "std/set.wat"   "repr" (func $set_repr   (param (ref $Set))   (result (ref $Str))))


  ;; ---- CPS apply (for the fink-importable repr) ------------------------

  (import "rt/apply.wat" "apply_1" (func $apply_1 (;apply-ctx;) (param (ref null any)) (param $val (ref null any)) (param $cont (ref null any))))
  (import "rt/apply.wat" "args_head"
    (func $args_head (param (ref null any)) (result (ref null any))))
  (import "rt/apply.wat" "args_tail"
    (func $args_tail (param (ref null any)) (result (ref null any))))
  (import "rt/apply.wat" "Closure"
    (type $Closure (sub any (struct (field funcref) (field (ref null any))))))
  (import "rt/apply.wat" "Captures"
    (type $Captures (sub any (array (mut (ref null any))))))
  (import "rt/apply.wat" "Fn3"
    (type $Fn3 (sub any (func (param (ref null any) (ref null any) (ref null any))))))
  (import "rt/types.wat" "Inst" (type $Inst (sub any)))
  (import "rt/types.wat" "FnInst" (type $FnInst (sub $Inst)))
  (import "std/str.wat" "fn_inst_fmt"
    (func $fn_inst_fmt (param (ref null any)) (result (ref $Str))))
  (import "rt/types.wat" "inst_payload"
    (func $inst_payload (param (ref null any)) (result (ref null any))))
  (import "rt/types.wat" "inst_type_name"
    (func $inst_type_name (param (ref null any)) (result (ref i31))))
  (import "rt/symbols.wat" "is_symbol" (func $is_symbol (param (ref null any)) (result i32)))
  (import "rt/symbols.wat" "repr" (func $symbol_repr (param (ref i31)) (result (ref $Str))))

  ;; ByteArray-assembly helpers for prepending a nominal type name to an
  ;; instance's payload repr (`Foo {bar: 1}`). Same idiom dict/list fmt use.
  (import "std/str.wat" "ByteArray" (type $ByteArray (array (mut i8))))
  (import "std/str.wat" "from_bytes"
    (func $str_from_bytes (param (ref $ByteArray)) (result (ref $Str))))
  (import "std/str.wat" "_str_len"
    (func $_str_len (param (ref $Str)) (result i32)))
  (import "std/str.wat" "_str_copy_to"
    (func $_str_copy_to (param (ref $Str)) (param (ref $ByteArray)) (param i32) (result i32)))

  ;; i31 (bool) renderer — repr same as fmt; share str.wat's helper.
  (import "std/str.wat" "_str_fmt_i31"
    (func $_str_fmt_i31 (param i32) (result (ref $Str))))


  ;; ---- Direct-style dispatcher ----------------------------------------

  ;; $repr_val : (ref any) -> (ref $Str)
  ;; Used by other WAT modules (notably collection fmt impls for elements).
  ;; br_on_cast chain in the same shape as str.wat:fmt_val.
  ;; Order matters for subtype dispatch — most specific first.
  (func $repr_val (@pub) (param $val (ref any)) (result (ref $Str))

    ;; Try $Str — quoted + escaped.
    (block $not_str
      (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref any) (ref $Str)
            (local.get $val))))
      (return_call $str_repr))

    ;; Try $Num — num.wat owns the inner Int / F64 / Decimal dispatch.
    (block $not_num
      (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref any) (ref $Num)
            (local.get $val))))
      (return_call $num_repr))

    ;; Try symbol (tagged i31) — repr as its source name (bare ident or
    ;; quoted). Must precede the bool i31 arm: a symbol IS an i31, so peel it
    ;; off first or it would render as a bool.
    (if (call $is_symbol (local.get $val))
      (then (return_call $symbol_repr
        (ref.cast (ref i31) (local.get $val)))))

    ;; Try i31ref (bool) — repr same as fmt.
    (block $not_i31
      (block $is_i31 (result (ref i31))
        (br $not_i31
          (br_on_cast $is_i31 (ref any) (ref i31)
            (local.get $val))))
      (return (call $_str_fmt_i31 (i31.get_s))))

    ;; Try $Range.
    (block $not_range
      (block $is_range (result (ref $Range))
        (br $not_range
          (br_on_cast $is_range (ref any) (ref $Range)
            (local.get $val))))
      (return_call $range_repr))

    ;; Try $Dict.
    (block $not_rec
      (block $is_rec (result (ref $Dict))
        (br $not_rec
          (br_on_cast $is_rec (ref any) (ref $Dict)
            (local.get $val))))
      (return_call $rec_repr))

    ;; Try $List.
    (block $not_list
      (block $is_list (result (ref $List))
        (br $not_list
          (br_on_cast $is_list (ref any) (ref $List)
            (local.get $val))))
      (return_call $list_repr))

    ;; Try $Set.
    (block $not_set
      (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref any) (ref $Set)
            (local.get $val))))
      (return_call $set_repr))

    ;; Try $Closure -- placeholder "<closure>" repr.
    (block $not_clos
      (block $is_clos (result (ref $Closure))
        (br $not_clos
          (br_on_cast $is_clos (ref any) (ref $Closure)
            (local.get $val))))
      (return_call $closure_repr))

    ;; Try $FnInst (function instance) BEFORE $Inst (it is a subtype): a fn has
    ;; no source-quoted form distinct from its fmt, so repr == fmt == `<name> fn:`.
    ;; Delegate to str.wat's fn_inst_fmt.
    (if (ref.test (ref $FnInst) (local.get $val))
      (then (return_call $fn_inst_fmt (local.get $val))))

    ;; Try $Inst (typed instance) — source-quote the nominal name before the
    ;; structural payload (`Foo {bar: 1}`). Anonymous types (null-name symbol)
    ;; render as the bare payload.
    (if (ref.test (ref $Inst) (local.get $val))
      (then (return_call $inst_repr (local.get $val))))

    ;; Unknown type — unreachable for now.
    (unreachable)
  )


  ;; ---- Typed-instance repr --------------------------------------------

  ;; $inst_repr : (ref any $Inst) -> (ref $Str)
  ;; Source-quote the nominal type name then the structural payload, joined by a
  ;; space: `Foo {bar: 1}`. The name is the type's `$name` symbol, repr'd via the
  ;; symbol repr (bare ident or quoted). An anonymous type carries the null-name
  ;; symbol, whose repr is empty -- detected by zero length, in which case the
  ;; bare payload is returned (no leading space).
  (func $inst_repr (param $val (ref null any)) (result (ref $Str))
    (local $name (ref $Str))
    (local $payload (ref $Str))
    (local $name_len i32)
    (local $pay_len i32)
    (local $buf (ref $ByteArray))
    (local $pos i32)
    (local.set $name (call $symbol_repr (call $inst_type_name (local.get $val))))
    (local.set $payload
      (call $repr_val (ref.as_non_null (call $inst_payload (local.get $val)))))
    (local.set $name_len (call $_str_len (local.get $name)))
    ;; Anonymous type: empty name -> bare payload, no leading space.
    (if (i32.eqz (local.get $name_len))
      (then (return (local.get $payload))))
    (local.set $pay_len (call $_str_len (local.get $payload)))
    ;; name + " " + payload.
    (local.set $buf
      (array.new $ByteArray (i32.const 0)
        (i32.add (local.get $name_len)
          (i32.add (i32.const 1) (local.get $pay_len)))))
    (local.set $pos (call $_str_copy_to (local.get $name) (local.get $buf) (i32.const 0)))
    (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x20)) ;; space
    (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
    (local.set $pos (call $_str_copy_to (local.get $payload) (local.get $buf) (local.get $pos)))
    (return_call $str_from_bytes (local.get $buf)))


  ;; ---- Fink-importable wrapper (CPS) ----------------------------------

  ;; std/repr.fnk:repr — user-facing `repr x` call site.
  ;;
  ;; Shape: a no-capture $Closure returned by the bare-@impl accessor.
  ;; User code does `{repr} = import 'std/repr.fnk'` (which fetches this
  ;; closure value) and then `repr 42` (which dispatches through apply_3
  ;; with the caller's ctx). The closure body peels (cont, val) off the
  ;; args list and forwards ctx into apply_1 so the cont resumes under
  ;; the caller's universe.

  (elem declare func $repr_apply)

  (func $repr_apply (type $Fn3)
    (param $_caps (ref null any))
    (param $ctx (ref null any))
    (param $args (ref null any))

    (local $cont (ref null any))
    (local $val (ref null any))

    (local.set $cont (call $args_head (local.get $args)))
    (local.set $args (call $args_tail (local.get $args)))
    (local.set $val  (call $args_head (local.get $args)))

    (return_call $apply_1
      (local.get $ctx)
      (call $repr_val (ref.as_non_null (local.get $val)))
      (local.get $cont)))

  (global $repr_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $repr_apply)
      (ref.null $Captures)))

  (func $repr (@pub) (@impl "std/repr.fnk:repr") (result (ref any))
    (global.get $repr_closure))

)
