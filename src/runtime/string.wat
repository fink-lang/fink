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
;;   ├── $StrDataImpl  (sub $Str)       ← (offset, length) into data section
;;   └── $StrBytesImpl (sub $Str)       ← heap byte array
;;
;; Escape sequences are resolved at compile time — the runtime only sees
;; cooked UTF-8 bytes. The two string subtypes differ only in storage:
;;   $StrDataImpl — points into the WASM data section (compiler-emitted literals)
;;   $StrBytesImpl — heap-allocated byte array (runtime-constructed, e.g. concat)

(module
  ;; Continuation dispatch: $apply_1 (defined in list.wat) wraps a single
  ;; result in a list and tail-calls $_croc (defined in dispatch.wat).

  ;; ---- Internal types (not visible to user code) ----

  ;; Byte array: UTF-8 bytes. Used by $StrBytesImpl.
  ;; Mutable at the WASM level for construction (array.set during escape
  ;; processing), but treated as immutable once wrapped in a $Str* struct.
  (type $ByteArray (array (mut i8)))

  ;; $StrDataImpl — data section string (offset, length into linear memory).
  (type $StrDataImpl (sub $Str (struct
    (field $offset i32)
    (field $length i32))))

  ;; $StrBytesImpl — heap-allocated string (byte array).
  (type $StrBytesImpl (sub $Str (struct
    (field $bytes (ref $ByteArray)))))


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

  ;; ---- Equality ----

  ;; str_eq : (ref $Str), (ref $Str) -> i32
  ;; Compare two byte-bearing strings for byte-level equality.
  ;; Fast path: ref.eq (same object → 1).
  ;; Slow path: dispatch on concrete types, compare byte-by-byte.
  ;; Returns 1 if equal, 0 if not.
  (func $str_eq (export "str_eq")
    (param $a (ref $Str))
    (param $b (ref $Str))
    (result i32)

    (local $da (ref $StrDataImpl))
    (local $db (ref $StrDataImpl))

    ;; Fast path: same object
    (if (ref.eq (local.get $a) (local.get $b))
      (then (return (i32.const 1))))

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
        (return (call $_str_eq_dd
          (struct.get $StrDataImpl $offset (local.get $da))
          (struct.get $StrDataImpl $length (local.get $da))
          (struct.get $StrDataImpl $offset (local.get $db))
          (struct.get $StrDataImpl $length (local.get $db)))))

      ;; $a is data, $b is array
      (return (call $_str_eq_da
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
      (return (call $_str_eq_da
        (struct.get $StrDataImpl $offset (local.get $db))
        (struct.get $StrDataImpl $length (local.get $db))
        (call $_get_byte_array (local.get $a)))))

    ;; Both arrays
    (call $_str_eq_aa
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

  ;; $_str_eq_dd : (i32, i32, i32, i32) -> i32
  ;; Compare two data-section strings by linear memory reads.
  (func $_str_eq_dd
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

  ;; $_str_eq_da : (i32, i32, ref $ByteArray) -> i32
  ;; Compare data-section string vs heap array.
  (func $_str_eq_da
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

  ;; $_str_eq_aa : (ref $ByteArray, ref $ByteArray) -> i32
  ;; Compare two heap arrays byte-by-byte.
  (func $_str_eq_aa
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
  (func $str_render_escape (export "str_render_escape")
    (param $raw (ref $Str))
    (result (ref $Str))

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
  (func $str_render_unescape (export "str_render_unescape")
    (param $str (ref $Str))
    (result (ref $Str))

    (local $src (ref $ByteArray))
    (local $src_len i32)
    (local $i i32)
    (local $out_len i32)
    (local $out (ref $ByteArray))
    (local $j i32)
    (local $byte i32)

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
  (func $str_hash_i31 (export "str_hash_i31")
    (param $str (ref $Str))
    (result i32)

    (local $data (ref $StrDataImpl))

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

    ;; Unknown type — unreachable for now.
    (unreachable)
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
  ;; result to the continuation via _croc.
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
    (return_call $apply_1
      (struct.new $StrBytesImpl (local.get $dst))
      (local.get $cont))
  )

)
