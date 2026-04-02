// WASM binary emitter — encodes lifted CPS IR to WASM via wasm-encoder.
//
// Produces a WASM binary with:
//   - WasmGC types ($Num, $Closure, $Captures, $FnN per arity)
//   - Imported builtins as functions
//   - Defined functions from collected CPS IR
//   - Globals for module-level fn aliases
//   - Exports
//   - Name section (function, local, global names)
//   - Byte offset mappings for DWARF / DAP source maps
//
// The emitter tracks byte offsets during code emission so that each
// source-mapped instruction can be correlated with the original .fnk
// source location. These offsets are later used to build DWARF line
// tables and WasmMapping entries for the DAP debugger.
//
// ## Source map marking rules
//
// Each WASM instruction gets at most one DWARF line entry (one source
// location per byte offset). The rules for what maps where:
//
// - **call instructions** (`return_call`, `return_call_ref`, `call`)
//   → point to the callee: operator token for builtins (e.g. `+`),
//     call site for user function calls.
//   For builtins, the operator mark is emitted *after* args (at the
//   return_call instruction offset) to avoid colliding with the first
//   arg's value mark at the same byte offset.
//
// - **local.set** → point to the binding name (e.g. `x` in `x = 42`).
//   The mark is placed just before the local.set instruction.
//
// - **literals** (struct.new $Num wrapping f64.const, or ref.i31 wrapping
//   i32.const for booleans) → point to the literal value in source.
//   Each value gets a mark from emit_val.
//
// - **structural items** (func headers, params, globals, exports) are
//   recorded as StructuralLoc entries, not DWARF, since they don't
//   correspond to code section byte offsets.
//
// DWARF line tables have one entry per byte offset. When two marks
// collide (same offset), the last one wins. The formatter reconstructs
// WAT text source maps by looking up DWARF entries for each instruction
// and structural locs for non-code items.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use wasm_encoder::{
  AbstractHeapType, CodeSection, CompositeInnerType, CompositeType,
  ConstExpr, DataSection, ElementSection, Elements, ExportKind, ExportSection,
  FieldType, FuncType, Function, FunctionSection, GlobalSection, GlobalType,
  HeapType, ImportSection, IndirectNameMap, Instruction, MemorySection,
  MemoryType, NameMap, NameSection, RefType, StorageType, SubType,
  StructType, TypeSection, ValType,
};

// Pre-compiled canonical type definitions from src/runtime/types.wat.
// Compiled at build time by build.rs; see that file for details.
static CANONICAL_TYPES_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/types.wasm"));

use wasmparser;

use crate::lexer::Loc;
use crate::passes::cps::ir::{
  Arg, BuiltIn, Callable, Cont, CpsId, Expr, ExprKind,
  Lit, Ref, Val, ValKind,
};
use super::collect::{
  CollectedFn, IrCtx, Module as CpsModule,
  builtin_name, collect_locals, split_args,
};

// ---------------------------------------------------------------------------
// String intern table — deduplicates string literals in a flat data blob.
// ---------------------------------------------------------------------------

/// Interned string data: a flat byte blob where each unique literal is stored
/// once. The intern ID is the byte offset into the blob; the length is stored
/// alongside. Substring overlap is exploited: if `"hello"` already exists
/// anywhere in the blob, a new reference to `"hello"` reuses that offset.
struct StringData {
  /// Accumulated data section bytes.
  bytes: Vec<u8>,
}

impl StringData {
  fn new() -> Self {
    Self { bytes: Vec::new() }
  }

  /// Intern a string literal. Returns `(offset, length)` into the data blob.
  /// If the byte sequence already exists as a substring, reuses that offset.
  fn intern(&mut self, s: &str) -> (u32, u32) {
    let needle = s.as_bytes();
    let len = needle.len() as u32;
    // Search for existing substring.
    if let Some(pos) = find_bytes(&self.bytes, needle) {
      return (pos as u32, len);
    }
    // Not found — append.
    let offset = self.bytes.len() as u32;
    self.bytes.extend_from_slice(needle);
    (offset, len)
  }

  fn is_empty(&self) -> bool {
    self.bytes.is_empty()
  }

  /// Size in WASM pages (64 KiB each), rounded up.
  fn pages(&self) -> u64 {
    let size = self.bytes.len() as u64;
    size.div_ceil(65536)
  }
}

/// Find `needle` as a contiguous subsequence in `haystack`.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
  if needle.is_empty() {
    return Some(0);
  }
  haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Canonical types from types.wat
// ---------------------------------------------------------------------------

/// Parsed canonical type definitions from the pre-compiled types.wasm.
struct CanonicalTypes {
  /// The rec group subtypes, ready to inject via TypeSection::rec().
  rec_group: Vec<SubType>,
  /// Type name → index within the rec group (e.g. "$Num" → 0, "$Closure" → 9).
  names: BTreeMap<String, u32>,
  /// Total number of canonical types.
  count: u32,
}

/// Parse the pre-compiled canonical types WASM and extract the rec group.
fn parse_canonical_types() -> CanonicalTypes {
  let wasm = CANONICAL_TYPES_WASM;
  let mut rec_group = Vec::new();
  let mut type_names: BTreeMap<u32, String> = BTreeMap::new();

  for payload in wasmparser::Parser::new(0).parse_all(wasm) {
    match payload.expect("invalid canonical types WASM") {
      wasmparser::Payload::TypeSection(reader) => {
        for rg in reader.into_iter() {
          let rg = rg.expect("invalid rec group in canonical types");
          for st in rg.into_types() {
            rec_group.push(convert_wasmparser_subtype(&st));
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

  let count = rec_group.len() as u32;
  let names: BTreeMap<String, u32> = type_names.into_iter()
    .map(|(idx, name)| (format!("${name}"), idx))
    .collect();

  CanonicalTypes { rec_group, names, count }
}

// -- wasmparser → wasm-encoder type conversion (no index remapping) ----------

fn convert_wasmparser_subtype(st: &wasmparser::SubType) -> SubType {
  SubType {
    is_final: st.is_final,
    supertype_idx: st.supertype_idx
      .map(|idx| idx.as_module_index().unwrap_or(0)),
    composite_type: convert_wasmparser_composite(&st.composite_type),
  }
}

fn convert_wasmparser_composite(ct: &wasmparser::CompositeType) -> CompositeType {
  CompositeType {
    inner: match &ct.inner {
      wasmparser::CompositeInnerType::Func(f) => {
        CompositeInnerType::Func(FuncType::new(
          f.params().iter().map(|vt| convert_wasmparser_val(*vt)).collect::<Vec<_>>(),
          f.results().iter().map(|vt| convert_wasmparser_val(*vt)).collect::<Vec<_>>(),
        ))
      }
      wasmparser::CompositeInnerType::Struct(s) => {
        CompositeInnerType::Struct(StructType {
          fields: s.fields.iter().map(convert_wasmparser_field).collect(),
        })
      }
      wasmparser::CompositeInnerType::Array(a) => {
        CompositeInnerType::Array(wasm_encoder::ArrayType(convert_wasmparser_field(&a.0)))
      }
      wasmparser::CompositeInnerType::Cont(_) => {
        panic!("canonical types: continuation types not supported")
      }
    },
    shared: ct.shared,
    descriptor: None,
    describes: None,
  }
}

fn convert_wasmparser_val(vt: wasmparser::ValType) -> ValType {
  match vt {
    wasmparser::ValType::I32 => ValType::I32,
    wasmparser::ValType::I64 => ValType::I64,
    wasmparser::ValType::F32 => ValType::F32,
    wasmparser::ValType::F64 => ValType::F64,
    wasmparser::ValType::V128 => ValType::V128,
    wasmparser::ValType::Ref(rt) => ValType::Ref(convert_wasmparser_ref(rt)),
  }
}

fn convert_wasmparser_ref(rt: wasmparser::RefType) -> RefType {
  RefType {
    nullable: rt.is_nullable(),
    heap_type: match rt.heap_type() {
      wasmparser::HeapType::Abstract { shared, ty } => HeapType::Abstract {
        shared,
        ty: convert_wasmparser_abstract_heap(ty),
      },
      wasmparser::HeapType::Concrete(idx) => {
        HeapType::Concrete(idx.as_module_index().unwrap_or(0))
      }
      wasmparser::HeapType::Exact(idx) => {
        HeapType::Concrete(idx.as_module_index().unwrap_or(0))
      }
    },
  }
}

fn convert_wasmparser_abstract_heap(ty: wasmparser::AbstractHeapType) -> AbstractHeapType {
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

fn convert_wasmparser_field(f: &wasmparser::FieldType) -> FieldType {
  FieldType {
    element_type: match f.element_type {
      wasmparser::StorageType::I8 => StorageType::I8,
      wasmparser::StorageType::I16 => StorageType::I16,
      wasmparser::StorageType::Val(vt) => StorageType::Val(convert_wasmparser_val(vt)),
    },
    mutable: f.mutable,
  }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Result of WASM binary emission.
pub struct EmitResult {
  pub wasm: Vec<u8>,
  pub offset_mappings: Vec<OffsetMapping>,
  /// Structural source locations for non-code items (func headers, globals, exports, params).
  /// The formatter uses these to place source marks on WAT structural lines.
  pub structural_locs: Vec<StructuralLoc>,
  /// Whether the module imports operators from @fink/runtime/operators.
  pub needs_operators: bool,
  /// Whether the module imports list functions from @fink/runtime/list.
  pub needs_list: bool,
  /// Whether the module uses string literals (needs @fink/runtime/string).
  pub needs_string: bool,
}

/// A single source-map entry: WASM byte offset → .fnk source location.
pub struct OffsetMapping {
  pub wasm_offset: u32,
  pub loc: Loc,
}

/// Source location for a structural (non-code) WASM item.
#[derive(Debug, Clone)]
pub struct StructuralLoc {
  pub kind: StructuralKind,
  pub loc: Loc,
}

/// Kind of structural item.
#[derive(Debug, Clone)]
pub enum StructuralKind {
  /// Function header: (func $name ...)
  FuncHeader { func_idx: u32 },
  /// Function parameter: (param $name ...)
  FuncParam { func_idx: u32, param_idx: u32 },
  /// Global alias: (global $name ...)
  Global { global_idx: u32 },
  /// Export: (export "name" ...)
  Export { name: String },
}

/// Emit a WASM binary from a collected CPS module.
pub fn emit<'a>(module: &CpsModule<'a>, ctx: &IrCtx<'_, '_>) -> EmitResult {
  let mut e = Emitter::new(module, ctx);
  // Scan builtins and closure captures.
  let mut builtins: BTreeMap<String, usize> = BTreeMap::new();
  let mut closure_captures: BTreeSet<usize> = BTreeSet::new();
  let mut has_panic = false;
  for func in &module.funcs {
    scan_builtins(func.body, &mut builtins);
    scan_closure_captures(func.body, &mut closure_captures);
    if !has_panic { has_panic = scan_panic(func.body); }
  }
  // Split builtins: implemented ones become defined functions, rest stay as imports.
  let (impl_builtins, import_builtins): (BTreeMap<String, usize>, BTreeMap<String, usize>) =
    builtins.into_iter().partition(|(name, _)| super::builtins::is_implemented(name));
  let has_operator_imports = import_builtins.keys().any(|n| n.starts_with("op_") || n == "empty");
  let has_list_imports = import_builtins.keys().any(|n| n.starts_with("seq_"));
  let has_rec_imports = import_builtins.keys().any(|n| n.starts_with("rec_"))
    || module.funcs.iter().any(|f| scan_rec_lit(f.body));
  // Scan string literals and intern into the data blob.
  for func in &module.funcs {
    scan_strings(func.body, &mut e.string_data);
  }
  let has_strings = !e.string_data.is_empty();

  // $Fn2(caps, args) for continuations. $Fn3(caps, args, cont) for user functions.
  let mut extra_arities: BTreeSet<usize> = BTreeSet::new();
  extra_arities.insert(0); // $Fn0 — $_panic
  extra_arities.insert(2); // $Fn2 — continuations + _apply(args, callee)
  extra_arities.insert(3); // $Fn3 — user functions + _apply_cont(args, cont, callee)

  e.closure_captures = closure_captures.clone();
  e.needs_croc_for_operators = has_operator_imports;
  e.needs_list = has_list_imports;
  e.needs_rec = has_rec_imports;
  e.needs_string = has_strings;
  e.has_panic = has_panic;
  // Type section needs arities from both imported and implemented builtins.
  let mut all_builtins = import_builtins.clone();
  all_builtins.extend(impl_builtins.iter().map(|(k, v)| (k.clone(), *v)));
  e.emit_types(module, &all_builtins, &extra_arities, &closure_captures);
  e.emit_imports_from(module, &import_builtins);
  e.impl_builtins = impl_builtins.clone();
  // Build set of function labels with has_cont=true ($Fn3 calling convention).
  // Include both the function label and its alias (LetVal binding name).
  e.cont_fns = module.funcs.iter()
    .filter(|f| f.has_cont)
    .flat_map(|f| {
      let mut labels = vec![f.label.clone()];
      if let Some((_, alias)) = &f.alias {
        labels.push(alias.clone());
      }
      labels
    })
    .collect();
  e.emit_functions(module, &closure_captures);
  e.emit_memory();
  e.emit_globals(module);
  e.emit_exports(module);
  e.emit_elements();
  e.emit_code(module, &closure_captures);
  e.emit_data();
  e.emit_names(module, &closure_captures);
  let wasm = e.module.finish();

  // Fixup: convert func-local offsets to absolute offsets.
  let mut mappings = fixup_offsets(&wasm, e.raw_mappings);
  // Sort by wasm_offset for monotonic DWARF line table emission.
  mappings.sort_by_key(|m| m.wasm_offset);

  EmitResult { wasm, offset_mappings: mappings, structural_locs: e.structural_locs, needs_operators: e.needs_croc_for_operators, needs_list: e.needs_list, needs_string: has_strings }
}

// ---------------------------------------------------------------------------
// Index management
// ---------------------------------------------------------------------------

/// Maps labels and builtins to WASM index spaces.
struct Indices {
  /// Type name → type index (e.g. "$Num" → 0, "$Fn2" → 2).
  types: BTreeMap<String, u32>,
  /// Builtin import name → function index.
  imports: BTreeMap<String, u32>,
  /// Defined function label → function index (offset by import count).
  funcs: BTreeMap<String, u32>,
  /// Global alias label → global index.
  globals: BTreeMap<String, u32>,
  /// Number of imported functions.
  import_count: u32,
}

impl Indices {
  fn new() -> Self {
    Self {
      types: BTreeMap::new(),
      imports: BTreeMap::new(),
      funcs: BTreeMap::new(),
      globals: BTreeMap::new(),
      import_count: 0,
    }
  }

  /// Resolve a function index — import or defined.
  fn func_idx(&self, label: &str) -> u32 {
    if let Some(&idx) = self.imports.get(label) {
      idx
    } else if let Some(&idx) = self.funcs.get(label) {
      idx
    } else {
      panic!("unknown function: {}", label)
    }
  }

  /// Resolve a global index.
  fn global_idx(&self, label: &str) -> u32 {
    *self.globals.get(label).unwrap_or_else(|| panic!("unknown global: {}", label))
  }

  /// Resolve a type index.
  fn type_idx(&self, name: &str) -> u32 {
    *self.types.get(name).unwrap_or_else(|| panic!("unknown type: {}", name))
  }

  /// Get the $FnN type index for a given arity.
  fn fn_type_idx(&self, arity: usize) -> u32 {
    self.type_idx(&format!("$Fn{}", arity))
  }
}

// ---------------------------------------------------------------------------
// Emitter state
// ---------------------------------------------------------------------------

struct Emitter<'a, 'src> {
  module: wasm_encoder::Module,
  idx: Indices,
  ctx: &'a IrCtx<'a, 'src>,
  /// Raw mappings: (func_index, func_local_byte_offset, loc).
  /// Converted to absolute offsets after module.finish().
  raw_mappings: Vec<RawMapping>,
  /// Structural source locations for non-code items.
  structural_locs: Vec<StructuralLoc>,
  /// Closure capture counts found in this module (for _croc dispatch).
  closure_captures: BTreeSet<usize>,
  /// Builtins with known implementations (emitted as defined functions).
  impl_builtins: BTreeMap<String, usize>,
  /// Whether _croc is needed for imported operators (even without user closures).
  needs_croc_for_operators: bool,
  /// Whether list runtime imports are present (seq_prepend, seq_concat, seq_pop).
  needs_list: bool,
  /// Whether record runtime imports are present (rec_set, rec_pop, rec_merge).
  needs_rec: bool,
  /// Whether string literals are present (needs memory + data section + string runtime).
  needs_string: bool,
  /// Whether ValKind::Panic appears in any function body (needs $_panic function).
  has_panic: bool,
  /// Interned string literal data.
  string_data: StringData,
  /// Function labels known to have has_cont=true ($Fn3 calling convention).
  cont_fns: HashSet<String>,
}

struct RawMapping {
  /// Index of the defined function (0-based within defined funcs, not imports).
  func_def_index: u32,
  /// Byte offset within the function body (from Function::byte_len()).
  func_byte_offset: u32,
  /// Source location.
  loc: Loc,
}

impl<'a, 'src> Emitter<'a, 'src> {
  fn new(_module: &CpsModule<'a>, ctx: &'a IrCtx<'a, 'src>) -> Self {
    Self {
      module: wasm_encoder::Module::new(),
      idx: Indices::new(),
      ctx,
      raw_mappings: Vec::new(),
      structural_locs: Vec::new(),
      closure_captures: BTreeSet::new(),
      impl_builtins: BTreeMap::new(),
      needs_croc_for_operators: false,
      needs_list: false,
      needs_rec: false,
      needs_string: false,
      has_panic: false,
      string_data: StringData::new(),
      cont_fns: HashSet::new(),
    }
  }

  // -------------------------------------------------------------------------
  // Type section
  // -------------------------------------------------------------------------

  fn emit_types(&mut self, _cps_mod: &CpsModule<'a>, builtins: &BTreeMap<String, usize>, extra_arities: &BTreeSet<usize>, _closure_captures: &BTreeSet<usize>) {
    let mut types = TypeSection::new();

    // 1. Canonical runtime types from types.wat — injected as a rec group.
    //    These define the shared type vocabulary ($Num, $Str, $List, etc.)
    //    that all modules and runtime fragments share after linking.
    let canonical = parse_canonical_types();
    types.ty().rec(canonical.rec_group);
    for (name, &idx) in &canonical.names {
      self.idx.types.insert(name.clone(), idx);
    }
    let mut next_idx = canonical.count;

    // 2. Module-specific types — appended after the canonical rec group.

    // $BoxFuncTy = (func (param funcref) (result (ref null any)))
    // Type for the _box_func helper exported for the host.
    let func_ref = ValType::Ref(RefType {
      nullable: true,
      heap_type: HeapType::Abstract { shared: false, ty: AbstractHeapType::Func },
    });
    let any_ref = ValType::Ref(RefType {
      nullable: true,
      heap_type: HeapType::Abstract { shared: false, ty: AbstractHeapType::Any },
    });
    types.ty().subtype(&SubType {
      is_final: true,
      supertype_idx: None,
      composite_type: CompositeType {
        inner: CompositeInnerType::Func(FuncType::new(
          vec![func_ref],
          vec![any_ref],
        )),
        shared: false,
        descriptor: None,
        describes: None,
      },
    });
    self.idx.types.insert("$BoxFuncTy".into(), next_idx);
    next_idx += 1;

    // $TmpImport0 = (func (param i32 i32) (result (ref $Str)))
    // Temporary type for the str import — only exists pre-link.
    // The linker unifies this with string.wat's actual function type.
    if self.needs_string {
      let str_idx = self.idx.type_idx("$Str");
      let str_ref = ValType::Ref(RefType {
        nullable: false,
        heap_type: HeapType::Concrete(str_idx),
      });
      types.ty().subtype(&SubType {
        is_final: true,
        supertype_idx: None,
        composite_type: CompositeType {
          inner: CompositeInnerType::Func(FuncType::new(
            vec![ValType::I32, ValType::I32],
            vec![str_ref],
          )),
          shared: false,
          descriptor: None,
          describes: None,
        },
      });
      self.idx.types.insert("$TmpImport0".into(), next_idx);
      next_idx += 1;
    }

    // $TmpImportThunk = (func (result (ref null any)))
    // Used for list_nil and rec_new imports (both return a single value).
    types.ty().subtype(&SubType {
      is_final: true,
      supertype_idx: None,
      composite_type: CompositeType {
        inner: CompositeInnerType::Func(FuncType::new(
          vec![],
          vec![any_ref],
        )),
        shared: false,
        descriptor: None,
        describes: None,
      },
    });
    self.idx.types.insert("$TmpImportThunk".into(), next_idx);
    next_idx += 1;

    // $TmpImportListOp = (func (param (ref null any)) (result (ref null any)))
    // Temporary type for list_head / list_tail imports — used for param unpacking.
    types.ty().subtype(&SubType {
      is_final: true,
      supertype_idx: None,
      composite_type: CompositeType {
        inner: CompositeInnerType::Func(FuncType::new(
          vec![any_ref],
          vec![any_ref],
        )),
        shared: false,
        descriptor: None,
        describes: None,
      },
    });
    self.idx.types.insert("$TmpImportListOp".into(), next_idx);
    next_idx += 1;

    // $TmpImportListPrepend = (func (param (ref null any) (ref null any)) (result (ref null any)))
    // Temporary type for list_prepend import — used to build args lists at call sites.
    types.ty().subtype(&SubType {
      is_final: true,
      supertype_idx: None,
      composite_type: CompositeType {
        inner: CompositeInnerType::Func(FuncType::new(
          vec![any_ref, any_ref],
          vec![any_ref],
        )),
        shared: false,
        descriptor: None,
        describes: None,
      },
    });
    self.idx.types.insert("$TmpImportListPrepend".into(), next_idx);
    next_idx += 1;

    // $FnN for all needed arities.
    // $Fn2: continuations (caps, args) AND _apply_2(args, callee)
    // $Fn3: user functions (caps, args, cont) AND _apply_3(args, cont, callee)
    // Plus builtin arities.
    let mut all_arities: BTreeSet<usize> = BTreeSet::new();
    for &arity in builtins.values() {
      all_arities.insert(arity);
    }
    for &arity in extra_arities {
      all_arities.insert(arity);
    }
    for &arity in &all_arities {
      let name = format!("$Fn{}", arity);
      // Skip if already defined in canonical types (e.g. $Fn2, $Fn3 from types.wat).
      if self.idx.types.contains_key(&name) { continue; }
      let params: Vec<ValType> = vec![any_ref; arity];
      types.ty().subtype(&SubType {
        is_final: true,
        supertype_idx: None,
        composite_type: CompositeType {
          inner: CompositeInnerType::Func(FuncType::new(params, vec![])),
          shared: false,
          descriptor: None,
          describes: None,
        },
      });
      self.idx.types.insert(name, next_idx);
      next_idx += 1;
    }

    self.module.section(&types);
  }

  // -------------------------------------------------------------------------
  // Import section — builtins as imported functions
  // -------------------------------------------------------------------------

  fn emit_imports_from(&mut self, _cps_mod: &CpsModule<'a>, builtins: &BTreeMap<String, usize>) {
    let mut imports = ImportSection::new();
    let mut next_func_idx = 0u32;

    for (name, arity) in builtins {
      let type_idx = self.idx.fn_type_idx(*arity);
      imports.import("@fink/runtime", name, wasm_encoder::EntityType::Function(type_idx));
      self.idx.imports.insert(name.clone(), next_func_idx);
      next_func_idx += 1;
    }

    // str: (i32, i32) -> (ref $Str) — wraps data-section pointer.
    if self.needs_string {
      let type_idx = self.idx.type_idx("$TmpImport0");
      imports.import("@fink/runtime", "str", wasm_encoder::EntityType::Function(type_idx));
      self.idx.imports.insert("str".into(), next_func_idx);
      next_func_idx += 1;
    }

    // rec_empty: () -> (ref $RecImpl) — creates empty record.
    if self.needs_rec {
      let type_idx = self.idx.type_idx("$TmpImportThunk");
      imports.import("@fink/runtime", "rec_new", wasm_encoder::EntityType::Function(type_idx));
      self.idx.imports.insert("rec_new".into(), next_func_idx);
      next_func_idx += 1;
    }

    // List operations for the universal calling convention.
    // These are the _any wrappers (defined in list.wat) that take/return
    // (ref null any), bridging the emitter's type system with $Cons internals.
    {
      let list_op_idx = self.idx.type_idx("$TmpImportListOp");
      imports.import("@fink/runtime", "list_head_any", wasm_encoder::EntityType::Function(list_op_idx));
      self.idx.imports.insert("list_head".into(), next_func_idx);
      next_func_idx += 1;

      imports.import("@fink/runtime", "list_tail_any", wasm_encoder::EntityType::Function(list_op_idx));
      self.idx.imports.insert("list_tail".into(), next_func_idx);
      next_func_idx += 1;

      let list_prepend_idx = self.idx.type_idx("$TmpImportListPrepend");
      imports.import("@fink/runtime", "list_prepend_any", wasm_encoder::EntityType::Function(list_prepend_idx));
      self.idx.imports.insert("list_prepend".into(), next_func_idx);
      next_func_idx += 1;

      imports.import("@fink/runtime", "list_concat_any", wasm_encoder::EntityType::Function(list_prepend_idx));
      self.idx.imports.insert("list_concat".into(), next_func_idx);
      next_func_idx += 1;

      let list_nil_idx = self.idx.type_idx("$TmpImportThunk");
      imports.import("@fink/runtime", "list_nil", wasm_encoder::EntityType::Function(list_nil_idx));
      self.idx.imports.insert("list_nil".into(), next_func_idx);
      next_func_idx += 1;
    }

    // _apply / _apply_cont: closure dispatch (defined in dispatch.wat).
    {
      let fn2_idx = self.idx.fn_type_idx(2);
      imports.import("@fink/runtime", "_apply", wasm_encoder::EntityType::Function(fn2_idx));
      self.idx.imports.insert("_apply".into(), next_func_idx);
      next_func_idx += 1;

      let fn3_idx = self.idx.fn_type_idx(3);
      imports.import("@fink/runtime", "_apply_cont", wasm_encoder::EntityType::Function(fn3_idx));
      self.idx.imports.insert("_apply_cont".into(), next_func_idx);
      next_func_idx += 1;
    }

    self.idx.import_count = next_func_idx;

    if next_func_idx > 0 {
      self.module.section(&imports);
    }
  }

  // -------------------------------------------------------------------------
  // Function section — declares function signatures
  // -------------------------------------------------------------------------

  fn emit_functions(&mut self, cps_mod: &CpsModule<'a>, _closure_captures: &BTreeSet<usize>) {
    let mut functions = FunctionSection::new();

    // CPS-defined functions — $Fn3(caps, args, cont) or $Fn2(caps, args).
    let fn2_type = self.idx.fn_type_idx(2);
    let fn3_type = self.idx.fn_type_idx(3);
    for (i, func) in cps_mod.funcs.iter().enumerate() {
      let type_idx = if func.has_cont { fn3_type } else { fn2_type };
      functions.function(type_idx);
      let func_idx = self.idx.import_count + i as u32;
      self.idx.funcs.insert(func.label.clone(), func_idx);

      // Record structural loc for func header.
      if let Some(node) = self.ctx.ast_node(func.fn_id) {
        self.structural_locs.push(StructuralLoc {
          kind: StructuralKind::FuncHeader { func_idx },
          loc: node.loc,
        });
      }

      // Record structural locs for params.
      for (p_idx, (p_id, _, _)) in func.params.iter().enumerate() {
        if let Some(node) = self.ctx.ast_node(*p_id) {
          self.structural_locs.push(StructuralLoc {
            kind: StructuralKind::FuncParam { func_idx, param_idx: p_idx as u32 },
            loc: node.loc,
          });
        }
      }
    }

    // Helper functions are appended after CPS-defined functions.
    let mut next_func_idx = self.idx.import_count + cps_mod.funcs.len() as u32;

    // Implemented builtins as defined functions.
    for (name, arity) in &self.impl_builtins {
      let type_idx = self.idx.fn_type_idx(*arity);
      functions.function(type_idx);
      let internal_name = format!("_{}", name);
      self.idx.funcs.insert(internal_name, next_func_idx);
      next_func_idx += 1;
    }

    // _box_func helper: (func (param funcref) (result (ref null any)))
    let box_func_type_idx = self.idx.type_idx("$BoxFuncTy");
    functions.function(box_func_type_idx);
    self.idx.funcs.insert("_box_func".into(), next_func_idx);
    next_func_idx += 1;

    // _list_nil helper: wraps list_nil for the runner.
    // Type: (func (result (ref null any)))
    let list_nil_type = self.idx.type_idx("$TmpImportThunk");
    functions.function(list_nil_type);
    self.idx.funcs.insert("_list_nil".into(), next_func_idx);
    next_func_idx += 1;

    // _list_prepend helper: wraps list_prepend for the runner to build args lists.
    // Type: (func (param (ref null any) (ref null any)) (result (ref null any)))
    let list_prepend_type = self.idx.type_idx("$TmpImportListPrepend");
    functions.function(list_prepend_type);
    self.idx.funcs.insert("_list_prepend".into(), next_func_idx);
    next_func_idx += 1;

    // _fn2_stub: exported so the runner can get the $Fn2 type for host callbacks.
    let fn2_type = self.idx.fn_type_idx(2);
    functions.function(fn2_type);
    self.idx.funcs.insert("_fn2_stub".into(), next_func_idx);
    next_func_idx += 1;

    // $_panic: (func) — unreachable trap for irrefutable pattern failure.
    // Only emitted when ValKind::Panic appears in function bodies.
    if self.has_panic {
      let type_idx = self.idx.fn_type_idx(0);
      functions.function(type_idx);
      self.idx.funcs.insert("_panic".into(), next_func_idx);
      next_func_idx += 1;
    }

    let _ = next_func_idx;
    self.module.section(&functions);
  }

  // -------------------------------------------------------------------------
  // Memory section — linear memory (required by string runtime)
  // -------------------------------------------------------------------------

  fn emit_memory(&mut self) {
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
      minimum: self.string_data.pages().max(1),
      maximum: None,
      memory64: false,
      shared: false,
      page_size_log2: None,
    });
    self.module.section(&memories);
  }

  // -------------------------------------------------------------------------
  // Global section — module-level fn aliases
  // -------------------------------------------------------------------------

  fn emit_globals(&mut self, cps_mod: &CpsModule<'a>) {
    let mut globals = GlobalSection::new();
    let mut next_global_idx = 0u32;

    let fn2_type = self.idx.fn_type_idx(2);
    let fn3_type = self.idx.fn_type_idx(3);
    for func in &cps_mod.funcs {
      if let Some((alias_id, alias_label)) = &func.alias {
        let fn_type_idx = if func.has_cont { fn3_type } else { fn2_type };
        let func_idx = self.idx.func_idx(&func.label);

        // Record structural loc for global alias.
        if let Some(node) = self.ctx.ast_node(*alias_id) {
          self.structural_locs.push(StructuralLoc {
            kind: StructuralKind::Global { global_idx: next_global_idx },
            loc: node.loc,
          });
        }

        globals.global(
          GlobalType {
            val_type: ValType::Ref(RefType {
              nullable: true,
              heap_type: HeapType::Concrete(fn_type_idx),
            }),
            mutable: false,
            shared: false,
          },
          &ConstExpr::ref_func(func_idx),
        );
        self.idx.globals.insert(alias_label.clone(), next_global_idx);
        next_global_idx += 1;
      }
    }

    if next_global_idx > 0 {
      self.module.section(&globals);
    }
  }

  // -------------------------------------------------------------------------
  // Export section
  // -------------------------------------------------------------------------

  fn emit_exports(&mut self, cps_mod: &CpsModule<'a>) {
    let mut exports = ExportSection::new();

    for func in &cps_mod.funcs {
      if let Some(name) = &func.export_as {
        let func_idx = self.idx.func_idx(&func.label);
        exports.export(name, ExportKind::Func, func_idx);

        // Record structural loc for export.
        if let Some(bind_id) = func.export_bind_id
          && let Some(node) = self.ctx.ast_node(bind_id) {
            self.structural_locs.push(StructuralLoc {
              kind: StructuralKind::Export { name: name.clone() },
              loc: node.loc,
            });
          }
      }
    }

    // Always export __box_func for the host to create boxed continuations.
    let box_func_idx = self.idx.func_idx("_box_func");
    exports.export("_box_func", ExportKind::Func, box_func_idx);

    // _list_nil / _list_prepend: exported for the runner to build args lists.
    let list_nil_idx = self.idx.func_idx("_list_nil");
    exports.export("_list_nil", ExportKind::Func, list_nil_idx);
    let list_prepend_idx = self.idx.func_idx("_list_prepend");
    exports.export("_list_prepend", ExportKind::Func, list_prepend_idx);

    // _fn2_stub: exported so the runner can get the canonical $Fn2 type.
    let fn2_stub_idx = self.idx.func_idx("_fn2_stub");
    exports.export("_fn2_stub", ExportKind::Func, fn2_stub_idx);

    // TODO: memory should not be exported in production builds — only
    // needed for the runner to read string data during testing.
    exports.export("memory", ExportKind::Memory, 0);

    self.module.section(&exports);
  }

  // -------------------------------------------------------------------------
  // Element section — declarative segment for ref.func validation
  // -------------------------------------------------------------------------

  /// Emit a declarative element segment listing all defined functions.
  /// WASM requires functions referenced by ref.func (in code or global
  /// initialisers) to appear in an element segment.
  fn emit_elements(&mut self) {
    let mut func_indices: Vec<u32> = self.idx.funcs.values().copied().collect();
    func_indices.sort();
    let mut elements = ElementSection::new();
    elements.declared(Elements::Functions(func_indices.into()));
    self.module.section(&elements);
  }

  // -------------------------------------------------------------------------
  // Code section — function bodies with byte offset tracking
  // -------------------------------------------------------------------------

  fn emit_code(&mut self, cps_mod: &CpsModule<'a>, _closure_captures: &BTreeSet<usize>) {
    let mut code = CodeSection::new();

    for (def_idx, func) in cps_mod.funcs.iter().enumerate() {
      let wasm_func = self.emit_func_body(func, def_idx as u32);
      code.function(&wasm_func);
    }

    // Implemented builtin bodies.
    {
      let type_indices = super::builtins::TypeIndices {
        num: self.idx.type_idx("$Num"),
        closure: self.idx.type_idx("$Closure"),
        captures: self.idx.type_idx("$Captures"),
        fn1: self.idx.fn_type_idx(2), // $Fn2 — continuations
        croc1: self.idx.funcs.get("_apply_2").copied(),
      };
      let names: Vec<String> = self.impl_builtins.keys().cloned().collect();
      for name in names {
        let f = super::builtins::emit_builtin(&name, &type_indices);
        code.function(&f);
      }
    }

    // _box_func body: struct.new $Closure (funcref, ref.null $Captures)
    {
      let mut f = Function::new(vec![]);
      let closure_idx = self.idx.type_idx("$Closure");
      let captures_idx = self.idx.type_idx("$Captures");
      f.instruction(&Instruction::LocalGet(0));
      f.instruction(&Instruction::RefNull(HeapType::Concrete(captures_idx)));
      f.instruction(&Instruction::StructNew(closure_idx));
      f.instruction(&Instruction::End);
      code.function(&f);
    }

    // _list_nil body: delegates to imported list_nil.
    {
      let mut f = Function::new(vec![]);
      let list_nil_idx = self.idx.func_idx("list_nil");
      f.instruction(&Instruction::Call(list_nil_idx));
      f.instruction(&Instruction::End);
      code.function(&f);
    }

    // _list_prepend body: delegates to imported list_prepend.
    {
      let mut f = Function::new(vec![]);
      let list_prepend_idx = self.idx.func_idx("list_prepend");
      f.instruction(&Instruction::LocalGet(0));
      f.instruction(&Instruction::LocalGet(1));
      f.instruction(&Instruction::Call(list_prepend_idx));
      f.instruction(&Instruction::End);
      code.function(&f);
    }

    // _fn2_stub body: unreachable (only exists for its type).
    {
      let mut f = Function::new(vec![]);
      f.instruction(&Instruction::Unreachable);
      f.instruction(&Instruction::End);
      code.function(&f);
    }

    // $_panic body: unreachable
    if self.has_panic {
      let mut f = Function::new(vec![]);
      f.instruction(&Instruction::Unreachable);
      f.instruction(&Instruction::End);
      code.function(&f);
    }

    self.module.section(&code);
  }

  fn emit_func_body(&mut self, func: &CollectedFn<'a>, def_idx: u32) -> Function {
    let any_ref = ValType::Ref(RefType {
      nullable: true,
      heap_type: HeapType::Abstract { shared: false, ty: AbstractHeapType::Any },
    });

    // v2 calling convention:
    //   $Fn2(captures, args)        — continuations, match arms
    //   $Fn3(captures, args, cont)  — user functions
    //
    // WASM params:
    //   local 0 = captures (ref null any — cast to $Captures array)
    //   local 1 = args list (ref null any)
    //   local 2 = cont (ref null any) — only if has_cont
    //
    // CPS params split into:
    //   [0..n_captures)              → from captures array (array.get)
    //   [n_captures..N-has_cont)     → from args list (list_head/list_tail)
    //   [N-1] if has_cont            → from WASM cont param (local 2)
    let wasm_param_count: u32 = if func.has_cont { 3 } else { 2 };

    let mut local_map: HashMap<String, u32> = HashMap::new();
    let param_count = func.params.len() as u32;
    for (i, (_id, label, _is_spread)) in func.params.iter().enumerate() {
      local_map.insert(label.clone(), wasm_param_count + i as u32);
    }

    let locals_list = collect_locals(func.body, self.ctx);
    for (i, label) in locals_list.iter().enumerate() {
      local_map.insert(label.clone(), wasm_param_count + param_count + i as u32);
    }

    // Declare locals: CPS params + body locals + 1 scratch (all (ref null any)).
    let scratch_local = wasm_param_count + param_count + locals_list.len() as u32;
    let total_locals = param_count + locals_list.len() as u32 + 1;
    let locals: Vec<(u32, ValType)> = vec![(total_locals, any_ref)];
    let mut wasm_func = Function::new(locals);

    let captures_idx = self.idx.type_idx("$Captures");
    let list_head_idx = self.idx.func_idx("list_head");
    let list_tail_idx = self.idx.func_idx("list_tail");

    // Unpack capture params from the $Captures array (local 0).
    for i in 0..func.n_captures {
      wasm_func.instruction(&Instruction::LocalGet(0));  // captures
      wasm_func.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(captures_idx)));
      wasm_func.instruction(&Instruction::I32Const(i as i32));
      wasm_func.instruction(&Instruction::ArrayGet(captures_idx));
      wasm_func.instruction(&Instruction::LocalSet(wasm_param_count + i as u32));
    }

    // Unpack value params from the args list (local 1).
    // Cont (if has_cont) comes from WASM param, not the list.
    let val_end = if func.has_cont { func.params.len().saturating_sub(1).max(func.n_captures) } else { func.params.len() };
    let val_params = &func.params[func.n_captures..val_end];
    for (j, (_id, _label, is_spread)) in val_params.iter().enumerate() {
      let local_idx = wasm_param_count + (func.n_captures + j) as u32;
      if *is_spread {
        // Spread param gets the remaining args list as-is.
        wasm_func.instruction(&Instruction::LocalGet(1));  // args
        wasm_func.instruction(&Instruction::LocalSet(local_idx));
      } else {
        // Head → param local.
        wasm_func.instruction(&Instruction::LocalGet(1));  // args
        wasm_func.instruction(&Instruction::Call(list_head_idx));
        wasm_func.instruction(&Instruction::LocalSet(local_idx));
        // Tail → advance args cursor (unless this is the last val param).
        if j + 1 < val_params.len() {
          wasm_func.instruction(&Instruction::LocalGet(1));
          wasm_func.instruction(&Instruction::Call(list_tail_idx));
          wasm_func.instruction(&Instruction::LocalSet(1));
        }
      }
    }

    // Cont param: copy from WASM local 2 → CPS local.
    if func.has_cont && !func.params.is_empty() {
      let cont_local = wasm_param_count + (func.params.len() - 1) as u32;
      wasm_func.instruction(&Instruction::LocalGet(2));
      wasm_func.instruction(&Instruction::LocalSet(cont_local));
    }

    // Emit body instructions.
    let mut fc = FuncContext {
      func: &mut wasm_func,
      local_map: &local_map,
      emitter_idx: &self.idx,
      ctx: self.ctx,
      raw_mappings: &mut self.raw_mappings,
      def_idx,
      _has_closures: !self.closure_captures.is_empty(),
      string_data: &self.string_data,
      scratch_local,
      cont_fns: &self.cont_fns,
    };
    emit_body(func.body, &mut fc);

    // Every function body must end with `end`.
    wasm_func.instruction(&Instruction::End);

    wasm_func
  }

  // -------------------------------------------------------------------------
  // Data section — interned string literals
  // -------------------------------------------------------------------------

  fn emit_data(&mut self) {
    if self.string_data.is_empty() {
      return;
    }
    let mut data = DataSection::new();
    let offset = ConstExpr::i32_const(0);
    data.active(0, &offset, self.string_data.bytes.iter().copied());
    self.module.section(&data);
  }

  // -------------------------------------------------------------------------
  // Name section
  // -------------------------------------------------------------------------

  fn emit_names(&mut self, cps_mod: &CpsModule<'a>, _closure_captures: &BTreeSet<usize>) {
    let mut names = NameSection::new();

    // Function names (imports + defined + helpers).
    let mut func_names = NameMap::new();
    for (name, &idx) in &self.idx.imports {
      func_names.append(idx, name);
    }
    for func in &cps_mod.funcs {
      let idx = self.idx.func_idx(&func.label);
      func_names.append(idx, &func.label);
    }
    // _croc is now in the runtime (dispatch.wat), not emitted.
    // Implemented builtin names.
    for name in self.impl_builtins.keys() {
      let internal_name = format!("_{}", name);
      let idx = self.idx.func_idx(&internal_name);
      func_names.append(idx, &internal_name);
    }
    // Internal helpers.
    for name in &["_box_func", "_list_nil", "_list_prepend", "_fn2_stub"] {
      if let Some(&idx) = self.idx.funcs.get(*name) {
        func_names.append(idx, name);
      }
    }
    // $_panic helper.
    if self.has_panic {
      let panic_idx = self.idx.func_idx("_panic");
      func_names.append(panic_idx, "_panic");
    }
    names.functions(&func_names);

    // Local names per defined function.
    // Layout: WASM params (caps, args, [cont]), then CPS params, body locals, scratch.
    let mut all_locals = IndirectNameMap::new();
    for (def_idx, func) in cps_mod.funcs.iter().enumerate() {
      let func_idx = self.idx.import_count + def_idx as u32;
      let wasm_param_count: u32 = if func.has_cont { 3 } else { 2 };
      let mut local_names = NameMap::new();
      local_names.append(0, "_caps");
      local_names.append(1, "_args");
      if func.has_cont { local_names.append(2, "_cont"); }
      // CPS params.
      for (i, (_id, label, _)) in func.params.iter().enumerate() {
        local_names.append(wasm_param_count + i as u32, label);
      }
      let locals_list = collect_locals(func.body, self.ctx);
      let param_count = func.params.len() as u32;
      for (i, label) in locals_list.iter().enumerate() {
        local_names.append(wasm_param_count + param_count + i as u32, label);
      }
      all_locals.append(func_idx, &local_names);
    }
    names.locals(&all_locals);

    // Global names.
    let mut global_names = NameMap::new();
    for (label, &idx) in &self.idx.globals {
      global_names.append(idx, label);
    }
    names.globals(&global_names);

    self.module.section(&names);
  }
}

// ---------------------------------------------------------------------------
// Function body emission context
// ---------------------------------------------------------------------------

struct FuncContext<'a, 'b, 'src> {
  func: &'a mut Function,
  local_map: &'a HashMap<String, u32>,
  emitter_idx: &'a Indices,
  ctx: &'a IrCtx<'b, 'src>,
  raw_mappings: &'a mut Vec<RawMapping>,
  def_idx: u32,
  /// Whether this module has any closures (for future optimisation).
  _has_closures: bool,
  /// Interned string data for looking up literal offsets.
  string_data: &'a StringData,
  /// Scratch local for building args lists at call sites.
  scratch_local: u32,
  /// Function labels known to have has_cont=true ($Fn3 calling convention).
  cont_fns: &'a HashSet<String>,
}

impl<'a, 'b, 'src> FuncContext<'a, 'b, 'src> {
  /// Record a source mapping at the current byte position.
  fn mark(&mut self, id: CpsId) {
    if let Some(node) = self.ctx.ast_node(id)
      && node.loc.start.line > 0 {
        self.raw_mappings.push(RawMapping {
          func_def_index: self.def_idx,
          func_byte_offset: self.func.byte_len() as u32,
          loc: node.loc,
        });
      }
  }

  fn local_idx(&self, label: &str) -> u32 {
    *self.local_map.get(label).unwrap_or_else(|| panic!("unknown local: {}", label))
  }

  fn instr(&mut self, instruction: &Instruction<'_>) {
    self.func.instruction(instruction);
  }
}

// ---------------------------------------------------------------------------
// Body emission — mirrors wat/writer.rs emit_body logic
// ---------------------------------------------------------------------------

fn emit_body(expr: &Expr, fc: &mut FuncContext<'_, '_, '_>) {
  match &expr.kind {
    ExprKind::LetVal { name, val, cont } => {
      // Emit value with its own source mark (e.g. 42 → "42").
      emit_val(val, fc);
      // Mark the local.set instruction with the binding loc (e.g. x).
      fc.mark(name.id);
      let local_label = fc.ctx.label(name.id);
      let idx = fc.local_idx(&local_label);
      fc.instr(&Instruction::LocalSet(idx));

      match cont {
        Cont::Expr { body, .. } => emit_body(body, fc),
        Cont::Ref(id) => {
          // Tail call to continuation: unbox $Closure, return_call_ref $Fn1
          let local_label = fc.ctx.label(name.id);
          let local_idx = fc.local_idx(&local_label);
          fc.instr(&Instruction::LocalGet(local_idx));
          let cont_label = fc.ctx.label(*id);
          emit_get(fc, &cont_label);
          let closure_idx = fc.emitter_idx.type_idx("$Closure");
          fc.instr(&Instruction::RefCastNullable(HeapType::Concrete(closure_idx)));
          fc.instr(&Instruction::StructGet { struct_type_index: closure_idx, field_index: 0 });
          let fn1_type = fc.emitter_idx.fn_type_idx(2);
          fc.instr(&Instruction::RefCastNullable(HeapType::Concrete(fn1_type)));
          fc.mark(val.id);
          fc.instr(&Instruction::ReturnCallRef(fn1_type));
        }
      }
    }

    ExprKind::App { func, args } => {
      // Source mapping: detect cont calls vs user calls vs builtins.
      match func {
        Callable::BuiltIn(_) => {
          // Operator mark is emitted inside emit_builtin, after args,
          // to avoid DWARF collision with the first arg's value mark.
        }
        Callable::Val(_) => {
          // Source mark is placed inside emit_call, at the call instruction.
        }
      }
      emit_app(func, args, expr.id, fc);
    }

    ExprKind::If { cond, then, else_ } => {
      fc.mark(expr.id);
      // Unbox cond: ref.cast i31, i31.get_s → i32 (0 or 1).
      emit_val(cond, fc);
      fc.instr(&Instruction::RefCastNonNull(HeapType::Abstract {
        shared: false,
        ty: AbstractHeapType::I31,
      }));
      fc.instr(&Instruction::I31GetS);

      fc.instr(&Instruction::If(wasm_encoder::BlockType::Empty));
      emit_body(then, fc);
      fc.instr(&Instruction::Else);
      emit_body(else_, fc);
      fc.instr(&Instruction::End);
    }

    ExprKind::LetFn { cont, .. } => {
      // LetFn inside a fn body shouldn't appear post-lifting.
      if let Cont::Expr { body, .. } = cont {
        emit_body(body, fc);
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Application emission
// ---------------------------------------------------------------------------

fn emit_app(func: &Callable, args: &[Arg], expr_id: CpsId, fc: &mut FuncContext<'_, '_, '_>) {
  match func {
    Callable::BuiltIn(op) => emit_builtin(*op, args, expr_id, fc),
    Callable::Val(val) => emit_call(val, args, expr_id, fc),
  }
}

/// Emit a call to a user function or continuation.
///
/// Universal calling convention: all args (values + continuation) are packed
/// into a cons-cell list and passed as a single (ref null any) argument.
/// Dispatch goes through `_croc` which handles both plain funcrefs and closures.
fn emit_call(func_val: &Val, args: &[Arg], expr_id: CpsId, fc: &mut FuncContext<'_, '_, '_>) {
  let (val_args, cont_arg) = split_args(args);

  // Determine the source mark id: for cont calls, mark the result value;
  // for user calls, mark the call expression itself.
  let is_cont_call = match &func_val.kind {
    ValKind::ContRef(_) => true,
    ValKind::Ref(Ref::Synth(id)) => matches!(
      fc.ctx.ast_node(*id).map(|n| &n.kind),
      None | Some(crate::ast::NodeKind::Fn { .. })
    ),
    _ => false,
  };
  let mark_id = if is_cont_call {
    args.iter()
      .find_map(|a| if let Arg::Val(v) = a { Some(v.id) } else { None })
      .unwrap_or(expr_id)
  } else {
    expr_id
  };

  let list_prepend_idx = fc.emitter_idx.func_idx("list_prepend");
  let list_concat_idx = fc.emitter_idx.func_idx("list_concat");
  let scratch = fc.scratch_local;

  // Check if the callee is known to be $Fn2 (not in cont_fns).
  // When the callee is known $Fn2, fold the cont into the args list and use
  // _apply instead of _apply_cont. This prevents the _apply_cont fallback
  // from corrupting args by prepending cont for a callee that doesn't expect it.
  // For unknown callees (params, captures) we keep _apply_cont — the runtime
  // dispatch handles both $Fn3 and $Fn2 correctly when the callee IS $Fn3.
  let callee_known_fn2 = cont_arg.is_some() && match &func_val.kind {
    ValKind::Ref(Ref::Synth(id)) => !fc.cont_fns.contains(&fc.ctx.label(*id)),
    _ => false,
  };

  // Build args list. Start with nil, then prepend in reverse order.
  // For known $Fn2 callees with a cont, prepend cont first so it ends up
  // as the last element after val_args are prepended on top.
  let list_nil_idx = fc.emitter_idx.func_idx("list_nil");
  fc.instr(&Instruction::Call(list_nil_idx));
  fc.instr(&Instruction::LocalSet(scratch));

  if callee_known_fn2 && let Some(cont) = cont_arg {
    emit_cont(cont, fc);
    fc.instr(&Instruction::LocalGet(scratch));
    fc.instr(&Instruction::Call(list_prepend_idx));
    fc.instr(&Instruction::LocalSet(scratch));
  }

  for arg in val_args.iter().rev() {
    match arg {
      Arg::Spread(v) => {
        emit_val(v, fc);
        fc.instr(&Instruction::LocalGet(scratch));
        fc.instr(&Instruction::Call(list_concat_idx));
        fc.instr(&Instruction::LocalSet(scratch));
      }
      _ => {
        emit_arg(arg, fc);
        fc.instr(&Instruction::LocalGet(scratch));
        fc.instr(&Instruction::Call(list_prepend_idx));
        fc.instr(&Instruction::LocalSet(scratch));
      }
    }
  }

  // Dispatch: _apply_cont(args, cont, callee) or _apply(args, callee).
  fc.instr(&Instruction::LocalGet(scratch));  // args list
  if let Some(cont) = cont_arg {
    if callee_known_fn2 {
      // Cont already folded into args list above.
      emit_val_ref(func_val, fc);               // callee
      fc.mark(mark_id);
      let idx = fc.emitter_idx.func_idx("_apply");
      fc.instr(&Instruction::ReturnCall(idx));
    } else {
      emit_cont(cont, fc);                      // cont
      emit_val_ref(func_val, fc);               // callee
      fc.mark(mark_id);
      let idx = fc.emitter_idx.func_idx("_apply_cont");
      fc.instr(&Instruction::ReturnCall(idx));
    }
  } else {
    emit_val_ref(func_val, fc);               // callee
    fc.mark(mark_id);
    let idx = fc.emitter_idx.func_idx("_apply");
    fc.instr(&Instruction::ReturnCall(idx));
  }
}

/// Emit a builtin operation call.
fn emit_builtin(op: BuiltIn, args: &[Arg], expr_id: CpsId, fc: &mut FuncContext<'_, '_, '_>) {
  if op == BuiltIn::FnClosure {
    let (val_args, cont) = split_args(args);
    let n_captures = val_args.len().saturating_sub(1); // first arg is funcref

    // Build $Closure inline: funcref + array.new_fixed $Captures N (caps...)
    // First arg is the raw funcref (not boxed).
    if let Some(first) = val_args.first() {
      emit_arg_raw_funcref(first, fc);
    }

    let closure_idx = fc.emitter_idx.type_idx("$Closure");
    let captures_idx = fc.emitter_idx.type_idx("$Captures");

    if n_captures == 0 {
      // No captures — null captures array.
      fc.instr(&Instruction::RefNull(HeapType::Concrete(captures_idx)));
    } else {
      // Push captures onto stack, then array.new_fixed.
      for arg in val_args.iter().skip(1) {
        emit_arg(arg, fc);
      }
      fc.instr(&Instruction::ArrayNewFixed { array_type_index: captures_idx, array_size: n_captures as u32 });
    }

    // struct.new $Closure (funcref, captures_array_or_null)
    fc.instr(&Instruction::StructNew(closure_idx));

    match cont {
      Some(Cont::Expr { args: bind_args, body }) => {
        if let Some(bind) = bind_args.first() {
          let label = fc.ctx.label(bind.id);
          let idx = fc.local_idx(&label);
          fc.instr(&Instruction::LocalSet(idx));
        }
        emit_body(body, fc);
      }
      Some(Cont::Ref(id)) => {
        // return_call_ref $Fn1 cont closure_result
        // Stack has: closure_result. Need: cont closure_result.
        // Emit cont_get, then swap via local.
        // Actually wasm stack order for return_call_ref is: args... funcref
        // So: (return_call_ref $Fn1 (local.get $cont) (call $closure_N ...))
        // Wait — return_call_ref pops funcref last. The type sig is $Fn1 = (func (param (ref $Any)))
        // So stack order: arg0 funcref → return_call_ref
        // But $Fn1 takes 1 param, so: the one param, then the funcref.
        // Actually return_call_ref $Fn1 expects: [param0] [funcref] on stack.
        // No wait — in WAT inline form it's (return_call_ref $Fn1 callee arg),
        // but in stack machine, return_call_ref type_idx pops [args...] [funcref].
        // For $Fn1 with 1 param: stack must be [param0, funcref].
        //
        // The closure call result is already on the stack. We need:
        //   local.get $cont   (funcref)
        //   call $closure_N   (produces param0 = closure value)
        // But we already emitted call, so closure result is on stack.
        // We need to get the cont ref BEFORE the call... Let me restructure.
        //
        // For return_call_ref $Fn1: stack = [param0: ref $Any, funcref: ref $Fn1]
        // So emit: cont_get, then call closure, then return_call_ref.
        // No — that puts cont on stack first, then closure result on top.
        // Stack: [cont, closure_result] → but return_call_ref wants [param, funcref]
        // where param=closure_result, funcref=cont.
        // So we need: [closure_result, cont] on stack.
        //
        // Since we already emitted the call and the closure result is on stack,
        // we need to get cont on top. We can use a local.tee or just reorder.
        // Simplest: emit cont ref, emit call, swap is tricky in wasm.
        //
        // Actually, let me re-examine. The WAT writer emits:
        //   (return_call_ref $Fn1 (local.get $cont) (call $closure_N ...))
        // In WAT folded form, the first arg is the funcref for return_call_ref?
        // No — in WAT, return_call_ref $type takes the funcref as the LAST arg
        // in the unfolded stack form. But folded s-expressions evaluate left to right.
        //
        // Let me check: return_call_ref type pops the table: [args...] [funcref].
        // $Fn1 = (func (param (ref $Any))). So the stack is: [(ref $Any), (ref $Fn1)].
        // The funcref is on TOP of stack.
        //
        // WAT folded: (return_call_ref $Fn1 <arg0> <funcref>)
        // Evaluates left to right: pushes arg0, pushes funcref, then return_call_ref.
        //
        // So we need: push closure_result (the arg), push cont (the funcref).
        // But we already called closure and result is on stack.
        // So: result is on stack, now push cont ref, then return_call_ref.

        let cont_label = fc.ctx.label(*id);
        emit_get(fc, &cont_label);
        // Unbox $Closure → funcref.
        let closure_idx = fc.emitter_idx.type_idx("$Closure");
        fc.instr(&Instruction::RefCastNullable(HeapType::Concrete(closure_idx)));
        fc.instr(&Instruction::StructGet { struct_type_index: closure_idx, field_index: 0 });
        let fn1_type = fc.emitter_idx.fn_type_idx(2);
        fc.instr(&Instruction::RefCastNullable(HeapType::Concrete(fn1_type)));
        // Mark with the first val arg (the funcref to the lifted fn).
        let mark_id = val_args.first()
          .and_then(|a| if let Arg::Val(v) = a { Some(v.id) } else { None })
          .unwrap_or(expr_id);
        fc.mark(mark_id);
        fc.instr(&Instruction::ReturnCallRef(fn1_type));
      }
      None => {
        // Standalone call — result stays on stack (dropped by end).
      }
    }
    return;
  }

  if op == BuiltIn::StrFmt {
    let (val_args, cont_arg) = split_args(args);

    // Build $VarArgs array inline from value args.
    let varargs_idx = fc.emitter_idx.type_idx("$VarArgs");
    for arg in val_args {
      emit_arg(arg, fc);
    }
    fc.instr(&Instruction::ArrayNewFixed { array_type_index: varargs_idx, array_size: val_args.len() as u32 });

    // Push continuation.
    if let Some(cont) = cont_arg {
      emit_cont(cont, fc);
    }

    // return_call $str_fmt (segments_array, cont)
    let internal_name = format!("_{}", builtin_name(op));
    let func_idx = if fc.emitter_idx.funcs.contains_key(&internal_name) {
      fc.emitter_idx.func_idx(&internal_name)
    } else {
      fc.emitter_idx.func_idx(builtin_name(op))
    };
    fc.instr(&Instruction::ReturnCall(func_idx));
    return;
  }

  // Regular builtin: return_call $builtin_name args...
  // All args get their own source mark. The operator mark is placed after
  // args (at the return_call instruction), so no collision.
  let fn_name = builtin_name(op);
  let (val_args, cont_arg) = split_args(args);

  for arg in val_args {
    match arg {
      Arg::Val(v) | Arg::Spread(v) => emit_val(v, fc),
      Arg::Cont(cont) => emit_cont(cont, fc),
      _ => fc.instr(&Instruction::Unreachable),
    }
  }
  if let Some(cont) = cont_arg {
    emit_cont(cont, fc);
  }

  // Place operator mark right before return_call — after all args, so it
  // doesn't collide with the first arg's value mark at the same byte offset.
  if let Some(node) = fc.ctx.ast_node(expr_id) {
    let loc = match &node.kind {
      crate::ast::NodeKind::InfixOp { op, .. }
      | crate::ast::NodeKind::UnaryOp { op, .. } => op.loc,
      _ => node.loc,
    };
    if loc.start.line > 0 {
      fc.raw_mappings.push(RawMapping {
        func_def_index: fc.def_idx,
        func_byte_offset: fc.func.byte_len() as u32,
        loc,
      });
    }
  }

  // Implemented builtins are registered with _ prefix; imported ones keep original name.
  let internal_name = format!("_{}", fn_name);
  let func_idx = if fc.emitter_idx.funcs.contains_key(&internal_name) {
    fc.emitter_idx.func_idx(&internal_name)
  } else {
    fc.emitter_idx.func_idx(fn_name)
  };
  fc.instr(&Instruction::ReturnCall(func_idx));
}

// ---------------------------------------------------------------------------
// Value emission
// ---------------------------------------------------------------------------

/// Emit a value onto the stack (for inline use in expressions).
/// Marks the value's source location before emitting its instructions.
fn emit_val(val: &Val, fc: &mut FuncContext<'_, '_, '_>) {
  fc.mark(val.id);
  emit_val_inner(val, fc);
}

fn emit_val_inner(val: &Val, fc: &mut FuncContext<'_, '_, '_>) {
  match &val.kind {
    ValKind::Lit(lit) => emit_lit(lit, fc),
    ValKind::Ref(Ref::Synth(id)) => {
      let label = fc.ctx.label(*id);
      emit_get(fc, &label);
    }
    ValKind::Ref(Ref::Unresolved(_)) => {
      fc.instr(&Instruction::Unreachable);
    }
    ValKind::ContRef(id) => {
      let label = fc.ctx.label(*id);
      let idx = fc.local_idx(&label);
      fc.instr(&Instruction::LocalGet(idx));
    }
    ValKind::Panic => {
      // Emit a $Closure wrapping the $_panic function.
      let panic_idx = fc.emitter_idx.func_idx("_panic");
      let closure_idx = fc.emitter_idx.type_idx("$Closure");
      let captures_idx = fc.emitter_idx.type_idx("$Captures");
      fc.instr(&Instruction::RefFunc(panic_idx));
      fc.instr(&Instruction::RefNull(HeapType::Concrete(captures_idx)));
      fc.instr(&Instruction::StructNew(closure_idx));
    }
    ValKind::BuiltIn(_) => {
      fc.instr(&Instruction::Unreachable);
    }
  }
}

/// Emit a value reference — same as emit_val but used for callee position.
fn emit_val_ref(val: &Val, fc: &mut FuncContext<'_, '_, '_>) {
  emit_val(val, fc);
}

/// Emit a literal value.
fn emit_lit(lit: &Lit, fc: &mut FuncContext<'_, '_, '_>) {
  let num_idx = fc.emitter_idx.type_idx("$Num");
  match lit {
    Lit::Int(n) => {
      fc.instr(&Instruction::F64Const((*n as f64).into()));
      fc.instr(&Instruction::StructNew(num_idx));
    }
    Lit::Float(f) | Lit::Decimal(f) => {
      fc.instr(&Instruction::F64Const((*f).into()));
      fc.instr(&Instruction::StructNew(num_idx));
    }
    Lit::Bool(b) => {
      let v = if *b { 1i32 } else { 0i32 };
      fc.instr(&Instruction::I32Const(v));
      fc.instr(&Instruction::RefI31);
    }
    Lit::Str(s) => {
      // Look up the interned offset — string was pre-scanned, so this
      // always finds a match in the data blob.
      let (offset, len) = find_bytes(&fc.string_data.bytes, s.as_bytes())
        .map(|pos| (pos as u32, s.len() as u32))
        .expect("string literal not interned");
      let str_raw_idx = fc.emitter_idx.func_idx("str");
      fc.instr(&Instruction::I32Const(offset as i32));
      fc.instr(&Instruction::I32Const(len as i32));
      fc.instr(&Instruction::Call(str_raw_idx));
    }
    Lit::Seq => {
      // Empty list via list_nil (returns $Nil).
      let list_nil_idx = fc.emitter_idx.func_idx("list_nil");
      fc.instr(&Instruction::Call(list_nil_idx));
    }
    Lit::Rec => {
      // Empty record via rec_empty().
      let rec_empty_idx = fc.emitter_idx.func_idx("rec_new");
      fc.instr(&Instruction::Call(rec_empty_idx));
    }
  }
}

/// Emit a funcref argument without $FuncBox boxing.
/// Used for closure constructor's first arg which expects a raw funcref.
fn emit_arg_raw_funcref(arg: &Arg, fc: &mut FuncContext<'_, '_, '_>) {
  match arg {
    Arg::Val(v) | Arg::Spread(v) => {
      fc.mark(v.id);
      emit_get_raw_funcref(fc, v);
    }
    _ => emit_arg(arg, fc),
  }
}

/// Emit a raw funcref (ref.func or global.get) without $FuncBox wrapping.
fn emit_get_raw_funcref(fc: &mut FuncContext<'_, '_, '_>, val: &Val) {
  match &val.kind {
    ValKind::Ref(Ref::Synth(id)) => {
      let label = fc.ctx.label(*id);
      if fc.emitter_idx.globals.contains_key(&label) {
        let idx = fc.emitter_idx.global_idx(&label);
        fc.instr(&Instruction::GlobalGet(idx));
      } else if fc.emitter_idx.funcs.contains_key(&label) {
        let idx = fc.emitter_idx.func_idx(&label);
        fc.instr(&Instruction::RefFunc(idx));
      } else {
        // Local — should already be a funcref from closure struct extraction.
        let idx = fc.local_idx(&label);
        fc.instr(&Instruction::LocalGet(idx));
      }
    }
    _ => emit_val_inner(val, fc),
  }
}

/// Emit a call argument.
fn emit_arg(arg: &Arg, fc: &mut FuncContext<'_, '_, '_>) {
  match arg {
    Arg::Val(v) => emit_val(v, fc),
    Arg::Spread(v) => {
      // Wrap spread value in $SpreadArgs struct for runtime detection.
      emit_val(v, fc);
      let spread_idx = fc.emitter_idx.type_idx("$SpreadArgs");
      fc.instr(&Instruction::RefCastNonNull(HeapType::Concrete(
        fc.emitter_idx.type_idx("$List"),
      )));
      fc.instr(&Instruction::StructNew(spread_idx));
    }
    Arg::Cont(cont) => emit_cont(cont, fc),
    Arg::Expr(_) => {
      fc.instr(&Instruction::Unreachable);
    }
  }
}

/// Emit a continuation reference onto the stack.
fn emit_cont(cont: &Cont, fc: &mut FuncContext<'_, '_, '_>) {
  match cont {
    Cont::Ref(id) => {
      let label = fc.ctx.label(*id);
      emit_get(fc, &label);
    }
    Cont::Expr { .. } => {
      // Inline cont-as-arg should not appear.
      fc.instr(&Instruction::Unreachable);
    }
  }
}

/// Emit global.get, ref.func, or local.get depending on what the label refers to.
/// Function references (global.get for fn aliases, ref.func for lifted fns) are
/// boxed in $Closure (funcref, null captures) so they can flow through (ref null any) slots.
fn emit_get(fc: &mut FuncContext<'_, '_, '_>, label: &str) {
  let closure_idx = fc.emitter_idx.type_idx("$Closure");
  let captures_idx = fc.emitter_idx.type_idx("$Captures");
  if fc.emitter_idx.globals.contains_key(label) {
    let idx = fc.emitter_idx.global_idx(label);
    fc.instr(&Instruction::GlobalGet(idx));
    // Global is a funcref — box in $Closure for (ref any) compatibility.
    fc.instr(&Instruction::RefNull(HeapType::Concrete(captures_idx)));
    fc.instr(&Instruction::StructNew(closure_idx));
  } else if fc.emitter_idx.funcs.contains_key(label) {
    // Non-global function reference (e.g. lifted continuation) — use ref.func.
    let idx = fc.emitter_idx.func_idx(label);
    fc.instr(&Instruction::RefFunc(idx));
    // Box in $Closure for (ref any) compatibility.
    fc.instr(&Instruction::RefNull(HeapType::Concrete(captures_idx)));
    fc.instr(&Instruction::StructNew(closure_idx));
  } else {
    let idx = fc.local_idx(label);
    fc.instr(&Instruction::LocalGet(idx));
  }
}

// ---------------------------------------------------------------------------
// Builtin scanning — collect all builtins referenced in function bodies
// ---------------------------------------------------------------------------

fn scan_builtins(expr: &Expr, builtins: &mut BTreeMap<String, usize>) {
  match &expr.kind {
    ExprKind::App { func: Callable::BuiltIn(op), args } => {
      if *op == BuiltIn::FnClosure {
        // FnClosure is inlined (no function call) — just scan continuation bodies.
        for arg in args {
          if let Arg::Cont(Cont::Expr { body, .. }) = arg {
            scan_builtins(body, builtins);
          }
        }
      } else {
        let name = builtin_name(*op).to_string();
        // Builtin arity = total args (values + cont).
        let (val_args, cont_arg) = split_args(args);
        let arity = val_args.len() + if cont_arg.is_some() { 1 } else { 0 };
        builtins.entry(name).or_insert(arity);
      }
    }
    ExprKind::App { args, .. } => {
      // User call — scan cont bodies for nested builtins.
      for arg in args {
        if let Arg::Cont(Cont::Expr { body, .. }) = arg {
          scan_builtins(body, builtins);
        }
      }
    }
    ExprKind::LetVal { cont, .. } | ExprKind::LetFn { cont, .. } => {
      match cont {
        Cont::Expr { body, .. } => scan_builtins(body, builtins),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::If { then, else_, .. } => {
      scan_builtins(then, builtins);
      scan_builtins(else_, builtins);
    }
  }
  // Also scan fn bodies in LetFn.
  if let ExprKind::LetFn { fn_body, .. } = &expr.kind {
    scan_builtins(fn_body, builtins);
  }
}

/// Scan for ·fn_closure call sites and collect the capture count for each.
/// The capture count is val_args.len() - 1 (first val arg is the funcref).
/// Returns the set of distinct capture counts, used for _croc_N dispatch branches.
fn scan_closure_captures(expr: &Expr, captures: &mut BTreeSet<usize>) {
  match &expr.kind {
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::FnClosure), args } => {
      let (val_args, _) = split_args(args);
      // val_args = [funcref, cap_0, cap_1, ...], so captures = len - 1.
      let n_captures = val_args.len().saturating_sub(1);
      captures.insert(n_captures);
      for arg in args {
        if let Arg::Cont(Cont::Expr { body, .. }) = arg {
          scan_closure_captures(body, captures);
        }
      }
    }
    ExprKind::App { args, .. } => {
      for arg in args {
        if let Arg::Cont(Cont::Expr { body, .. }) = arg {
          scan_closure_captures(body, captures);
        }
      }
    }
    ExprKind::LetVal { cont, .. } | ExprKind::LetFn { cont, .. } => {
      match cont {
        Cont::Expr { body, .. } => scan_closure_captures(body, captures),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::If { then, else_, .. } => {
      scan_closure_captures(then, captures);
      scan_closure_captures(else_, captures);
    }
  }
  if let ExprKind::LetFn { fn_body, .. } = &expr.kind {
    scan_closure_captures(fn_body, captures);
  }
}

/// Check if any Val in the expression tree is ValKind::Panic.
fn scan_panic(expr: &Expr) -> bool {
  match &expr.kind {
    ExprKind::App { func, args } => {
      if let Callable::Val(v) = func
        && matches!(v.kind, ValKind::Panic) { return true; }
      for arg in args {
        match arg {
          Arg::Val(v) | Arg::Spread(v) => {
            if matches!(v.kind, ValKind::Panic) { return true; }
          }
          Arg::Cont(Cont::Expr { body, .. }) => {
            if scan_panic(body) { return true; }
          }
          _ => {}
        }
      }
    }
    ExprKind::LetVal { cont, .. } | ExprKind::LetFn { cont, .. } => {
      if let Cont::Expr { body, .. } = cont
        && scan_panic(body) { return true; }
    }
    ExprKind::If { then, else_, .. } => {
      if scan_panic(then) || scan_panic(else_) { return true; }
    }
  }
  if let ExprKind::LetFn { fn_body, .. } = &expr.kind
    && scan_panic(fn_body) { return true; }
  false
}

/// Check whether any value in the expression tree is a Lit::Rec.
fn scan_rec_lit(expr: &Expr) -> bool {
  fn is_rec_val(val: &Val) -> bool {
    matches!(val.kind, ValKind::Lit(Lit::Rec))
  }
  match &expr.kind {
    ExprKind::LetVal { val, cont, .. } => {
      if is_rec_val(val) { return true; }
      if let Cont::Expr { body, .. } = cont
        && scan_rec_lit(body) { return true; }
    }
    ExprKind::App { args, .. } => {
      for arg in args {
        match arg {
          Arg::Val(v) | Arg::Spread(v) => {
            if is_rec_val(v) { return true; }
          }
          Arg::Cont(Cont::Expr { body, .. }) => {
            if scan_rec_lit(body) { return true; }
          }
          _ => {}
        }
      }
    }
    ExprKind::LetFn { cont, .. } => {
      if let Cont::Expr { body, .. } = cont
        && scan_rec_lit(body) { return true; }
    }
    ExprKind::If { then, else_, .. } => {
      if scan_rec_lit(then) || scan_rec_lit(else_) { return true; }
    }
  }
  if let ExprKind::LetFn { fn_body, .. } = &expr.kind
    && scan_rec_lit(fn_body) { return true; }
  false
}

/// Intern any string literal found in a Val.
fn scan_val_strings(val: &Val, data: &mut StringData) {
  if let ValKind::Lit(Lit::Str(s)) = &val.kind {
    data.intern(s);
  }
}

/// Scan for string literals and intern them into the StringData blob.
fn scan_strings(expr: &Expr, data: &mut StringData) {
  match &expr.kind {
    ExprKind::LetVal { val, cont, .. } => {
      scan_val_strings(val, data);
      match cont {
        Cont::Expr { body, .. } => scan_strings(body, data),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::LetFn { fn_body, cont, .. } => {
      scan_strings(fn_body, data);
      match cont {
        Cont::Expr { body, .. } => scan_strings(body, data),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::App { args, .. } => {
      for arg in args {
        match arg {
          Arg::Val(v) | Arg::Spread(v) => scan_val_strings(v, data),
          Arg::Cont(Cont::Expr { body, .. }) => scan_strings(body, data),
          Arg::Cont(Cont::Ref(_)) => {}
          Arg::Expr(e) => scan_strings(e, data),
        }
      }
    }
    ExprKind::If { cond, then, else_, .. } => {
      scan_val_strings(cond, data);
      scan_strings(then, data);
      scan_strings(else_, data);
    }
  }
}

// ---------------------------------------------------------------------------
// Offset fixup — convert func-local offsets to absolute WASM byte offsets
// ---------------------------------------------------------------------------

fn fixup_offsets(wasm: &[u8], raw: Vec<RawMapping>) -> Vec<OffsetMapping> {
  use wasmparser::{Parser, Payload};

  if raw.is_empty() {
    return vec![];
  }

  // Find the code section and each function body's absolute offset.
  let mut func_body_offsets: Vec<u32> = Vec::new();

  for payload in Parser::new(0).parse_all(wasm) {
    match payload {
      Ok(Payload::CodeSectionStart { range, count, .. }) => {
        // Parse individual function bodies to get their offsets.
        let _ = (range, count);
      }
      Ok(Payload::CodeSectionEntry(body)) => {
        // Function::byte_len() counts from the start of the encoded function
        // body (including the locals declaration). body.range().start is the
        // absolute offset of the body's first byte (after the LEB128 size prefix).
        func_body_offsets.push(body.range().start as u32);
      }
      _ => {}
    }
  }

  raw.into_iter().map(|m| {
    let base = func_body_offsets.get(m.func_def_index as usize)
      .copied()
      .unwrap_or(0);
    OffsetMapping {
      wasm_offset: base + m.func_byte_offset,
      loc: m.loc,
    }
  }).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;
  use crate::passes::wasm::collect;

  fn compile(src: &str) -> EmitResult {
    let (lifted, desugared) = crate::to_lifted(src).unwrap_or_else(|e| panic!("{e}"));

    let ir_ctx = IrCtx::new(&lifted.result.origin, &desugared.ast_index);
    let module = collect::collect(&lifted.result.root, &ir_ctx);
    let ir_ctx = ir_ctx.with_globals(module.globals.clone());
    emit(&module, &ir_ctx)
  }

  #[test]
  fn t_simple_emit_parses() {
    // Verify the emitted binary is valid WASM.
    let result = compile("add = fn a, b: a + b");
    assert!(!result.wasm.is_empty(), "WASM output should not be empty");

    // Validate with wasmparser.
    use wasmparser::{Parser, Payload};
    let mut found_code = false;
    for payload in Parser::new(0).parse_all(&result.wasm) {
      match payload {
        Ok(Payload::CodeSectionEntry(_)) => { found_code = true; }
        Err(e) => panic!("invalid WASM: {}", e),
        _ => {}
      }
    }
    assert!(found_code, "should have a code section");
  }

  #[test]
  fn t_offset_mappings_present() {
    let result = compile("add = fn a, b: a + b");
    assert!(!result.offset_mappings.is_empty(), "should have offset mappings");
    // All offsets should be non-zero (after WASM header).
    for m in &result.offset_mappings {
      assert!(m.wasm_offset > 0, "offset should be > 0");
      assert!(m.loc.start.line > 0, "source line should be > 0");
    }
  }

  #[test]
  fn t_exports_present() {
    let result = compile("add = fn a, b: a + b");
    use wasmparser::{Parser, Payload};
    let mut found_export = false;
    for payload in Parser::new(0).parse_all(&result.wasm) {
      if let Ok(Payload::ExportSection(reader)) = payload {
        for export in reader {
          let export = export.unwrap();
          if export.name == "add" {
            found_export = true;
          }
        }
      }
    }
    assert!(found_export, "should export 'add'");
  }

  #[test]
  fn t_names_present() {
    let result = compile("add = fn a, b: a + b");
    use wasmparser::{Parser, Payload};
    let mut found_names = false;
    for payload in Parser::new(0).parse_all(&result.wasm) {
      if let Ok(Payload::CustomSection(reader)) = payload {
        if reader.name() == "name" {
          found_names = true;
        }
      }
    }
    assert!(found_names, "should have a name section");
  }

  #[test]
  fn t_literal_int_locals() {
    let result = compile("main = fn:\n  42");
    // Parse back and count locals for the first (user) function.
    use wasmparser::{Parser, Payload};
    for payload in Parser::new(0).parse_all(&result.wasm) {
      if let Ok(Payload::CodeSectionEntry(body)) = payload {
        let mut local_count = 0u32;
        let locals = body.get_locals_reader().unwrap();
        for group in locals {
          let (count, _ty) = group.unwrap();
          local_count += count;
        }
        // v2 calling convention: 1 CPS param (cont) + 1 scratch local = 2.
        assert_eq!(local_count, 2, "main = fn: 42 should have 2 locals (cont + scratch)");
        break; // Only check the first defined function (user code).
      }
    }
  }
}
