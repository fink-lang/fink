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
  ;; Continuation dispatch: $std/list.wat:apply_1 (defined in list.wat) wraps a single
  ;; result in a list and tail-calls $_apply (defined in dispatch.wat).

  ;; ---- Internal types (not visible to user code) ----

  ;; Byte array: UTF-8 bytes. Used by $StrBytesImpl.
  ;; Mutable at the WASM level for construction (array.set during escape
  ;; processing), but treated as immutable once wrapped in a $Str* struct.
  (type $ByteArray (array (mut i8)))

  ;; $StrEmpty — singleton empty string. No fields, no storage.
  (type $StrEmpty (sub $Str (struct)))

  ;; $StrDataImpl — data section string (offset, length into linear memory).
  (type $StrDataImpl (sub $Str (struct
    (field $offset i32)
    (field $length i32))))

  ;; $StrBytesImpl — heap-allocated string (byte array).
  (type $StrBytesImpl (sub $Str (struct
    (field $bytes (ref $ByteArray)))))


  ;; ---- Singleton empty string ----

  (global $str_empty (ref $StrEmpty) (struct.new $StrEmpty))
  (func $str_empty (export "str_empty") (result (ref $Str))
    (global.get $str_empty))

  ;; ---- Construction (compiler-emitted) ----

  ;; str : (i32, i32) -> (ref $StrDataImpl)
  ;; Wrap a data-section pointer into a string value.
  (func $str (export "str")
    (param $offset i32)
    (param $length i32)
    (result (ref $StrDataImpl))

    (struct.new $StrDataImpl
      (local.get $offset)
      (local.get $length))
  )

  ;; _str_wrap_bytes : (ref $ByteArray) -> (ref $Str)
  ;; Wrap a GC byte array into a $StrBytesImpl.
  ;; Used by the host to create strings from IO data without
  ;; touching linear memory — the host creates the $ByteArray
  ;; directly via the GC API.
  (func $_str_wrap_bytes (export "_str_wrap_bytes")
    (param $bytes (ref null any))
    (result (ref any))

    (if (i32.eqz (array.len (ref.cast (ref $ByteArray) (local.get $bytes))))
      (then (return (global.get $str_empty))))
    (struct.new $StrBytesImpl
      (ref.cast (ref $ByteArray) (local.get $bytes)))
  )


  ;; ---- Access ----

  ;; str_bytes : (ref $Str) -> (ref $ByteArray)
  ;; Get the byte content of a string.
  ;; Dispatches via br_on_cast:
  ;;   $StrDataImpl  → copies bytes from data section into a $ByteArray
  ;;   $StrBytesImpl → returns the existing $ByteArray
  (func $str_bytes (export "str_bytes")
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
  (func $str_repr (export "str_repr")
    (param $str (ref $Str))
    (result (ref $Str))

    (local $src (ref $ByteArray))
    (local $len i32)
    (local $i i32)
    (local $b i32)
    (local $total i32)
    (local $buf (ref $ByteArray))
    (local $pos i32)

    (local.set $src (call $str_bytes (local.get $str)))
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
  (func $str_op_eq
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
  (func $str_render_escape
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
  (func $str_render_unescape
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
  (func $str_hash_i31
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

    ;; Try $Num — format f64
    (block $not_num
      (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref any) (ref $Num)
            (local.get $val))))
      (return (call $_str_fmt_num (struct.get $Num $val))))

    ;; Try i31ref — bool or small int
    (block $not_i31
      (block $is_i31 (result (ref i31))
        (br $not_i31
          (br_on_cast $is_i31 (ref any) (ref i31)
            (local.get $val))))
      (return (call $_str_fmt_i31 (i31.get_s))))

    ;; Try $Range — format as "start..end" or "start...end"
    (block $not_range
      (block $is_range (result (ref $Range))
        (br $not_range
          (br_on_cast $is_range (ref any) (ref $Range)
            (local.get $val))))
      (return (call $_str_fmt_range)))

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
      (return (call $str_repr)))

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

  ;; _str_fmt_num : f64 -> (ref $Str)
  ;; Format an f64 as a string. Handles NaN, ±Infinity, integers, and floats.
  (func $_str_fmt_num (param $v f64) (result (ref $Str))

    (local $i64v i64)

    ;; NaN check — f64.ne with itself is true only for NaN.
    (if (f64.ne (local.get $v) (local.get $v))
      (then (return (call $_str_from_ascii_3 (i32.const 0x4E) (i32.const 0x61) (i32.const 0x4E))))) ;; "NaN"

    ;; +Infinity
    (if (f64.eq (local.get $v) (f64.const inf))
      (then (return (call $_str_from_ascii_8
        (i32.const 0x49) (i32.const 0x6E) (i32.const 0x66) (i32.const 0x69)
        (i32.const 0x6E) (i32.const 0x69) (i32.const 0x74) (i32.const 0x79))))) ;; "Infinity"

    ;; -Infinity
    (if (f64.eq (local.get $v) (f64.const -inf))
      (then (return (call $_str_from_ascii_9
        (i32.const 0x2D)  ;; "-"
        (i32.const 0x49) (i32.const 0x6E) (i32.const 0x66) (i32.const 0x69)
        (i32.const 0x6E) (i32.const 0x69) (i32.const 0x74) (i32.const 0x79))))) ;; "-Infinity"

    ;; If the value is an integer that fits in i32, render as integer.
    (if (f64.eq (local.get $v) (f64.trunc (local.get $v)))
      (then
        (local.set $i64v (i64.trunc_sat_f64_s (local.get $v)))
        (if (i32.and
              (i64.le_s (local.get $i64v) (i64.const 2147483647))
              (i64.ge_s (local.get $i64v) (i64.const -2147483648)))
          (then
            (return (call $_str_fmt_int
              (i32.wrap_i64 (local.get $i64v))))))))

    ;; Non-integer float.
    (call $_str_fmt_float (local.get $v))
  )

  ;; _str_fmt_int : i32 -> (ref $Str)
  ;; Format a signed i32 as a decimal string.
  (func $_str_fmt_int (param $v i32) (result (ref $Str))

    (local $neg i32)
    (local $abs i32)
    (local $digits i32)
    (local $tmp i32)
    (local $buf (ref $ByteArray))
    (local $pos i32)

    ;; Zero special case.
    (if (i32.eqz (local.get $v))
      (then
        (local.set $buf (array.new $ByteArray (i32.const 0) (i32.const 1)))
        (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x30))
        (return (struct.new $StrBytesImpl (local.get $buf)))))

    ;; Handle sign.
    (local.set $neg (i32.lt_s (local.get $v) (i32.const 0)))
    (if (local.get $neg)
      (then (local.set $abs (i32.sub (i32.const 0) (local.get $v))))
      (else (local.set $abs (local.get $v))))

    ;; Count digits.
    (local.set $digits (i32.const 0))
    (local.set $tmp (local.get $abs))
    (block $done
      (loop $count
        (local.set $digits (i32.add (local.get $digits) (i32.const 1)))
        (local.set $tmp (i32.div_u (local.get $tmp) (i32.const 10)))
        (br_if $count (local.get $tmp))
      ))

    ;; Allocate buffer.
    (local.set $buf
      (array.new $ByteArray (i32.const 0)
        (i32.add (local.get $digits) (local.get $neg))))

    ;; Write '-' if negative.
    (if (local.get $neg)
      (then (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x2D))))

    ;; Write digits right-to-left.
    (local.set $pos
      (i32.sub
        (i32.add (local.get $digits) (local.get $neg))
        (i32.const 1)))
    (local.set $tmp (local.get $abs))
    (block $done
      (loop $write
        (array.set $ByteArray (local.get $buf) (local.get $pos)
          (i32.add (i32.const 0x30) (i32.rem_u (local.get $tmp) (i32.const 10))))
        (local.set $tmp (i32.div_u (local.get $tmp) (i32.const 10)))
        (local.set $pos (i32.sub (local.get $pos) (i32.const 1)))
        (br_if $write (local.get $tmp))
      ))

    (struct.new $StrBytesImpl (local.get $buf))
  )

  ;; _str_fmt_float : f64 -> (ref $Str)
  ;; Format a non-integer f64 as "int_part.frac_part" with trailing zeros stripped.
  ;; Simple approach: multiply fractional part by 1e15, render as i64, strip trailing '0's.
  (func $_str_fmt_float (param $v f64) (result (ref $Str))

    (local $neg i32)
    (local $abs f64)
    (local $int_part f64)
    (local $frac f64)
    (local $frac_i64 i64)
    (local $int_buf (ref $ByteArray))
    (local $int_len i32)
    (local $frac_buf (ref $ByteArray))
    (local $frac_digits i32)
    (local $frac_len i32)
    (local $tmp i64)
    (local $buf (ref $ByteArray))
    (local $total i32)
    (local $pos i32)
    (local $i i32)

    ;; Handle sign.
    (local.set $neg (f64.lt (local.get $v) (f64.const 0)))
    (if (local.get $neg)
      (then (local.set $abs (f64.neg (local.get $v))))
      (else (local.set $abs (local.get $v))))

    ;; Split into integer and fractional parts.
    (local.set $int_part (f64.trunc (local.get $abs)))
    (local.set $frac (f64.sub (local.get $abs) (local.get $int_part)))

    ;; Render integer part as i64 digits into a temporary buffer.
    ;; Max i64 digits = 20, allocate 20.
    (local.set $int_buf (array.new $ByteArray (i32.const 0) (i32.const 20)))
    (local.set $int_len (i32.const 0))
    (block $int_zero
      (local.set $tmp (i64.trunc_sat_f64_u (local.get $int_part)))
      (if (i64.eqz (local.get $tmp))
        (then
          ;; Integer part is 0 — write single '0'.
          (array.set $ByteArray (local.get $int_buf) (i32.const 0) (i32.const 0x30))
          (local.set $int_len (i32.const 1))
          (br $int_zero)))
      ;; Write digits right-to-left into int_buf, then we'll reverse.
      (loop $iloop
        (array.set $ByteArray (local.get $int_buf) (local.get $int_len)
          (i32.add (i32.const 0x30) (i32.wrap_i64 (i64.rem_u (local.get $tmp) (i64.const 10)))))
        (local.set $int_len (i32.add (local.get $int_len) (i32.const 1)))
        (local.set $tmp (i64.div_u (local.get $tmp) (i64.const 10)))
        (br_if $iloop (i64.ne (local.get $tmp) (i64.const 0)))))

    ;; Render fractional part: multiply by 1e15, convert to i64, render digits.
    ;; 15 digits is within i64 range and covers f64 precision.
    (local.set $frac_i64 (i64.trunc_sat_f64_u
      (f64.add
        (f64.mul (local.get $frac) (f64.const 1e15))
        (f64.const 0.5)))) ;; round

    ;; Render all 15 digits (with leading zeros) then strip trailing zeros.
    (local.set $frac_buf (array.new $ByteArray (i32.const 0) (i32.const 15)))
    (local.set $frac_digits (i32.const 15))
    (local.set $tmp (local.get $frac_i64))
    ;; Write right-to-left.
    (local.set $i (i32.const 14))
    (loop $floop
      (array.set $ByteArray (local.get $frac_buf) (local.get $i)
        (i32.add (i32.const 0x30) (i32.wrap_i64 (i64.rem_u (local.get $tmp) (i64.const 10)))))
      (local.set $tmp (i64.div_u (local.get $tmp) (i64.const 10)))
      (if (local.get $i)
        (then
          (local.set $i (i32.sub (local.get $i) (i32.const 1)))
          (br $floop))))

    ;; Strip trailing '0's from frac_buf, but keep at least 1 digit.
    (local.set $frac_len (local.get $frac_digits))
    (loop $strip
      (if (i32.and
            (i32.gt_s (local.get $frac_len) (i32.const 1))
            (i32.eq
              (array.get_u $ByteArray (local.get $frac_buf)
                (i32.sub (local.get $frac_len) (i32.const 1)))
              (i32.const 0x30)))
        (then
          (local.set $frac_len (i32.sub (local.get $frac_len) (i32.const 1)))
          (br $strip))))

    ;; Assemble: ['-'] int_digits '.' frac_digits
    ;; int_buf has digits in reverse order (except if single '0').
    (local.set $total (i32.add
      (i32.add (local.get $neg) (local.get $int_len))
      (i32.add (i32.const 1) (local.get $frac_len)))) ;; +1 for '.'

    (local.set $buf (array.new $ByteArray (i32.const 0) (local.get $total)))
    (local.set $pos (i32.const 0))

    ;; Write '-' if negative.
    (if (local.get $neg)
      (then
        (array.set $ByteArray (local.get $buf) (i32.const 0) (i32.const 0x2D))
        (local.set $pos (i32.const 1))))

    ;; Write integer digits (int_buf is in reverse order, write backwards).
    (local.set $i (i32.sub (local.get $int_len) (i32.const 1)))
    (loop $wcopy
      (array.set $ByteArray (local.get $buf) (local.get $pos)
        (array.get_u $ByteArray (local.get $int_buf) (local.get $i)))
      (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
      (if (local.get $i)
        (then
          (local.set $i (i32.sub (local.get $i) (i32.const 1)))
          (br $wcopy))))

    ;; Write '.'.
    (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x2E))
    (local.set $pos (i32.add (local.get $pos) (i32.const 1)))

    ;; Write fractional digits (frac_buf is in correct order).
    (local.set $i (i32.const 0))
    (loop $fcopy
      (array.set $ByteArray (local.get $buf) (local.get $pos)
        (array.get_u $ByteArray (local.get $frac_buf) (local.get $i)))
      (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $fcopy (i32.lt_u (local.get $i) (local.get $frac_len))))

    (struct.new $StrBytesImpl (local.get $buf))
  )

  ;; _str_fmt_range : (ref $Range) -> (ref $Str)
  ;; Format a range as "start..end" (exclusive) or "start...end" (inclusive).
  (func $_str_fmt_range (param $range (ref $Range)) (result (ref $Str))
    (local $impl (ref $RangeImpl))
    (local $start_str (ref $Str))
    (local $end_str (ref $Str))
    (local $start_bytes (ref $ByteArray))
    (local $end_bytes (ref $ByteArray))
    (local $start_len i32)
    (local $end_len i32)
    (local $dot_len i32)  ;; 2 for "..", 3 for "..."
    (local $total i32)
    (local $buf (ref $ByteArray))
    (local $pos i32)
    (local $i i32)

    ;; Downcast to $RangeImpl.
    (local.set $impl (ref.cast (ref $RangeImpl) (local.get $range)))

    ;; Format start and end numbers.
    (local.set $start_str
      (call $_str_fmt_num (struct.get $Num $val
        (struct.get $RangeImpl $start (local.get $impl)))))
    (local.set $end_str
      (call $_str_fmt_num (struct.get $Num $val
        (struct.get $RangeImpl $end (local.get $impl)))))

    ;; Get byte arrays.
    (local.set $start_bytes (call $str_bytes (local.get $start_str)))
    (local.set $end_bytes (call $str_bytes (local.get $end_str)))
    (local.set $start_len (array.len (local.get $start_bytes)))
    (local.set $end_len (array.len (local.get $end_bytes)))

    ;; Dot count: 2 for exclusive, 3 for inclusive.
    (local.set $dot_len
      (if (result i32) (struct.get $RangeImpl $incl (local.get $impl))
        (then (i32.const 3))
        (else (i32.const 2))))

    ;; Allocate result buffer.
    (local.set $total
      (i32.add (i32.add (local.get $start_len) (local.get $dot_len))
        (local.get $end_len)))
    (local.set $buf (array.new $ByteArray (i32.const 0) (local.get $total)))

    ;; Copy start bytes.
    (local.set $pos (i32.const 0))
    (local.set $i (i32.const 0))
    (block $s_done (loop $s_copy
      (br_if $s_done (i32.ge_u (local.get $i) (local.get $start_len)))
      (array.set $ByteArray (local.get $buf) (local.get $pos)
        (array.get_u $ByteArray (local.get $start_bytes) (local.get $i)))
      (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $s_copy)))

    ;; Write dots: 0x2E = '.'
    (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x2E))
    (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
    (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x2E))
    (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
    (if (i32.eq (local.get $dot_len) (i32.const 3))
      (then
        (array.set $ByteArray (local.get $buf) (local.get $pos) (i32.const 0x2E))
        (local.set $pos (i32.add (local.get $pos) (i32.const 1)))))

    ;; Copy end bytes.
    (local.set $i (i32.const 0))
    (block $e_done (loop $e_copy
      (br_if $e_done (i32.ge_u (local.get $i) (local.get $end_len)))
      (array.set $ByteArray (local.get $buf) (local.get $pos)
        (array.get_u $ByteArray (local.get $end_bytes) (local.get $i)))
      (local.set $pos (i32.add (local.get $pos) (i32.const 1)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $e_copy)))

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

    (local.set $bytes (call $str_bytes (local.get $str)))
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
          (else (call $_str_len (call $str_repr (local.get $str)))))))

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
      (call $std/rec.wat:_hamt_size_node (local.get $node)))

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
              (call $str_repr (local.get $str))
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
                (call $std/rec.wat:_hamt_size_node
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
    (if (call $std/list.wat:list_op_empty (local.get $list))
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
          (call $std/list.wat:list_op_empty (local.get $cur)))
        (local.set $total
          (i32.add (local.get $total)
            (call $_str_len
              (call $_str_fmt_val_repr
                (ref.as_non_null
                  (call $std/list.wat:list_head_any (local.get $cur)))))))
        (local.set $count (i32.add (local.get $count) (i32.const 1)))
        (local.set $cur
          (call $std/list.wat:list_tail_any (local.get $cur)))
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
          (call $std/list.wat:list_op_empty (local.get $cur)))

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
                (call $std/list.wat:list_head_any (local.get $cur))))
            (local.get $buf)
            (local.get $pos)))

        (local.set $cur
          (call $std/list.wat:list_tail_any (local.get $cur)))
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

  ;; _str_from_ascii_3 : (i32, i32, i32) -> (ref $Str)
  ;; Build a 3-byte string from ASCII code points.
  (func $_str_from_ascii_3 (param $a i32) (param $b i32) (param $c i32) (result (ref $Str))
    (local $buf (ref $ByteArray))
    (local.set $buf (array.new $ByteArray (i32.const 0) (i32.const 3)))
    (array.set $ByteArray (local.get $buf) (i32.const 0) (local.get $a))
    (array.set $ByteArray (local.get $buf) (i32.const 1) (local.get $b))
    (array.set $ByteArray (local.get $buf) (i32.const 2) (local.get $c))
    (struct.new $StrBytesImpl (local.get $buf))
  )

  ;; _str_from_ascii_8 : 8 bytes -> (ref $Str)
  (func $_str_from_ascii_8
    (param $a i32) (param $b i32) (param $c i32) (param $d i32)
    (param $e i32) (param $f i32) (param $g i32) (param $h i32)
    (result (ref $Str))
    (local $buf (ref $ByteArray))
    (local.set $buf (array.new $ByteArray (i32.const 0) (i32.const 8)))
    (array.set $ByteArray (local.get $buf) (i32.const 0) (local.get $a))
    (array.set $ByteArray (local.get $buf) (i32.const 1) (local.get $b))
    (array.set $ByteArray (local.get $buf) (i32.const 2) (local.get $c))
    (array.set $ByteArray (local.get $buf) (i32.const 3) (local.get $d))
    (array.set $ByteArray (local.get $buf) (i32.const 4) (local.get $e))
    (array.set $ByteArray (local.get $buf) (i32.const 5) (local.get $f))
    (array.set $ByteArray (local.get $buf) (i32.const 6) (local.get $g))
    (array.set $ByteArray (local.get $buf) (i32.const 7) (local.get $h))
    (struct.new $StrBytesImpl (local.get $buf))
  )

  ;; _str_from_ascii_9 : 9 bytes -> (ref $Str)
  (func $_str_from_ascii_9
    (param $a i32) (param $b i32) (param $c i32) (param $d i32)
    (param $e i32) (param $f i32) (param $g i32) (param $h i32)
    (param $i i32)
    (result (ref $Str))
    (local $buf (ref $ByteArray))
    (local.set $buf (array.new $ByteArray (i32.const 0) (i32.const 9)))
    (array.set $ByteArray (local.get $buf) (i32.const 0) (local.get $a))
    (array.set $ByteArray (local.get $buf) (i32.const 1) (local.get $b))
    (array.set $ByteArray (local.get $buf) (i32.const 2) (local.get $c))
    (array.set $ByteArray (local.get $buf) (i32.const 3) (local.get $d))
    (array.set $ByteArray (local.get $buf) (i32.const 4) (local.get $e))
    (array.set $ByteArray (local.get $buf) (i32.const 5) (local.get $f))
    (array.set $ByteArray (local.get $buf) (i32.const 6) (local.get $g))
    (array.set $ByteArray (local.get $buf) (i32.const 7) (local.get $h))
    (array.set $ByteArray (local.get $buf) (i32.const 8) (local.get $i))
    (struct.new $StrBytesImpl (local.get $buf))
  )


  ;; CPS wrappers — stripped by unit test harness (prepare_wat).

  ;; str_fmt : (ref null any, ref null any) -> void
  ;; CPS string formatter. First arg is a $VarArgs array of string segments,
  ;; second arg is the continuation. Formats each segment via _str_fmt_val,
  ;; concatenates all results into a single $StrBytesImpl, and passes the
  ;; result to the continuation via _apply.
  (func $str_fmt (export "str_fmt")
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
    (return_call $std/list.wat:apply_1
      (struct.new $StrBytesImpl (local.get $dst))
      (local.get $cont))
  )


  ;; ---- String slicing ----

  ;; _str_slice(str, start_f, end_f) → (ref null $Str)
  ;; Internal direct-style slice: extract bytes [start..end).
  ;; Negative values resolve from end: -1 → len-1, -0.0 → len.
  ;; Returns null on out-of-bounds (after resolution: 0 <= start <= end <= len).
  ;; Returns empty string for empty slice, original for full range.
  (func $_str_slice
    (param $str (ref $Str)) (param $start_f f64) (param $end_f f64)
    (result (ref null $Str))

    (local $len i32)
    (local $start i32)
    (local $end i32)
    (local $src (ref $ByteArray))
    (local $slice_len i32)
    (local $dst (ref $ByteArray))
    (local $i i32)

    (local.set $len (call $_str_len (local.get $str)))

    ;; Resolve negative values from end (including -0.0 via copysign check)
    (if (f64.lt
          (f64.copysign (f64.const 1) (local.get $start_f))
          (f64.const 0))
      (then
        (local.set $start
          (i32.add (local.get $len)
            (i32.trunc_f64_s (local.get $start_f)))))
      (else
        (local.set $start
          (i32.trunc_f64_s (local.get $start_f)))))

    (if (f64.lt
          (f64.copysign (f64.const 1) (local.get $end_f))
          (f64.const 0))
      (then
        (local.set $end
          (i32.add (local.get $len)
            (i32.trunc_f64_s (local.get $end_f)))))
      (else
        (local.set $end
          (i32.trunc_f64_s (local.get $end_f)))))

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
    (local.set $src (call $str_bytes (local.get $str)))
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
  (func $str_slice (export "str_slice")
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
      (struct.get $Num 0 (ref.cast (ref $Num) (local.get $start)))
      (struct.get $Num 0 (ref.cast (ref $Num) (local.get $end)))))

    (if (ref.is_null (local.get $result))
      (then
        (return_call $std/list.wat:apply_1
          (local.get $str)
          (local.get $fail))))

    (return_call $std/list.wat:apply_1
      (ref.as_non_null (local.get $result))
      (local.get $succ))
  )


  ;; str_op_dot(str, key, cont)
  ;; Member access on strings:
  ;;   $Range key → byte slice (start..end or start...end)
  ;;   $Num key   → single byte at index
  ;; Out of bounds → unreachable
  (func $str_op_dot (export "str_op_dot")
    (param $str (ref null any)) (param $key (ref null any)) (param $cont (ref null any))

    (local $s (ref $Str))
    (local $range (ref $Range))
    (local $start_f f64)
    (local $end_f f64)
    (local $result (ref null $Str))

    (local.set $s (ref.cast (ref $Str) (local.get $str)))

    ;; Try $Range key
    (block $not_range
      (block $is_range (result (ref $Range))
        (br $not_range
          (br_on_cast $is_range (ref null any) (ref $Range)
            (local.get $key))))
      (local.set $range)

      (local.set $start_f (struct.get $Num 0 (call $range_start (local.get $range))))
      (local.set $end_f (struct.get $Num 0 (call $range_end (local.get $range))))

      ;; Adjust end for inclusive range
      (if (call $range_is_incl (local.get $range))
        (then
          (local.set $end_f (f64.add (local.get $end_f) (f64.const 1)))))

      (local.set $result
        (call $_str_slice (local.get $s) (local.get $start_f) (local.get $end_f)))
      (if (ref.is_null (local.get $result))
        (then (unreachable)))
      (return_call $std/list.wat:apply_1
        (ref.as_non_null (local.get $result))
        (local.get $cont)))

    ;; Try $Num key — single byte
    (block $not_num
      (block $is_num (result (ref $Num))
        (br $not_num
          (br_on_cast $is_num (ref null any) (ref $Num)
            (local.get $key))))
      (local.set $start_f (struct.get $Num 0))

      (local.set $result
        (call $_str_slice
          (local.get $s)
          (local.get $start_f)
          (f64.add (local.get $start_f) (f64.const 1))))
      (if (ref.is_null (local.get $result))
        (then (unreachable)))
      (return_call $std/list.wat:apply_1
        (ref.as_non_null (local.get $result))
        (local.get $cont)))

    (unreachable)
  )


  ;; str_match(subj, prefix, suffix, fail, succ)
  ;; CPS string template pattern matching.
  ;; Checks subj starts with prefix and ends with suffix (non-overlapping).
  ;; On match: calls succ(middle_slice). On mismatch: calls fail().
  (func $str_match (export "str_match")
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
      (then (return_call $std/list.wat:apply_0 (local.get $fail))))
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
      (then (return_call $std/list.wat:apply_0 (local.get $fail))))

    ;; Get byte arrays
    (local.set $s_bytes (call $str_bytes (local.get $s_str)))

    ;; Check prefix match
    (if (local.get $p_len)
      (then
        (local.set $p_bytes (call $str_bytes (local.get $p_str)))
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
        (return_call $std/list.wat:apply_0 (local.get $fail))))

    ;; Check suffix match
    (local.set $mid_start (local.get $p_len))
    (local.set $mid_len (i32.sub (local.get $s_len)
                          (i32.add (local.get $p_len) (local.get $x_len))))
    (if (local.get $x_len)
      (then
        (local.set $x_bytes (call $str_bytes (local.get $x_str)))
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
        (return_call $std/list.wat:apply_0 (local.get $fail))))

    ;; Both matched — slice the middle
    (if (i32.eqz (local.get $mid_len))
      (then (return_call $std/list.wat:apply_1 (call $str_empty) (local.get $succ))))

    ;; Full string — no prefix or suffix
    (if (i32.eq (local.get $mid_len) (local.get $s_len))
      (then (return_call $std/list.wat:apply_1 (local.get $s_str) (local.get $succ))))

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
    (return_call $std/list.wat:apply_1
      (struct.new $StrBytesImpl (local.get $mid))
      (local.get $succ))
  )

)
