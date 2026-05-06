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
  (import "std/dict.wat"  "Rec"   (type $Rec   (sub any)))
  (import "std/set.wat"   "Set"   (type $Set   (sub any)))


  ;; ---- Per-type repr impl imports --------------------------------------
  ;;
  ;; num.wat owns the inner numeric dispatch ($Int / $F64 / $Decimal).

  (import "std/str.wat"   "repr" (func $str_repr   (param (ref $Str))   (result (ref $Str))))
  (import "std/num.wat"   "repr" (func $num_repr   (param (ref $Num))   (result (ref $Str))))
  (import "std/range.wat" "repr" (func $range_repr (param (ref $Range)) (result (ref $Str))))
  (import "std/list.wat"  "repr" (func $list_repr  (param (ref $List))  (result (ref $Str))))
  (import "std/dict.wat"  "repr" (func $rec_repr   (param (ref $Rec))   (result (ref $Str))))
  (import "std/set.wat"   "repr" (func $set_repr   (param (ref $Set))   (result (ref $Str))))


  ;; ---- CPS apply (for the fink-importable repr) ------------------------

  (import "rt/apply.wat" "apply_1"
    (func $apply_1 (param $val (ref null any)) (param $cont (ref null any))))
  (import "std/list.wat" "head_any"
    (func $head_any (param $list (ref null any)) (result (ref null any))))

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

    ;; Try $Rec.
    (block $not_rec
      (block $is_rec (result (ref $Rec))
        (br $not_rec
          (br_on_cast $is_rec (ref any) (ref $Rec)
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

    ;; Unknown type — unreachable for now.
    (unreachable)
  )


  ;; ---- Fink-importable wrapper (CPS) ----------------------------------

  ;; std/repr.fnk:repr — user-facing `repr x` call site.
  ;; Standard CPS shape: peel value off args[0], call $repr_val, apply_1
  ;; result to cont.
  (func $repr (@pub) (@impl "std/repr.fnk:repr")
    (param $args (ref null any)) (param $cont (ref null any))
    (return_call $apply_1
      (call $repr_val (ref.as_non_null (call $head_any (local.get $args))))
      (local.get $cont))
  )

)
