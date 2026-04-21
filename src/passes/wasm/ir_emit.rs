//! IR → WASM bytes.
//!
//! Takes a *linked* [`Fragment`] and produces a user-side WASM
//! module. The result is handed to the existing static linker
//! (`wasm::link::link`) alongside `runtime.wasm` to produce the
//! final standalone binary.
//!
//! # The `rt/*` ABI
//!
//! CPS/lower owns the runtime ABI — the emitter **dictates** the
//! function signatures; the runtime WAT is an *implementation*.
//! Signatures are defined in `runtime_contract.rs` as locally-
//! declared function types with the `rt.` naming prefix
//! (`$rt.FnAnyToAny`, `$rt.FnBinOp`, …). WASM structural
//! equivalence matches them against the runtime's signatures at
//! link time.
//!
//! Only **value-type imports** (`rt.Num`, `rt.Fn2`) are genuine IR
//! type imports. Their identity is shared across the ABI — a user
//! `struct.new $rt.Num` must point at runtime's `$Num` type index.
//! Emit resolves them by looking up `types.wasm` (the build-time
//! canonical-types artefact).
//!
//! Every `rt.<fn>` function import becomes a real WASM import
//! against `"@fink/runtime"`, referencing the emitter-declared
//! signature type.
//!
//! # Scope (tracer phase)
//!
//! Only the IR constructs `ir_lower` currently produces need to be
//! emitted. Grow by demand.
//!
//! # Non-scope
//!
//! * DWARF / sourcemap emission into the final binary. The Fragment
//!   already carries origins; threading them into WASM custom
//!   sections is follow-up.
//! * Multi-fragment merge (`ir_link` still single-fragment).

use std::collections::HashMap;
use std::sync::OnceLock;

use wasm_encoder::{
  AbstractHeapType, CodeSection, CompositeInnerType, CompositeType, ExportKind,
  ExportSection, FieldType, FuncType, Function, FunctionSection, HeapType,
  ImportSection, Instruction, MemorySection, MemoryType, Module as WasmModule,
  RefType, StorageType, SubType, StructType, TypeSection, ValType as WEValType,
};

use super::ir::*;

/// Real WASM module name for the compiler's runtime ABI. The IR
/// uses `"rt"` for readability; at emit time we translate to
/// `"@fink/runtime"` to match the existing linker's contract.
const RT_IR: &str = "rt";
const RT_WASM: &str = "@fink/runtime";

// ──────────────────────────────────────────────────────────────────
// Runtime type + function dictionaries — sourced from runtime.wasm
// and types.wasm at build time.
// ──────────────────────────────────────────────────────────────────

static CANONICAL_TYPES_WASM: &[u8] =
  include_bytes!(concat!(env!("OUT_DIR"), "/types.wasm"));

/// Parsed canonical value-types rec group, plus a name→idx map for
/// resolving `rt.<TypeName>` value-type imports.
struct CanonTypes {
  rec_group: Vec<SubType>,
  by_name: HashMap<String, u32>,
}

fn canon_types() -> &'static CanonTypes {
  static CELL: OnceLock<CanonTypes> = OnceLock::new();
  CELL.get_or_init(parse_canon_types)
}

fn parse_canon_types() -> CanonTypes {
  let mut rec_group = Vec::new();
  let mut type_names: HashMap<u32, String> = HashMap::new();

  for payload in wasmparser::Parser::new(0).parse_all(CANONICAL_TYPES_WASM) {
    match payload.expect("invalid canonical types WASM") {
      wasmparser::Payload::TypeSection(reader) => {
        for rg in reader.into_iter() {
          let rg = rg.expect("invalid rec group in canonical types");
          for st in rg.into_types() {
            rec_group.push(convert_subtype(&st));
          }
        }
      }
      wasmparser::Payload::CustomSection(reader) => {
        if let wasmparser::KnownCustom::Name(name_reader) = reader.as_known() {
          for name in name_reader.into_iter().flatten() {
            if let wasmparser::Name::Type(map) = name {
              for n in map.into_iter().flatten() {
                type_names.insert(n.index, n.name.to_string());
              }
            }
          }
        }
      }
      _ => {}
    }
  }

  let by_name = type_names.into_iter()
    .map(|(idx, name)| (name, idx))
    .collect();

  CanonTypes { rec_group, by_name }
}

// ── wasmparser → wasm-encoder type converters (local copies) ──────
//
// Keeping these local avoids coupling to `emit.rs`'s internals.
// Only used to parse the build-time canonical-types artefact; once
// that's read, the rest of emit walks the Fragment directly.

fn convert_subtype(st: &wasmparser::SubType) -> SubType {
  SubType {
    is_final: st.is_final,
    supertype_idx: st.supertype_idx.map(|i| i.as_module_index().unwrap_or(0)),
    composite_type: convert_composite(&st.composite_type),
  }
}

fn convert_composite(ct: &wasmparser::CompositeType) -> CompositeType {
  CompositeType {
    inner: match &ct.inner {
      wasmparser::CompositeInnerType::Func(f) => {
        CompositeInnerType::Func(FuncType::new(
          f.params().iter().map(|vt| convert_val(*vt)).collect::<Vec<_>>(),
          f.results().iter().map(|vt| convert_val(*vt)).collect::<Vec<_>>(),
        ))
      }
      wasmparser::CompositeInnerType::Struct(s) => {
        CompositeInnerType::Struct(StructType {
          fields: s.fields.iter().map(convert_field).collect(),
        })
      }
      wasmparser::CompositeInnerType::Array(a) => {
        CompositeInnerType::Array(wasm_encoder::ArrayType(convert_field(&a.0)))
      }
      wasmparser::CompositeInnerType::Cont(_) => {
        panic!("ir_emit: continuation types not supported")
      }
    },
    shared: ct.shared,
    descriptor: None,
    describes: None,
  }
}

fn convert_val(vt: wasmparser::ValType) -> WEValType {
  match vt {
    wasmparser::ValType::I32 => WEValType::I32,
    wasmparser::ValType::I64 => WEValType::I64,
    wasmparser::ValType::F32 => WEValType::F32,
    wasmparser::ValType::F64 => WEValType::F64,
    wasmparser::ValType::V128 => WEValType::V128,
    wasmparser::ValType::Ref(rt) => WEValType::Ref(convert_ref(rt)),
  }
}

fn convert_ref(rt: wasmparser::RefType) -> RefType {
  RefType {
    nullable: rt.is_nullable(),
    heap_type: match rt.heap_type() {
      wasmparser::HeapType::Abstract { shared, ty } => HeapType::Abstract {
        shared,
        ty: convert_abs_heap(ty),
      },
      wasmparser::HeapType::Concrete(idx) =>
        HeapType::Concrete(idx.as_module_index().unwrap_or(0)),
      wasmparser::HeapType::Exact(idx) =>
        HeapType::Concrete(idx.as_module_index().unwrap_or(0)),
    },
  }
}

fn convert_abs_heap(ty: wasmparser::AbstractHeapType) -> AbstractHeapType {
  match ty {
    wasmparser::AbstractHeapType::Func => AbstractHeapType::Func,
    wasmparser::AbstractHeapType::Extern => AbstractHeapType::Extern,
    wasmparser::AbstractHeapType::Any => AbstractHeapType::Any,
    wasmparser::AbstractHeapType::None => AbstractHeapType::None,
    wasmparser::AbstractHeapType::NoExtern => AbstractHeapType::NoExtern,
    wasmparser::AbstractHeapType::NoFunc => AbstractHeapType::NoFunc,
    wasmparser::AbstractHeapType::Eq => AbstractHeapType::Eq,
    wasmparser::AbstractHeapType::Struct => AbstractHeapType::Struct,
    wasmparser::AbstractHeapType::Array => AbstractHeapType::Array,
    wasmparser::AbstractHeapType::I31 => AbstractHeapType::I31,
    wasmparser::AbstractHeapType::Exn => AbstractHeapType::Exn,
    wasmparser::AbstractHeapType::NoExn => AbstractHeapType::NoExn,
    wasmparser::AbstractHeapType::Cont => AbstractHeapType::Cont,
    wasmparser::AbstractHeapType::NoCont => AbstractHeapType::NoCont,
  }
}

fn convert_field(f: &wasmparser::FieldType) -> FieldType {
  FieldType {
    element_type: match f.element_type {
      wasmparser::StorageType::I8 => StorageType::I8,
      wasmparser::StorageType::I16 => StorageType::I16,
      wasmparser::StorageType::Val(vt) => StorageType::Val(convert_val(vt)),
    },
    mutable: f.mutable,
  }
}

// ──────────────────────────────────────────────────────────────────
// Emit
// ──────────────────────────────────────────────────────────────────

/// Emit a linked user Fragment as WASM bytes, ready to be handed to
/// the existing static linker alongside `runtime.wasm`.
pub fn emit(frag: &Fragment) -> Vec<u8> {
  let canon = canon_types();

  // ── plan type section ─────────────────────────────────────────
  //
  // Start with the canonical rec group from `types.wasm` — the
  // shared type table that both user and runtime fragments agree
  // on. Then append any **locally-declared** types from the
  // fragment (function-signature types with the `rt.` ABI prefix,
  // plus any future user-declared types).

  let mut type_sec = TypeSection::new();
  type_sec.ty().rec(canon.rec_group.clone());
  let canon_count = canon.rec_group.len() as u32;

  // Remap `TypeSym → final wasm type index`.
  //
  // * Value-type imports (`rt.Num`, `rt.Fn2`): resolve against
  //   canonical types by name.
  // * Locally-declared types (structural): append to the type
  //   section after the canonical rec group.

  let mut type_remap: Vec<u32> = Vec::with_capacity(frag.types.len());
  let mut extra_type_count: u32 = 0;

  for ty in &frag.types {
    let final_idx = match &ty.import {
      Some(ImportKey { module, name }) if module == RT_IR => {
        // Value-type import — look up in canonical-types dict.
        *canon.by_name.get(name).unwrap_or_else(|| {
          panic!("ir_emit: unknown rt value-type import `{}`", name)
        })
      }
      Some(other) => panic!(
        "ir_emit: non-rt type import not yet supported: {}/{}",
        other.module, other.name
      ),
      None => {
        // Locally-declared type — append structurally.
        let idx = canon_count + extra_type_count;
        extra_type_count += 1;
        match &ty.kind {
          TypeKind::Func { params, results } => {
            let we_params: Vec<WEValType> = params.iter().map(|v| val_from_ir(v, &type_remap)).collect();
            let we_results: Vec<WEValType> = results.iter().map(|v| val_from_ir(v, &type_remap)).collect();
            type_sec.ty().function(we_params, we_results);
          }
          _ => panic!("ir_emit: only locally-declared func types supported so far"),
        }
        idx
      }
    };
    type_remap.push(final_idx);
  }

  // ── plan function imports + local funcs ──────────────────────
  //
  // Imports come first in WASM's function index space. Walk once
  // to enumerate imports (assign low indices) and local funcs
  // (assign indices after the imports).

  let mut import_sec = ImportSection::new();
  let mut func_remap: Vec<u32> = Vec::with_capacity(frag.funcs.len());
  let mut import_count: u32 = 0;
  let mut local_func_sigs: Vec<u32> = Vec::new();
  let mut local_func_slots: Vec<usize> = Vec::new();

  for (i, f) in frag.funcs.iter().enumerate() {
    if let Some(ImportKey { module, name }) = &f.import {
      assert!(module == RT_IR, "ir_emit: non-rt func import not supported yet");
      let sig_ty = type_remap[f.sig.0 as usize];
      import_sec.import(RT_WASM, name, wasm_encoder::EntityType::Function(sig_ty));
      func_remap.push(import_count);
      import_count += 1;
      let _ = i;
    } else {
      func_remap.push(u32::MAX); // fill in the second pass
    }
  }

  for (i, f) in frag.funcs.iter().enumerate() {
    if f.import.is_none() {
      let final_idx = import_count + local_func_sigs.len() as u32;
      func_remap[i] = final_idx;
      local_func_sigs.push(type_remap[f.sig.0 as usize]);
      local_func_slots.push(i);
    }
  }

  // ── function section (declares types of local funcs) ──────────
  let mut func_sec = FunctionSection::new();
  for sig in &local_func_sigs {
    func_sec.function(*sig);
  }

  // ── code section (bodies of local funcs) ──────────────────────
  let mut code_sec = CodeSection::new();
  for &slot in &local_func_slots {
    let f = &frag.funcs[slot];
    let func = emit_func(frag, f, &type_remap, &func_remap);
    code_sec.function(&func);
  }

  // ── memory section ────────────────────────────────────────────
  //
  // Runtime.wasm declares no memory of its own — every fink
  // program's user fragment brings the sole memory. One-page
  // minimum covers string intern data; wasm-opt / linker will
  // grow as needed.
  let mut mem_sec = MemorySection::new();
  mem_sec.memory(MemoryType {
    minimum: 1,
    maximum: None,
    memory64: false,
    shared: false,
    page_size_log2: None,
  });

  // ── export section ────────────────────────────────────────────
  let mut export_sec = ExportSection::new();
  for (i, f) in frag.funcs.iter().enumerate() {
    if let Some(name) = &f.export {
      export_sec.export(name, ExportKind::Func, func_remap[i]);
    }
  }

  // ── finalise module ───────────────────────────────────────────
  let mut module = WasmModule::new();
  module.section(&type_sec);
  module.section(&import_sec);
  module.section(&func_sec);
  module.section(&mem_sec);
  module.section(&export_sec);
  module.section(&code_sec);
  module.finish()
}

// ──────────────────────────────────────────────────────────────────
// Function body emission
// ──────────────────────────────────────────────────────────────────

fn emit_func(
  frag: &Fragment,
  f: &FuncDecl,
  type_remap: &[u32],
  func_remap: &[u32],
) -> Function {
  // Locals declaration: wasm-encoder wants (count, val_type) groups.
  // One entry per local — don't coalesce consecutive same-type
  // locals. wasm-opt will tidy up later if it matters.
  let mut locals: Vec<(u32, WEValType)> = Vec::new();
  for l in &f.locals {
    locals.push((1, val_from_ir(&l.ty, type_remap)));
  }
  let mut func = Function::new(locals);
  for &id in &f.body {
    emit_instr(&mut func, frag, &frag.instrs[id.0 as usize], type_remap, func_remap);
  }
  func.instruction(&Instruction::End);
  func
}

fn emit_instr(
  func: &mut Function,
  _frag: &Fragment,
  instr: &Instr,
  type_remap: &[u32],
  func_remap: &[u32],
) {
  match &instr.kind {
    InstrKind::LocalSet { idx, src } => {
      emit_operand(func, src, type_remap, func_remap);
      func.instruction(&Instruction::LocalSet(idx.0));
    }
    InstrKind::GlobalSet { sym, src } => {
      emit_operand(func, src, type_remap, func_remap);
      // TODO: global remap. Not needed for tracer programs.
      func.instruction(&Instruction::GlobalSet(sym.0));
    }
    InstrKind::StructNew { ty, fields, into } => {
      for fld in fields {
        emit_operand(func, fld, type_remap, func_remap);
      }
      func.instruction(&Instruction::StructNew(type_remap[ty.0 as usize]));
      func.instruction(&Instruction::LocalSet(into.0));
    }
    InstrKind::Call { target, args, into } => {
      for a in args {
        emit_operand(func, a, type_remap, func_remap);
      }
      func.instruction(&Instruction::Call(func_remap[target.0 as usize]));
      if let Some(l) = into {
        func.instruction(&Instruction::LocalSet(l.0));
      }
    }
    InstrKind::ReturnCall { target, args } => {
      for a in args {
        emit_operand(func, a, type_remap, func_remap);
      }
      func.instruction(&Instruction::ReturnCall(func_remap[target.0 as usize]));
    }
    // Grow as fixtures demand:
    InstrKind::RefNull { .. }
    | InstrKind::RefNullConcrete { .. }
    | InstrKind::RefI31 { .. }
    | InstrKind::I31GetS { .. }
    | InstrKind::RefFunc { .. }
    | InstrKind::RefCastNonNull { .. }
    | InstrKind::RefCastNullable { .. }
    | InstrKind::If { .. }
    | InstrKind::Unreachable
    | InstrKind::Drop { .. } => {
      panic!("ir_emit: InstrKind {:?} not yet implemented", instr.kind);
    }
  }
}

fn emit_operand(
  func: &mut Function,
  op: &Operand,
  _type_remap: &[u32],
  func_remap: &[u32],
) {
  match op {
    Operand::I32(v) => { func.instruction(&Instruction::I32Const(*v)); }
    Operand::F64(v) => { func.instruction(&Instruction::F64Const((*v).into())); }
    Operand::Local(idx) => { func.instruction(&Instruction::LocalGet(idx.0)); }
    Operand::Global(sym) => {
      // TODO: global remap. Not needed for tracer programs.
      func.instruction(&Instruction::GlobalGet(sym.0));
    }
    Operand::RefFunc(fsym) => {
      func.instruction(&Instruction::RefFunc(func_remap[fsym.0 as usize]));
    }
    Operand::RefNull(ht) => {
      func.instruction(&Instruction::RefNull(HeapType::Abstract {
        shared: false,
        ty: abs_heap_ir(*ht),
      }));
    }
    Operand::DataRef { .. } => {
      panic!("ir_emit: Operand::DataRef not yet implemented");
    }
  }
}

fn abs_heap_ir(h: AbsHeap) -> AbstractHeapType {
  match h {
    AbsHeap::Any  => AbstractHeapType::Any,
    AbsHeap::Eq   => AbstractHeapType::Eq,
    AbsHeap::I31  => AbstractHeapType::I31,
    AbsHeap::Func => AbstractHeapType::Func,
  }
}

fn val_from_ir(v: &ValType, type_remap: &[u32]) -> WEValType {
  match v {
    ValType::I32 => WEValType::I32,
    ValType::F64 => WEValType::F64,
    ValType::RefAbstract { nullable, ht } => WEValType::Ref(RefType {
      nullable: *nullable,
      heap_type: HeapType::Abstract { shared: false, ty: abs_heap_ir(*ht) },
    }),
    ValType::RefConcrete { nullable, ty } => WEValType::Ref(RefType {
      nullable: *nullable,
      heap_type: HeapType::Concrete(type_remap[ty.0 as usize]),
    }),
  }
}
