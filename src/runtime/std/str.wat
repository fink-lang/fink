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
  (import "std/num.wat"     "Num"     (type $Num     (sub any)))
  (import "std/num.wat"     "fmt"     (func $num_fmt   (param (ref $Num))   (result (ref $Str))))
  (import "std/int.wat"     "Int"     (type $Int     (sub any) (struct)))
  (import "std/int.wat"     "I64"     (type $I64     (sub $Int (struct (field $ival i64)))))
  (import "std/range.wat"   "fmt"      (func $range_fmt     (param (ref $Range)) (result (ref $Str))))
  (import "std/range.wat"   "start"    (func $range_start   (param (ref $Range)) (result (ref $I64))))
  (import "std/range.wat"   "end"      (func $range_end     (param (ref $Range)) (result (ref null $I64))))
  (import "std/range.wat"   "is_incl"  (func $range_is_incl (param (ref $Range)) (result i32)))
  (import "std/range.wat"  "Range"     (type $Range     (sub any)))
  (import "std/dict.wat"   "Rec"       (type $Rec       (sub any)))
  (import "std/set.wat"    "Set"       (type $Set       (sub any)))
  (import "std/list.wat"   "List"      (type $List      (sub any)))
  (import "rt/apply.wat"   "VarArgs"   (type $VarArgs   (sub any)))
  (import "rt/apply.wat"   "Closure"   (type $Closure   (sub any)))
  (import "rt/apply.wat"   "Captures"  (type $Captures  (sub any)))
  (import "rt/apply.wat"   "Fn3"       (type $Fn3       (sub any)))
  (import "rt/apply.wat"   "args_head"
    (func $args_head (param (ref null any)) (result (ref null any))))
  (import "rt/apply.wat"   "args_tail"
    (func $args_tail (param (ref null any)) (result (ref null any))))

  ;; Func imports
  (import "rt/apply.wat" "apply_1" (func $apply_1 (;apply-ctx;) (param (ref null any)) (param $val (ref null any)) (param $cont (ref null any))))
  (import "rt/apply.wat" "apply_0" (func $apply_0 (;apply-ctx;) (param (ref null any)) (param $cont (ref null any))))
  (import "rt/apply.wat" "apply_2_vals" (func $apply_2_vals (;apply-ctx;) (param (ref null any)) (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))))
  (import "std/set.wat" "fmt"
    (func $set_fmt (param (ref $Set)) (result (ref $Str))))

  (import "std/dict.wat" "fmt"
    (func $rec_fmt (param (ref $Rec)) (result (ref $Str))))

  (import "std/list.wat" "size"
    (func $list_size (param $list (ref $List)) (result i32)))
  (import "std/list.wat" "head_any"
    (func $head_any (param $list (ref null any)) (result (ref null any))))
  (import "std/list.wat" "tail_any"
    (func $tail_any (param $list (ref null any)) (result (ref null any))))

  (import "std/list.wat" "op_empty"
    (func $list_op_empty (param $val (ref null any)) (result i32)))
  (import "std/list.wat" "fmt"
    (func $list_fmt (param (ref $List)) (result (ref $Str))))

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

  ;; from_data : (i32, i32) -> (ref $StrDataImpl)
  ;; Wrap a data-section pointer into a string value. Used by the
  ;; codegen for string literals — see lower.rs:1457 (`rt.str_from_data()`).
  ;; TODO: this is a constructor for literals. find protocol for it!
  (func $from_data (@impl "std/str.fnk:from_data")
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
  (func $repr (@pub) (@impl "std/repr.fnk:repr" $Str)
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
  (func $_str_len (@pub) (param $str (ref $Str)) (result i32)

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
  (func $_str_copy_to (@pub)
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

  ;; fmt_val : (ref any) -> (ref $Str)
  ;; Public dispatcher: format any value as a string by runtime type.
  ;; Per-type fmt impls (int.wat:fmt, float.wat:fmt, etc.) own the
  ;; rendering; this picks the right one. Collection fmt impls
  ;; (list.wat, dict.wat, set.wat) call back into this for elements.
  (func $fmt_val (@pub) (param $val (ref any)) (result (ref $Str))

    ;; Try $Str — passthrough
    (block $not_str
      (block $is_str (result (ref $Str))
        (br $not_str
          (br_on_cast $is_str (ref any) (ref $Str)
            (local.get $val))))
      (return))

    ;; Try $Num — num.wat owns the inner Int / F64 / Decimal dispatch.
    (block $not_num
      (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref any) (ref $Num)
            (local.get $val))))
      (return_call $num_fmt))

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

    ;; Try $Rec — delegate to dict.wat:fmt.
    (block $not_rec
      (block $is_rec (result (ref $Rec))
        (br $not_rec
          (br_on_cast $is_rec (ref any) (ref $Rec)
            (local.get $val))))
      (return_call $rec_fmt))

    ;; Try $List — delegate to list.wat:fmt.
    (block $not_list
      (block $is_list (result (ref $List))
        (br $not_list
          (br_on_cast $is_list (ref any) (ref $List)
            (local.get $val))))
      (return_call $list_fmt))

    ;; Try $Set — delegate to set.wat:fmt.
    (block $not_set
      (block $is_set (result (ref $Set))
        (br $not_set
          (br_on_cast $is_set (ref any) (ref $Set)
            (local.get $val))))
      (return_call $set_fmt))

    ;; Try $Closure -- placeholder "<closure>" fmt.
    (block $not_clos
      (block $is_clos (result (ref $Closure))
        (br $not_clos
          (br_on_cast $is_clos (ref any) (ref $Closure)
            (local.get $val))))
      (return_call $closure_fmt))

    ;; Unknown type — unreachable for now.
    (unreachable)
  )

  ;; _str_fmt_i31 : i32 -> (ref $Str)
  ;; Format an i31ref value as a boolean: 0 → "false", 1 → "true".
  ;; i31ref is currently only used for booleans; integer i31 rendering
  ;; will be added when i31ref is used for small integers.
  (func $_str_fmt_i31 (@pub) (param $v i32) (result (ref $Str))

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



  ;; _str_from_ascii_2 : (i32, i32) -> (ref $Str)
  ;; Build a 2-byte string from ASCII code points.
  (func $_str_from_ascii_2 (@pub) (param $a i32) (param $b i32) (result (ref $Str))
    (local $buf (ref $ByteArray))
    (local.set $buf (array.new $ByteArray (i32.const 0) (i32.const 2)))
    (array.set $ByteArray (local.get $buf) (i32.const 0) (local.get $a))
    (array.set $ByteArray (local.get $buf) (i32.const 1) (local.get $b))
    (struct.new $StrBytesImpl (local.get $buf))
  )



  ;; CPS wrappers — stripped by unit test harness (prepare_wat).

  ;; str_fmt : (ctx, segments_any, cont) -> void
  ;; CPS string formatter. First arg is the caller's ctx (forwarded to
  ;; the cont). Second arg is a $VarArgs array of string segments.
  ;; Third arg is the continuation. Formats each segment via _str_fmt_val,
  ;; concatenates all results into a single $StrBytesImpl, and passes the
  ;; result to the continuation via _apply.
  ;;
  ;; This is the direct-call CPS shape used by lower.rs's StrFmt sym
  ;; path for template-string interpolation (`'foo ${x}'`). The
  ;; user-importable `{fmt} = import 'std/str.fnk'` path goes through
  ;; the closure accessor below ($fmt + $fmt_apply).
  ;;
  ;; TODO ctx: $ctx is forwarded to the cont but not consulted by
  ;; dispatch. Per-type fmt impls are monomorphic kernels today, but the
  ;; moment a user-defined `fmt` impl exists for a user type, this is
  ;; the boundary where ctx-scoped fmt dispatch must thread ctx into
  ;; the per-element fmt call.
  (func $_fmt_inner (@pub)
      (param $ctx (ref null any))  ;; TODO ctx: not consulted — see comment above
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
              (call $fmt_val
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
            (call $fmt_val
              (ref.as_non_null
                (array.get $VarArgs (local.get $segments) (local.get $i))))
            (local.get $dst)
            (local.get $pos)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $copy_loop)))

    ;; Wrap and pass to continuation.
    (return_call $apply_1
      (local.get $ctx)
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
      (ref.null any)
          (local.get $str)
          (local.get $fail))))

    (return_call $apply_1
      (ref.null any)
      (ref.as_non_null (local.get $result))
      (local.get $succ))
  )


  ;; str_op_dot(str, key, cont)
  ;; Member access on strings:
  ;;   $Range key → byte slice (start..end or start...end)
  ;;   $Num key   → single byte at index
  ;; Out of bounds → unreachable
  (func $op_dot (@impl "std/operators.fnk:op_dot" $Str _)
    (param $ctx (ref null any))
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

      ;; Open-end: end = string length (in bytes). Otherwise read $ival.
      (block $end_done
        (block $end_ref (result (ref $I64))
          (br_on_non_null $end_ref (call $range_end (local.get $range)))
          (local.set $end_i (i64.extend_i32_s
            (array.len (call $bytes (local.get $s)))))
          (br $end_done))
        (local.set $end_i (struct.get $I64 $ival))

        ;; Adjust end for inclusive range (only when end was set explicitly).
        (if (call $range_is_incl (local.get $range))
          (then
            (local.set $end_i (i64.add (local.get $end_i) (i64.const 1))))))

      (local.set $result
        (call $_str_slice (local.get $s) (local.get $start_i) (local.get $end_i)))
      (if (ref.is_null (local.get $result))
        (then (unreachable)))
      (return_call $apply_1
      (local.get $ctx)
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
      (local.get $ctx)
        (ref.as_non_null (local.get $result))
        (local.get $cont)))

    (unreachable)
  )


  ;; str_match(subj, prefix, suffix, fail, succ)
  ;; CPS string template pattern matching.
  ;; Checks subj starts with prefix and ends with suffix (non-overlapping).
  ;; On match: calls succ(middle_slice). On mismatch: calls fail().
  ;; ctx: $ctx is forwarded to fail/succ via apply_N at every tail-call
  ;; site, so the conts resume under the caller's universe. Dispatch
  ;; itself is purely byte-level — ctx is not consulted.
  (func $match (@impl "std/str.fnk:match")
      (param $ctx (ref null any))  ;; TODO ctx: not consulted — forwarded only
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
      (then (return_call $apply_0
      (local.get $ctx) (local.get $fail))))
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
      (then (return_call $apply_0
      (local.get $ctx) (local.get $fail))))

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
        (return_call $apply_0
      (local.get $ctx) (local.get $fail))))

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
        (return_call $apply_0
      (local.get $ctx) (local.get $fail))))

    ;; Both matched — slice the middle
    (if (i32.eqz (local.get $mid_len))
      (then (return_call $apply_1
      (local.get $ctx) (call $str_empty) (local.get $succ))))

    ;; Full string — no prefix or suffix
    (if (i32.eq (local.get $mid_len) (local.get $s_len))
      (then (return_call $apply_1
      (local.get $ctx) (local.get $s_str) (local.get $succ))))

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
      (local.get $ctx)
      (struct.new $StrBytesImpl (local.get $mid))
      (local.get $succ))
  )


  ;; ---- User-importable `fmt` closure -----------------------------------
  ;;
  ;; `{fmt} = import 'std/str.fnk'` binds the closure below; `fmt x`
  ;; dispatches it through apply_3 with the caller's ctx. The closure
  ;; body peels (cont, val) off the args list, runs fmt_val on the value,
  ;; and resumes the cont under the caller's ctx via apply_1.
  ;;
  ;; This is independent of the template-string `'${x}'` path, which
  ;; goes through Sym::StrFmt → $_fmt_inner direct-CPS call (see above).

  (elem declare func $fmt_apply)

  (func $fmt_apply (type $Fn3)
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
      (call $fmt_val (ref.as_non_null (local.get $val)))
      (local.get $cont)))

  (global $fmt_closure (ref $Closure)
    (struct.new $Closure
      (ref.func $fmt_apply)
      (ref.null $Captures)))

  (func $fmt (@pub) (@impl "std/str.fnk:fmt") (result (ref any))
    (global.get $fmt_closure))


  ;; Closure repr -- prints "<closure>". Useful when a list/rec/etc.
  ;; contains a fn value and we want to format the container without
  ;; tripping a missing-impl trap. Doesn't reveal anything about the
  ;; closure (the funcref / captures are opaque to fink).
  (func $closure_repr (@pub) (@impl "std/repr.fnk:repr" $Closure)
    (param $_clos (ref $Closure)) (result (ref $Str))
    (local $buf (ref $ByteArray))
    (local.set $buf (array.new $ByteArray (i32.const 0) (i32.const 9)))
    (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x3C)) ;; <
    (array.set $ByteArray (local.get $buf) (i32.const 1) (i32.const 0x63)) ;; c
    (array.set $ByteArray (local.get $buf) (i32.const 2) (i32.const 0x6C)) ;; l
    (array.set $ByteArray (local.get $buf) (i32.const 3) (i32.const 0x6F)) ;; o
    (array.set $ByteArray (local.get $buf) (i32.const 4) (i32.const 0x73)) ;; s
    (array.set $ByteArray (local.get $buf) (i32.const 5) (i32.const 0x75)) ;; u
    (array.set $ByteArray (local.get $buf) (i32.const 6) (i32.const 0x72)) ;; r
    (array.set $ByteArray (local.get $buf) (i32.const 7) (i32.const 0x65)) ;; e
    (array.set $ByteArray (local.get $buf) (i32.const 8) (i32.const 0x3E)) ;; >
    (return_call $from_bytes (local.get $buf)))

  ;; Closure fmt -- same as repr.
  (func $closure_fmt (@pub) (@impl "std/str.fnk:fmt" $Closure)
    (param $clos (ref $Closure)) (result (ref $Str))
    (return_call $closure_repr (local.get $clos)))

)
