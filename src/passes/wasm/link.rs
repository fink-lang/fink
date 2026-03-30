// Static linker — merges pre-compiled runtime WASM fragments into the
// compiler's emitted WASM output, producing a single standalone module.
//
// ## Design
//
// The fink compiler emits a WASM fragment for user code (via emit.rs).
// Runtime data structures (list, hamt, set, strings) are implemented as
// standalone WAT files compiled to WASM once. The linker merges these
// fragments into one self-contained WASM binary — no runtime imports,
// no component model, runs on any current WASM engine.
//
// ## Pipeline position
//
//   CPS → lift → collect → emit → **link** → DWARF → CompileResult
//                                    ↑
//                            runtime .wasm fragments
//                            (pre-compiled from WAT)
//
// ## Type unification
//
// WASM GC uses nominal typing within rec groups — structurally identical
// types from different modules are distinct. All shared types are defined
// in `runtime/types.wat` as the single canonical source.
//
// The universal value type is `(ref any)` — WASM GC's built-in top type.
// No custom $Any supertype. See `runtime/types.wat` for the full hierarchy.
//
// Each runtime WAT module and the compiler's emitted fragment reference
// shared types by name. The linker:
//   1. Emits canonical types (from types.wat) once in the output type section
//   2. Assigns each module's internal types with namespaced names (no clashes)
//   3. Remaps all type index references in merged code
//
// ## Import convention
//
// Dependencies between fragments are declared as WASM imports using
// the `@fink/` module namespace:
//
//   ;; "I depend on the entire list module"
//   (import "@fink/runtime/list" "*" (func (param anyref)))
//
//   ;; "I depend on a specific function from hamt"
//   (import "@fink/runtime/hamt" "rec_pop" (func (param anyref)))
//
// The `(func (param anyref))` descriptor is a dummy — cheapest valid
// import to keep the WASM validator happy. The linker strips all `@fink/`
// imports and resolves them from the link set.
//
// Granular imports (naming specific functions) enable selective linking:
// if user code only uses `seq_pop`, only `list.wasm` is pulled in, not
// `hamt.wasm` or `set.wasm`. For now the linker pulls the entire module
// for any import from it. Future: trace internal call graph for finer
// tree shaking, or defer to the WASM runtime's optimizer.
//
// ## Linking steps
//
//   1. Parse all WASM fragments (wasmparser)
//   2. Scan imports — identify `@fink/` dependencies, build link set
//   3. Unify type sections:
//      - Canonical types (from types.wat) → emitted once
//      - Module-internal types → namespaced (e.g. `@fink/runtime/list:Cons`)
//      - Build old-index → new-index remap tables per fragment
//   4. Merge function sections:
//      - Resolve import references → defined function indices
//      - Namespace internal function names per module
//      - Build function index remap tables per fragment
//   5. Merge code sections:
//      - Rewrite type and function index references in instructions
//   6. Merge name sections:
//      - Combine debug names from all fragments, preserving namespaces
//      - Name format: `@fink/runtime/list:list_append` (free-form UTF-8)
//   7. Adjust DWARF:
//      - Runtime fragments (hand-written WAT) carry no DWARF
//      - User code DWARF offsets adjusted by prepended runtime code size
//   8. Emit single WASM binary (wasm-encoder)
//
// ## Name section conventions
//
// WASM name section entries are free-form UTF-8 strings. The linker uses
// module-qualified names for all merged items:
//
//   @fink/runtime/types:Num        — shared type
//   @fink/runtime/list:list_append — runtime function
//   @fink/runtime/hamt:_hash       — runtime internal function
//
// These names appear in WAT disassembly and debug tools. User-defined
// functions keep their original names without a module prefix.
