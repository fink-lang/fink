;; String — runtime support for fink's three-tier immutable strings
;;
;; This module provides direct-style primitives for string construction
;; and access. These are used by the compiler (emitted code) and by the
;; std-lib (which wraps them in CPS functions exposed to fink code).
;;
;; No CPS, no user-facing functions — those live in the std-lib.
;;
;;
;; Type hierarchy (types.wat defines the opaque base types):
;;
;;   $Str
;;   ├── $StrTempl                         ← opaque template
;;   │     $StrTemplImpl (sub $StrTempl)   ← internal: segment array
;;   └── $StrVal                           ← opaque byte-bearing string
;;       ├── $StrRaw                       ← escapes unresolved
;;       │     $StrDataImpl (sub $StrRaw)  ← internal: (offset, length) into data section
;;       │     $StrRawBytesImpl (sub $StrRaw) ← internal: heap byte array (from raw'')
;;       └── $StrBytes                     ← escapes resolved
;;             $StrBytesImpl (sub $StrBytes) ← internal: heap byte array (from fmt'')
;;
;;
;; Templates ($StrTempl):
;;   The fink "string" value. A list of segments where each segment is
;;   either a $StrRaw ref, a $StrBytes ref, or an arbitrary value (ref any).
;;   'hello ${name}' → template with segments: [StrData "hello ", value name].
;;   A plain string 'hello' is a template with a single StrData segment.
;;   First-class, immutable, lazy — formatting deferred to std-lib.
;;   Equality is structural via deep_eq (std-lib, CPS).
;;
;; Raw ($StrRaw, subtypes $StrDataImpl / $StrRawBytesImpl):
;;   Bytes with escapes NOT resolved. \n stored as two bytes ('\', 'n').
;;   $StrDataImpl — (offset, length) pointing into the WASM data section.
;;     Produced by compiler for string literals.
;;   $StrRawBytesImpl — heap byte array.
;;     Produced by raw'' formatter (copies data section, preserves escapes).
;;
;; Bytes ($StrBytes, subtype $StrBytesImpl):
;;   Flat UTF-8 encoded bytes — escapes resolved. \n is byte 0x0A.
;;   Produced by fmt'' formatter. Heap-allocated (GC array of bytes).
;;
;;
;; Formatting (std-lib, not here):
;;   Two built-in formatters in the std-lib:
;;     - fmt : resolve escapes in $StrRaw segments, stringify values → $StrBytes
;;     - raw : no escape processing, copy $StrRaw as-is, stringify values → $StrRaw
;;   Tagged formatters (html'...', sql'...') are user-level fink functions.
;;   All formatters are CPS — they dispatch through protocols to stringify
;;   values. This module provides direct helpers they build on.
;;
;;
;; Boundary: runtime vs std-lib
;;   Runtime (this file): direct-style, no user code callbacks, no lazy
;;   values. Construction, access, byte processing.
;;   Std-lib: CPS functions exposed to fink code. Formatters, equality,
;;   anything that touches lazy values or dispatches through protocols.
;;   The runtime may provide efficient direct helpers to support std-lib
;;   implementations (e.g. escape sequence processing).

(import "@fink/runtime/types" "*" (func (param anyref)))


(module
  ;; ---- Internal types (not visible to user code) ----

  ;; Segment array: interleaved $StrRaw/$StrBytes refs and arbitrary values.
  ;; Stored as (ref any) — raws, bytes, values, and nested templates all fit.
  (type $StrSegments (array (ref any)))

  ;; Byte array: UTF-8 bytes. Shared by $StrRawBytesImpl and $StrBytesImpl.
  ;; Mutable at the WASM level for construction (array.set during escape
  ;; processing), but treated as immutable once wrapped in a $Str* struct.
  (type $ByteArray (array (mut i8)))

  ;; $StrDataImpl — internal layout for $StrRaw (data section variant).
  ;; offset = byte offset into linear memory (data section)
  ;; length = byte count
  (type $StrDataImpl (sub $StrRaw (struct
    (field $offset i32)
    (field $length i32))))

  ;; $StrRawBytesImpl — internal layout for $StrRaw (heap variant).
  ;; Produced by raw'' — escapes preserved in heap byte array.
  (type $StrRawBytesImpl (sub $StrRaw (struct
    (field $bytes (ref $ByteArray)))))

  ;; $StrTemplImpl — internal layout for $StrTempl.
  ;; Holds the segment array.
  (type $StrTemplImpl (sub $StrTempl (struct
    (field $segments (ref $StrSegments)))))

  ;; $StrBytesImpl — internal layout for $StrBytes.
  ;; Holds the formatted/escape-resolved byte array.
  (type $StrBytesImpl (sub $StrBytes (struct
    (field $bytes (ref $ByteArray)))))


  ;; ---- Construction (compiler-emitted) ----

  ;; str_raw : (i32, i32) -> (ref $StrRaw)
  ;; Wrap a data-section pointer into a raw string.
  (func $str_raw (export "str_raw")
    (param $offset i32)
    (param $length i32)
    (result (ref $StrRaw))

    (struct.new $StrDataImpl
      (local.get $offset)
      (local.get $length))
  )

  ;; str_templ : (ref $StrSegments) -> (ref $StrTempl)
  ;; Build a template from a segment array.
  (func $str_templ (export "str_templ")
    (param $segments (ref $StrSegments))
    (result (ref $StrTempl))

    (struct.new $StrTemplImpl (local.get $segments))
  )


  ;; ---- Access ----

  ;; str_bytes : (ref $StrVal) -> (ref $ByteArray)
  ;; Get the byte content of any byte-bearing string.
  ;; Dispatches via br_on_cast:
  ;;   $StrDataImpl     → copies bytes from data section into a $ByteArray
  ;;   $StrRawBytesImpl → returns the existing $ByteArray
  ;;   $StrBytesImpl    → returns the existing $ByteArray
  ;; Templates are statically excluded — format first, then get bytes.
  ;; This is the IO boundary: callers don't need to know the string type.
  (func $str_bytes (export "str_bytes")
    (param $str (ref $StrVal))
    (result (ref $ByteArray))

    (local $offset i32)
    (local $length i32)
    (local $result (ref $ByteArray))
    (local $i i32)

    ;; Try $StrDataImpl — copy from data section to heap
    (block $not_data
      (block $is_data (result (ref $StrDataImpl))
        (br $not_data
          (br_on_cast $is_data (ref $StrVal) (ref $StrDataImpl)
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

    ;; Try $StrRawBytesImpl — return existing array
    (block $not_raw_bytes
      (block $is_raw_bytes (result (ref $StrRawBytesImpl))
        (br $not_raw_bytes
          (br_on_cast $is_raw_bytes (ref $StrVal) (ref $StrRawBytesImpl)
            (local.get $str))))
      (return (struct.get $StrRawBytesImpl $bytes)))

    ;; Must be $StrBytesImpl — return existing array
    (struct.get $StrBytesImpl $bytes
      (ref.cast (ref $StrBytesImpl) (local.get $str)))
  )

  ;; str_tmpl_count : (ref $StrTempl) -> i32
  ;; Number of segments in a template.
  (func $str_tmpl_count (export "str_tmpl_count")
    (param $tmpl (ref $StrTempl))
    (result i32)

    (array.len
      (struct.get $StrTemplImpl $segments
        (ref.cast (ref $StrTemplImpl) (local.get $tmpl))))
  )

  ;; str_tmpl_get : (ref $StrTempl), i32 -> (ref any)
  ;; Get segment at index (raw or value).
  (func $str_tmpl_get (export "str_tmpl_get")
    (param $tmpl (ref $StrTempl))
    (param $index i32)
    (result (ref any))

    (array.get $StrSegments
      (struct.get $StrTemplImpl $segments
        (ref.cast (ref $StrTemplImpl) (local.get $tmpl)))
      (local.get $index))
  )


  ;; ---- Equality ----

  ;; str_eq : (ref $StrVal), (ref $StrVal) -> i32
  ;; Compare two byte-bearing strings for byte-level equality.
  ;; Fast path: ref.eq (same object → 1).
  ;; Slow path: dispatch on concrete types, compare byte-by-byte.
  ;; Returns 1 if equal, 0 if not.
  (func $str_eq (export "str_eq")
    (param $a (ref $StrVal))
    (param $b (ref $StrVal))
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
          (br_on_cast $a_is_data (ref $StrVal) (ref $StrDataImpl)
            (local.get $a))))
      (local.set $da)

      ;; $a is data — dispatch on $b
      (block $b_not_data
        (block $b_is_data (result (ref $StrDataImpl))
          (br $b_not_data
            (br_on_cast $b_is_data (ref $StrVal) (ref $StrDataImpl)
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
          (br_on_cast $b_is_data2 (ref $StrVal) (ref $StrDataImpl)
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

  ;; $_get_byte_array : (ref $StrVal) -> (ref $ByteArray)
  ;; Extract the $ByteArray from a $StrRawBytesImpl or $StrBytesImpl.
  ;; Caller must ensure it's not a $StrDataImpl.
  (func $_get_byte_array
    (param $str (ref $StrVal))
    (result (ref $ByteArray))

    (block $not_raw_bytes
      (block $is_raw_bytes (result (ref $StrRawBytesImpl))
        (br $not_raw_bytes
          (br_on_cast $is_raw_bytes (ref $StrVal) (ref $StrRawBytesImpl)
            (local.get $str))))
      (return (struct.get $StrRawBytesImpl $bytes)))

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

  ;; str_render_escape : (ref $StrRaw) -> (ref $StrBytes)
  ;; Resolve escape sequences in a raw string's bytes:
  ;; \n, \t, \r, \f, \v, \b, \\, \', \$, \xNN, \u{NNNNNN}
  ;; Dispatches to $_escape_data (linear memory) or $_escape_array (heap).
  ;; Pure byte processing — no user code, no CPS needed.
  (func $str_render_escape (export "str_render_escape")
    (param $raw (ref $StrRaw))
    (result (ref $StrBytes))

    ;; Try $StrDataImpl — read from linear memory
    (block $not_data
      (block $is_data (result (ref $StrDataImpl))
        (br $not_data
          (br_on_cast $is_data (ref $StrRaw) (ref $StrDataImpl)
            (local.get $raw))))
      ;; Cast succeeded — $StrDataImpl on stack
      (return
        (call $_escape_data
          (struct.get $StrDataImpl $offset)
          (struct.get $StrDataImpl $length
            (ref.cast (ref $StrDataImpl) (local.get $raw))))))

    ;; Must be $StrRawBytesImpl — read from heap array
    (call $_escape_array
      (struct.get $StrRawBytesImpl $bytes
        (ref.cast (ref $StrRawBytesImpl) (local.get $raw))))
  )

  ;; $_escape_data : (i32, i32) -> (ref $StrBytes)
  ;; Escape processing for $StrDataImpl — reads from linear memory.
  ;; Two-pass: count output bytes, then write.
  (func $_escape_data
    (param $base i32)
    (param $src_len i32)
    (result (ref $StrBytes))

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

  ;; $_escape_array : (ref $ByteArray) -> (ref $StrBytes)
  ;; Escape processing for $StrRawBytesImpl — reads from heap array.
  ;; Two-pass: count output bytes, then write.
  (func $_escape_array
    (param $src (ref $ByteArray))
    (result (ref $StrBytes))

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


  ;; str_render_unescape : (ref $StrBytes) -> (ref $StrBytes)
  ;; Insert escape sequences for non-printable/special bytes:
  ;; 0x0A → \n, 0x09 → \t, 0x0D → \r, 0x0C → \f, 0x0B → \v,
  ;; 0x08 → \b, 0x5C → \\, 0x27 → \', 0x24 → \$
  ;; Not a true inverse of escape — lossy, since $StrRaw can have
  ;; both \n and literal \ n which both escape to the same byte.
  ;; For debug output and serialization.
  ;;
  ;; Two-pass: count then write.
  (func $str_render_unescape (export "str_render_unescape")
    (param $str (ref $StrBytes))
    (result (ref $StrBytes))

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

)
