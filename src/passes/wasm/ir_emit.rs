//! IR → WASM bytes, with runtime-ir.wasm spliced in.
//!
//! Takes a *linked* user [`Fragment`] and produces a final standalone
//! WASM binary by splicing it onto the merged runtime bundle
//! (`runtime-ir.wasm`). The runtime provides the complete type
//! vocabulary, all the rt/std/interop functions, env imports, globals,
//! data, and element segments. The user fragment contributes its own
//! function signatures, functions, memory, and exports.
//!
//! # Algorithm
//!
//! 1. Parse runtime-ir.wasm with wasmparser. Build a lookup table from
//!    qualified export name (`<fragment-url>:<name>`, e.g.
//!    `"std/list.wat:args_head"`) to concrete index + kind.
//! 2. Walk the user fragment's imports. For each `ImportKey { module,
//!    name }`, compose the qualified key `"{module}:{name}"` and look
//!    it up in the runtime's export table. Record `TypeSym → runtime
//!    type index` and `FuncSym → runtime func index` remaps. Imports
//!    are resolved: no import entries get emitted for them.
//! 3. Emit the merged module via wasm-encoder:
//!    - type section: runtime types + user's locally-declared types
//!    - import section: runtime's env imports (host-facing) only
//!    - function section: runtime func sigs + user's local func sigs
//!    - memory section: user's memory (runtime has none today)
//!    - global section: runtime globals (passthrough)
//!    - export section: runtime exports (passthrough) + user exports
//!    - element section: runtime elements (passthrough)
//!    - code section: runtime code bodies (raw) + user code bodies
//!    - data section: user data (runtime has none today)
//!    - name section: runtime names (passthrough, minus ones shifted)
//!
//! # Scope (tracer phase)
//!
//! Only the IR constructs `ir_lower` currently produces need to be
//! emitted. Grow by demand.
//!
//! # Non-scope
//!
//! * DWARF / sourcemap emission into the final binary.
//! * Multi-fragment merge (`ir_link` still single-fragment passthrough).

use std::collections::HashMap;
use std::sync::OnceLock;

use wasm_encoder::{
  AbstractHeapType, CodeSection, CompositeInnerType, CompositeType, ExportKind,
  ExportSection, FieldType, FuncType, Function, FunctionSection,
  HeapType, ImportSection, Instruction, MemorySection, MemoryType,
  Module as WasmModule, RefType, StorageType, StructType, SubType, TypeSection,
  ValType as WEValType,
};

use super::ir::*;

// ──────────────────────────────────────────────────────────────────
// Runtime bundle — compiled at build time, spliced at emit time.
// ──────────────────────────────────────────────────────────────────

static RUNTIME_IR_WASM: &[u8] =
  include_bytes!(concat!(env!("OUT_DIR"), "/runtime-ir.wasm"));

fn runtime() -> &'static Runtime {
  static CELL: OnceLock<Runtime> = OnceLock::new();
  CELL.get_or_init(|| parse_runtime(RUNTIME_IR_WASM))
}

/// Parsed runtime bundle — everything ir_emit needs to splice.
///
/// Indices in the vectors mirror the runtime's own WASM indices in
/// each namespace (types, funcs, ...), so the runtime's exports,
/// code, and element sections can be forwarded verbatim.
struct Runtime {
  /// One entry per rec group in the order they appear in the
  /// runtime's type section. Singleton types are stored as
  /// one-element groups too — the encoder emits them as either an
  /// explicit `(rec ...)` or a standalone type based on `explicit`.
  type_groups: Vec<TypeGroup>,
  /// Total number of SubTypes across all groups.
  type_count: u32,
  /// Map from the WAT `$name` (from the custom name section's type
  /// subsection) to the type's module index. Used for resolving user
  /// type imports via (module, name) → name → index.
  type_by_name: HashMap<String, u32>,
  /// Host-facing imports from module `"env"`. Forwarded verbatim.
  env_imports: Vec<EnvImport>,
  /// Type indices for each local function (not including imports).
  /// Indexed by `local_func_idx`, which equals `global_func_idx -
  /// import_count`.
  local_func_sigs: Vec<u32>,
  /// Total function count = imports + local funcs. New user funcs
  /// get appended after this.
  func_count_total: u32,
  /// Map from qualified export name → (kind, index). Key format is
  /// `"<fragment-url>:<name>"` for cross-fragment exports, bare for
  /// interop exports. ir_emit composes the same key from
  /// user fragment `ImportKey` entries.
  export_by_name: HashMap<String, (ExportKind, u32)>,
  /// Exports to forward verbatim into the merged module's export
  /// section.
  exports_to_forward: Vec<(String, ExportKind, u32)>,
  /// Raw bytes of the runtime's global section body (including the
  /// leading count LEB128). `None` if no global section.
  globals_body: Option<Vec<u8>>,
  /// Raw bytes of the runtime's element section body (including
  /// count LEB128). Same shape.
  elements_body: Option<Vec<u8>>,
  /// Raw bytes of each local function's body as they appear in the
  /// code section (ready for `CodeSection::raw`).
  code_bodies_raw: Vec<Vec<u8>>,
}

struct TypeGroup {
  types: Vec<SubType>,
  explicit: bool,
}

struct EnvImport {
  module: String,
  name: String,
  entity: wasm_encoder::EntityType,
}

fn parse_runtime(bytes: &[u8]) -> Runtime {
  let mut type_groups: Vec<TypeGroup> = Vec::new();
  let mut type_count: u32 = 0;
  let mut type_by_name: HashMap<String, u32> = HashMap::new();
  let mut env_imports: Vec<EnvImport> = Vec::new();
  let mut local_func_sigs: Vec<u32> = Vec::new();
  let mut import_count: u32 = 0;
  let mut exports_all: Vec<(String, ExportKind, u32)> = Vec::new();
  let mut export_by_name: HashMap<String, (ExportKind, u32)> = HashMap::new();
  let mut globals_body: Option<Vec<u8>> = None;
  let mut elements_body: Option<Vec<u8>> = None;
  let mut code_bodies_raw: Vec<Vec<u8>> = Vec::new();

  for payload in wasmparser::Parser::new(0).parse_all(bytes) {
    let payload = payload.expect("runtime-ir.wasm: parse error");
    match payload {
      wasmparser::Payload::TypeSection(reader) => {
        for rg in reader.into_iter() {
          let rg = rg.expect("runtime-ir.wasm: invalid rec group");
          let explicit = rg.is_explicit_rec_group();
          let types: Vec<SubType> = rg.into_types().map(|st| convert_subtype(&st)).collect();
          type_count += types.len() as u32;
          type_groups.push(TypeGroup { types, explicit });
        }
      }
      wasmparser::Payload::ImportSection(reader) => {
        for group in reader {
          let group = group.expect("runtime-ir.wasm: invalid import");
          match group {
            wasmparser::Imports::Single(_, imp) => {
              let entity = match imp.ty {
                wasmparser::TypeRef::Func(idx) => wasm_encoder::EntityType::Function(idx),
                _ => panic!("runtime-ir.wasm: non-func import not yet supported"),
              };
              assert_eq!(imp.module, "env",
                "runtime-ir.wasm: unexpected non-env import `{}`.`{}`", imp.module, imp.name);
              env_imports.push(EnvImport {
                module: imp.module.to_string(),
                name: imp.name.to_string(),
                entity,
              });
              import_count += 1;
            }
            wasmparser::Imports::Compact1 { module: mod_name, items } => {
              for item in items.into_iter().flatten() {
                let entity = match item.ty {
                  wasmparser::TypeRef::Func(idx) => wasm_encoder::EntityType::Function(idx),
                  _ => panic!("runtime-ir.wasm: non-func import not yet supported"),
                };
                assert_eq!(mod_name, "env",
                  "runtime-ir.wasm: unexpected non-env import `{}`.`{}`", mod_name, item.name);
                env_imports.push(EnvImport {
                  module: mod_name.to_string(),
                  name: item.name.to_string(),
                  entity,
                });
                import_count += 1;
              }
            }
            _ => panic!("runtime-ir.wasm: unsupported import group variant"),
          }
        }
      }
      wasmparser::Payload::FunctionSection(reader) => {
        for sig in reader {
          local_func_sigs.push(sig.expect("runtime-ir.wasm: invalid func sig"));
        }
      }
      wasmparser::Payload::GlobalSection(reader) => {
        if reader.count() > 0 {
          globals_body = Some(bytes[reader.range()].to_vec());
        }
      }
      wasmparser::Payload::ExportSection(reader) => {
        for exp in reader {
          let exp = exp.expect("runtime-ir.wasm: invalid export");
          let kind = match exp.kind {
            wasmparser::ExternalKind::Func | wasmparser::ExternalKind::FuncExact => ExportKind::Func,
            wasmparser::ExternalKind::Table => ExportKind::Table,
            wasmparser::ExternalKind::Memory => ExportKind::Memory,
            wasmparser::ExternalKind::Global => ExportKind::Global,
            wasmparser::ExternalKind::Tag => ExportKind::Tag,
          };
          let name = exp.name.to_string();
          exports_all.push((name.clone(), kind, exp.index));
          export_by_name.insert(name, (kind, exp.index));
        }
      }
      wasmparser::Payload::ElementSection(reader) => {
        if reader.count() > 0 {
          elements_body = Some(bytes[reader.range()].to_vec());
        }
      }
      wasmparser::Payload::CodeSectionEntry(body) => {
        let range = body.range();
        code_bodies_raw.push(bytes[range].to_vec());
      }
      wasmparser::Payload::CustomSection(reader) => {
        if let wasmparser::KnownCustom::Name(name_reader) = reader.as_known() {
          for name in name_reader.into_iter().flatten() {
            if let wasmparser::Name::Type(map) = name {
              for n in map.into_iter().flatten() {
                type_by_name.insert(n.name.to_string(), n.index);
              }
            }
          }
        }
      }
      _ => {}
    }
  }

  Runtime {
    type_groups,
    type_count,
    type_by_name,
    env_imports,
    local_func_sigs,
    func_count_total: import_count + code_bodies_raw.len() as u32,
    export_by_name,
    exports_to_forward: exports_all,
    globals_body,
    elements_body,
    code_bodies_raw,
  }
}

// ── wasmparser → wasm-encoder type converters ──────────────────────

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

/// Emit a linked user Fragment as a final standalone WASM binary,
/// with runtime-ir.wasm spliced in as the prefix.
pub fn emit(frag: &Fragment) -> Vec<u8> {
  let rt = runtime();


  // ── resolve user type imports + plan local-type indices ──────
  //
  // Each user `TypeSym` maps to a concrete final type index:
  // - Imported: resolved against runtime's export table by
  //   composed key `<module>:<name>`. The runtime's type exports
  //   (value-types that cross the ABI) are registered in
  //   `export_by_name` with kind Global/Func/etc — actually types
  //   aren't directly exportable in stock WASM, so we use the
  //   type-name custom section for this resolution.
  // - Local: appended to the type section after runtime's types.

  let mut type_remap: Vec<u32> = Vec::with_capacity(frag.types.len());
  let mut user_local_types: Vec<(TypeSym, &TypeDecl)> = Vec::new();
  for (i, ty) in frag.types.iter().enumerate() {
    match &ty.import {
      Some(ImportKey { module, name }) => {
        // Compose qualified key and look up as a type by name.
        let qualified = format!("{module}:{name}");
        let idx = rt.type_by_name.get(&qualified).copied()
          .or_else(|| rt.type_by_name.get(name.as_str()).copied())
          .unwrap_or_else(|| panic!(
            "ir_emit: unknown runtime type import `{}`. Not found as `{}` or `{}` in type-name table",
            qualified, qualified, name));
        type_remap.push(idx);
      }
      None => {
        // Placeholder — filled in after we know how many we have.
        type_remap.push(u32::MAX);
        user_local_types.push((TypeSym(i as u32), ty));
      }
    }
  }
  for (seq, (sym, _)) in user_local_types.iter().enumerate() {
    type_remap[sym.0 as usize] = rt.type_count + seq as u32;
  }

  // ── resolve user function imports + plan local-func indices ──

  let mut func_remap: Vec<u32> = Vec::with_capacity(frag.funcs.len());
  let mut user_local_funcs: Vec<(FuncSym, &FuncDecl)> = Vec::new();
  for (i, f) in frag.funcs.iter().enumerate() {
    match &f.import {
      Some(ImportKey { module, name }) => {
        let qualified = format!("{module}:{name}");
        let (kind, idx) = rt.export_by_name.get(&qualified)
          .or_else(|| rt.export_by_name.get(name.as_str()))
          .unwrap_or_else(|| panic!(
            "ir_emit: unknown runtime func import `{}`. Not found in runtime export table",
            qualified))
          .clone();
        assert_eq!(kind, ExportKind::Func,
          "ir_emit: expected func export for `{}`, got {:?}", qualified, kind);
        func_remap.push(idx);
      }
      None => {
        func_remap.push(u32::MAX);
        user_local_funcs.push((FuncSym(i as u32), f));
      }
    }
  }
  for (seq, (sym, _)) in user_local_funcs.iter().enumerate() {
    func_remap[sym.0 as usize] = rt.func_count_total + seq as u32;
  }

  // ── emit sections ─────────────────────────────────────────────

  let mut type_sec = TypeSection::new();
  for group in &rt.type_groups {
    if group.explicit || group.types.len() > 1 {
      type_sec.ty().rec(group.types.iter().cloned());
    } else {
      type_sec.ty().subtype(&group.types[0]);
    }
  }
  for (_, ty) in &user_local_types {
    match &ty.kind {
      TypeKind::Func { params, results } => {
        let we_params: Vec<WEValType> = params.iter().map(|v| val_from_ir(v, &type_remap)).collect();
        let we_results: Vec<WEValType> = results.iter().map(|v| val_from_ir(v, &type_remap)).collect();
        type_sec.ty().function(we_params, we_results);
      }
      _ => panic!("ir_emit: only locally-declared func types supported (got {:?})", ty.kind),
    }
  }

  let mut import_sec = ImportSection::new();
  for imp in &rt.env_imports {
    import_sec.import(&imp.module, &imp.name, imp.entity);
  }

  let mut func_sec = FunctionSection::new();
  for &sig in &rt.local_func_sigs {
    func_sec.function(sig);
  }
  for (_, f) in &user_local_funcs {
    func_sec.function(type_remap[f.sig.0 as usize]);
  }

  // Memory: runtime has none; user fragment brings one page.
  let mut mem_sec = MemorySection::new();
  mem_sec.memory(MemoryType {
    minimum: 1,
    maximum: None,
    memory64: false,
    shared: false,
    page_size_log2: None,
  });

  // Globals: splice runtime's global section body as-is (it already
  // includes its count LEB128 prefix). The runtime's global
  // init_exprs use struct.new / array.new_fixed which aren't
  // constructible via wasm-encoder's ConstExpr API.
  let globals_section_body = rt.globals_body.as_deref();

  // Exports: forward runtime's, then append user's (remapped).
  let mut export_sec = ExportSection::new();
  for (name, kind, idx) in &rt.exports_to_forward {
    export_sec.export(name, *kind, *idx);
  }
  for (i, f) in frag.funcs.iter().enumerate() {
    if let Some(name) = &f.export {
      export_sec.export(name, ExportKind::Func, func_remap[i]);
    }
  }

  // Elements: raw-splice same as globals.
  let elements_section_body = rt.elements_body.as_deref();

  // Code: runtime's bodies raw, then user's bodies encoded.
  let mut code_sec = CodeSection::new();
  for body in &rt.code_bodies_raw {
    code_sec.raw(body);
  }
  for (_, f) in &user_local_funcs {
    let func = emit_func(frag, f, &type_remap, &func_remap);
    code_sec.function(&func);
  }

  // Data: user fragment may have strings; runtime has none today.
  // (Data sections built from Fragment.data would go here.)

  // ── finalise module ──────────────────────────────────────────
  // WASM section IDs — from the spec.
  const SECTION_GLOBAL: u8 = 6;
  const SECTION_ELEMENT: u8 = 9;

  let mut module = WasmModule::new();
  module.section(&type_sec);
  module.section(&import_sec);
  module.section(&func_sec);
  module.section(&mem_sec);
  if let Some(body) = globals_section_body {
    module.section(&wasm_encoder::RawSection { id: SECTION_GLOBAL, data: body });
  }
  module.section(&export_sec);
  if let Some(body) = elements_section_body {
    module.section(&wasm_encoder::RawSection { id: SECTION_ELEMENT, data: body });
  }
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
    InstrKind::RefI31 { src, into } => {
      emit_operand(func, src, type_remap, func_remap);
      func.instruction(&Instruction::RefI31);
      func.instruction(&Instruction::LocalSet(into.0));
    }
    InstrKind::RefNull { .. }
    | InstrKind::RefNullConcrete { .. }
    | InstrKind::I31GetS { .. }
    | InstrKind::RefFunc { .. }
    | InstrKind::ArrayNewFixed { .. }
    | InstrKind::ArrayGet { .. }
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
