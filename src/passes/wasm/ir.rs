//! WASM object-file IR — "unlinked WASM".
//!
//! Design sketch. Types are TODO placeholders to anchor discussion; no
//! implementation yet.
//!
//! See `.brain/.scratch/ir-link-refactor-plan.md` for the broader context.
//!
//! # What this is
//!
//! A Fink-flavoured WASM object-file representation. Close to WAT's
//! section model, with unresolved cross-fragment references that the
//! linker resolves at link time.
//!
//! * Not a general-purpose compiler IR. No optimisation passes. No
//!   multi-target story.
//! * Not a fork of `wasm-encoder`. We drive `wasm-encoder` from the
//!   linker, and only from the linker.
//! * Not a superset of WAT. Only the subset Fink actually emits. Grow
//!   by demand.
//!
//! # Responsibility split
//!
//! * `wasm_ir::lower`: CPS `LiftedCps` → `Fragment`.
//! * `wasm_ir::link`: `[Fragment]` → linked `wasm-encoder`-produced
//!   bytes. The *only* place `wasm-encoder` is used. DWARF + sourcemap
//!   emission also live here (final byte offsets known).
//! * `wasm_ir::fmt`: `Fragment` → WAT text directly. No wasm-byte
//!   parsing for the common test path.
//!
//! # Names vs ids
//!
//! Mirrors the CPS convention: *identity is numeric, names are
//! presentation*. Within a fragment, cross-references use typed symbol
//! ids (`FuncSym`, `TypeSym`, `GlobalSym`, `DataSym`). Each symbol's
//! declaration may carry an optional display name (from CPS bind
//! origin or synth), used by `fmt` — never part of resolution.
//!
//! Strings only appear at the *import/export seam*: WAT's
//! `(import "module" "name" ...)` / `(export "name" ...)`. That is the
//! only place cross-fragment resolution by string occurs. The linker
//! matches a fragment's imports against another fragment's exports by
//! (module, name) — same as real WASM linking.
//!
//! # Storage
//!
//! Flat arenas per category, using `PropGraph<Id, T>`, same pattern as
//! AST / CPS / passes. Sibling `PropGraph<Id, Origin>` for sourcemap
//! origins, etc. Append-only.
//!
//! # Instruction shape
//!
//! Statement-level sequence per function body (`Vec<InstrId>`).
//! Instruction operands are always *leaves*: constants, local/global
//! reads, ref-constructor reads, or symbolic refs. This matches CPS
//! A-normal form — no arbitrary nesting. Lowering is a 1:1 translation
//! from CPS (`LetVal` → `LocalSet`, ref → `LocalGet`, `App` → `Call`).
//!
//! # WAT as the shape reference
//!
//! Terms and variant names mirror WAT spec:
//! `func`, `global`, `local`, `param`, `result`, `type`, `import`,
//! `export`, `elem`, `data`. Instruction variants mirror WAT opcode
//! names minus the dots: `LocalGet` renders `local.get`, `StructNew`
//! renders `struct.new`, etc.
//!
//! Invariant: a pretty-printed `Fragment` reads like WAT that a wasm
//! engineer could have written. If it doesn't, the IR has drifted.

// ──────────────────────────────────────────────────────────────────────
// Symbol ids (typed indices into per-fragment symbol tables)
// ──────────────────────────────────────────────────────────────────────

/// Identifies a function within a fragment. Linker resolves to a
/// global function index at link time.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FuncSym(pub u32);

/// Identifies a Fink module (entry or dep) within a package compile.
///
/// Allocated by `ir_compile_package` during URL canonicalisation /
/// dedup. Used as a stable, opaque identity for cross-module
/// references in IR — `pub` registers into `registry[mod_id]`, `import`
/// reads from it, both via i31-typed runtime ABI args. URL strings
/// only enter the picture in two places:
///
/// * The package compiler / linker holds the (ModuleId → URL) map for
///   formatter consumption.
/// * Producer-side `pub` calls still take a `$Str` binding name — the
///   consumer's destructure cont uses string-keyed rec-pop.
///
/// Single-fragment compiles use `ModuleId(0)` by default.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ModuleId(pub u32);

/// Identifies a type (struct / array / func-signature) within a
/// fragment.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TypeSym(pub u32);

/// Identifies a global within a fragment.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct GlobalSym(pub u32);

/// Identifies a data segment entry (interned string blob) within a
/// fragment. Linker lays out the final data section and assigns
/// absolute byte offsets at link time.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct DataSym(pub u32);

/// Identifies an instruction node within a fragment's instruction
/// arena. Used for attaching sourcemap origins and the like.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct InstrId(pub u32);

/// Local variable index within a function. Numeric like CPS locals —
/// display name (if any) lives in `FuncDecl.locals[i].name`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct LocalIdx(pub u32);

// ──────────────────────────────────────────────────────────────────────
// Types (WASM type section entries)
// ──────────────────────────────────────────────────────────────────────

/// A value type — the types that can be stored in locals/globals and
/// passed across function boundaries. Mirrors WASM's `valtype`.
///
/// Note: `i31ref` (the WAT shorthand) is not a separate variant —
/// it's `RefAbstract { nullable: false, ht: I31 }`. Same with
/// `anyref`, `eqref`, `funcref`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValType {
  I32,
  F64,
  /// `(ref null? $T)` — concrete.
  RefConcrete { nullable: bool, ty: TypeSym },
  /// `(ref null? <abstract>)` — any, eq, i31, func.
  RefAbstract { nullable: bool, ht: AbsHeap },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AbsHeap { Any, Eq, I31, Func }

/// A type-section entry — struct / array / func signature.
///
/// May also be an *imported* type (cross-fragment reference that the
/// linker resolves by `(module, name)` — analogous to imported
/// functions/globals). For import notation, see the WebAssembly
/// "Type Imports and Exports" proposal (Phase 1):
/// <https://github.com/WebAssembly/proposal-type-imports>. We borrow
/// the `(import "mod" "Name" (type $T (sub any)))` syntax *inside*
/// our IR and resolve it at link time — real WASM bytes never
/// contain a type import, because the linker unifies the type
/// against the provider fragment's declaration and rewrites the
/// user-side `TypeSym` to the merged type's index.
///
/// If `import` is Some, `kind` describes the expected *bound* (the
/// shape the linker unifies against). If `import` is None, `kind` is
/// the full concrete declaration.
///
/// Note: struct + array correspond to WasmGC `(struct ...)` /
/// `(array ...)`. Func is `(func (param ...) (result ...))`.
#[derive(Clone, Debug)]
pub struct TypeDecl {
  pub kind: TypeKind,
  pub display: Option<String>,
  pub import: Option<ImportKey>,
}

#[derive(Clone, Debug)]
pub enum TypeKind {
  Struct { fields: Vec<StructField> },
  Array  { elem: ValType, mutable: bool },
  Func   { params: Vec<ValType>, results: Vec<ValType> },
  /// A `(sub <abstract>)` bound — no concrete body. Used for imported
  /// types where the importer doesn't need the layout, only that the
  /// type is a subtype of `any` / `eq` / `func`.
  SubBound { ht: AbsHeap },
}

#[derive(Clone, Debug)]
pub struct StructField {
  pub ty: ValType,
  pub mutable: bool,
  pub display: Option<String>,
}

// ──────────────────────────────────────────────────────────────────────
// Functions
// ──────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct FuncDecl {
  pub sig: TypeSym,
  /// Function parameters. Indexed first (LocalIdx 0..params.len()).
  /// Display names come from here. Types must match the signature's
  /// `TypeKind::Func { params }` entry; we store them alongside for
  /// convenience — imported signatures (`TypeKind::SubBound`) don't
  /// carry structural params, so the renderer needs the params here.
  pub params: Vec<LocalDecl>,
  /// Extra locals declared on top of the params. Indexed after the
  /// params (LocalIdx params.len()..params.len()+locals.len()).
  pub locals: Vec<LocalDecl>,
  pub body: Vec<InstrId>,
  /// Display name for fmt — derived from CPS bind origin or synthetic.
  /// Not part of resolution.
  pub display: Option<String>,
  /// If this function is imported, this is its import key
  /// (module_name, field_name). Cross-fragment func linkage resolves
  /// through this pair exactly as in WASM's import/export spec.
  pub import: Option<ImportKey>,
  /// If this function is exported, its public name. The linker
  /// matches importers against this.
  pub export: Option<String>,
}

#[derive(Clone, Debug)]
pub struct LocalDecl {
  pub ty: ValType,
  /// Display name for fmt. Not part of resolution.
  pub display: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ImportKey {
  pub module: String,
  pub name: String,
}

// ──────────────────────────────────────────────────────────────────────
// Globals
// ──────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct GlobalDecl {
  pub ty: ValType,
  pub mutable: bool,
  /// Initial value. For user-module bring-up globals this will
  /// typically be `null` of the right ref type; the module's
  /// `fink_module` init function populates the actual value at
  /// bring-up time.
  pub init: GlobalInit,
  pub display: Option<String>,
  pub import: Option<ImportKey>,
  pub export: Option<String>,
}

#[derive(Clone, Debug)]
pub enum GlobalInit {
  I32Const(i32),
  F64Const(f64),
  RefNull(AbsHeap),
  RefNullConcrete(TypeSym),
  RefFunc(FuncSym),
}

// ──────────────────────────────────────────────────────────────────────
// Data
// ──────────────────────────────────────────────────────────────────────

/// An interned byte blob. Linker lays out the combined data section
/// and assigns each `DataSym` its final absolute offset.
#[derive(Clone, Debug)]
pub struct DataDecl {
  pub bytes: Vec<u8>,
  pub display: Option<String>,
}

// ──────────────────────────────────────────────────────────────────────
// Instructions — statement-level nodes
// ──────────────────────────────────────────────────────────────────────

/// One statement in a function body.
///
/// Operands are always *leaves*: constants, local/global reads, ref
/// constructors, or symbolic refs. No nested compound expressions.
/// This matches CPS A-normal form — no synth locals are needed
/// during lowering.
///
/// TODO: grow the variant set only as lowering actually needs it.
/// Starter set covers `42 + 123` (the tracer target) plus a
/// smattering of close neighbours likely to appear in early tests.
#[derive(Clone, Debug)]
pub struct Instr {
  pub kind: InstrKind,
  /// Source byte range this instruction corresponds to, if known.
  /// `None` for bring-up plumbing and compiler-synthesised helpers.
  /// Fed into the native sourcemap by the formatter.
  pub origin: Option<crate::sourcemap::native::ByteRange>,
}

#[derive(Clone, Debug)]
pub enum InstrKind {
  /// local.set — write a local from a leaf operand.
  ///
  /// `i32.const` / `f64.const` don't have dedicated `Instr` variants:
  /// a const only ever appears as `LocalSet { src: Operand::I32(..) }`
  /// (or the F64 equivalent). CPS always binds literals to a local,
  /// so the statement-level constant form never shows up on its own.
  LocalSet { idx: LocalIdx, src: Operand },
  /// global.set. `global.get` doesn't have an `Instr` variant — it's
  /// an `Operand::Global` whenever it appears as an operand.
  GlobalSet { sym: GlobalSym, src: Operand },
  /// ref.null of an abstract heap type.
  RefNull { ht: AbsHeap, into: LocalIdx },
  /// ref.null of a concrete heap type.
  RefNullConcrete { ty: TypeSym, into: LocalIdx },
  /// ref.i31.
  RefI31 { src: Operand, into: LocalIdx },
  /// i31.get_s.
  I31GetS { src: Operand, into: LocalIdx },
  /// ref.func.
  RefFunc { func: FuncSym, into: LocalIdx },
  /// struct.new — all fields are leaves.
  StructNew { ty: TypeSym, fields: Vec<Operand>, into: LocalIdx },
  /// array.new_fixed — all elements are leaves. `size` must equal
  /// `elems.len()` (matches the WASM instruction's N).
  ArrayNewFixed { ty: TypeSym, size: u32, elems: Vec<Operand>, into: LocalIdx },
  /// array.get — read element `idx` from array `arr`. Element type
  /// follows the array type's element decl.
  ArrayGet { ty: TypeSym, arr: Operand, idx: Operand, into: LocalIdx },
  /// ref.cast (non-null concrete).
  RefCastNonNull { ty: TypeSym, src: Operand, into: LocalIdx },
  /// ref.cast (nullable concrete).
  RefCastNullable { ty: TypeSym, src: Operand, into: LocalIdx },
  /// ref.cast to a non-null abstract heap type (e.g. `(ref i31)`).
  RefCastNonNullAbs { ht: AbsHeap, src: Operand, into: LocalIdx },
  /// call — target is a symbol, args are leaves.
  Call { target: FuncSym, args: Vec<Operand>, into: Option<LocalIdx> },
  /// return_call — tail call.
  ReturnCall { target: FuncSym, args: Vec<Operand> },
  /// if/else — structured control flow. Bodies are themselves
  /// sequences of statements. `cond` is always a leaf (typically an
  /// i31.get_s into a local, then LocalGet of that local).
  If { cond: Operand, then_body: Vec<InstrId>, else_body: Vec<InstrId> },
  /// unreachable.
  Unreachable,
  /// drop — discard the value of a leaf operand (rare — emit.rs
  /// uses this when a Call's result is unused).
  Drop { src: Operand },
}

/// A leaf operand for an instruction. Covers the forms that CPS
/// guarantees are the only things that can appear as operands:
/// numeric constants, local reads, global reads, and symbolic refs.
///
/// Deliberately smaller than `Instr` — if a value can appear as an
/// operand it must be trivially computable and thus fit here.
#[derive(Clone, Debug)]
pub enum Operand {
  I32(i32),
  F64(f64),
  Local(LocalIdx),
  Global(GlobalSym),
  /// ref.func $name — used inline in element sections and as a
  /// function-constant operand.
  RefFunc(FuncSym),
  /// ref.null (abstract).
  RefNull(AbsHeap),
  /// Data segment start offset + length. At emit time the linker
  /// substitutes the final absolute offset; until then it's
  /// symbolic.
  DataRef { sym: DataSym, len: u32 },
}

// ──────────────────────────────────────────────────────────────────────
// Fragment — the top-level unlinked unit
// ──────────────────────────────────────────────────────────────────────

/// A compiled Fink module (or a hand-written runtime module), not yet
/// linked. Mirrors a WAT `(module ...)` section-by-section:
///
/// ```text
///   (module
///     (type ...)       ← types
///     (import ...)     ← imports (on funcs, globals — encoded inline)
///     (func ...)       ← funcs
///     (global ...)     ← globals
///     (memory ...)     ← memories (always 1 for now)
///     (data ...)       ← data
///     (export ...)     ← exports (on funcs, globals — encoded inline)
///     (elem ...)       ← elements (for ref.func tables)
///   )
/// ```
///
/// Imports and exports are encoded on the item itself (`FuncDecl.import`
/// / `FuncDecl.export`, same for `GlobalDecl`) to keep the data close
/// to the thing being imported/exported. This matches how the WASM
/// spec stores them in practice.
///
/// TODO: sibling `PropGraph<InstrId, Origin>` and similar for sourcemap
/// / DWARF data — wire in when lower starts producing origins.
#[derive(Clone, Debug, Default)]
pub struct Fragment {
  /// This fragment's stable module identity. Single-fragment compiles
  /// default to `ModuleId(0)`. Multi-fragment package compiles assign
  /// each fragment a unique id during BFS.
  pub module_id: ModuleId,
  /// Imports declared by this fragment, in canonical-URL form. Maps
  /// the source URL string (as written in `import './...'`) to the
  /// ModuleId of the target fragment. Populated by
  /// `ir_compile_package` after URL canonicalisation + dedup. Used
  /// by `ir_lower`'s import-call lowering to convert source URLs
  /// into ModuleId i31 args.
  pub module_imports: std::collections::BTreeMap<String, ModuleId>,
  pub types:   Vec<TypeDecl>,
  pub funcs:   Vec<FuncDecl>,
  pub globals: Vec<GlobalDecl>,
  pub data:    Vec<DataDecl>,
  pub instrs:  Vec<Instr>,   // arena keyed by InstrId(u32)
  // TODO: memory declarations (1 for now — just a min page count).
  // TODO: element section (for ref.func tables, used by closure dispatch).
  // TODO: `start` function? Fink doesn't use it, but the spec has it.
}

// ──────────────────────────────────────────────────────────────────────
// Builder helpers (to be fleshed out alongside lowering)
// ──────────────────────────────────────────────────────────────────────
//
// Per the fink code-style rule ("Prefer named builder helpers over
// ad-hoc inline construction"), call sites do NOT construct these
// structs/enums inline. For every datum there is (or will be) a
// small helper that reads like a DSL at the call site:
//
//   // value types
//   val_i32();    val_f64();    val_i31ref();
//   val_anyref(/*nullable*/ true);
//   val_ref_concrete(num_ty_sym, /*nullable*/ false);
//
//   // type section
//   let num_ty = ty_struct(frag, vec![field(val_f64(), /*mut*/ false, "val")],
//                          Some("Num"));
//   let fn2_sig = ty_func(frag, vec![val_anyref(true), val_anyref(true)],
//                         vec![], Some("Fn2"));
//
//   // operands (leaves)
//   op_i32(42);   op_local(l);  op_global(g);  op_ref_func(f);
//
//   // instructions — append to the given body, return InstrId
//   push_i32_const(frag, body, 42, into_local);
//   push_struct_new(frag, body, num_ty, vec![op_f64(42.0)], into_local);
//   push_call(frag, body, num_op_add_sym,
//             vec![op_local(a), op_local(b)], Some(out));
//   push_return_call(frag, body, done_cont_sym, vec![op_local(result)]);
//   push_if(frag, body, op_local(cond), then_body, else_body);
//
//   // higher-level composition
//   add_func(frag, sig_sym, locals, body, display);
//   add_global(frag, ty, mutable, init, display);
//   add_import_func(frag, module, name, sig_sym);
//
// These helpers own:
//   * display-name defaulting (fewer `None` / `Some("…")` at call sites);
//   * symbol-id allocation (never write `FuncSym(self.funcs.len() as u32)`
//     inline);
//   * arena appends;
//   * invariants that aren't type-level (e.g. a LocalIdx in a body
//     actually indexes an existing local in that func).
//
// TODO: implement these alongside the first lowering — don't
// speculatively add helpers for variants we aren't using yet. Grow
// the vocabulary by demand, same rule as the instruction set.
//
// ──────────────────────────────────────────────────────────────────────
// Open design questions (to resolve as we go)
// ──────────────────────────────────────────────────────────────────────
//
// 1. How are imports physically stored at emit? WASM encodes them in
//    a separate section indexed before the local funcs/globals. We'd
//    fold that back into linear order in the emitted binary by
//    counting imports first, but the IR should *not* require import
//    funcs to come before local funcs — that's an encoding detail.
//
// 2. (DECIDED) Operands carry local/global reads and constants;
//    `Instr` never does. `LocalGet`, `GlobalGet`, `I32Const`,
//    `F64Const` are not `Instr` variants — reads are `Operand::Local`
//    / `Operand::Global`, constants are `Operand::I32` / `Operand::F64`.
//    Statements that *produce* a new value (struct.new, ref.null,
//    ref.func, ref.i31, i31.get_s, ref.cast) write into `into:
//    LocalIdx`; lowering emits them as "compute expr, set local N".
//
// 3. Multi-value. WASM has block/loop result types with arbitrary
//    multi-value signatures. Fink doesn't use any of that yet — CPS
//    threads everything through explicit continuations. If we need
//    it, it slots in on `If`'s return type. Don't pre-build.
//
// 4. Arena vs Vec. Both work; `Vec<T>` with index = `XxxSym(u32)` is
//    simpler than introducing `PropGraph<XxxSym, T>` when there's no
//    cross-cutting sibling data yet. Upgrade to PropGraph when we
//    add sibling origin data.


// ──────────────────────────────────────────────────────────────────────
// Builder helpers — minimal set exercised by the `42 + 123` tracer
// ──────────────────────────────────────────────────────────────────────
//
// Helpers are free functions that take `&mut Fragment` as the first
// argument where needed. Keeps call sites terse and uniform.
//
// Convention: allocators (`ty_*`, `import_func`, `func`, `global`,
// `data`, `push_*`) return the freshly-allocated id. Operand
// constructors (`op_*`) are pure — no fragment needed.

// --- value types --------------------------------------------------

pub fn val_i32() -> ValType { ValType::I32 }
pub fn val_f64() -> ValType { ValType::F64 }
pub fn val_anyref(nullable: bool) -> ValType {
  ValType::RefAbstract { nullable, ht: AbsHeap::Any }
}
pub fn val_funcref(nullable: bool) -> ValType {
  ValType::RefAbstract { nullable, ht: AbsHeap::Func }
}
pub fn val_ref(ty: TypeSym, nullable: bool) -> ValType {
  ValType::RefConcrete { nullable, ty }
}
pub fn val_ref_abs(ht: AbsHeap, nullable: bool) -> ValType {
  ValType::RefAbstract { nullable, ht }
}

// --- type section -------------------------------------------------

pub fn field(ty: ValType, mutable: bool, display: &str) -> StructField {
  StructField { ty, mutable, display: Some(display.into()) }
}

pub fn ty_struct(
  frag: &mut Fragment,
  fields: Vec<StructField>,
  display: &str,
) -> TypeSym {
  let sym = TypeSym(frag.types.len() as u32);
  frag.types.push(TypeDecl {
    kind: TypeKind::Struct { fields },
    display: Some(display.into()),
    import: None,
  });
  sym
}

pub fn ty_func(
  frag: &mut Fragment,
  params: Vec<ValType>,
  results: Vec<ValType>,
  display: &str,
) -> TypeSym {
  let sym = TypeSym(frag.types.len() as u32);
  frag.types.push(TypeDecl {
    kind: TypeKind::Func { params, results },
    display: Some(display.into()),
    import: None,
  });
  sym
}

/// Declare an imported type — cross-fragment reference resolved by the
/// linker. `bound` is the abstract heap bound (typically `AbsHeap::Any`
/// for struct/array types, `AbsHeap::Func` for function-signature types).
pub fn ty_import(
  frag: &mut Fragment,
  module: &str,
  name: &str,
  bound: AbsHeap,
) -> TypeSym {
  let sym = TypeSym(frag.types.len() as u32);
  frag.types.push(TypeDecl {
    kind: TypeKind::SubBound { ht: bound },
    display: Some(name.into()),
    import: Some(ImportKey { module: module.into(), name: name.into() }),
  });
  sym
}

// --- func section -------------------------------------------------

pub fn local(ty: ValType, display: &str) -> LocalDecl {
  LocalDecl { ty, display: Some(display.into()) }
}

/// Append a function to the fragment. `body` is the sequence of
/// `InstrId` already pushed into `frag.instrs`.
pub fn func(
  frag: &mut Fragment,
  sig: TypeSym,
  params: Vec<LocalDecl>,
  locals: Vec<LocalDecl>,
  body: Vec<InstrId>,
  display: &str,
) -> FuncSym {
  let sym = FuncSym(frag.funcs.len() as u32);
  frag.funcs.push(FuncDecl {
    sig,
    params,
    locals,
    body,
    display: Some(display.into()),
    import: None,
    export: None,
  });
  sym
}

/// Append an imported function (no body).
pub fn import_func(
  frag: &mut Fragment,
  sig: TypeSym,
  module: &str,
  name: &str,
) -> FuncSym {
  let sym = FuncSym(frag.funcs.len() as u32);
  frag.funcs.push(FuncDecl {
    sig,
    params: Vec::new(),
    locals: Vec::new(),
    body: Vec::new(),
    display: Some(name.into()),
    import: Some(ImportKey { module: module.into(), name: name.into() }),
    export: None,
  });
  sym
}

/// Append a global declaration.
pub fn add_global(
  frag: &mut Fragment,
  ty: ValType,
  mutable: bool,
  init: GlobalInit,
  display: &str,
  export: Option<String>,
) -> GlobalSym {
  let sym = GlobalSym(frag.globals.len() as u32);
  frag.globals.push(GlobalDecl {
    ty,
    mutable,
    init,
    display: Some(display.into()),
    import: None,
    export,
  });
  sym
}

// --- operands (leaves) -------------------------------------------

pub fn op_i32(v: i32) -> Operand { Operand::I32(v) }
pub fn op_f64(v: f64) -> Operand { Operand::F64(v) }
pub fn op_local(idx: LocalIdx) -> Operand { Operand::Local(idx) }
pub fn op_global(sym: GlobalSym) -> Operand { Operand::Global(sym) }
pub fn op_ref_func(f: FuncSym) -> Operand { Operand::RefFunc(f) }
pub fn op_ref_null(ht: AbsHeap) -> Operand { Operand::RefNull(ht) }

// --- instruction appenders ---------------------------------------

fn push(frag: &mut Fragment, kind: InstrKind) -> InstrId {
  push_with_origin(frag, kind, None)
}

fn push_with_origin(
  frag: &mut Fragment,
  kind: InstrKind,
  origin: Option<crate::sourcemap::native::ByteRange>,
) -> InstrId {
  let id = InstrId(frag.instrs.len() as u32);
  frag.instrs.push(Instr { kind, origin });
  id
}

pub fn push_local_set(frag: &mut Fragment, idx: LocalIdx, src: Operand) -> InstrId {
  push(frag, InstrKind::LocalSet { idx, src })
}

pub fn push_global_set(frag: &mut Fragment, sym: GlobalSym, src: Operand) -> InstrId {
  push(frag, InstrKind::GlobalSet { sym, src })
}

pub fn push_struct_new(
  frag: &mut Fragment,
  ty: TypeSym,
  fields: Vec<Operand>,
  into: LocalIdx,
) -> InstrId {
  push(frag, InstrKind::StructNew { ty, fields, into })
}

pub fn push_array_new_fixed(
  frag: &mut Fragment,
  ty: TypeSym,
  elems: Vec<Operand>,
  into: LocalIdx,
) -> InstrId {
  let size = elems.len() as u32;
  push(frag, InstrKind::ArrayNewFixed { ty, size, elems, into })
}

pub fn push_array_get(
  frag: &mut Fragment,
  ty: TypeSym,
  arr: Operand,
  idx: Operand,
  into: LocalIdx,
) -> InstrId {
  push(frag, InstrKind::ArrayGet { ty, arr, idx, into })
}

pub fn push_ref_null_concrete(
  frag: &mut Fragment,
  ty: TypeSym,
  into: LocalIdx,
) -> InstrId {
  push(frag, InstrKind::RefNullConcrete { ty, into })
}

pub fn push_ref_i31(frag: &mut Fragment, src: Operand, into: LocalIdx) -> InstrId {
  push(frag, InstrKind::RefI31 { src, into })
}

pub fn push_ref_cast_non_null(
  frag: &mut Fragment,
  ty: TypeSym,
  src: Operand,
  into: LocalIdx,
) -> InstrId {
  push(frag, InstrKind::RefCastNonNull { ty, src, into })
}

pub fn push_ref_cast_non_null_abs(
  frag: &mut Fragment,
  ht: AbsHeap,
  src: Operand,
  into: LocalIdx,
) -> InstrId {
  push(frag, InstrKind::RefCastNonNullAbs { ht, src, into })
}

pub fn push_call(
  frag: &mut Fragment,
  target: FuncSym,
  args: Vec<Operand>,
  into: Option<LocalIdx>,
) -> InstrId {
  push(frag, InstrKind::Call { target, args, into })
}

pub fn push_return_call(
  frag: &mut Fragment,
  target: FuncSym,
  args: Vec<Operand>,
) -> InstrId {
  push(frag, InstrKind::ReturnCall { target, args })
}

pub fn push_i31_get_s(frag: &mut Fragment, src: Operand, into: LocalIdx) -> InstrId {
  push(frag, InstrKind::I31GetS { src, into })
}

pub fn push_if(
  frag: &mut Fragment,
  cond: Operand,
  then_body: Vec<InstrId>,
  else_body: Vec<InstrId>,
) -> InstrId {
  push(frag, InstrKind::If { cond, then_body, else_body })
}

pub fn push_unreachable(frag: &mut Fragment) -> InstrId {
  push(frag, InstrKind::Unreachable)
}

/// Intern byte content into `frag.data`, returning a `DataSym` keyed
/// by the bytes. Reuses an existing entry if the same bytes are
/// already present (whole-blob match). The linker / emitter lays out
/// the data section sequentially and resolves each `DataSym` to its
/// final offset.
pub fn intern_data(frag: &mut Fragment, bytes: &[u8]) -> DataSym {
  if let Some((i, _)) = frag.data.iter().enumerate()
    .find(|(_, d)| d.bytes == bytes)
  {
    return DataSym(i as u32);
  }
  let sym = DataSym(frag.data.len() as u32);
  frag.data.push(DataDecl {
    bytes: bytes.to_vec(),
    display: None,
  });
  sym
}

/// Attach / overwrite the origin on an already-pushed instruction.
/// Used by lowering when the CPS origin for a node is known *after*
/// the helper that created it already ran.
pub fn set_origin(frag: &mut Fragment, id: InstrId, origin: crate::sourcemap::native::ByteRange) {
  frag.instrs[id.0 as usize].origin = Some(origin);
}

// ──────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
  use super::*;

  /// Hand-build a `Fragment` shaped like what lowering would produce
  /// for the tracer target program:
  ///
  /// ```text
  ///   main = fn: 42 + 123
  /// ```
  ///
  /// Goal: exercise the construction surface; no emit/link/run yet.
  /// The test asserts structural invariants of the built fragment,
  /// not a round-tripped WAT text.
  ///
  /// Shape modelled (CPS-lowered, simplified):
  /// ```text
  ///   ;; runtime types, imported
  ///   ;;   (type $Num (struct (field f64 "val")))
  ///   ;;   (type $Fn2 (func (param anyref anyref)))
  ///   ;; runtime funcs, imported
  ///   ;;   (import "@fink/runtime" "num_op_add"
  ///   ;;     (func (param anyref anyref anyref)))  ;; done, a, b
  ///   ;;   (import "@fink/runtime" "_apply"
  ///   ;;     (func (param anyref anyref)))         ;; args, callee
  ///   ;;
  ///   ;; user: a `Fn2` body that computes 42 + 123 and tail-calls done
  ///   (func $main (param $_caps anyref) (param $_args anyref)
  ///     (local $done  anyref)
  ///     (local $a     (ref $Num))
  ///     (local $b     (ref $Num))
  ///     local.set $done (list_head_any $_args)   ;; elided — pretend $_args[0]
  ///     local.set $a (struct.new $Num (f64.const 42))
  ///     local.set $b (struct.new $Num (f64.const 123))
  ///     return_call $num_op_add (local.get $done, local.get $a, local.get $b)
  ///   )
  /// ```
  ///
  /// We don't model the `list_head_any` step — the test starts with
  /// `done` already in a local. That's enough to exercise struct.new,
  /// multi-arg return_call, and cross-fragment symbolic calls.
  #[test]
  fn tracer_42_plus_123_shape() {
    let mut frag = Fragment::default();

    // Runtime types, imported (for our purposes, just declared locally
    // — the linker will unify by name at link time).
    let num_ty = ty_struct(
      &mut frag,
      vec![field(val_f64(), false, "val")],
      "Num",
    );
    let fn2_sig = ty_func(
      &mut frag,
      vec![val_anyref(true), val_anyref(true)],
      vec![],
      "Fn2",
    );
    // num_op_add takes (done_cont, a, b) — all anyref.
    let num_add_sig = ty_func(
      &mut frag,
      vec![val_anyref(true), val_anyref(true), val_anyref(true)],
      vec![],
      "NumOpAddSig",
    );

    // Imported runtime functions.
    let num_op_add = import_func(&mut frag, num_add_sig, "@fink/runtime", "num_op_add");

    // Function body: build the three statements first.
    let l_done = LocalIdx(2); // param 0 = caps, param 1 = args, local 2 = done
    let l_a    = LocalIdx(3);
    let l_b    = LocalIdx(4);

    // local.set $a (struct.new $Num (f64.const 42))
    let i1 = push_struct_new(&mut frag, num_ty, vec![op_f64(42.0)], l_a);
    // local.set $b (struct.new $Num (f64.const 123))
    let i2 = push_struct_new(&mut frag, num_ty, vec![op_f64(123.0)], l_b);
    // return_call $num_op_add ($done, $a, $b)
    let i3 = push_return_call(
      &mut frag,
      num_op_add,
      vec![op_local(l_done), op_local(l_a), op_local(l_b)],
    );

    let main_fn = func(
      &mut frag,
      fn2_sig,
      vec![
        local(val_anyref(true), "_caps"),
        local(val_anyref(true), "_args"),
      ],
      vec![
        local(val_anyref(true), "done"),
        local(val_ref(num_ty, false), "a"),
        local(val_ref(num_ty, false), "b"),
      ],
      vec![i1, i2, i3],
      "main",
    );

    // --- structural assertions ---
    assert_eq!(frag.types.len(), 3, "three types: Num, Fn2, NumOpAddSig");
    assert_eq!(frag.funcs.len(), 2, "one import + one user func");
    assert_eq!(frag.instrs.len(), 3, "three statement-level instrs");
    assert_eq!(main_fn, FuncSym(1), "main is second func (after import)");

    // num_op_add is an import.
    assert!(frag.funcs[num_op_add.0 as usize].import.is_some());
    assert!(frag.funcs[main_fn.0 as usize].import.is_none());

    // main's body references the three instrs in order.
    let main = &frag.funcs[main_fn.0 as usize];
    assert_eq!(main.body, vec![i1, i2, i3]);
    assert_eq!(main.params.len(), 2);
    assert_eq!(main.locals.len(), 3);

    // Last instr is a ReturnCall into num_op_add with three args.
    let last = &frag.instrs[i3.0 as usize];
    match &last.kind {
      InstrKind::ReturnCall { target, args } => {
        assert_eq!(*target, num_op_add);
        assert_eq!(args.len(), 3);
        assert!(matches!(args[0], Operand::Local(LocalIdx(2))));
      }
      _ => panic!("expected ReturnCall, got {:?}", last),
    }
  }
}
