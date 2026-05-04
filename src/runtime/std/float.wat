;; Float types and operations.
;;
;; Step 3a: type declarations only. $F64 currently shares $Num's f64 slot;
;; primitives still live in num.wat / int.wat. Subsequent steps split float
;; arithmetic into this module and narrow the value field.

(module

  ;; Type imports
  (import "std/num.wat" "Num" (type $Num (sub any) (struct (field $val f64))))

  ;; $F64 — IEEE 754 binary64. Subtype of $Num; for now shares $Num's
  ;; `f64 $val` slot.
  (type $F64 (@pub) (sub final $Num (struct (field $val f64))))

)
