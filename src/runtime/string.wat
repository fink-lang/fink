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
;;   $Str                                  ← opaque base, enables br_on_cast
;;     $StrRaw                             ← opaque raw source bytes
;;       $StrRawImpl (sub $StrRaw)         ← internal: (offset, length) into data section
;;     $StrTempl                           ← opaque template
;;       $StrTemplImpl (sub $StrTempl)     ← internal: segment array
;;     $StrRendered                        ← opaque rendered bytes
;;       $StrRenderedImpl (sub $StrRendered) ← internal: byte array
;;
;;
;; Literals ($StrRaw):
;;   A (data offset, length) pair pointing into the WASM data section.
;;   Contains raw source bytes — no escape processing, no interpretation.
;;   Escapes like \n are stored as two bytes ('\', 'n'), not as a
;;   newline byte. Escape resolution happens at format time (in std-lib).
;;   Not directly exposed to fink code; building blocks inside templates.
;;
;; Templates ($StrTempl):
;;   The fink "string" value. A list of segments where each segment is
;;   either a $StrRaw ref or an arbitrary value (ref any).
;;   'hello ${name}' → template with segments: [raw "hello ", value name].
;;   A plain string 'hello' is a template with a single raw segment.
;;   First-class, immutable, lazy — formatting deferred to std-lib.
;;   Equality is structural via deep_eq (std-lib, CPS).
;;
;; Rendered ($StrRendered):
;;   Flat UTF-8 encoded bytes — the result of formatting a template.
;;   Produced by std-lib formatters (fmt, raw) at IO boundaries or
;;   explicit fmt'...' calls. Heap-allocated (GC array of bytes).
;;
;;
;; Formatting (std-lib, not here):
;;   Two built-in formatters in the std-lib:
;;     - fmt : resolve escapes in literals, stringify values → $StrRendered
;;     - raw : no escape processing, stringify values as-is → $StrRendered
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

  ;; Segment array: interleaved $StrRaw refs and arbitrary values.
  ;; Stored as (ref any) — raws, values, and nested templates all fit.
  (type $StrSegments (array (ref any)))

  ;; Rendered byte array: immutable UTF-8 bytes.
  (type $StrBytes (array i8))

  ;; $StrRawImpl — internal layout for $StrRaw.
  ;; offset = byte offset into linear memory (data section)
  ;; length = byte count
  (type $StrRawImpl (sub $StrRaw (struct
    (field $offset i32)
    (field $length i32))))

  ;; $StrTemplImpl — internal layout for $StrTempl.
  ;; Holds the segment array.
  (type $StrTemplImpl (sub $StrTempl (struct
    (field $segments (ref $StrSegments)))))

  ;; $StrRenderedImpl — internal layout for $StrRendered.
  ;; Holds the formatted byte array.
  (type $StrRenderedImpl (sub $StrRendered (struct
    (field $bytes (ref $StrBytes)))))


  ;; ---- Construction (compiler-emitted) ----

  ;; str_raw : (i32, i32) -> (ref $StrRaw)
  ;; Wrap a data-section pointer into a raw string.
  ;; (func $str_raw ...)

  ;; str_templ : (ref $StrSegments) -> (ref $StrTempl)
  ;; Build a template from a segment array.
  ;; (func $str_templ ...)

  ;; str_rendered : (ref $StrBytes) -> (ref $StrRendered)
  ;; Build a rendered string from a byte array.
  ;; Used by std-lib formatters to produce output.
  ;; (func $str_rendered ...)


  ;; ---- Template access ----

  ;; str_tmpl_count : (ref $StrTempl) -> i32
  ;; Number of segments in a template.
  ;; (func $str_tmpl_count ...)

  ;; str_tmpl_get : (ref $StrTempl), i32 -> (ref any)
  ;; Get segment at index (raw or value).
  ;; (func $str_tmpl_get ...)


  ;; ---- Raw access ----

  ;; str_raw_offset : (ref $StrRaw) -> i32
  ;; Get data-section offset of a raw string.
  ;; (func $str_raw_offset ...)

  ;; str_raw_len : (ref $StrRaw) -> i32
  ;; Get byte length of a raw string.
  ;; (func $str_raw_len ...)


  ;; ---- Rendered access ----

  ;; str_rendered_len : (ref $StrRendered) -> i32
  ;; Byte length of a rendered string.
  ;; (func $str_rendered_len ...)

  ;; str_rendered_get_byte : (ref $StrRendered), i32 -> i32
  ;; Get byte at index.
  ;; (func $str_rendered_get_byte ...)


  ;; ---- Byte processing helpers (for std-lib formatters) ----

  ;; str_decode_codepoint : (ref $StrRendered), i32 -> (i32, i32)
  ;; Decode next UTF-8 codepoint at offset.
  ;; Returns (codepoint, next_offset).
  ;; Returns (-1, offset) at end.
  ;; (func $str_decode_codepoint ...)

  ;; str_render_escape : (ref $StrRaw) -> (ref $StrRendered)
  ;; Process escape sequences in a raw string's bytes:
  ;; \n, \t, \r, \f, \v, \b, \\, \', \$, \xNN, \uNNNNNN
  ;; Returns rendered UTF-8 bytes with escapes resolved.
  ;; Pure byte processing — no user code, no CPS needed.
  ;; (func $str_render_escape ...)

)
