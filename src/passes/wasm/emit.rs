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
//! Only the IR constructs `lower` currently produces need to be
//! emitted. Grow by demand.
//!
//! # Non-scope
//!
//! * DWARF / sourcemap emission into the final binary.
//! * Multi-fragment merge (`link` still single-fragment passthrough).

use std::collections::HashMap;
use std::sync::OnceLock;

use wasm_encoder::{
  AbstractHeapType, CodeSection, CompositeInnerType, CompositeType, ConstExpr,
  ElementSection, Elements, Encode, ExportKind, ExportSection, FieldType, FuncType,
  Function, FunctionSection, GlobalType, HeapType, ImportSection, Instruction,
  MemorySection, MemoryType, Module as WasmModule, RefType, StorageType, StructType,
  SubType, TypeSection, ValType as WEValType,
};

use super::ir::*;
use super::runtime_contract::import_key;

/// Output of `emit::emit`. The binary plus a per-InstrId map of absolute
/// byte offsets in the binary. Only InstrIds that were tagged with a
/// `cps_id` in lower (and thus need to participate in mark finalisation)
/// are present in the map. Used by Section 5's finalize step to join
/// `DebugMarks` (keyed by CpsId) with the actual emitted PCs.
#[derive(Default)]
pub struct EmitOutput {
  pub binary: Vec<u8>,
  pub instr_offsets: std::collections::BTreeMap<InstrId, u32>,
}

/// Resolve a `TypeSym` to its final merged-binary type index.
///
/// `Local(i)` consults the per-fragment `type_remap` (built at the
/// top of `emit_fragment`). `Runtime(sym)` consults the runtime's
/// type-name table — runtime types live at fixed indices in the
/// merged binary, no per-fragment remap needed.
fn resolve_type(sym: TypeSym, type_remap: &[u32]) -> u32 {
  match sym {
    TypeSym::Local(i) => type_remap[i as usize],
    TypeSym::Runtime(s) => {
      let rt = runtime();
      let key = import_key(s);
      rt.type_by_name.get(key).copied()
        .unwrap_or_else(|| panic!(
          "emit: runtime type `{:?}` ({}) not found in runtime type-name table",
          s, key))
    }
  }
}

/// Resolve a `FuncSym` to its final merged-binary func index.
///
/// `Local(i)` consults the per-fragment `func_remap`. `Runtime(sym)`
/// consults the runtime's export-by-name table.
fn resolve_func(sym: FuncSym, func_remap: &[u32]) -> u32 {
  match sym {
    FuncSym::Local(i) => func_remap[i as usize],
    FuncSym::Runtime(s) => {
      let rt = runtime();
      let key = import_key(s);
      *rt.func_by_name.get(key)
        .unwrap_or_else(|| panic!(
          "emit: runtime func `{:?}` ({}) not found in runtime func-name table",
          s, key))
    }
  }
}

// ──────────────────────────────────────────────────────────────────
// Runtime bundle — linked from .wat sources at first use.
// ──────────────────────────────────────────────────────────────────

/// Reverse of `runtime().func_by_name`: index → qualified name. Used
/// by the public `annotate_func_indices` error helper.
pub fn runtime_func_names() -> HashMap<u32, String> {
  let rt = runtime();
  rt.func_by_name.iter()
    .filter(|(name, _)| name.contains(".wat:"))  // skip fnk-fqn aliases
    .map(|(name, &idx)| (idx, name.clone()))
    .collect()
}

/// The raw merged-runtime wasm bytes — same bytes produced by the
/// linker + wat_crate at first use. Used by the error annotator to
/// resolve byte offsets back to WAT lines via wasmprinter.
pub fn runtime_wasm_bytes() -> &'static [u8] {
  &linked_runtime().bytes
}

struct LinkedRuntime {
  bytes: Vec<u8>,
  impls: HashMap<String, String>,
}

fn linked_runtime() -> &'static LinkedRuntime {
  static CELL: OnceLock<LinkedRuntime> = OnceLock::new();
  CELL.get_or_init(|| {
    let modules: &[(&str, &str)] = &[
      ("interop/rust.wat", include_str!("../../runtime/interop/rust.wat")),
      ("rt/types.wat",     include_str!("../../runtime/rt/types.wat")),
      ("rt/apply.wat",     include_str!("../../runtime/rt/apply.wat")),
      ("rt/modules.wat",   include_str!("../../runtime/rt/modules.wat")),
      ("rt/protocols.wat", include_str!("../../runtime/rt/protocols.wat")),
      ("std/num.wat",      include_str!("../../runtime/std/num.wat")),
      ("std/str.wat",      include_str!("../../runtime/std/str.wat")),
      ("std/list.wat",     include_str!("../../runtime/std/list.wat")),
      ("std/int.wat",      include_str!("../../runtime/std/int.wat")),
      ("std/range.wat",    include_str!("../../runtime/std/range.wat")),
      ("std/set.wat",      include_str!("../../runtime/std/set.wat")),
      ("std/dict.wat",     include_str!("../../runtime/std/dict.wat")),
      ("std/hashing.wat",  include_str!("../../runtime/std/hashing.wat")),
      ("std/channel.wat",  include_str!("../../runtime/std/channel.wat")),
      ("std/async.wat",    include_str!("../../runtime/std/async.wat")),
    ];
    let result = crate::wat_linker::link(modules);
    let mut parser = wat_crate::Parser::new();
    parser.generate_dwarf(wat_crate::GenerateDwarf::Lines);
    let bytes = parser.parse_bytes(None, result.wat.as_bytes())
      .unwrap_or_else(|e| panic!("runtime: merged WAT failed to compile to wasm:\n{e}"))
      .into_owned();
    LinkedRuntime { bytes, impls: result.impls }
  })
}

fn runtime() -> &'static Runtime {
  static CELL: OnceLock<Runtime> = OnceLock::new();
  CELL.get_or_init(|| {
    let lr = linked_runtime();
    parse_runtime(&lr.bytes, &lr.impls)
  })
}

/// Parsed runtime bundle — everything emit needs to splice.
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
  /// Map from runtime func name → func index.
  ///
  /// Populated from the wasm name section's Function subsection. Keys
  /// are the qualified `<url>:<name>` ids the linker emits (the `$`
  /// prefix is stripped at insert time so callers can compose without
  /// it). Bare `(@impl "fqn")` annotations also get inserted under
  /// their fqn key, so emitter resolution accepts either qualified-id
  /// or fnk-fqn lookups uniformly.
  func_by_name: HashMap<String, u32>,
  /// Host-facing exports to forward verbatim into the merged module's
  /// export section. Pulled from the linker's export section (only
  /// host exports survive — internal cross-wat refs are resolved by
  /// id, not by export).
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

fn parse_runtime(bytes: &[u8], impls: &HashMap<String, String>) -> Runtime {
  let mut type_groups: Vec<TypeGroup> = Vec::new();
  let mut type_count: u32 = 0;
  let mut type_by_name: HashMap<String, u32> = HashMap::new();
  let mut env_imports: Vec<EnvImport> = Vec::new();
  let mut local_func_sigs: Vec<u32> = Vec::new();
  let mut import_count: u32 = 0;
  let mut exports_all: Vec<(String, ExportKind, u32)> = Vec::new();
  let mut func_by_name: HashMap<String, u32> = HashMap::new();
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
      wasmparser::Payload::GlobalSection(reader)
        if reader.count() > 0 => {
          globals_body = Some(bytes[reader.range()].to_vec());
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
          exports_all.push((exp.name.to_string(), kind, exp.index));
        }
      }
      wasmparser::Payload::ElementSection(reader)
        if reader.count() > 0 => {
          elements_body = Some(bytes[reader.range()].to_vec());
        }
      wasmparser::Payload::CodeSectionEntry(body) => {
        let range = body.range();
        code_bodies_raw.push(bytes[range].to_vec());
      }
      wasmparser::Payload::CustomSection(reader) => {
        if let wasmparser::KnownCustom::Name(name_reader) = reader.as_known() {
          for name in name_reader.into_iter().flatten() {
            match name {
              wasmparser::Name::Type(map) => {
                for n in map.into_iter().flatten() {
                  type_by_name.insert(n.name.to_string(), n.index);
                }
              }
              wasmparser::Name::Function(map) => {
                for n in map.into_iter().flatten() {
                  func_by_name.insert(n.name.to_string(), n.index);
                }
              }
              _ => {}
            }
          }
        }
      }
      _ => {}
    }
  }

  // Alias every bare `(@impl "fqn")` annotation to its qualified
  // func id, so emitter lookups by fnk fqn resolve to the same index
  // as a lookup by `<url>:<func>`.
  for (fqn, qualified) in impls {
    let id = qualified.strip_prefix('$').unwrap_or(qualified);
    if let Some(&idx) = func_by_name.get(id) {
      func_by_name.insert(fqn.clone(), idx);
    }
  }

  Runtime {
    type_groups,
    type_count,
    type_by_name,
    env_imports,
    local_func_sigs,
    func_count_total: import_count + code_bodies_raw.len() as u32,
    func_by_name,
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
        panic!("emit: continuation types not supported")
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
  emit_with_offsets(frag).binary
}

/// Variant of [`emit`] that also returns the per-InstrId absolute byte
/// offset table. Used by Section 5's finalize step.
pub fn emit_with_offsets(frag: &Fragment) -> EmitOutput {
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
  let mut user_local_types: Vec<(u32, &TypeDecl)> = Vec::new();
  for (i, ty) in frag.types.iter().enumerate() {
    match &ty.import {
      Some(ImportKey { module, name }) => {
        // Compose qualified key and look up as a type by name.
        let qualified = format!("{module}:{name}");
        let idx = rt.type_by_name.get(&qualified).copied()
          .or_else(|| rt.type_by_name.get(name.as_str()).copied())
          .unwrap_or_else(|| panic!(
            "emit: unknown runtime type import `{}`. Not found as `{}` or `{}` in type-name table",
            qualified, qualified, name));
        type_remap.push(idx);
      }
      None => {
        // Placeholder — filled in after we know how many we have.
        type_remap.push(u32::MAX);
        user_local_types.push((i as u32, ty));
      }
    }
  }
  for (seq, (idx, _)) in user_local_types.iter().enumerate() {
    type_remap[*idx as usize] = rt.type_count + seq as u32;
  }

  // ── resolve user function imports + plan local-func indices ──

  let mut func_remap: Vec<u32> = Vec::with_capacity(frag.funcs.len());
  let mut user_local_funcs: Vec<(u32, &FuncDecl)> = Vec::new();
  for (i, f) in frag.funcs.iter().enumerate() {
    match &f.import {
      Some(ImportKey { module, name }) => {
        let qualified = format!("{module}:{name}");
        let idx = rt.func_by_name.get(&qualified).copied()
          .or_else(|| rt.func_by_name.get(name.as_str()).copied())
          .unwrap_or_else(|| panic!(
            "emit: unknown runtime func import `{}`. Not found in runtime func-name table",
            qualified));
        func_remap.push(idx);
      }
      None => {
        func_remap.push(u32::MAX);
        user_local_funcs.push((i as u32, f));
      }
    }
  }
  for (seq, (idx, _)) in user_local_funcs.iter().enumerate() {
    func_remap[*idx as usize] = rt.func_count_total + seq as u32;
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
      _ => panic!("emit: only locally-declared func types supported (got {:?})", ty.kind),
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
    func_sec.function(resolve_type(f.sig, &type_remap));
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

  // Globals: runtime entries are raw-spliced (init_exprs use struct.new
  // etc. which wasm-encoder's ConstExpr API can't construct); user
  // entries are encoded via wasm-encoder. When both are present we
  // combine by rewriting the leading LEB128 count and concatenating.
  let globals_section_body = rt.globals_body.as_deref();
  let user_global_count = frag.globals.len() as u32;
  let (combined_globals_body, user_global_base) = if user_global_count == 0 {
    (globals_section_body.map(|b| b.to_vec()), 0u32)
  } else {
    let (rt_count, rt_body_no_count) = match globals_section_body {
      Some(raw) => decode_leb_u32_prefix(raw),
      None => (0u32, &[][..]),
    };
    // Encode user globals manually (no count prefix) so we can
    // concatenate with runtime's body.
    let mut user_bytes: Vec<u8> = Vec::new();
    for g in &frag.globals {
      let ty = val_from_ir(&g.ty, &type_remap);
      let init = match &g.init {
        GlobalInit::RefNull(ht) => ConstExpr::ref_null(HeapType::Abstract {
          shared: false,
          ty: abs_heap_ir(*ht),
        }),
        GlobalInit::RefNullConcrete(ts) => ConstExpr::ref_null(HeapType::Concrete(resolve_type(*ts, &type_remap))),
        GlobalInit::RefFunc(fs) => ConstExpr::ref_func(resolve_func(*fs, &func_remap)),
        GlobalInit::I32Const(v) => ConstExpr::i32_const(*v),
        GlobalInit::F64Const(v) => ConstExpr::f64_const((*v).into()),
      };
      GlobalType { val_type: ty, mutable: g.mutable, shared: false }.encode(&mut user_bytes);
      init.encode(&mut user_bytes);
    }
    let mut body = Vec::with_capacity(5 + rt_body_no_count.len() + user_bytes.len());
    encode_leb_u32(&mut body, rt_count + user_global_count);
    body.extend_from_slice(rt_body_no_count);
    body.extend_from_slice(&user_bytes);
    (Some(body), rt_count)
  };

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
  // User globals are appended after runtime globals; their module
  // indices start at `user_global_base` (the runtime global count).
  for (i, g) in frag.globals.iter().enumerate() {
    if let Some(name) = &g.export {
      export_sec.export(name, ExportKind::Global, user_global_base + i as u32);
    }
  }
  // Export the user fragment's memory (memory 0) so the host harness
  // can read string-literal data segments. Only if the runtime didn't
  // already export one with this name.
  let already_has_memory = rt.exports_to_forward.iter()
    .any(|(name, kind, _)| name == "memory" && matches!(kind, ExportKind::Memory));
  if !already_has_memory {
    export_sec.export("memory", ExportKind::Memory, 0);
  }

  // Elements: runtime entries raw-spliced; user-declared funcref
  // entries (needed for `ref.func` validation on any user func) are
  // encoded via wasm-encoder and combined with a rewritten count LEB.
  let elements_section_body = rt.elements_body.as_deref();
  let user_func_refs: Vec<u32> = user_local_funcs.iter()
    .map(|(idx, _)| func_remap[*idx as usize])
    .collect();
  let combined_elements_body = if user_func_refs.is_empty() {
    elements_section_body.map(|b| b.to_vec())
  } else {
    let (rt_elem_count, rt_body_no_count) = match elements_section_body {
      Some(raw) => decode_leb_u32_prefix(raw),
      None => (0u32, &[][..]),
    };
    let mut user_sec = ElementSection::new();
    user_sec.declared(Elements::Functions(user_func_refs.into()));
    // `ElementSection::encode` emits: payload_size_leb + count_leb + entries.
    // Strip the two LEBs to extract the entries.
    let mut full_bytes: Vec<u8> = Vec::new();
    user_sec.encode(&mut full_bytes);
    let (_payload_size, rest) = decode_leb_u32_prefix(&full_bytes);
    let (_user_elem_count, user_entries) = decode_leb_u32_prefix(rest);
    let user_entries = user_entries.to_vec();
    let mut body = Vec::with_capacity(5 + rt_body_no_count.len() + user_entries.len());
    encode_leb_u32(&mut body, rt_elem_count + 1);
    body.extend_from_slice(rt_body_no_count);
    body.extend_from_slice(&user_entries);
    Some(body)
  };

  // Data: lay out user fragment's `frag.data` blobs sequentially in
  // memory starting at offset 0. Each `DataSym(i)` resolves to the
  // running offset, used by `Operand::DataRef` at emit time.
  let mut data_offsets: Vec<u32> = Vec::with_capacity(frag.data.len());
  let mut data_blob: Vec<u8> = Vec::new();
  for d in &frag.data {
    data_offsets.push(data_blob.len() as u32);
    data_blob.extend_from_slice(&d.bytes);
  }

  // Code: runtime's bodies raw, then user's bodies encoded.
  let mut code_sec = CodeSection::new();
  for body in &rt.code_bodies_raw {
    code_sec.raw(body);
  }
  // Per-user-function body-relative offset tables, plus the func's
  // final binary index. Resolved to absolute offsets after the module
  // is finalised.
  let mut user_body_offsets: Vec<(u32, Vec<(InstrId, u32)>)> = Vec::new();
  for (orig_idx, f) in &user_local_funcs {
    let final_idx = func_remap[*orig_idx as usize];
    let (func, body_offsets) = emit_func(frag, f, &type_remap, &func_remap, user_global_base, &data_offsets);
    code_sec.function(&func);
    user_body_offsets.push((final_idx, body_offsets));
  }

  // Data section: one active segment at offset 0 in memory 0 holding
  // the concatenated blobs. Skip if there's no data.
  let data_sec = if data_blob.is_empty() {
    None
  } else {
    let mut sec = wasm_encoder::DataSection::new();
    sec.active(
      0,                                 // memory index
      &ConstExpr::i32_const(0),          // offset
      data_blob.iter().copied(),         // bytes
    );
    Some(sec)
  };

  // ── finalise module ──────────────────────────────────────────
  // WASM section IDs — from the spec.
  const SECTION_GLOBAL: u8 = 6;
  const SECTION_ELEMENT: u8 = 9;

  let mut module = WasmModule::new();
  module.section(&type_sec);
  module.section(&import_sec);
  module.section(&func_sec);
  module.section(&mem_sec);
  if let Some(body) = &combined_globals_body {
    module.section(&wasm_encoder::RawSection { id: SECTION_GLOBAL, data: body });
  }
  module.section(&export_sec);
  if let Some(body) = &combined_elements_body {
    module.section(&wasm_encoder::RawSection { id: SECTION_ELEMENT, data: body });
  }
  module.section(&code_sec);
  if let Some(sec) = &data_sec {
    module.section(sec);
  }
  let binary = module.finish();

  // Resolve per-function body-relative offsets to absolute binary
  // offsets. Re-parse the binary with wasmparser; the code section
  // exposes per-function `range()` values whose `start` is the byte
  // offset of the function body's locals-count LEB. The instructions
  // that emit_func tracked are body-relative starting after the
  // locals declarations, so we offset by the locals length too.
  let instr_offsets = resolve_abs_offsets(&binary, &user_body_offsets);

  EmitOutput { binary, instr_offsets }
}


/// Translate per-function body-relative offsets into absolute binary
/// offsets. Re-parses the binary with wasmparser to find each
/// function body's absolute start (`FunctionBody::range().start`) —
/// `wasm_encoder::Function::byte_len()` (used in emit_func) returns
/// counts from that same anchor (locals-count LEB onward), so absolute
/// = body_start + body_relative.
fn resolve_abs_offsets(
  binary: &[u8],
  user_body_offsets: &[(u32, Vec<(InstrId, u32)>)],
) -> std::collections::BTreeMap<InstrId, u32> {
  use std::collections::BTreeMap;

  // Walk the binary's payloads to find the code section. Per
  // wasmparser, FunctionBody yields one entry per function body in
  // declaration order; the i-th entry corresponds to func index
  // `imported_func_count + i` in the final binary's func index space.
  let mut imported_funcs = 0u32;
  let mut func_starts: Vec<u32> = Vec::new();
  let mut parser = wasmparser::Parser::new(0);
  let mut data = binary;
  loop {
    use wasmparser::Payload;
    match parser.parse(data, true) {
      Ok(wasmparser::Chunk::NeedMoreData(_)) => break,
      Ok(wasmparser::Chunk::Parsed { payload, consumed }) => {
        match payload {
          Payload::ImportSection(reader) => {
            for imp in reader.into_imports() {
              let imp = imp.expect("emit: malformed import in own output");
              if matches!(imp.ty, wasmparser::TypeRef::Func(_)) {
                imported_funcs += 1;
              }
            }
          }
          Payload::CodeSectionEntry(body) => {
            func_starts.push(body.range().start as u32);
          }
          Payload::End(_) => break,
          _ => {}
        }
        data = &data[consumed..];
      }
      Err(e) => panic!("emit: failed to re-parse own binary: {e}"),
    }
  }

  let mut out: BTreeMap<InstrId, u32> = BTreeMap::new();
  for (final_func_idx, body_offsets) in user_body_offsets {
    // user-fragment funcs land after imports + runtime funcs in the
    // final func index space. The code-section entry index is
    // `final_func_idx - imported_funcs`. (Runtime funcs are not
    // imports — they're emitted as code-section entries before user
    // funcs, so the index space is contiguous.)
    let code_idx = (final_func_idx - imported_funcs) as usize;
    let body_start = func_starts[code_idx];
    for (instr_id, body_rel) in body_offsets {
      out.insert(*instr_id, body_start + body_rel);
    }
  }
  out
}

// ──────────────────────────────────────────────────────────────────
// Function body emission
// ──────────────────────────────────────────────────────────────────

fn emit_func(
  frag: &Fragment,
  f: &FuncDecl,
  type_remap: &[u32],
  func_remap: &[u32],
  user_global_base: u32,
  data_offsets: &[u32],
) -> (Function, Vec<(InstrId, u32)>) {
  let mut locals: Vec<(u32, WEValType)> = Vec::new();
  for l in &f.locals {
    locals.push((1, val_from_ir(&l.ty, type_remap)));
  }
  let mut func = Function::new(locals);
  // Body-relative offset per InstrId — only recorded for instructions
  // that were tagged with a `cps_id` in lower. The body-relative offset
  // is `func.byte_len()` immediately before the instruction is emitted;
  // it does not include the variable-width body-length prefix that
  // `code.function(&func)` will write before the body bytes.
  let mut body_offsets: Vec<(InstrId, u32)> = Vec::new();
  for &id in &f.body {
    let instr = &frag.instrs[id.0 as usize];
    if instr.cps_id.is_some() {
      body_offsets.push((id, func.byte_len() as u32));
    }
    emit_instr(&mut func, frag, instr, type_remap, func_remap, user_global_base, data_offsets);
  }
  func.instruction(&Instruction::End);
  (func, body_offsets)
}

fn emit_instr(
  func: &mut Function,
  frag: &Fragment,
  instr: &Instr,
  type_remap: &[u32],
  func_remap: &[u32],
  user_global_base: u32,
  data_offsets: &[u32],
) {
  match &instr.kind {
    InstrKind::LocalSet { idx, src } => {
      emit_operand(func, src, type_remap, func_remap, user_global_base, data_offsets);
      func.instruction(&Instruction::LocalSet(idx.0));
    }
    InstrKind::GlobalSet { sym, src } => {
      emit_operand(func, src, type_remap, func_remap, user_global_base, data_offsets);
      func.instruction(&Instruction::GlobalSet(user_global_base + sym.0));
    }
    InstrKind::StructNew { ty, fields, into } => {
      for fld in fields {
        emit_operand(func, fld, type_remap, func_remap, user_global_base, data_offsets);
      }
      func.instruction(&Instruction::StructNew(resolve_type(*ty, type_remap)));
      func.instruction(&Instruction::LocalSet(into.0));
    }
    InstrKind::Call { target, args, into } => {
      for a in args {
        emit_operand(func, a, type_remap, func_remap, user_global_base, data_offsets);
      }
      func.instruction(&Instruction::Call(resolve_func(*target, func_remap)));
      if let Some(l) = into {
        func.instruction(&Instruction::LocalSet(l.0));
      }
    }
    InstrKind::ReturnCall { target, args } => {
      for a in args {
        emit_operand(func, a, type_remap, func_remap, user_global_base, data_offsets);
      }
      func.instruction(&Instruction::ReturnCall(resolve_func(*target, func_remap)));
    }
    InstrKind::RefI31 { src, into } => {
      emit_operand(func, src, type_remap, func_remap, user_global_base, data_offsets);
      func.instruction(&Instruction::RefI31);
      func.instruction(&Instruction::LocalSet(into.0));
    }
    InstrKind::RefFunc { func: fsym, into } => {
      func.instruction(&Instruction::RefFunc(resolve_func(*fsym, func_remap)));
      func.instruction(&Instruction::LocalSet(into.0));
    }
    InstrKind::RefNullConcrete { ty, into } => {
      func.instruction(&Instruction::RefNull(HeapType::Concrete(resolve_type(*ty, type_remap))));
      func.instruction(&Instruction::LocalSet(into.0));
    }
    InstrKind::RefCastNonNull { ty, src, into } => {
      emit_operand(func, src, type_remap, func_remap, user_global_base, data_offsets);
      func.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(resolve_type(*ty, type_remap))));
      func.instruction(&Instruction::LocalSet(into.0));
    }
    InstrKind::ArrayNewFixed { ty, size, elems, into } => {
      for e in elems {
        emit_operand(func, e, type_remap, func_remap, user_global_base, data_offsets);
      }
      func.instruction(&Instruction::ArrayNewFixed {
        array_type_index: resolve_type(*ty, type_remap),
        array_size: *size,
      });
      func.instruction(&Instruction::LocalSet(into.0));
    }
    InstrKind::ArrayGet { ty, arr, idx, into } => {
      emit_operand(func, arr, type_remap, func_remap, user_global_base, data_offsets);
      emit_operand(func, idx, type_remap, func_remap, user_global_base, data_offsets);
      func.instruction(&Instruction::ArrayGet(resolve_type(*ty, type_remap)));
      func.instruction(&Instruction::LocalSet(into.0));
    }
    InstrKind::RefNull { ht, into } => {
      func.instruction(&Instruction::RefNull(HeapType::Abstract {
        shared: false,
        ty: abs_heap_ir(*ht),
      }));
      func.instruction(&Instruction::LocalSet(into.0));
    }
    InstrKind::I31GetS { src, into } => {
      emit_operand(func, src, type_remap, func_remap, user_global_base, data_offsets);
      func.instruction(&Instruction::I31GetS);
      func.instruction(&Instruction::LocalSet(into.0));
    }
    InstrKind::RefCastNullable { ty, src, into } => {
      emit_operand(func, src, type_remap, func_remap, user_global_base, data_offsets);
      func.instruction(&Instruction::RefCastNullable(HeapType::Concrete(resolve_type(*ty, type_remap))));
      func.instruction(&Instruction::LocalSet(into.0));
    }
    InstrKind::RefCastNonNullAbs { ht, src, into } => {
      emit_operand(func, src, type_remap, func_remap, user_global_base, data_offsets);
      func.instruction(&Instruction::RefCastNonNull(HeapType::Abstract {
        shared: false,
        ty: abs_heap_ir(*ht),
      }));
      func.instruction(&Instruction::LocalSet(into.0));
    }
    InstrKind::If { cond, then_body, else_body } => {
      // cond is a leaf operand evaluating to i32.
      emit_operand(func, cond, type_remap, func_remap, user_global_base, data_offsets);
      func.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
      for id in then_body {
        emit_instr(func, frag, &frag.instrs[id.0 as usize],
          type_remap, func_remap, user_global_base, data_offsets);
      }
      if !else_body.is_empty() {
        func.instruction(&Instruction::Else);
        for id in else_body {
          emit_instr(func, frag, &frag.instrs[id.0 as usize],
            type_remap, func_remap, user_global_base, data_offsets);
        }
      }
      func.instruction(&Instruction::End);
    }
    InstrKind::Unreachable => {
      func.instruction(&Instruction::Unreachable);
    }
    InstrKind::Drop { src } => {
      emit_operand(func, src, type_remap, func_remap, user_global_base, data_offsets);
      func.instruction(&Instruction::Drop);
    }
  }
}

fn emit_operand(
  func: &mut Function,
  op: &Operand,
  _type_remap: &[u32],
  func_remap: &[u32],
  user_global_base: u32,
  data_offsets: &[u32],
) {
  match op {
    Operand::I32(v) => { func.instruction(&Instruction::I32Const(*v)); }
    Operand::F64(v) => { func.instruction(&Instruction::F64Const((*v).into())); }
    Operand::Local(idx) => { func.instruction(&Instruction::LocalGet(idx.0)); }
    Operand::Global(sym) => {
      func.instruction(&Instruction::GlobalGet(user_global_base + sym.0));
    }
    Operand::RefFunc(fsym) => {
      func.instruction(&Instruction::RefFunc(resolve_func(*fsym, func_remap)));
    }
    Operand::RefNull(ht) => {
      func.instruction(&Instruction::RefNull(HeapType::Abstract {
        shared: false,
        ty: abs_heap_ir(*ht),
      }));
    }
    Operand::DataRef { sym, len } => {
      // DataRef expands to TWO consts: (offset, len). Used by string
      // literal lowering — `call $str (i32.const offset) (i32.const len)`.
      let offset = data_offsets[sym.0 as usize];
      func.instruction(&Instruction::I32Const(offset as i32));
      func.instruction(&Instruction::I32Const(*len as i32));
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
      heap_type: HeapType::Concrete(resolve_type(*ty, type_remap)),
    }),
  }
}

// ──────────────────────────────────────────────────────────────────
// LEB128 helpers (unsigned, u32-range)
// ──────────────────────────────────────────────────────────────────

fn encode_leb_u32(out: &mut Vec<u8>, mut v: u32) {
  loop {
    let byte = (v & 0x7f) as u8;
    v >>= 7;
    if v == 0 { out.push(byte); break; }
    out.push(byte | 0x80);
  }
}

/// Decode the leading LEB128 u32 count from a section body and return
/// `(count, remainder)`. Panics if the input is malformed (we control
/// the producer).
fn decode_leb_u32_prefix(raw: &[u8]) -> (u32, &[u8]) {
  let mut val: u32 = 0;
  let mut shift = 0u32;
  for (i, b) in raw.iter().enumerate() {
    val |= u32::from(b & 0x7f) << shift;
    if b & 0x80 == 0 {
      return (val, &raw[i + 1..]);
    }
    shift += 7;
    if shift >= 32 {
      panic!("emit: LEB128 u32 overflow");
    }
  }
  panic!("emit: truncated LEB128");
}
