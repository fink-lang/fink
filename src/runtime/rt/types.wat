;; Fink WASM GC Type Hierarchy
;;
;; Universal value type: (ref any)
;;
;; Everything flows as (ref any) in untyped/phase-0 code. No custom $Any
;; supertype — WASM GC's built-in `any` is the true top type. Type inference
;; (future) narrows (ref any) → concrete types, eliminating casts.
;;
;; Zero boxing: i31ref, structs, arrays, and funcrefs are all subtypes of
;; `any` — no wrapper structs needed to pass values through generic slots.
;;
;;
;; Built-in WASM GC hierarchy (for reference):
;; ────────────────────────────────────────────
;;
;;   any
;;   ├── eq                           ← ref.eq works on these
;;   │   ├── i31                      ← unboxed 31-bit signed int
;;   │   ├── struct
;;   │   │   └── (fink struct types)
;;   │   └── array
;;   │       └── (fink array types)
;;   ├── func                         ← function references
;;   │   └── (fink func types)
;;   └── extern
;;
;;
;; Fink value types:
;; ─────────────────
;;
;;   (ref any)                        ← universal value slot
;;   │
;;   ├── eq                           ← GC-managed, ref.eq works on these
;;   │   ├── i31ref                   ← int literals (-2^30..2^30-1), booleans (0/1)
;;   │   │
;;   │   └── struct
;;   │       ├── $Num (field f64)     ← float / large number
;;   │       ├── $Str                 ← base string type
;;   │       ├── $List                ← list (opaque — internals in list.wat)
;;   │       ├── $Rec                 ← record (opaque — internals in rec.wat)
;;   │       ├── $Dict                ← dict (opaque — internals in rec.wat)
;;   │       ├── $Set                 ← set (opaque — internals in set.wat)
;;   │       ├── $Range               ← numeric range (opaque — internals in range.wat)
;;   │       ├── $Channel             ← async channel (non-final)
;;   │       │   └── $HostChannel     ← host-managed IO channel (stdin etc.)
;;   │       ├── $Future              ← opaque future (settled flag + value)
;;   │       ├── $SpreadArgs ($List)  ← spread call marker (wraps list)
;;   │       ├── $VarArgs (array)     ← variable-length argument array
;;   │       ├── $Captures (array)    ← flat capture value array
;;   │       └── $Closure (funcref, $Captures) ← universal closure type
;;   │
;;   └── func                         ← not GC-managed (opaque refs)
;;       └── $FnN (func ...)          ← typed function refs (per arity)
;;
;;
;; Collection API boundary:
;; ────────────────────────
;;
;;   User code passes (ref any). Collection functions accept (ref eq) for
;;   keys/values — the caller casts (ref.cast (ref eq)) at the boundary.
;;   This is one cheap tag check per call, not per-node traversal.
;;   Internal ref.eq comparisons work directly on (ref eq).
;;
;;
;; Runtime vs std-lib boundary:
;; ────────────────────────────
;;
;;   Runtime (.wat files): direct-style primitives. No CPS, no user code
;;   callbacks, no lazy value evaluation. Used by the compiler (emitted
;;   code) and by std-lib internals.
;;
;;   Std-lib (fink code): CPS functions exposed to fink user code.
;;   Formatters, equality protocols, anything that dispatches through
;;   protocols or touches lazy values. Wraps runtime primitives.
;;
;;
;; Value comparison (direct-style):
;; ────────────────────────────────
;;
;;   Set membership, dict/record keys, and list_find require value
;;   comparison. Only types that can be compared direct-style (no CPS,
;;   no protocol dispatch) are valid:
;;
;;     i31ref       — ref.eq (identity)
;;     $Num         — f64 comparison
;;     $Str         — byte-level comparison
;;
;;   Templates, closures, records, lists, sets are NOT valid for these
;;   operations until an Eq protocol exists in the std-lib.
;;   Runtime deep_eq handles all built-in types via br_on_cast dispatch.
;;   User-defined types will extend it via the Eq protocol (future).
;;
;;
;; Literals (phase 0):
;; ───────────────────
;;
;;   42        → i31.const 42             (no allocation)
;;   true      → i31.const 1              (no allocation)
;;   false     → i31.const 0              (no allocation)
;;   3.14      → struct.new $Num (f64.const 3.14)
;;
;;
;; Evolution:
;; ──────────
;;
;;   Phase 0 (now):  everything (ref any), casts at operation boundaries
;;   Phase 1 (type inference):  narrow signatures, eliminate casts statically
;;   Phase 2 (optimization):  unbox to i31/f64/raw call_ref where possible


(module

  ;; -- Shared types -------------------------------------------------------
  ;;
  ;; Canonical type definitions for all fink values. Every runtime module
  ;; and the compiler's emitted code reference these via the linker.
  ;; Defined in a single rec group so WasmGC treats them as one nominal
  ;; family — cross-module casts work correctly after linking.

  (rec
    ;; $Bool = i31ref (0 = false, 1 = true)
    ;; $Int  = i31ref (-2^30..2^30-1)

    ;; TODO: $Utf8 — validated UTF-8 string subtype of $Str for codepoint-level
    ;; slicing and indexing; $Str stays as the byte-level base type.

    ;; $Fn_host_wrapper — host-facing per-module wrapper signature.
    ;; (key_bytes: ref null any, cont_id: i32) -> ()
    ;; Each fragment's lower-synthesised wrapper export has this
    ;; type; declared once here so all modules share it instead of
    ;; emitting a per-fragment local copy.
    ;; TODO: move to interop/rust.wat once we use the WAT linker.
    (type $Fn_host_wrapper (func (param (ref null any)) (param i32)))
  )

)
