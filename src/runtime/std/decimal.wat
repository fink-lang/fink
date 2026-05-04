;; Decimal type and operations.
;;
;; Status: type declaration only. $Decimal currently shares $Num's f64
;; slot — the carrier is the same, only the nominal type tag differs.
;; Real exact-arithmetic decimal repr (coefficient + exponent fields)
;; lands when decimal arithmetic gets implemented; until then this
;; type exists to keep decimal values out of $Num so $Num can become
;; an abstract empty parent.

(module

  ;; Type imports
  (import "std/num.wat" "Num" (type $Num (sub any) (struct (field $val f64))))

  ;; $Decimal — distinct from $F64, both share f64 slot for now.
  (type $Decimal (@pub) (sub final $Num (struct (field $val f64))))

)
