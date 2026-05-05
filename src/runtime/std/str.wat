;; String — runtime support for fink's immutable strings
;;
;; This module provides direct-style primitives for string construction
;; and access. These are used by the compiler (emitted code) and by the
;; std-lib (which wraps them in CPS functions exposed to fink code).
;;
;; No CPS, no user-facing functions — those live in the std-lib.
;;
;;
;; Type hierarchy ($Str is the only public type; everything else is internal):
;;
;;   $Str                               ← public interface (only type visible outside)
;;   ├── $StrEmpty     (sub $Str)       ← singleton empty string
;;   ├── $StrDataImpl  (sub $Str)       ← (offset, length) into data section
;;   └── $StrBytesImpl (sub $Str)       ← heap byte array
;;
;; Escape sequences are resolved at compile time — the runtime only sees
;; cooked UTF-8 bytes. The two string subtypes differ only in storage:
;;   $StrDataImpl — points into the WASM data section (compiler-emitted literals)
;;   $StrBytesImpl — heap-allocated byte array (runtime-constructed, e.g. concat)

(module

  ;; Type imports
  (import "std/num.wat"    "Num"       (type $Num       (sub any)))
  (import "std/int.wat"     "Int"     (type $Int     (sub any) (struct)))
  (import "std/int.wat"     "I64"     (type $I64     (sub $Int (struct (field $ival i64)))))
  (import "std/int.wat"     "U64"     (type $U64     (sub $Int (struct (field $ival i64)))))
  (import "std/int.wat"     "fmt"     (func $int_fmt     (param (ref $Int))     (result (ref $Str))))
  (import "std/float.wat"   "F64"      (type $F64     (sub any) (struct (field $val f64))))
  (import "std/float.wat"   "fmt"      (func $float_fmt   (param (ref $F64)) (result (ref $Str))))
  (import "std/range.wat"   "fmt"      (func $range_fmt     (param (ref $Range)) (result (ref $Str))))
  (import "std/range.wat"   "start"    (func $range_start   (param (ref $Range)) (result (ref $I64))))
  (import "std/range.wat"   "end"      (func $range_end     (param (ref $Range)) (result (ref $I64))))
  (import "std/range.wat"   "is_incl"  (func $range_is_incl (param (ref $Range)) (result i32)))
  (import "std/decimal.wat" "Decimal" (type $Decimal (sub any) (struct (field $coeff i64) (field $exp i32))))
  (import "std/decimal.wat" "fmt"     (func $decimal_fmt (param (ref $Decimal)) (result (ref $Str))))
  (import "std/range.wat"  "Range"     (type $Range     (sub any)))
  (import "std/dict.wat"   "Rec"           (type $Rec           (sub any)))
  (import "std/dict.wat"   "RecImpl"       (type $RecImpl       (sub any)))
  ;; TODO: HAMT internals leaked for the rec formatter; replace with a
  ;; public dict iterator and drop these imports.
  (import "std/dict.wat"   "HamtNode"      (type $HamtNode      (sub any)))
  (import "std/dict.wat"   "HamtChildren"  (type $HamtChildren  (sub any)))
  (import "std/dict.wat"   "HamtCollision" (type $HamtCollision (sub any)))
  (import "std/dict.wat"   "HamtLeaf"      (type $HamtLeaf      (sub any)))
  (import "std/set.wat"    "Set"       (type $Set       (sub any)))
  (import "std/list.wat"   "List"      (type $List      (sub any)))
  (import "rt/apply.wat"   "VarArgs"   (type $VarArgs   (sub any)))

  ;; Func imports
  (import "rt/apply.wat" "apply"
    (func $_apply (param $args (ref null any)) (param $callee (ref null any))))
  (import "rt/apply.wat" "apply_1"
    (func $apply_1 (param $val (ref null any)) (param $cont (ref null any))))
  (import "rt/apply.wat" "apply_0"
    (func $apply_0 (param $cont (ref null any))))
  (import "rt/apply.wat" "apply_2_vals"
    (func $apply_2_vals (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))))

  (import "std/set.wat" "repr"
    (func $set_repr (param $set (ref $Set)) (result (ref $Str))))

  (import "std/dict.wat" "size"
    (func $dict_size (param $rec (ref $Rec)) (result i32)))
  (import "std/dict.wat" "_hamt_size_node"
    (func $_hamt_size_node (param $node (ref $HamtNode)) (result i32)))

  (import "std/list.wat" "size"
    (func $list_size (param $list (ref $List)) (result i32)))
  (import "std/list.wat" "head_any"
    (func $head_any (param $list (ref null any)) (result (ref null any))))
  (import "std/list.wat" "tail_any"
    (func $tail_any (param $list (ref null any)) (result (ref null any))))

  (import "std/list.wat" "op_empty"
    (func $list_op_empty (param $val (ref null any)) (result i32)))

  ;; Continuation dispatch: $std/list.wat:apply_1 (defined in list.wat) wraps a single
  ;; result in a list and tail-calls $_apply (defined in dispatch.wat).

  ;; ---- Public type ----

  ;; $Str — base string type. Opaque.
  ;; All internal subtypes (below) are not visible outside this module.
  (type $Str (@pub) (sub (struct)))

  ;; ---- Internal types (not visible to user code) ----

  ;; Byte array: UTF-8 bytes. Used by $StrBytesImpl.
  ;; Mutable at the WASM level for construction (array.set during escape
  ;; processing), but treated as immutable once wrapped in a $Str* struct.
  ;; TODO: ByteArray is internal — exposed (@pub) only because set.wat
  ;; needs it for repr's buffer construction. Move buffer-allocation
  ;; behind a str helper to drop this exposure.
  (type $ByteArray (@pub) (@todo-no-rec) (array (mut i8)))

  ;; $StrEmpty — singleton empty string. No fields, no storage.
  (type $StrEmpty (sub $Str (struct)))

  ;; $StrDataImpl — data section string (offset, length into linear memory).
  (type $StrDataImpl (sub $Str (struct
    (field $offset i32)
    (field $length i32))))

  ;; $StrBytesImpl — heap-allocated string (byte array).
  (type $StrBytesImpl (@pub) (sub $Str (struct
    (field $bytes (ref $ByteArray)))))


  ;; ---- Singleton empty string ----

  (global $_str_empty (ref $StrEmpty) (struct.new $StrEmpty))

  ;; TODO: this is empty contructor protocol!
  ;; (func $str_new (@pub) (@impl "std/types.fnk:new" $Str) (result (ref $Str)))
  (func $str_empty (@impl "std/str.fnk:str_empty") (result (ref $Str))
    (global.get $_str_empty))

  ;; ---- Construction (compiler-emitted) ----

  ;; str : (i32, i32) -> (ref $StrDataImpl)
  ;; Wrap a data-section pointer into a string value.
  ;; TODO: this is a contructor for literal. find protocol for it!
  ;; TODO: rename to `from_data` — the name `str` clashes with the
  ;; `$Str` type's surface (std/str.fnk:str = the type). Update the
  ;; lowering in src/passes/wasm/lower.rs to emit `from_data` for
  ;; string literals at the same time.
  (func $str (@impl "std/str.fnk:str")
    (param $offset i32)
    (param $length i32)
    (result (ref $StrDataImpl))

    (struct.new $StrDataImpl
      (local.get $offset)
      (local.get $length))
  )

  ;; from_bytes : (ref $ByteArray) -> (ref $Str)
  ;; Wrap a GC byte array as a $Str. Public constructor used by per-type
  ;; `fmt` impls in other runtime modules — they build their digits/chars
  ;; into a $ByteArray locally and call this to wrap. Empty arrays
  ;; collapse to the shared $_str_empty global.
  (func $from_bytes (@pub) (param $bytes (ref $ByteArray)) (result (ref $Str))
    (if (i32.eqz (array.len (local.get $bytes)))
      (then (return (global.get $_str_empty))))
    (struct.new $StrBytesImpl (local.get $bytes)))

  ;; _str_wrap_bytes : (ref null any) -> (ref any)
  ;; Host-facing entry point with loose typing for JS/native interop —
  ;; the host hands us a (ref null any) and expects (ref any) back.
  ;; Internal callers should use `from_bytes` instead.
  (func $_str_wrap_bytes (@pub) (export "env:_str_wrap_bytes")
    (param $bytes (ref null any))
    (result (ref any))

    (return_call $from_bytes
      (ref.cast (ref $ByteArray) (local.get $bytes))))


  ;; ---- Access ----

  ;; str_bytes : (ref $Str) -> (ref $ByteArray)
  ;; Get the byte content of a string.
  ;; Dispatches via br_on_cast:
  ;;   $StrDataImpl  → copies bytes from data section into a $ByteArray
  ;;   $StrBytesImpl → returns the existing $ByteArray
  ;; TODO is this interop???
  (func $bytes (export "bytes")
    (param $str (ref $Str))
    (result (ref $ByteArray))

    (local $offset i32)
    (local $length i32)
    (local $result (ref $ByteArray))
    (local $i i32)

    ;; Empty string — return zero-length array
    (if (ref.test (ref $StrEmpty) (local.get $str))
      (then (return (array.new_fixed $ByteArray 0))))

    ;; Try $StrDataImpl — copy from data section to heap
    (block $not_data
      (block $is_data (result (ref $StrDataImpl))
        (br $not_data
          (br_on_cast $is_data (ref $Str) (ref $StrDataImpl)
            (local.get $str))))
      ;; Cast succeeded — $StrDataImpl is on the stack
      (local.set $offset (struct.get $StrDataImpl $offset))
      (local.set $length
        ;; re-read from param since we consumed the ref
        (struct.get $StrDataImpl $length
          (ref.cast (ref $StrDataImpl) (local.get $str))))
      (local.set $result
        (array.new $ByteArray (i32.const 0) (local.get $length)))
      (local.set $i (i32.const 0))
      (block $done
        (loop $copy
          (br_if $done
            (i32.ge_u (local.get $i) (local.get $length)))
          (array.set $ByteArray (local.get $result) (local.get $i)
            (i32.load8_u (i32.add (local.get $offset) (local.get $i))))
          (local.set $i
            (i32.add (local.get $i) (i32.const 1)))
          (br $copy)))
      (return (local.get $result)))

    ;; Must be $StrBytesImpl — return existing array
    (struct.get $StrBytesImpl $bytes
      (ref.cast (ref $StrBytesImpl) (local.get $str)))
  )

  ;; ---- Repr ----

  ;; str_repr : (ref $Str) -> (ref $Str)
  ;; Produce a quoted, escaped representation of a string: 'hello' → '\'hello\''
  ;; Escapes: \n \r \t \\ \' and non-printable bytes as \xNN.
  (func $repr (@impl "std/repr.fnk:repr" $Str)
    (param $str (ref $Str))
    (result (ref $Str))

    (local $src (ref $ByteArray))
    (local $len i32)
    (local $i i32)
    (local $b i32)
    (local $total i32)
    (local $buf (ref $ByteArray))
    (local $pos i32)

    (local.set $src (call $bytes (local.get $str)))
    (local.set $len (array.len (local.get $src)))

    ;; Pass 1: compute output length.
    ;; Start with 2 for surrounding quotes.
    (local.set $total (i32.const 2))
    (local.set $i (i32.const 0))
    (block $done1
      (loop $scan
        (br_if $done1 (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $b
          (array.get_u $ByteArray (local.get $src) (local.get $i)))

        (local.set $total (i32.add (local.get $total)
          (if (result i32) (i32.or
                (i32.eq (local.get $b) (i32.const 0x0A))  ;; \n
                (i32.or
                  (i32.eq (local.get $b) (i32.const 0x0D))  ;; \r
                  (i32.or
                    (i32.eq (local.get $b) (i32.const 0x09))  ;; \t
                    (i32.or
                      (i32.eq (local.get $b) (i32.const 0x5C))  ;; backslash
                      (i32.eq (local.get $b) (i32.const 0x27))  ;; single quote
                    ))))
            (then (i32.const 2))  ;; escape sequence: 2 bytes
            (else
              (if (result i32) (i32.or
                    (i32.lt_u (local.get $b) (i32.const 0x20))  ;; control chars
                    (i32.eq (local.get $b) (i32.const 0x7F)))   ;; DEL
                (then (i32.const 4))  ;; \xNN: 4 bytes
                (else (i32.const 1)))))))  ;; normal byte

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $scan)))

    ;; Allocate buffer.
    (local.set $buf (array.new $ByteArray (i32.const 0) (local.get $total)))

    ;; Write opening quote.
    (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x27))
    (local.set $pos (i32.const 1))

    ;; Pass 2: write escaped bytes.
    (local.set $i (i32.const 0))
    (block $done2
      (loop $write
        (br_if $done2 (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $b
          (array.get_u $ByteArray (local.get $src) (local.get $i)))

        ;; \n
        (if (i32.eq (local.get $b) (i32.const 0x0A))
          (then
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x5C))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x6E))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $write)))

        ;; \r
        (if (i32.eq (local.get $b) (i32.const 0x0D))
          (then
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x5C))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x72))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $write)))

        ;; \t
        (if (i32.eq (local.get $b) (i32.const 0x09))
          (then
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x5C))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x74))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $write)))

        ;; \\
        (if (i32.eq (local.get $b) (i32.const 0x5C))
          (then
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x5C))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x5C))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $write)))

        ;; \'
        (if (i32.eq (local.get $b) (i32.const 0x27))
          (then
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x5C))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x27))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $write)))

        ;; Control chars / DEL → \xNN
        (if (i32.or
              (i32.lt_u (local.get $b) (i32.const 0x20))
              (i32.eq (local.get $b) (i32.const 0x7F)))
          (then
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x5C))  ;; '\'
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x78))  ;; 'x'
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (array.set $ByteArray (local.get $buf) (local.get $pos)
              (call $_hex_char (i32.shr_u (local.get $b) (i32.const 4))))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (array.set $ByteArray (local.get $buf) (local.get $pos)
              (call $_hex_char (i32.and (local.get $b) (i32.const 0x0F))))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $write)))

        ;; Normal byte — copy as-is.
        (array.set $ByteArray (local.get $buf) (local.get $pos) (local.get $b))
        (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $write)))

    ;; Write closing quote.
    (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x27))

    (struct.new $StrBytesImpl (local.get $buf))
  )

  ;; _hex_char : i32 -> i32
  ;; Convert 0-15 to ASCII hex character ('0'-'9', 'A'-'F').
  (func $_hex_char (param $v i32) (result i32)
    (if (result i32) (i32.lt_u (local.get $v) (i32.const 10))
      (then (i32.add (local.get $v) (i32.const 0x30)))  ;; '0'
      (else (i32.add (i32.sub (local.get $v) (i32.const 10)) (i32.const 0x41))))  ;; 'A'
  )


  ;; ---- Equality ----

  ;; str_op_eq : (ref $Str), (ref $Str) -> i32
  ;; Compare two byte-bearing strings for byte-level equality.
  ;; Fast path: ref.eq (same object → 1).
  ;; Slow path: dispatch on concrete types, compare byte-by-byte.
  ;; Returns 1 if equal, 0 if not.
  (func $op_eq
    (param $a (ref $Str))
    (param $b (ref $Str))
    (result i32)

    (local $da (ref $StrDataImpl))
    (local $db (ref $StrDataImpl))

    ;; Fast path: same object (also catches empty == empty)
    (if (ref.eq (local.get $a) (local.get $b))
      (then (return (i32.const 1))))

    ;; Empty string is only equal to itself (handled above).
    (if (ref.test (ref $StrEmpty) (local.get $a))
      (then (return (i32.const 0))))
    (if (ref.test (ref $StrEmpty) (local.get $b))
      (then (return (i32.const 0))))

    ;; Dispatch on $a's type — try $StrDataImpl
    (block $a_not_data
      (block $a_is_data (result (ref $StrDataImpl))
        (br $a_not_data
          (br_on_cast $a_is_data (ref $Str) (ref $StrDataImpl)
            (local.get $a))))
      (local.set $da)

      ;; $a is data — dispatch on $b
      (block $b_not_data
        (block $b_is_data (result (ref $StrDataImpl))
          (br $b_not_data
            (br_on_cast $b_is_data (ref $Str) (ref $StrDataImpl)
              (local.get $b))))
        (local.set $db)
        ;; Both data
        (return (call $_str_op_eq_dd
          (struct.get $StrDataImpl $offset (local.get $da))
          (struct.get $StrDataImpl $length (local.get $da))
          (struct.get $StrDataImpl $offset (local.get $db))
          (struct.get $StrDataImpl $length (local.get $db)))))

      ;; $a is data, $b is array
      (return (call $_str_op_eq_da
        (struct.get $StrDataImpl $offset (local.get $da))
        (struct.get $StrDataImpl $length (local.get $da))
        (call $_get_byte_array (local.get $b)))))

    ;; $a is array — dispatch on $b
    (block $b_not_data2
      (block $b_is_data2 (result (ref $StrDataImpl))
        (br $b_not_data2
          (br_on_cast $b_is_data2 (ref $Str) (ref $StrDataImpl)
            (local.get $b))))
      (local.set $db)
      ;; $b is data, $a is array — flip args
      (return (call $_str_op_eq_da
        (struct.get $StrDataImpl $offset (local.get $db))
        (struct.get $StrDataImpl $length (local.get $db))
        (call $_get_byte_array (local.get $a)))))

    ;; Both arrays
    (call $_str_op_eq_aa
      (call $_get_byte_array (local.get $a))
      (call $_get_byte_array (local.get $b)))
  )

  ;; $_get_byte_array : (ref $Str) -> (ref $ByteArray)
  ;; Extract the $ByteArray from a $StrBytesImpl.
  ;; Caller must ensure it's not a $StrDataImpl.
  (func $_get_byte_array
    (param $str (ref $Str))
    (result (ref $ByteArray))

    (struct.get $StrBytesImpl $bytes
      (ref.cast (ref $StrBytesImpl) (local.get $str)))
  )

  ;; $_str_op_eq_dd : (i32, i32, i32, i32) -> i32
  ;; Compare two data-section strings by linear memory reads.
  (func $_str_op_eq_dd
    (param $off_a i32) (param $len_a i32)
    (param $off_b i32) (param $len_b i32)
    (result i32)

    (local $i i32)

    (if (i32.ne (local.get $len_a) (local.get $len_b))
      (then (return (i32.const 0))))

    (local.set $i (i32.const 0))
    (block $done
      (loop $cmp
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $len_a)))
        (if (i32.ne
              (i32.load8_u (i32.add (local.get $off_a) (local.get $i)))
              (i32.load8_u (i32.add (local.get $off_b) (local.get $i))))
          (then (return (i32.const 0))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $cmp)))

    (i32.const 1)
  )

  ;; $_str_op_eq_da : (i32, i32, ref $ByteArray) -> i32
  ;; Compare data-section string vs heap array.
  (func $_str_op_eq_da
    (param $off i32) (param $len i32)
    (param $arr (ref $ByteArray))
    (result i32)

    (local $i i32)

    (if (i32.ne (local.get $len) (array.len (local.get $arr)))
      (then (return (i32.const 0))))

    (local.set $i (i32.const 0))
    (block $done
      (loop $cmp
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $len)))
        (if (i32.ne
              (i32.load8_u (i32.add (local.get $off) (local.get $i)))
              (array.get_u $ByteArray (local.get $arr) (local.get $i)))
          (then (return (i32.const 0))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $cmp)))

    (i32.const 1)
  )

  ;; $_str_op_eq_aa : (ref $ByteArray, ref $ByteArray) -> i32
  ;; Compare two heap arrays byte-by-byte.
  (func $_str_op_eq_aa
    (param $a (ref $ByteArray))
    (param $b (ref $ByteArray))
    (result i32)

    (local $len i32)
    (local $i i32)

    (local.set $len (array.len (local.get $a)))
    (if (i32.ne (local.get $len) (array.len (local.get $b)))
      (then (return (i32.const 0))))

    (local.set $i (i32.const 0))
    (block $done
      (loop $cmp
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $len)))
        (if (i32.ne
              (array.get_u $ByteArray (local.get $a) (local.get $i))
              (array.get_u $ByteArray (local.get $b) (local.get $i)))
          (then (return (i32.const 0))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $cmp)))

    (i32.const 1)
  )


  ;; ---- Byte processing (for std-lib formatters) ----

  ;; str_render_escape : (ref $Str) -> (ref $Str)
  ;; Resolve escape sequences in a string's bytes:
  ;; \n, \t, \r, \f, \v, \b, \\, \', \$, \xNN, \u{NNNNNN}
  ;; Dispatches to $_escape_data (linear memory) or $_escape_array (heap).
  ;; Pure byte processing — no user code, no CPS needed.
  (func $render_escape
    (param $raw (ref $Str))
    (result (ref $Str))

    ;; Empty string — nothing to escape
    (if (ref.test (ref $StrEmpty) (local.get $raw))
      (then (return (local.get $raw))))

    ;; Try $StrDataImpl — read from linear memory
    (block $not_data
      (block $is_data (result (ref $StrDataImpl))
        (br $not_data
          (br_on_cast $is_data (ref $Str) (ref $StrDataImpl)
            (local.get $raw))))
      ;; Cast succeeded — $StrDataImpl on stack
      (return
        (call $_escape_data
          (struct.get $StrDataImpl $offset)
          (struct.get $StrDataImpl $length
            (ref.cast (ref $StrDataImpl) (local.get $raw))))))

    ;; Must be $StrBytesImpl — read from heap array
    (call $_escape_array
      (struct.get $StrBytesImpl $bytes
        (ref.cast (ref $StrBytesImpl) (local.get $raw))))
  )

  ;; $_escape_data : (i32, i32) -> (ref $Str)
  ;; Escape processing for $StrDataImpl — reads from linear memory.
  ;; Two-pass: count output bytes, then write.
  (func $_escape_data
    (param $base i32)
    (param $src_len i32)
    (result (ref $Str))

    (local $i i32)
    (local $out_len i32)
    (local $out (ref $ByteArray))
    (local $j i32)
    (local $byte i32)
    (local $hex_val i32)
    (local $digit i32)
    (local $k i32)

    ;; ---- Pass 1: count output bytes ----
    (local.set $i (i32.const 0))
    (local.set $out_len (i32.const 0))
    (block $count_done
      (loop $count
        (br_if $count_done
          (i32.ge_u (local.get $i) (local.get $src_len)))
        (local.set $byte
          (i32.load8_u (i32.add (local.get $base) (local.get $i))))

        (if (i32.eq (local.get $byte) (i32.const 0x5C))
          (then
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (if (i32.ge_u (local.get $i) (local.get $src_len))
              (then
                (local.set $out_len
                  (i32.add (local.get $out_len) (i32.const 1)))
                (br $count_done)))

            (local.set $byte
              (i32.load8_u (i32.add (local.get $base) (local.get $i))))

            (if (i32.eq (local.get $byte) (i32.const 0x78)) ;; \x
              (then
                (local.set $out_len
                  (i32.add (local.get $out_len) (i32.const 1)))
                (local.set $i
                  (i32.add (local.get $i) (i32.const 3)))
                (br $count)))

            ;; \u{NNNNNN} → parse hex digits between braces, skip underscores
            (if (i32.eq (local.get $byte) (i32.const 0x75)) ;; \u
              (then
                (local.set $hex_val (i32.const 0))
                ;; skip past the '{' (i currently points at 'u', +1 = '{', +2 = first digit)
                (local.set $k
                  (i32.add (local.get $i) (i32.const 2)))
                (block $hex_done
                  (loop $hex
                    (br_if $hex_done
                      (i32.ge_u (local.get $k) (local.get $src_len)))
                    (local.set $byte
                      (i32.load8_u
                        (i32.add (local.get $base) (local.get $k))))
                    ;; '}' terminates
                    (br_if $hex_done
                      (i32.eq (local.get $byte) (i32.const 0x7D)))
                    ;; skip '_'
                    (if (i32.eq (local.get $byte) (i32.const 0x5F))
                      (then
                        (local.set $k
                          (i32.add (local.get $k) (i32.const 1)))
                        (br $hex)))
                    (local.set $digit
                      (call $_hex_digit (local.get $byte)))
                    (local.set $hex_val
                      (i32.add
                        (i32.shl (local.get $hex_val) (i32.const 4))
                        (local.get $digit)))
                    (local.set $k
                      (i32.add (local.get $k) (i32.const 1)))
                    (br $hex)))
                (local.set $out_len
                  (i32.add (local.get $out_len)
                    (call $_utf8_len (local.get $hex_val))))
                ;; skip past the '}'
                (local.set $i
                  (i32.add (local.get $k) (i32.const 1)))
                (br $count)))

            ;; All other escapes → 1 byte
            (local.set $out_len
              (i32.add (local.get $out_len) (i32.const 1)))
            (local.set $i
              (i32.add (local.get $i) (i32.const 1)))
            (br $count))

          (else
            (local.set $out_len
              (i32.add (local.get $out_len) (i32.const 1)))
            (local.set $i
              (i32.add (local.get $i) (i32.const 1)))
            (br $count)))))

    ;; ---- Pass 2: write output bytes ----
    (local.set $out
      (array.new $ByteArray (i32.const 0) (local.get $out_len)))
    (local.set $i (i32.const 0))
    (local.set $j (i32.const 0))

    (block $write_done
      (loop $write
        (br_if $write_done
          (i32.ge_u (local.get $i) (local.get $src_len)))
        (local.set $byte
          (i32.load8_u (i32.add (local.get $base) (local.get $i))))

        (if (i32.eq (local.get $byte) (i32.const 0x5C))
          (then
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (if (i32.ge_u (local.get $i) (local.get $src_len))
              (then
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (i32.const 0x5C))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write_done)))

            (local.set $byte
              (i32.load8_u (i32.add (local.get $base) (local.get $i))))

            ;; \n → 0x0A
            (if (i32.eq (local.get $byte) (i32.const 0x6E))
              (then
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (i32.const 0x0A))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write)))
            ;; \t → 0x09
            (if (i32.eq (local.get $byte) (i32.const 0x74))
              (then
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (i32.const 0x09))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write)))
            ;; \r → 0x0D
            (if (i32.eq (local.get $byte) (i32.const 0x72))
              (then
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (i32.const 0x0D))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write)))
            ;; \f → 0x0C
            (if (i32.eq (local.get $byte) (i32.const 0x66))
              (then
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (i32.const 0x0C))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write)))
            ;; \v → 0x0B
            (if (i32.eq (local.get $byte) (i32.const 0x76))
              (then
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (i32.const 0x0B))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write)))
            ;; \b → 0x08
            (if (i32.eq (local.get $byte) (i32.const 0x62))
              (then
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (i32.const 0x08))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write)))
            ;; \\, \', \$
            (if (i32.or
                  (i32.eq (local.get $byte) (i32.const 0x5C))
                  (i32.or
                    (i32.eq (local.get $byte) (i32.const 0x27))
                    (i32.eq (local.get $byte) (i32.const 0x24))))
              (then
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (local.get $byte))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write)))
            ;; \xNN
            (if (i32.eq (local.get $byte) (i32.const 0x78))
              (then
                (local.set $hex_val
                  (i32.add
                    (i32.shl
                      (call $_hex_digit
                        (i32.load8_u (i32.add (local.get $base)
                          (i32.add (local.get $i) (i32.const 1)))))
                      (i32.const 4))
                    (call $_hex_digit
                      (i32.load8_u (i32.add (local.get $base)
                        (i32.add (local.get $i) (i32.const 2)))))))
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (local.get $hex_val))
                (local.set $i (i32.add (local.get $i) (i32.const 3)))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write)))
            ;; \u{NNNNNN}
            (if (i32.eq (local.get $byte) (i32.const 0x75))
              (then
                (local.set $hex_val (i32.const 0))
                ;; skip past '{': i points at 'u', +1 = '{', +2 = first digit
                (local.set $i
                  (i32.add (local.get $i) (i32.const 2)))
                (block $hex_done
                  (loop $hex
                    (br_if $hex_done
                      (i32.ge_u (local.get $i) (local.get $src_len)))
                    (local.set $byte
                      (i32.load8_u (i32.add (local.get $base) (local.get $i))))
                    ;; '}' terminates
                    (br_if $hex_done
                      (i32.eq (local.get $byte) (i32.const 0x7D)))
                    ;; skip '_'
                    (if (i32.eq (local.get $byte) (i32.const 0x5F))
                      (then
                        (local.set $i
                          (i32.add (local.get $i) (i32.const 1)))
                        (br $hex)))
                    (local.set $digit
                      (call $_hex_digit (local.get $byte)))
                    (local.set $hex_val
                      (i32.add
                        (i32.shl (local.get $hex_val) (i32.const 4))
                        (local.get $digit)))
                    (local.set $i
                      (i32.add (local.get $i) (i32.const 1)))
                    (br $hex)))
                ;; skip past '}'
                (local.set $i
                  (i32.add (local.get $i) (i32.const 1)))
                (local.set $j
                  (call $_write_utf8
                    (local.get $out) (local.get $j) (local.get $hex_val)))
                (br $write)))
            ;; Unknown escape — emit literally
            (array.set $ByteArray (local.get $out) (local.get $j)
              (local.get $byte))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br $write))

          (else
            (array.set $ByteArray (local.get $out) (local.get $j)
              (local.get $byte))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br $write)))))

    (struct.new $StrBytesImpl (local.get $out))
  )

  ;; $_escape_array : (ref $ByteArray) -> (ref $Str)
  ;; Escape processing for $StrBytesImpl — reads from heap array.
  ;; Two-pass: count output bytes, then write.
  (func $_escape_array
    (param $src (ref $ByteArray))
    (result (ref $Str))

    (local $src_len i32)
    (local $i i32)
    (local $out_len i32)
    (local $out (ref $ByteArray))
    (local $j i32)
    (local $byte i32)
    (local $hex_val i32)
    (local $digit i32)
    (local $k i32)

    (local.set $src_len
      (array.len (local.get $src)))

    ;; ---- Pass 1: count output bytes ----
    (local.set $i (i32.const 0))
    (local.set $out_len (i32.const 0))
    (block $count_done
      (loop $count
        (br_if $count_done
          (i32.ge_u (local.get $i) (local.get $src_len)))
        (local.set $byte
          (array.get_u $ByteArray (local.get $src) (local.get $i)))

        (if (i32.eq (local.get $byte) (i32.const 0x5C))
          (then
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (if (i32.ge_u (local.get $i) (local.get $src_len))
              (then
                (local.set $out_len
                  (i32.add (local.get $out_len) (i32.const 1)))
                (br $count_done)))

            (local.set $byte
              (array.get_u $ByteArray (local.get $src) (local.get $i)))

            (if (i32.eq (local.get $byte) (i32.const 0x78)) ;; \x
              (then
                (local.set $out_len
                  (i32.add (local.get $out_len) (i32.const 1)))
                (local.set $i
                  (i32.add (local.get $i) (i32.const 3)))
                (br $count)))

            ;; \u{NNNNNN} → parse hex digits between braces, skip underscores
            (if (i32.eq (local.get $byte) (i32.const 0x75)) ;; \u
              (then
                (local.set $hex_val (i32.const 0))
                ;; skip past '{': i points at 'u', +1 = '{', +2 = first digit
                (local.set $k
                  (i32.add (local.get $i) (i32.const 2)))
                (block $hex_done
                  (loop $hex
                    (br_if $hex_done
                      (i32.ge_u (local.get $k) (local.get $src_len)))
                    (local.set $byte
                      (array.get_u $ByteArray (local.get $src) (local.get $k)))
                    ;; '}' terminates
                    (br_if $hex_done
                      (i32.eq (local.get $byte) (i32.const 0x7D)))
                    ;; skip '_'
                    (if (i32.eq (local.get $byte) (i32.const 0x5F))
                      (then
                        (local.set $k
                          (i32.add (local.get $k) (i32.const 1)))
                        (br $hex)))
                    (local.set $digit
                      (call $_hex_digit (local.get $byte)))
                    (local.set $hex_val
                      (i32.add
                        (i32.shl (local.get $hex_val) (i32.const 4))
                        (local.get $digit)))
                    (local.set $k
                      (i32.add (local.get $k) (i32.const 1)))
                    (br $hex)))
                (local.set $out_len
                  (i32.add (local.get $out_len)
                    (call $_utf8_len (local.get $hex_val))))
                ;; skip past '}'
                (local.set $i
                  (i32.add (local.get $k) (i32.const 1)))
                (br $count)))

            ;; All other escapes → 1 byte
            (local.set $out_len
              (i32.add (local.get $out_len) (i32.const 1)))
            (local.set $i
              (i32.add (local.get $i) (i32.const 1)))
            (br $count))

          (else
            (local.set $out_len
              (i32.add (local.get $out_len) (i32.const 1)))
            (local.set $i
              (i32.add (local.get $i) (i32.const 1)))
            (br $count)))))

    ;; ---- Pass 2: write output bytes ----
    (local.set $out
      (array.new $ByteArray (i32.const 0) (local.get $out_len)))
    (local.set $i (i32.const 0))
    (local.set $j (i32.const 0))

    (block $write_done
      (loop $write
        (br_if $write_done
          (i32.ge_u (local.get $i) (local.get $src_len)))
        (local.set $byte
          (array.get_u $ByteArray (local.get $src) (local.get $i)))

        (if (i32.eq (local.get $byte) (i32.const 0x5C))
          (then
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (if (i32.ge_u (local.get $i) (local.get $src_len))
              (then
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (i32.const 0x5C))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write_done)))

            (local.set $byte
              (array.get_u $ByteArray (local.get $src) (local.get $i)))

            ;; \n → 0x0A
            (if (i32.eq (local.get $byte) (i32.const 0x6E))
              (then
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (i32.const 0x0A))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write)))
            ;; \t → 0x09
            (if (i32.eq (local.get $byte) (i32.const 0x74))
              (then
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (i32.const 0x09))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write)))
            ;; \r → 0x0D
            (if (i32.eq (local.get $byte) (i32.const 0x72))
              (then
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (i32.const 0x0D))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write)))
            ;; \f → 0x0C
            (if (i32.eq (local.get $byte) (i32.const 0x66))
              (then
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (i32.const 0x0C))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write)))
            ;; \v → 0x0B
            (if (i32.eq (local.get $byte) (i32.const 0x76))
              (then
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (i32.const 0x0B))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write)))
            ;; \b → 0x08
            (if (i32.eq (local.get $byte) (i32.const 0x62))
              (then
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (i32.const 0x08))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write)))
            ;; \\, \', \$
            (if (i32.or
                  (i32.eq (local.get $byte) (i32.const 0x5C))
                  (i32.or
                    (i32.eq (local.get $byte) (i32.const 0x27))
                    (i32.eq (local.get $byte) (i32.const 0x24))))
              (then
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (local.get $byte))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write)))
            ;; \xNN
            (if (i32.eq (local.get $byte) (i32.const 0x78))
              (then
                (local.set $hex_val
                  (i32.add
                    (i32.shl
                      (call $_hex_digit
                        (array.get_u $ByteArray (local.get $src)
                          (i32.add (local.get $i) (i32.const 1))))
                      (i32.const 4))
                    (call $_hex_digit
                      (array.get_u $ByteArray (local.get $src)
                        (i32.add (local.get $i) (i32.const 2))))))
                (array.set $ByteArray (local.get $out) (local.get $j)
                  (local.get $hex_val))
                (local.set $i (i32.add (local.get $i) (i32.const 3)))
                (local.set $j (i32.add (local.get $j) (i32.const 1)))
                (br $write)))
            ;; \u{NNNNNN}
            (if (i32.eq (local.get $byte) (i32.const 0x75))
              (then
                (local.set $hex_val (i32.const 0))
                ;; skip past '{': i points at 'u', +1 = '{', +2 = first digit
                (local.set $i
                  (i32.add (local.get $i) (i32.const 2)))
                (block $hex_done
                  (loop $hex
                    (br_if $hex_done
                      (i32.ge_u (local.get $i) (local.get $src_len)))
                    (local.set $byte
                      (array.get_u $ByteArray (local.get $src) (local.get $i)))
                    ;; '}' terminates
                    (br_if $hex_done
                      (i32.eq (local.get $byte) (i32.const 0x7D)))
                    ;; skip '_'
                    (if (i32.eq (local.get $byte) (i32.const 0x5F))
                      (then
                        (local.set $i
                          (i32.add (local.get $i) (i32.const 1)))
                        (br $hex)))
                    (local.set $digit
                      (call $_hex_digit (local.get $byte)))
                    (local.set $hex_val
                      (i32.add
                        (i32.shl (local.get $hex_val) (i32.const 4))
                        (local.get $digit)))
                    (local.set $i
                      (i32.add (local.get $i) (i32.const 1)))
                    (br $hex)))
                ;; skip past '}'
                (local.set $i
                  (i32.add (local.get $i) (i32.const 1)))
                (local.set $j
                  (call $_write_utf8
                    (local.get $out) (local.get $j) (local.get $hex_val)))
                (br $write)))
            ;; Unknown escape — emit literally
            (array.set $ByteArray (local.get $out) (local.get $j)
              (local.get $byte))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br $write))

          (else
            (array.set $ByteArray (local.get $out) (local.get $j)
              (local.get $byte))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br $write)))))

    (struct.new $StrBytesImpl (local.get $out))
  )


  ;; str_render_unescape : (ref $Str) -> (ref $Str)
  ;; Insert escape sequences for non-printable/special bytes:
  ;; 0x0A → \n, 0x09 → \t, 0x0D → \r, 0x0C → \f, 0x0B → \v,
  ;; 0x08 → \b, 0x5C → \\, 0x27 → \', 0x24 → \$
  ;; Not a true inverse of escape — lossy, since raw strings can have
  ;; both \n and literal \ n which both escape to the same byte.
  ;; For debug output and serialization.
  ;;
  ;; Two-pass: count then write.
  (func $render_unescape
    (param $str (ref $Str))
    (result (ref $Str))

    (local $src (ref $ByteArray))
    (local $src_len i32)
    (local $i i32)
    (local $out_len i32)
    (local $out (ref $ByteArray))
    (local $j i32)
    (local $byte i32)

    ;; Empty string — nothing to unescape
    (if (ref.test (ref $StrEmpty) (local.get $str))
      (then (return (local.get $str))))

    (local.set $src
      (struct.get $StrBytesImpl $bytes
        (ref.cast (ref $StrBytesImpl) (local.get $str))))
    (local.set $src_len
      (array.len (local.get $src)))

    ;; ---- Pass 1: count output bytes ----
    (local.set $i (i32.const 0))
    (local.set $out_len (i32.const 0))
    (block $count_done
      (loop $count
        (br_if $count_done
          (i32.ge_u (local.get $i) (local.get $src_len)))
        (local.set $byte
          (array.get_u $ByteArray (local.get $src) (local.get $i)))

        ;; Check if this byte needs escaping (produces 2 output bytes)
        (if (call $_needs_unescape (local.get $byte))
          (then
            (local.set $out_len
              (i32.add (local.get $out_len) (i32.const 2))))
          (else
            (local.set $out_len
              (i32.add (local.get $out_len) (i32.const 1)))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $count)))

    ;; ---- Pass 2: write output bytes ----
    (local.set $out
      (array.new $ByteArray (i32.const 0) (local.get $out_len)))
    (local.set $i (i32.const 0))
    (local.set $j (i32.const 0))

    (block $write_done
      (loop $write
        (br_if $write_done
          (i32.ge_u (local.get $i) (local.get $src_len)))
        (local.set $byte
          (array.get_u $ByteArray (local.get $src) (local.get $i)))

        ;; 0x0A → \n
        (if (i32.eq (local.get $byte) (i32.const 0x0A))
          (then
            (array.set $ByteArray (local.get $out) (local.get $j)
              (i32.const 0x5C))
            (array.set $ByteArray (local.get $out)
              (i32.add (local.get $j) (i32.const 1))
              (i32.const 0x6E))
            (local.set $j (i32.add (local.get $j) (i32.const 2)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $write)))

        ;; 0x09 → \t
        (if (i32.eq (local.get $byte) (i32.const 0x09))
          (then
            (array.set $ByteArray (local.get $out) (local.get $j)
              (i32.const 0x5C))
            (array.set $ByteArray (local.get $out)
              (i32.add (local.get $j) (i32.const 1))
              (i32.const 0x74))
            (local.set $j (i32.add (local.get $j) (i32.const 2)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $write)))

        ;; 0x0D → \r
        (if (i32.eq (local.get $byte) (i32.const 0x0D))
          (then
            (array.set $ByteArray (local.get $out) (local.get $j)
              (i32.const 0x5C))
            (array.set $ByteArray (local.get $out)
              (i32.add (local.get $j) (i32.const 1))
              (i32.const 0x72))
            (local.set $j (i32.add (local.get $j) (i32.const 2)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $write)))

        ;; 0x0C → \f
        (if (i32.eq (local.get $byte) (i32.const 0x0C))
          (then
            (array.set $ByteArray (local.get $out) (local.get $j)
              (i32.const 0x5C))
            (array.set $ByteArray (local.get $out)
              (i32.add (local.get $j) (i32.const 1))
              (i32.const 0x66))
            (local.set $j (i32.add (local.get $j) (i32.const 2)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $write)))

        ;; 0x0B → \v
        (if (i32.eq (local.get $byte) (i32.const 0x0B))
          (then
            (array.set $ByteArray (local.get $out) (local.get $j)
              (i32.const 0x5C))
            (array.set $ByteArray (local.get $out)
              (i32.add (local.get $j) (i32.const 1))
              (i32.const 0x76))
            (local.set $j (i32.add (local.get $j) (i32.const 2)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $write)))

        ;; 0x08 → \b
        (if (i32.eq (local.get $byte) (i32.const 0x08))
          (then
            (array.set $ByteArray (local.get $out) (local.get $j)
              (i32.const 0x5C))
            (array.set $ByteArray (local.get $out)
              (i32.add (local.get $j) (i32.const 1))
              (i32.const 0x62))
            (local.set $j (i32.add (local.get $j) (i32.const 2)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $write)))

        ;; 0x5C → \\
        (if (i32.eq (local.get $byte) (i32.const 0x5C))
          (then
            (array.set $ByteArray (local.get $out) (local.get $j)
              (i32.const 0x5C))
            (array.set $ByteArray (local.get $out)
              (i32.add (local.get $j) (i32.const 1))
              (i32.const 0x5C))
            (local.set $j (i32.add (local.get $j) (i32.const 2)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $write)))

        ;; 0x27 → \'
        (if (i32.eq (local.get $byte) (i32.const 0x27))
          (then
            (array.set $ByteArray (local.get $out) (local.get $j)
              (i32.const 0x5C))
            (array.set $ByteArray (local.get $out)
              (i32.add (local.get $j) (i32.const 1))
              (i32.const 0x27))
            (local.set $j (i32.add (local.get $j) (i32.const 2)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $write)))

        ;; 0x24 → \$
        (if (i32.eq (local.get $byte) (i32.const 0x24))
          (then
            (array.set $ByteArray (local.get $out) (local.get $j)
              (i32.const 0x5C))
            (array.set $ByteArray (local.get $out)
              (i32.add (local.get $j) (i32.const 1))
              (i32.const 0x24))
            (local.set $j (i32.add (local.get $j) (i32.const 2)))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $write)))

        ;; Regular byte — copy through
        (array.set $ByteArray (local.get $out) (local.get $j)
          (local.get $byte))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (local.set $j (i32.add (local.get $j) (i32.const 1)))
        (br $write)))

    (struct.new $StrBytesImpl (local.get $out))
  )


  ;; ---- Hashing ----

  ;; str_hash_i31 : (ref $Str) -> i32
  ;; Content-based hash for any byte-bearing string.
  ;; Dispatches on concrete type to avoid allocating a $ByteArray copy.
  ;; Uses FNV-1a (32-bit), masked to 31 bits for i31ref.
  (func $hash_i31 (@impl "std/hashing.fnk:hash_i31" $Str)
    (param $str (ref $Str))
    (result i32)

    (local $data (ref $StrDataImpl))

    ;; Empty string — hash of zero bytes (FNV offset basis).
    (if (ref.test (ref $StrEmpty) (local.get $str))
      (then (return (i32.const 0x811c9dc5))))

    ;; Try $StrDataImpl — hash from linear memory
    (block $not_data
      (block $is_data (result (ref $StrDataImpl))
        (br $not_data
          (br_on_cast $is_data (ref $Str) (ref $StrDataImpl)
            (local.get $str))))
      (local.set $data)
      (return (call $_str_hash_data
        (struct.get $StrDataImpl $offset (local.get $data))
        (struct.get $StrDataImpl $length (local.get $data)))))

    ;; Try $StrBytesImpl — hash from heap array
    (block $not_bytes
      (block $is_bytes (result (ref $StrBytesImpl))
        (br $not_bytes
          (br_on_cast $is_bytes (ref $Str) (ref $StrBytesImpl)
            (local.get $str))))
      (return (call $_str_hash_array
        (struct.get $StrBytesImpl $bytes))))

    ;; Only two subtypes — unreachable by construction.
    (unreachable)
  )

  ;; $_str_hash_data : (i32, i32) -> i32
  ;; FNV-1a over bytes in linear memory. Result masked to 31 bits.
  (func $_str_hash_data
    (param $offset i32) (param $length i32)
    (result i32)

    (local $h i32)
    (local $i i32)

    ;; FNV offset basis
    (local.set $h (i32.const 0x811c9dc5))
    (local.set $i (i32.const 0))

    (block $done
      (loop $step
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $length)))
        ;; h ^= byte
        (local.set $h
          (i32.xor (local.get $h)
            (i32.load8_u (i32.add (local.get $offset) (local.get $i)))))
        ;; h *= FNV prime (0x01000193)
        (local.set $h
          (i32.mul (local.get $h) (i32.const 0x01000193)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $step)))

    ;; Mask to 31 bits
    (i32.and (local.get $h) (i32.const 0x7fffffff))
  )

  ;; $_str_hash_array : (ref $ByteArray) -> i32
  ;; FNV-1a over bytes in a heap array. Result masked to 31 bits.
  (func $_str_hash_array
    (param $arr (ref $ByteArray))
    (result i32)

    (local $h i32)
    (local $i i32)
    (local $len i32)

    (local.set $h (i32.const 0x811c9dc5))
    (local.set $len (array.len (local.get $arr)))
    (local.set $i (i32.const 0))

    (block $done
      (loop $step
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $h
          (i32.xor (local.get $h)
            (array.get_u $ByteArray (local.get $arr) (local.get $i))))
        (local.set $h
          (i32.mul (local.get $h) (i32.const 0x01000193)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $step)))

    (i32.and (local.get $h) (i32.const 0x7fffffff))
  )


  ;; ---- Internal helpers ----

  ;; $_needs_unescape : i32 -> i32
  ;; Returns 1 if a byte needs to be escaped in unescape output.
  (func $_needs_unescape
    (param $byte i32)
    (result i32)

    (i32.or (i32.or (i32.or
      (i32.eq (local.get $byte) (i32.const 0x0A))   ;; \n
      (i32.eq (local.get $byte) (i32.const 0x09)))   ;; \t
      (i32.or
      (i32.eq (local.get $byte) (i32.const 0x0D))   ;; \r
      (i32.eq (local.get $byte) (i32.const 0x0C))))  ;; \f
      (i32.or (i32.or
      (i32.eq (local.get $byte) (i32.const 0x0B))   ;; \v
      (i32.eq (local.get $byte) (i32.const 0x08)))   ;; \b
      (i32.or (i32.or
      (i32.eq (local.get $byte) (i32.const 0x5C))   ;; \\
      (i32.eq (local.get $byte) (i32.const 0x27)))   ;; \'
      (i32.eq (local.get $byte) (i32.const 0x24))))) ;; \$
  )

  ;; $_hex_digit : i32 -> i32
  ;; Parse a single hex digit (0-9, a-f, A-F) → 0-15. Returns -1 if invalid.
  (func $_hex_digit
    (param $byte i32)
    (result i32)

    ;; 0-9
    (if (i32.and
          (i32.ge_u (local.get $byte) (i32.const 0x30))
          (i32.le_u (local.get $byte) (i32.const 0x39)))
      (then
        (return (i32.sub (local.get $byte) (i32.const 0x30)))))

    ;; a-f
    (if (i32.and
          (i32.ge_u (local.get $byte) (i32.const 0x61))
          (i32.le_u (local.get $byte) (i32.const 0x66)))
      (then
        (return (i32.sub (local.get $byte) (i32.const 0x57)))))

    ;; A-F
    (if (i32.and
          (i32.ge_u (local.get $byte) (i32.const 0x41))
          (i32.le_u (local.get $byte) (i32.const 0x46)))
      (then
        (return (i32.sub (local.get $byte) (i32.const 0x37)))))

    (i32.const -1)
  )

  ;; $_utf8_len : i32 -> i32
  ;; Number of bytes needed to encode a Unicode codepoint as UTF-8.
  (func $_utf8_len
    (param $cp i32)
    (result i32)

    (if (i32.le_u (local.get $cp) (i32.const 0x7F))
      (then (return (i32.const 1))))
    (if (i32.le_u (local.get $cp) (i32.const 0x7FF))
      (then (return (i32.const 2))))
    (if (i32.le_u (local.get $cp) (i32.const 0xFFFF))
      (then (return (i32.const 3))))
    (i32.const 4)
  )

  ;; $_write_utf8 : (ref $ByteArray), i32, i32 -> i32
  ;; Write a Unicode codepoint as UTF-8 into dst at offset j.
  ;; Returns the new offset (j + bytes written).
  (func $_write_utf8
    (param $dst (ref $ByteArray))
    (param $j i32)
    (param $cp i32)
    (result i32)

    ;; 1-byte: 0xxxxxxx
    (if (i32.le_u (local.get $cp) (i32.const 0x7F))
      (then
        (array.set $ByteArray (local.get $dst) (local.get $j)
          (local.get $cp))
        (return (i32.add (local.get $j) (i32.const 1)))))

    ;; 2-byte: 110xxxxx 10xxxxxx
    (if (i32.le_u (local.get $cp) (i32.const 0x7FF))
      (then
        (array.set $ByteArray (local.get $dst) (local.get $j)
          (i32.or (i32.const 0xC0)
            (i32.shr_u (local.get $cp) (i32.const 6))))
        (array.set $ByteArray (local.get $dst)
          (i32.add (local.get $j) (i32.const 1))
          (i32.or (i32.const 0x80)
            (i32.and (local.get $cp) (i32.const 0x3F))))
        (return (i32.add (local.get $j) (i32.const 2)))))

    ;; 3-byte: 1110xxxx 10xxxxxx 10xxxxxx
    (if (i32.le_u (local.get $cp) (i32.const 0xFFFF))
      (then
        (array.set $ByteArray (local.get $dst) (local.get $j)
          (i32.or (i32.const 0xE0)
            (i32.shr_u (local.get $cp) (i32.const 12))))
        (array.set $ByteArray (local.get $dst)
          (i32.add (local.get $j) (i32.const 1))
          (i32.or (i32.const 0x80)
            (i32.and
              (i32.shr_u (local.get $cp) (i32.const 6))
              (i32.const 0x3F))))
        (array.set $ByteArray (local.get $dst)
          (i32.add (local.get $j) (i32.const 2))
          (i32.or (i32.const 0x80)
            (i32.and (local.get $cp) (i32.const 0x3F))))
        (return (i32.add (local.get $j) (i32.const 3)))))

    ;; 4-byte: 11110xxx 10xxxxxx 10xxxxxx 10xxxxxx
    (array.set $ByteArray (local.get $dst) (local.get $j)
      (i32.or (i32.const 0xF0)
        (i32.shr_u (local.get $cp) (i32.const 18))))
    (array.set $ByteArray (local.get $dst)
      (i32.add (local.get $j) (i32.const 1))
      (i32.or (i32.const 0x80)
        (i32.and
          (i32.shr_u (local.get $cp) (i32.const 12))
          (i32.const 0x3F))))
    (array.set $ByteArray (local.get $dst)
      (i32.add (local.get $j) (i32.const 2))
      (i32.or (i32.const 0x80)
        (i32.and
          (i32.shr_u (local.get $cp) (i32.const 6))
          (i32.const 0x3F))))
    (array.set $ByteArray (local.get $dst)
      (i32.add (local.get $j) (i32.const 3))
      (i32.or (i32.const 0x80)
        (i32.and (local.get $cp) (i32.const 0x3F))))
    (i32.add (local.get $j) (i32.const 4))
  )


  ;; ---- String formatting (CPS) ----

  ;; _str_len : (ref $Str) -> i32
  ;; Byte length of any string subtype.
  (func $_str_len (param $str (ref $Str)) (result i32)

    ;; Empty string — length 0.
    (if (ref.test (ref $StrEmpty) (local.get $str))
      (then (return (i32.const 0))))

    ;; Try $StrDataImpl
    (block $not_data
      (block $is_data (result (ref $StrDataImpl))
        (br $not_data
          (br_on_cast $is_data (ref $Str) (ref $StrDataImpl)
            (local.get $str))))
      (return (struct.get $StrDataImpl $length)))

    ;; Must be $StrBytesImpl
    (array.len
      (struct.get $StrBytesImpl $bytes
        (ref.cast (ref $StrBytesImpl) (local.get $str))))
  )

  ;; _str_copy_to : (ref $Str, ref $ByteArray, i32) -> i32
  ;; Copy bytes from a string into dst at offset. Returns new offset.
  (func $_str_copy_to
    (param $str (ref $Str)) (param $dst (ref $ByteArray)) (param $pos i32)
    (result i32)

    (local $offset i32)
    (local $length i32)
    (local $src (ref $ByteArray))
    (local $i i32)

    ;; Empty string — nothing to copy.
    (if (ref.test (ref $StrEmpty) (local.get $str))
      (then (return (local.get $pos))))

    ;; Try $StrDataImpl — copy from linear memory
    (block $not_data
      (block $is_data (result (ref $StrDataImpl))
        (br $not_data
          (br_on_cast $is_data (ref $Str) (ref $StrDataImpl)
            (local.get $str))))
      (local.set $offset (struct.get $StrDataImpl $offset))
      (local.set $length
        (struct.get $StrDataImpl $length
          (ref.cast (ref $StrDataImpl) (local.get $str))))
      (local.set $i (i32.const 0))
      (block $done
        (loop $copy
          (br_if $done
            (i32.ge_u (local.get $i) (local.get $length)))
          (array.set $ByteArray (local.get $dst)
            (i32.add (local.get $pos) (local.get $i))
            (i32.load8_u (i32.add (local.get $offset) (local.get $i))))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br $copy)))
      (return (i32.add (local.get $pos) (local.get $length))))

    ;; Must be $StrBytesImpl — copy from heap array
    (local.set $src
      (struct.get $StrBytesImpl $bytes
        (ref.cast (ref $StrBytesImpl) (local.get $str))))
    (local.set $length (array.len (local.get $src)))
    (local.set $i (i32.const 0))
    (block $done
      (loop $copy
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $length)))
        (array.set $ByteArray (local.get $dst)
          (i32.add (local.get $pos) (local.get $i))
          (array.get_u $ByteArray (local.get $src) (local.get $i)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $copy)))
    (i32.add (local.get $pos) (local.get $length))
  )

  ;; ---- Value formatting (direct-style) ----

  ;; _str_fmt_val : (ref any) -> (ref $Str)
  ;; Format any value as a string. Dispatches on runtime type.
  (func $_str_fmt_val (param $val (ref any)) (result (ref $Str))

    ;; Try $Str — passthrough
    (block $not_str
      (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref any) (ref $Str)
            (local.get $val))))
      (return))

    ;; Try $Int — delegate to int.wat:fmt (handles signed/unsigned
    ;; sub-dispatch + the digit loop).
    (block $not_int
      (block $is_int (result (ref $Int))
        (br $not_int
          (br_on_cast $is_int (ref any) (ref $Int)
            (local.get $val))))
      (return_call $int_fmt))

    ;; Try $F64 — delegate to float.wat:fmt.
    (block $not_f64
      (block $is_f64 (result (ref $F64))
        (br $not_f64
          (br_on_cast $is_f64 (ref any) (ref $F64)
            (local.get $val))))
      (return_call $float_fmt))

    ;; Try $Decimal — delegate to decimal.wat:fmt.
    (block $not_decimal
      (block $is_decimal (result (ref $Decimal))
        (br $not_decimal
          (br_on_cast $is_decimal (ref any) (ref $Decimal)
            (local.get $val))))
      (return_call $decimal_fmt))

    ;; Try i31ref — bool or small int
    (block $not_i31
      (block $is_i31 (result (ref i31))
        (br $not_i31
          (br_on_cast $is_i31 (ref any) (ref i31)
            (local.get $val))))
      (return (call $_str_fmt_i31 (i31.get_s))))

    ;; Try $Range — delegate to range.wat:fmt.
    (block $not_range
      (block $is_range (result (ref $Range))
        (br $not_range
          (br_on_cast $is_range (ref any) (ref $Range)
            (local.get $val))))
      (return_call $range_fmt))

    ;; Try $Rec — format as "{key: val, ...}"
    (block $not_rec
      (block $is_rec (result (ref $Rec))
        (br $not_rec
          (br_on_cast $is_rec (ref any) (ref $Rec)
            (local.get $val))))
      (return (call $_str_fmt_rec)))

    ;; Try $List — format as "[a, b, ...]"
    (block $not_list
      (block $is_list (result (ref $List))
        (br $not_list
          (br_on_cast $is_list (ref any) (ref $List)
            (local.get $val))))
      (return (call $_str_fmt_list)))

    ;; Try $Set — format as "set v1, v2, ..." (or "set _" for empty)
    (block $not_set
      (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref any) (ref $Set)
            (local.get $val))))
      (return (call $set_repr)))

    ;; Unknown type — unreachable for now.
    (unreachable)
  )

  ;; _str_fmt_val_repr : (ref any) -> (ref $Str)
  ;; Like _str_fmt_val but uses str_repr for strings (quoted + escaped).
  ;; Used by container formatters (list, rec) so strings display as 'hello'.
  (func $_str_fmt_val_repr (param $val (ref any)) (result (ref $Str))

    ;; Try $Str — repr (quoted + escaped)
    (block $not_str
      (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref any) (ref $Str)
            (local.get $val))))
      (return (call $repr)))

    ;; Everything else delegates to _str_fmt_val.
    (call $_str_fmt_val (local.get $val))
  )

  ;; _str_fmt_i31 : i32 -> (ref $Str)
  ;; Format an i31ref value as a boolean: 0 → "false", 1 → "true".
  ;; i31ref is currently only used for booleans; integer i31 rendering
  ;; will be added when i31ref is used for small integers.
  (func $_str_fmt_i31 (param $v i32) (result (ref $Str))

    (local $buf (ref $ByteArray))

    (if (i32.eqz (local.get $v))
      (then
        ;; "false" = 66 61 6C 73 65
        (local.set $buf (array.new $ByteArray (i32.const 0) (i32.const 5)))
        (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x66))
        (array.set $ByteArray (local.get $buf) (i32.const 1) (i32.const 0x61))
        (array.set $ByteArray (local.get $buf) (i32.const 2) (i32.const 0x6C))
        (array.set $ByteArray (local.get $buf) (i32.const 3) (i32.const 0x73))
        (array.set $ByteArray (local.get $buf) (i32.const 4) (i32.const 0x65))
        (return (struct.new $StrBytesImpl (local.get $buf)))))

    ;; "true" = 74 72 75 65
    (local.set $buf (array.new $ByteArray (i32.const 0) (i32.const 4)))
    (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x74))
    (array.set $ByteArray (local.get $buf) (i32.const 1) (i32.const 0x72))
    (array.set $ByteArray (local.get $buf) (i32.const 2) (i32.const 0x75))
    (array.set $ByteArray (local.get $buf) (i32.const 3) (i32.const 0x65))
    (struct.new $StrBytesImpl (local.get $buf))
  )



  ;; _str_is_ident : (ref $Str) -> i32
  ;; Check if a string is a valid fink identifier.
  ;; Returns 1 if all bytes are identifier chars (a-z, A-Z, 0-9, _, -, $, or >= 0x80).
  ;; Empty string returns 0. First char must not be a digit.
  (func $_str_is_ident (param $str (ref $Str)) (result i32)
    (local $bytes (ref $ByteArray))
    (local $len i32)
    (local $i i32)
    (local $b i32)

    (local.set $bytes (call $bytes (local.get $str)))
    (local.set $len (array.len (local.get $bytes)))

    ;; Empty string is not an identifier.
    (if (i32.eqz (local.get $len))
      (then (return (i32.const 0))))

    ;; First byte must not be a digit (0x30-0x39).
    (local.set $b (array.get_u $ByteArray (local.get $bytes) (i32.const 0)))
    (if (i32.and
          (i32.ge_u (local.get $b) (i32.const 0x30))
          (i32.le_u (local.get $b) (i32.const 0x39)))
      (then (return (i32.const 0))))

    ;; Check all bytes.
    (local.set $i (i32.const 0))
    (block $done
      (loop $check
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $b (array.get_u $ByteArray (local.get $bytes) (local.get $i)))

        ;; >= 0x80: UTF-8 continuation/start — always valid
        (if (i32.ge_u (local.get $b) (i32.const 0x80))
          (then
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $check)))

        ;; a-z (0x61-0x7A)
        (if (i32.and
              (i32.ge_u (local.get $b) (i32.const 0x61))
              (i32.le_u (local.get $b) (i32.const 0x7A)))
          (then
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $check)))

        ;; A-Z (0x41-0x5A)
        (if (i32.and
              (i32.ge_u (local.get $b) (i32.const 0x41))
              (i32.le_u (local.get $b) (i32.const 0x5A)))
          (then
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $check)))

        ;; 0-9 (0x30-0x39) — allowed after first char
        (if (i32.and
              (i32.ge_u (local.get $b) (i32.const 0x30))
              (i32.le_u (local.get $b) (i32.const 0x39)))
          (then
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $check)))

        ;; _ (0x5F), - (0x2D), $ (0x24)
        (if (i32.or
              (i32.eq (local.get $b) (i32.const 0x5F))
              (i32.or
                (i32.eq (local.get $b) (i32.const 0x2D))
                (i32.eq (local.get $b) (i32.const 0x24))))
          (then
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $check)))

        ;; Any other byte — not an identifier.
        (return (i32.const 0))))

    (i32.const 1)
  )

  ;; _str_fmt_rec_key_len : (ref eq) -> i32
  ;; Byte length of a formatted record key.
  ;; String ident: bare len. String non-ident: str_repr len. Other: repr len + 2 for parens.
  (func $_str_fmt_rec_key_len (param $key (ref eq)) (result i32)
    (local $str (ref $Str))

    ;; Try $Str
    (block $not_str
      (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref eq) (ref $Str)
            (local.get $key))))
      (local.set $str)
      ;; String key — identifier: bare, otherwise: str_repr (quoted + escaped).
      (return
        (if (result i32) (call $_str_is_ident (local.get $str))
          (then (call $_str_len (local.get $str)))
          (else (call $_str_len (call $repr (local.get $str)))))))

    ;; Non-string key — (repr), add 2 for parens.
    (i32.add
      (call $_str_len
        (call $_str_fmt_val (ref.cast (ref any) (local.get $key))))
      (i32.const 2))
  )

  ;; _str_fmt_rec : (ref $Rec) -> (ref $Str)
  ;; Format a record as "{key: val, key2: val2}".
  ;; Empty record formats as "{}".
  ;; Two-pass: first compute total byte length, then copy.
  (func $_str_fmt_rec (param $rec (ref $Rec)) (result (ref $Str))
    (local $node (ref $HamtNode))
    (local $total i32)
    (local $entry_count i32)
    (local $buf (ref $ByteArray))
    (local $pos i32)

    ;; Downcast to $RecImpl, extract HAMT root.
    (local.set $node
      (struct.get $RecImpl $hamt
        (ref.cast (ref $RecImpl) (local.get $rec))))

    ;; Count entries for separator sizing.
    (local.set $entry_count
      (call $dict_size (ref.cast (ref $Rec) (local.get $rec))))

    ;; Empty record: return "{}"
    (if (i32.eqz (local.get $entry_count))
      (then
        (return (call $_str_from_ascii_2
          (i32.const 0x7B) ;; '{'
          (i32.const 0x7D) ;; '}'
        ))))

    ;; Pass 1: compute total bytes.
    ;; Each entry contributes: key_len + 2 (": ") + val_len
    ;; Between entries: 2 (", ")
    ;; Plus 2 for "{" and "}"
    (local.set $total
      (i32.add
        (i32.const 2) ;; { and }
        (i32.add
          (call $_str_fmt_rec_size_node (local.get $node))
          (i32.mul
            (i32.sub (local.get $entry_count) (i32.const 1))
            (i32.const 2))))) ;; ", " separators

    ;; Allocate buffer.
    (local.set $buf
      (array.new $ByteArray (i32.const 0) (local.get $total)))

    ;; Write opening '{'.
    (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x7B))
    (local.set $pos (i32.const 1))

    ;; Pass 2: copy formatted entries.
    (local.set $pos
      (call $_str_fmt_rec_copy_node
        (local.get $node) (local.get $buf) (local.get $pos)
        (i32.const 0))) ;; is_first = true (0 entries written so far)

    ;; Write closing '}'.
    (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x7D))

    (struct.new $StrBytesImpl (local.get $buf))
  )

  ;; _str_fmt_rec_size_node : (ref $HamtNode) -> i32
  ;; Compute total bytes for all entries in a HAMT node (key + ": " + val).
  ;; Does NOT include separators between entries or braces.
  (func $_str_fmt_rec_size_node
    (param $node (ref $HamtNode))
    (result i32)

    (local $children (ref $HamtChildren))
    (local $len i32)
    (local $i i32)
    (local $total i32)
    (local $child (ref null struct))
    (local $leaf (ref $HamtLeaf))

    (local.set $children
      (struct.get $HamtNode $children (local.get $node)))
    (local.set $len
      (array.len (local.get $children)))
    (local.set $total (i32.const 0))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $len)))

        (local.set $child
          (array.get $HamtChildren
            (local.get $children)
            (local.get $i)))

        ;; Leaf: key_display_len + 2 (": ") + val_len
        (if (ref.test (ref $HamtLeaf) (local.get $child))
          (then
            (local.set $leaf
              (ref.cast (ref $HamtLeaf) (local.get $child)))
            (local.set $total
              (i32.add (local.get $total)
                (i32.add
                  (i32.add
                    (call $_str_fmt_rec_key_len
                      (struct.get $HamtLeaf $key (local.get $leaf)))
                    (i32.const 2)) ;; ": "
                  (call $_str_len
                    (call $_str_fmt_val_repr
                      (ref.cast (ref any)
                        (struct.get $HamtLeaf $val (local.get $leaf))))))))))

        ;; Sub-node: recurse
        (if (ref.test (ref $HamtNode) (local.get $child))
          (then
            (local.set $total
              (i32.add (local.get $total)
                (call $_str_fmt_rec_size_node
                  (ref.cast (ref $HamtNode) (local.get $child)))))))

        ;; Collision: walk leaves
        (if (ref.test (ref $HamtCollision) (local.get $child))
          (then
            (local.set $total
              (i32.add (local.get $total)
                (call $_str_fmt_rec_size_collision
                  (struct.get $HamtCollision $col_leaves
                    (ref.cast (ref $HamtCollision) (local.get $child))))))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $total)
  )

  ;; _str_fmt_rec_size_collision : (ref $HamtChildren) -> i32
  ;; Compute total bytes for all leaves in a collision array.
  (func $_str_fmt_rec_size_collision
    (param $leaves (ref $HamtChildren))
    (result i32)

    (local $len i32)
    (local $i i32)
    (local $total i32)
    (local $leaf (ref $HamtLeaf))

    (local.set $len (array.len (local.get $leaves)))
    (local.set $total (i32.const 0))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $leaf
          (ref.cast (ref $HamtLeaf)
            (array.get $HamtChildren (local.get $leaves) (local.get $i))))
        (local.set $total
          (i32.add (local.get $total)
            (i32.add
              (i32.add
                (call $_str_fmt_rec_key_len
                  (struct.get $HamtLeaf $key (local.get $leaf)))
                (i32.const 2))
              (call $_str_len
                (call $_str_fmt_val_repr
                  (ref.cast (ref any)
                    (struct.get $HamtLeaf $val (local.get $leaf))))))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $total)
  )

  ;; _str_fmt_rec_copy_key : (key, buf, pos) -> new_pos
  ;; Copy a formatted record key into buf.
  ;; String ident: bare. String non-ident: str_repr. Other: (repr).
  (func $_str_fmt_rec_copy_key
    (param $key (ref eq))
    (param $buf (ref $ByteArray))
    (param $pos i32)
    (result i32)

    (local $str (ref $Str))

    ;; Try $Str
    (block $not_str
      (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref eq) (ref $Str)
            (local.get $key))))
      (local.set $str)

      (if (call $_str_is_ident (local.get $str))
        (then
          ;; Identifier — copy as-is.
          (local.set $pos
            (call $_str_copy_to (local.get $str) (local.get $buf) (local.get $pos))))
        (else
          ;; Non-identifier string — use str_repr (quoted + escaped).
          (local.set $pos
            (call $_str_copy_to
              (call $repr (local.get $str))
              (local.get $buf)
              (local.get $pos)))))
      (return (local.get $pos)))

    ;; Non-string key — wrap in parens: (repr)
    (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x28)) ;; '('
    (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
    (local.set $pos
      (call $_str_copy_to
        (call $_str_fmt_val (ref.cast (ref any) (local.get $key)))
        (local.get $buf)
        (local.get $pos)))
    (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x29)) ;; ')'
    (local.set $pos (i32.add (local.get $pos) (i32.const 1)))

    (local.get $pos)
  )

  ;; _str_fmt_rec_copy_node : (node, buf, pos, written) -> new_pos
  ;; Copy formatted entries into buf. written = entries written so far
  ;; (used to decide whether to prepend ", ").
  (func $_str_fmt_rec_copy_node
    (param $node (ref $HamtNode))
    (param $buf (ref $ByteArray))
    (param $pos i32)
    (param $written i32)
    (result i32)

    (local $children (ref $HamtChildren))
    (local $len i32)
    (local $i i32)
    (local $child (ref null struct))
    (local $leaf (ref $HamtLeaf))

    (local.set $children
      (struct.get $HamtNode $children (local.get $node)))
    (local.set $len
      (array.len (local.get $children)))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $len)))

        (local.set $child
          (array.get $HamtChildren
            (local.get $children)
            (local.get $i)))

        ;; Leaf
        (if (ref.test (ref $HamtLeaf) (local.get $child))
          (then
            (local.set $leaf
              (ref.cast (ref $HamtLeaf) (local.get $child)))

            ;; Separator ", " if not first entry
            (if (local.get $written)
              (then
                (array.set $ByteArray (local.get $buf) (local.get $pos)
                  (i32.const 0x2C)) ;; ','
                (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
                (array.set $ByteArray (local.get $buf) (local.get $pos)
                  (i32.const 0x20)) ;; ' '
                (local.set $pos (i32.add (local.get $pos) (i32.const 1)))))

            ;; Copy key (with quotes if not an identifier)
            (local.set $pos
              (call $_str_fmt_rec_copy_key
                (struct.get $HamtLeaf $key (local.get $leaf))
                (local.get $buf) (local.get $pos)))

            ;; Write ": "
            (array.set $ByteArray (local.get $buf) (local.get $pos)
              (i32.const 0x3A)) ;; ':'
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (array.set $ByteArray (local.get $buf) (local.get $pos)
              (i32.const 0x20)) ;; ' '
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))

            ;; Copy val
            (local.set $pos
              (call $_str_copy_to
                (call $_str_fmt_val_repr
                  (ref.cast (ref any)
                    (struct.get $HamtLeaf $val (local.get $leaf))))
                (local.get $buf)
                (local.get $pos)))

            (local.set $written
              (i32.add (local.get $written) (i32.const 1)))))

        ;; Sub-node: recurse
        (if (ref.test (ref $HamtNode) (local.get $child))
          (then
            (local.set $pos
              (call $_str_fmt_rec_copy_node
                (ref.cast (ref $HamtNode) (local.get $child))
                (local.get $buf) (local.get $pos) (local.get $written)))
            ;; Update written count — we don't know exactly how many
            ;; entries the sub-node had, but we can check if pos advanced.
            ;; Simpler: count via _hamt_size_node.
            (local.set $written
              (i32.add (local.get $written)
                (call $_hamt_size_node
                  (ref.cast (ref $HamtNode) (local.get $child)))))))

        ;; Collision: copy all leaves
        (if (ref.test (ref $HamtCollision) (local.get $child))
          (then
            (local.set $pos
              (call $_str_fmt_rec_copy_collision
                (struct.get $HamtCollision $col_leaves
                  (ref.cast (ref $HamtCollision) (local.get $child)))
                (local.get $buf) (local.get $pos) (local.get $written)))
            (local.set $written
              (i32.add (local.get $written)
                (array.len
                  (struct.get $HamtCollision $col_leaves
                    (ref.cast (ref $HamtCollision) (local.get $child))))))))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $pos)
  )

  ;; _str_fmt_rec_copy_collision : (leaves, buf, pos, written) -> new_pos
  ;; Copy formatted collision entries into buf.
  (func $_str_fmt_rec_copy_collision
    (param $leaves (ref $HamtChildren))
    (param $buf (ref $ByteArray))
    (param $pos i32)
    (param $written i32)
    (result i32)

    (local $len i32)
    (local $i i32)
    (local $leaf (ref $HamtLeaf))

    (local.set $len (array.len (local.get $leaves)))
    (local.set $i (i32.const 0))

    (block $done
      (loop $walk
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $len)))

        (local.set $leaf
          (ref.cast (ref $HamtLeaf)
            (array.get $HamtChildren (local.get $leaves) (local.get $i))))

        ;; Separator
        (if (i32.or (local.get $written) (local.get $i))
          (then
            (array.set $ByteArray (local.get $buf) (local.get $pos)
              (i32.const 0x2C))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (array.set $ByteArray (local.get $buf) (local.get $pos)
              (i32.const 0x20))
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))))

        ;; Copy key (with quotes if not an identifier)
        (local.set $pos
          (call $_str_fmt_rec_copy_key
            (struct.get $HamtLeaf $key (local.get $leaf))
            (local.get $buf) (local.get $pos)))

        ;; Write ": "
        (array.set $ByteArray (local.get $buf) (local.get $pos)
          (i32.const 0x3A))
        (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
        (array.set $ByteArray (local.get $buf) (local.get $pos)
          (i32.const 0x20))
        (local.set $pos (i32.add (local.get $pos) (i32.const 1)))

        ;; Copy val
        (local.set $pos
          (call $_str_copy_to
            (call $_str_fmt_val_repr
              (ref.cast (ref any)
                (struct.get $HamtLeaf $val (local.get $leaf))))
            (local.get $buf)
            (local.get $pos)))

        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $walk)))

    (local.get $pos)
  )

  ;; _str_fmt_list : (ref $List) -> (ref $Str)
  ;; Format a list as "[a, b, c]".
  ;; Uses only the public list API (list_op_empty, list_head_any, list_tail_any).
  (func $_str_fmt_list (param $list (ref $List)) (result (ref $Str))
    (local $cur (ref null any))
    (local $total i32)
    (local $count i32)
    (local $buf (ref $ByteArray))
    (local $pos i32)
    (local $is_first i32)

    ;; Empty list: return "[]"
    (if (call $list_op_empty (local.get $list))
      (then
        (return (call $_str_from_ascii_2
          (i32.const 0x5B) ;; '['
          (i32.const 0x5D) ;; ']'
        ))))

    ;; Pass 1: compute total byte length.
    ;; Each element: formatted length. Between elements: 2 (", ").
    ;; Plus 2 for "[" and "]".
    (local.set $cur (local.get $list))
    (local.set $total (i32.const 2)) ;; [ and ]
    (local.set $count (i32.const 0))
    (block $done1
      (loop $len_loop
        (br_if $done1
          (call $list_op_empty (local.get $cur)))
        (local.set $total
          (i32.add (local.get $total)
            (call $_str_len
              (call $_str_fmt_val_repr
                (ref.as_non_null
                  (call $head_any (local.get $cur)))))))
        (local.set $count (i32.add (local.get $count) (i32.const 1)))
        (local.set $cur
          (call $tail_any (local.get $cur)))
        (br $len_loop)))

    ;; Add separator bytes: (count - 1) * 2
    (local.set $total
      (i32.add (local.get $total)
        (i32.mul
          (i32.sub (local.get $count) (i32.const 1))
          (i32.const 2))))

    ;; Allocate buffer.
    (local.set $buf
      (array.new $ByteArray (i32.const 0) (local.get $total)))

    ;; Write opening '['.
    (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x5B))
    (local.set $pos (i32.const 1))

    ;; Pass 2: format and copy each element.
    (local.set $cur (local.get $list))
    (local.set $is_first (i32.const 1))
    (block $done2
      (loop $copy_loop
        (br_if $done2
          (call $list_op_empty (local.get $cur)))

        ;; Write ", " separator (except before first element).
        (if (i32.eqz (local.get $is_first))
          (then
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x2C)) ;; ','
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
            (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x20)) ;; ' '
            (local.set $pos (i32.add (local.get $pos) (i32.const 1)))))
        (local.set $is_first (i32.const 0))

        ;; Format and copy element.
        (local.set $pos
          (call $_str_copy_to
            (call $_str_fmt_val_repr
              (ref.as_non_null
                (call $head_any (local.get $cur))))
            (local.get $buf)
            (local.get $pos)))

        (local.set $cur
          (call $tail_any (local.get $cur)))
        (br $copy_loop)))

    ;; Write closing ']'.
    (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x5D))

    (struct.new $StrBytesImpl (local.get $buf))
  )


  ;; _str_from_ascii_2 : (i32, i32) -> (ref $Str)
  ;; Build a 2-byte string from ASCII code points.
  (func $_str_from_ascii_2 (param $a i32) (param $b i32) (result (ref $Str))
    (local $buf (ref $ByteArray))
    (local.set $buf (array.new $ByteArray (i32.const 0) (i32.const 2)))
    (array.set $ByteArray (local.get $buf) (i32.const 0) (local.get $a))
    (array.set $ByteArray (local.get $buf) (i32.const 1) (local.get $b))
    (struct.new $StrBytesImpl (local.get $buf))
  )



  ;; CPS wrappers — stripped by unit test harness (prepare_wat).

  ;; str_fmt : (ref null any, ref null any) -> void
  ;; CPS string formatter. First arg is a $VarArgs array of string segments,
  ;; second arg is the continuation. Formats each segment via _str_fmt_val,
  ;; concatenates all results into a single $StrBytesImpl, and passes the
  ;; result to the continuation via _apply.
  ;; TODO might need a proper wrapper to be fink importable?
  (func $fmt (@pub) (@impl "std/str.fnk:fmt")
    (param $segments_any (ref null any)) (param $cont (ref null any))

    (local $segments (ref $VarArgs))
    (local $len i32)
    (local $i i32)
    (local $total i32)
    (local $dst (ref $ByteArray))
    (local $pos i32)
    (local $formatted (ref $Str))

    (local.set $segments
      (ref.cast (ref $VarArgs) (local.get $segments_any)))
    (local.set $len
      (array.len (local.get $segments)))

    ;; Pass 1: format each segment and compute total byte length.
    ;; We format twice (once for length, once for copy) to avoid
    ;; allocating a temp array of formatted strings. For short
    ;; templates this is cheap; revisit if profiling shows otherwise.
    (local.set $i (i32.const 0))
    (local.set $total (i32.const 0))
    (block $done1
      (loop $len_loop
        (br_if $done1
          (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $total
          (i32.add (local.get $total)
            (call $_str_len
              (call $_str_fmt_val
                (ref.as_non_null
                  (array.get $VarArgs (local.get $segments) (local.get $i)))))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $len_loop)))

    ;; Allocate destination buffer.
    (local.set $dst
      (array.new $ByteArray (i32.const 0) (local.get $total)))

    ;; Pass 2: format and copy each segment into the buffer.
    (local.set $i (i32.const 0))
    (local.set $pos (i32.const 0))
    (block $done2
      (loop $copy_loop
        (br_if $done2
          (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $pos
          (call $_str_copy_to
            (call $_str_fmt_val
              (ref.as_non_null
                (array.get $VarArgs (local.get $segments) (local.get $i))))
            (local.get $dst)
            (local.get $pos)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $copy_loop)))

    ;; Wrap and pass to continuation.
    (return_call $apply_1
      (struct.new $StrBytesImpl (local.get $dst))
      (local.get $cont))
  )


  ;; ---- String slicing ----

  ;; _str_slice(str, start_i, end_i) → (ref null $Str)
  ;; Internal direct-style slice: extract bytes [start..end).
  ;; Negative values resolve from end: -1 → len-1.
  ;; Returns null on out-of-bounds (after resolution: 0 <= start <= end <= len).
  ;; Returns empty string for empty slice, original for full range.
  (func $_str_slice
    (param $str (ref $Str)) (param $start_i i64) (param $end_i i64)
    (result (ref null $Str))

    (local $len i32)
    (local $start i32)
    (local $end i32)
    (local $src (ref $ByteArray))
    (local $slice_len i32)
    (local $dst (ref $ByteArray))
    (local $i i32)

    (local.set $len (call $_str_len (local.get $str)))

    ;; Resolve negative values from end.
    (if (i64.lt_s (local.get $start_i) (i64.const 0))
      (then
        (local.set $start
          (i32.add (local.get $len) (i32.wrap_i64 (local.get $start_i)))))
      (else
        (local.set $start (i32.wrap_i64 (local.get $start_i)))))

    (if (i64.lt_s (local.get $end_i) (i64.const 0))
      (then
        (local.set $end
          (i32.add (local.get $len) (i32.wrap_i64 (local.get $end_i)))))
      (else
        (local.set $end (i32.wrap_i64 (local.get $end_i)))))

    ;; Bounds check: 0 <= start <= end <= len
    (if (i32.or
          (i32.lt_s (local.get $start) (i32.const 0))
          (i32.or
            (i32.gt_s (local.get $start) (local.get $end))
            (i32.gt_s (local.get $end) (local.get $len))))
      (then (return (ref.null $Str))))

    ;; Empty slice
    (local.set $slice_len (i32.sub (local.get $end) (local.get $start)))
    (if (i32.eqz (local.get $slice_len))
      (then (return (call $str_empty))))

    ;; Full string — return original
    (if (i32.and
          (i32.eqz (local.get $start))
          (i32.eq (local.get $end) (local.get $len)))
      (then (return (local.get $str))))

    ;; Convert to byte array, then copy slice
    (local.set $src (call $bytes (local.get $str)))
    (local.set $dst (array.new $ByteArray (i32.const 0) (local.get $slice_len)))
    (local.set $i (i32.const 0))
    (block $done
      (loop $copy
        (br_if $done
          (i32.ge_u (local.get $i) (local.get $slice_len)))
        (array.set $ByteArray (local.get $dst)
          (local.get $i)
          (array.get_u $ByteArray (local.get $src)
            (i32.add (local.get $start) (local.get $i))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $copy)))
    (struct.new $StrBytesImpl (local.get $dst))
  )

  ;; str_slice(str, start, end, succ, fail)
  ;; CPS wrapper: extract bytes [start..end) from a string.
  ;;   succ — called with the substring on success
  ;;   fail — called with the original string on out-of-bounds
  (func $slice (@impl "TODO:slice")
    (param $str (ref null any))
    (param $start (ref null any))
    (param $end (ref null any))
    (param $succ (ref null any))
    (param $fail (ref null any))

    (local $s (ref $Str))
    (local $result (ref null $Str))

    (local.set $s (ref.cast (ref $Str) (local.get $str)))
    (local.set $result (call $_str_slice
      (local.get $s)
      (struct.get $I64 $ival (ref.cast (ref $I64) (local.get $start)))
      (struct.get $I64 $ival (ref.cast (ref $I64) (local.get $end)))))

    (if (ref.is_null (local.get $result))
      (then
        (return_call $apply_1
          (local.get $str)
          (local.get $fail))))

    (return_call $apply_1
      (ref.as_non_null (local.get $result))
      (local.get $succ))
  )


  ;; str_op_dot(str, key, cont)
  ;; Member access on strings:
  ;;   $Range key → byte slice (start..end or start...end)
  ;;   $Num key   → single byte at index
  ;; Out of bounds → unreachable
  (func $op_dot (@impl "std/operators.fnk:op_dot" $Str _)
    (param $str (ref null any)) (param $key (ref null any)) (param $cont (ref null any))

    (local $s (ref $Str))
    (local $range (ref $Range))
    (local $start_i i64)
    (local $end_i i64)
    (local $result (ref null $Str))

    (local.set $s (ref.cast (ref $Str) (local.get $str)))

    ;; Try $Range key
    (block $not_range
      (block $is_range (result (ref $Range))
        (br $not_range
          (br_on_cast $is_range (ref null any) (ref $Range)
            (local.get $key))))
      (local.set $range)

      ;; Range bounds are $I64 — read $ival directly.
      (local.set $start_i (struct.get $I64 $ival (call $range_start (local.get $range))))
      (local.set $end_i (struct.get $I64 $ival (call $range_end (local.get $range))))

      ;; Adjust end for inclusive range
      (if (call $range_is_incl (local.get $range))
        (then
          (local.set $end_i (i64.add (local.get $end_i) (i64.const 1)))))

      (local.set $result
        (call $_str_slice (local.get $s) (local.get $start_i) (local.get $end_i)))
      (if (ref.is_null (local.get $result))
        (then (unreachable)))
      (return_call $apply_1
        (ref.as_non_null (local.get $result))
        (local.get $cont)))

    ;; Try $I64 key — single byte at index
    (block $not_i64
      (block $is_i64 (result (ref $I64))
        (br $not_i64
          (br_on_cast $is_i64 (ref null any) (ref $I64)
            (local.get $key))))
      (local.set $start_i (struct.get $I64 $ival))

      (local.set $result
        (call $_str_slice
          (local.get $s)
          (local.get $start_i)
          (i64.add (local.get $start_i) (i64.const 1))))
      (if (ref.is_null (local.get $result))
        (then (unreachable)))
      (return_call $apply_1
        (ref.as_non_null (local.get $result))
        (local.get $cont)))

    (unreachable)
  )


  ;; str_match(subj, prefix, suffix, fail, succ)
  ;; CPS string template pattern matching.
  ;; Checks subj starts with prefix and ends with suffix (non-overlapping).
  ;; On match: calls succ(middle_slice). On mismatch: calls fail().
  (func $match (@impl "std/str.fnk:match")
    (param $subj (ref null any))
    (param $prefix (ref null any))
    (param $suffix (ref null any))
    (param $fail (ref null any))
    (param $succ (ref null any))

    (local $s_str (ref $Str))
    (local $p_str (ref $Str))
    (local $x_str (ref $Str))
    (local $s_bytes (ref $ByteArray))
    (local $p_bytes (ref $ByteArray))
    (local $x_bytes (ref $ByteArray))
    (local $s_len i32)
    (local $p_len i32)
    (local $x_len i32)
    (local $mid_start i32)
    (local $mid_len i32)
    (local $i i32)
    (local $mid (ref $ByteArray))

    ;; Cast subject to $Str — fail if not a string
    (if (i32.eqz (ref.test (ref $Str) (local.get $subj)))
      (then (return_call $apply_0 (local.get $fail))))
    (local.set $s_str (ref.cast (ref $Str) (local.get $subj)))

    ;; Cast prefix/suffix
    (local.set $p_str (ref.cast (ref $Str) (local.get $prefix)))
    (local.set $x_str (ref.cast (ref $Str) (local.get $suffix)))

    ;; Get lengths
    (local.set $s_len (call $_str_len (local.get $s_str)))
    (local.set $p_len (call $_str_len (local.get $p_str)))
    (local.set $x_len (call $_str_len (local.get $x_str)))

    ;; Non-overlapping check: subject must be at least prefix + suffix long
    (if (i32.lt_u (local.get $s_len)
          (i32.add (local.get $p_len) (local.get $x_len)))
      (then (return_call $apply_0 (local.get $fail))))

    ;; Get byte arrays
    (local.set $s_bytes (call $bytes (local.get $s_str)))

    ;; Check prefix match
    (if (local.get $p_len)
      (then
        (local.set $p_bytes (call $bytes (local.get $p_str)))
        (local.set $i (i32.const 0))
        (block $pfx_fail
          (loop $pfx_loop
            (br_if $pfx_fail
              (i32.ne
                (array.get_u $ByteArray (local.get $s_bytes) (local.get $i))
                (array.get_u $ByteArray (local.get $p_bytes) (local.get $i))))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br_if $pfx_loop (i32.lt_u (local.get $i) (local.get $p_len))))
          (br 1)) ;; skip fail — prefix matched
        (return_call $apply_0 (local.get $fail))))

    ;; Check suffix match
    (local.set $mid_start (local.get $p_len))
    (local.set $mid_len (i32.sub (local.get $s_len)
                          (i32.add (local.get $p_len) (local.get $x_len))))
    (if (local.get $x_len)
      (then
        (local.set $x_bytes (call $bytes (local.get $x_str)))
        (local.set $i (i32.const 0))
        (block $sfx_fail
          (loop $sfx_loop
            (br_if $sfx_fail
              (i32.ne
                (array.get_u $ByteArray (local.get $s_bytes)
                  (i32.add
                    (i32.add (local.get $mid_start) (local.get $mid_len))
                    (local.get $i)))
                (array.get_u $ByteArray (local.get $x_bytes) (local.get $i))))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br_if $sfx_loop (i32.lt_u (local.get $i) (local.get $x_len))))
          (br 1)) ;; skip fail — suffix matched
        (return_call $apply_0 (local.get $fail))))

    ;; Both matched — slice the middle
    (if (i32.eqz (local.get $mid_len))
      (then (return_call $apply_1 (call $str_empty) (local.get $succ))))

    ;; Full string — no prefix or suffix
    (if (i32.eq (local.get $mid_len) (local.get $s_len))
      (then (return_call $apply_1 (local.get $s_str) (local.get $succ))))

    ;; Copy middle bytes
    (local.set $mid (array.new $ByteArray (i32.const 0) (local.get $mid_len)))
    (local.set $i (i32.const 0))
    (block $done
      (loop $copy
        (br_if $done (i32.ge_u (local.get $i) (local.get $mid_len)))
        (array.set $ByteArray (local.get $mid)
          (local.get $i)
          (array.get_u $ByteArray (local.get $s_bytes)
            (i32.add (local.get $mid_start) (local.get $i))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $copy)))
    (return_call $apply_1
      (struct.new $StrBytesImpl (local.get $mid))
      (local.get $succ))
  )

)
