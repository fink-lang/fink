// WASM binary emitter — encodes lifted CPS IR to WASM via wasm-encoder.
//
// Produces a WASM binary with:
//   - WasmGC types ($Any, $Num, $FnN per arity)
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
// - **literals** (struct.new wrapping f64.const) → point to the literal
//   value in source. Each value gets a mark from emit_val.
//
// - **structural items** (func headers, params, globals, exports) are
//   recorded as StructuralLoc entries, not DWARF, since they don't
//   correspond to code section byte offsets.
//
// DWARF line tables have one entry per byte offset. When two marks
// collide (same offset), the last one wins. The formatter reconstructs
// WAT text source maps by looking up DWARF entries for each instruction
// and structural locs for non-code items.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use wasm_encoder::{
  AbstractHeapType, BlockType, CodeSection, CompositeInnerType, CompositeType,
  ConstExpr, ExportKind, ExportSection, FieldType, FuncType, Function,
  FunctionSection, GlobalSection, GlobalType, HeapType,
  ImportSection, IndirectNameMap, Instruction,
  NameMap, NameSection, RefType, StorageType, SubType,
  StructType, TypeSection, ValType,
};

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
// Public API
// ---------------------------------------------------------------------------

/// Result of WASM binary emission.
pub struct EmitResult {
  pub wasm: Vec<u8>,
  pub offset_mappings: Vec<OffsetMapping>,
  /// Structural source locations for non-code items (func headers, globals, exports, params).
  /// The formatter uses these to place source marks on WAT structural lines.
  pub structural_locs: Vec<StructuralLoc>,
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
pub fn emit(module: &CpsModule<'_, '_>, ctx: &IrCtx<'_, '_>) -> EmitResult {
  let mut e = Emitter::new(module, ctx);
  // Scan builtins and call arities needed for the type section.
  let mut builtins: BTreeMap<String, usize> = BTreeMap::new();
  let mut extra_arities: BTreeSet<usize> = BTreeSet::new();
  let mut closure_captures: BTreeSet<usize> = BTreeSet::new();
  for func in &module.funcs {
    scan_builtins(func.body, &mut builtins);
    scan_call_arities(func.body, &mut extra_arities);
    scan_closure_captures(func.body, &mut closure_captures);
  }
  // closure_N constructors are now emitted as defined functions, not imports.
  builtins.retain(|name, _| !name.starts_with("_closure_"));
  // call_ref_or_clos_N dispatch helpers need $FnN types for all call arities.
  // The closure lifted fn arities (call_arity + captures) are already covered
  // by the defined function arities in cps_mod.arities.
  e.closure_captures = closure_captures.clone();
  e.call_arities = extra_arities.clone();
  e.emit_types(module, &builtins, &extra_arities, &closure_captures);
  e.emit_imports_from(module, &builtins);
  e.emit_functions(module, &closure_captures, &extra_arities);
  e.emit_globals(module);
  e.emit_exports(module);
  e.emit_code(module, &closure_captures);
  e.emit_names(module, &closure_captures, &extra_arities);
  let wasm = e.module.finish();

  // Fixup: convert func-local offsets to absolute offsets.
  let mappings = fixup_offsets(&wasm, e.raw_mappings);

  EmitResult { wasm, offset_mappings: mappings, structural_locs: e.structural_locs }
}

// ---------------------------------------------------------------------------
// Index management
// ---------------------------------------------------------------------------

/// Maps labels and builtins to WASM index spaces.
struct Indices {
  /// Type name → type index (e.g. "$Any" → 0, "$Num" → 1, "$Fn2" → 3).
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
  /// Closure capture counts found in this module (for $ClosureN types).
  closure_captures: BTreeSet<usize>,
  /// Call-site arities for Callable::Val calls (for $call_ref_or_clos_N).
  call_arities: BTreeSet<usize>,
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
  fn new(_module: &CpsModule<'_, '_>, ctx: &'a IrCtx<'a, 'src>) -> Self {
    Self {
      module: wasm_encoder::Module::new(),
      idx: Indices::new(),
      ctx,
      raw_mappings: Vec::new(),
      structural_locs: Vec::new(),
      closure_captures: BTreeSet::new(),
      call_arities: BTreeSet::new(),
    }
  }

  // -------------------------------------------------------------------------
  // Type section
  // -------------------------------------------------------------------------

  fn emit_types(&mut self, cps_mod: &CpsModule<'_, '_>, builtins: &BTreeMap<String, usize>, extra_arities: &BTreeSet<usize>, closure_captures: &BTreeSet<usize>) {
    let mut types = TypeSection::new();
    let mut next_idx = 0u32;

    let any_idx = next_idx;

    // $Any = (sub (struct))
    types.ty().subtype(&SubType {
      is_final: false,
      supertype_idx: None,
      composite_type: CompositeType {
        inner: CompositeInnerType::Struct(StructType {
          fields: Box::new([]),
        }),
        shared: false,
        descriptor: None,
        describes: None,
      },
    });
    self.idx.types.insert("$Any".into(), next_idx);
    next_idx += 1;

    // $Num = (sub $Any (struct (field f64)))
    types.ty().subtype(&SubType {
      is_final: false,
      supertype_idx: Some(any_idx), // $Any
      composite_type: CompositeType {
        inner: CompositeInnerType::Struct(StructType {
          fields: Box::new([FieldType {
            element_type: StorageType::Val(ValType::F64),
            mutable: false,
          }]),
        }),
        shared: false,
        descriptor: None,
        describes: None,
      },
    });
    self.idx.types.insert("$Num".into(), next_idx);
    next_idx += 1;

    // $FuncBox = (sub $Any (struct (field funcref)))
    // Boxes a funcref so it can flow through (ref null $Any) slots.
    let func_ref = ValType::Ref(RefType {
      nullable: true,
      heap_type: HeapType::Abstract { shared: false, ty: AbstractHeapType::Func },
    });
    types.ty().subtype(&SubType {
      is_final: true,
      supertype_idx: Some(any_idx),
      composite_type: CompositeType {
        inner: CompositeInnerType::Struct(StructType {
          fields: Box::new([FieldType {
            element_type: StorageType::Val(func_ref),
            mutable: false,
          }]),
        }),
        shared: false,
        descriptor: None,
        describes: None,
      },
    });
    self.idx.types.insert("$FuncBox".into(), next_idx);
    next_idx += 1;

    // $BoxFuncTy = (func (param funcref) (result (ref null $Any)))
    // Type for the __box_func helper exported for the host.
    let any_ref_val = ValType::Ref(RefType {
      nullable: true,
      heap_type: HeapType::Concrete(any_idx),
    });
    types.ty().subtype(&SubType {
      is_final: true,
      supertype_idx: None,
      composite_type: CompositeType {
        inner: CompositeInnerType::Func(FuncType::new(
          vec![func_ref],
          vec![any_ref_val],
        )),
        shared: false,
        descriptor: None,
        describes: None,
      },
    });
    self.idx.types.insert("$BoxFuncTy".into(), next_idx);
    next_idx += 1;

    // $FnN for each arity (from defined functions + builtins).
    let any_ref = ValType::Ref(RefType {
      nullable: true,
      heap_type: HeapType::Concrete(any_idx), // $Any
    });
    let mut all_arities = cps_mod.arities.clone();
    for &arity in builtins.values() {
      all_arities.insert(arity);
    }
    for &arity in extra_arities {
      all_arities.insert(arity);
    }
    // Closure constructors and dispatch helpers also need $FnN types.
    // $closure_N takes N captures + funcref → result, so its arity = N + 1.
    // But $closure_N returns (ref $Any), so it's not a $FnN type — it uses
    // a custom type. However, the lifted fn called from dispatch has
    // arity = call_arity + N captures, which must also be in $FnN.
    for &n_cap in closure_captures {
      for &call_arity in extra_arities.iter().chain(cps_mod.arities.iter()) {
        all_arities.insert(call_arity + n_cap);
      }
    }
    for &arity in &all_arities {
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
      self.idx.types.insert(format!("$Fn{}", arity), next_idx);
      next_idx += 1;
    }

    // $ClosureN = (sub $Any (struct (field (ref $FnM)) (field (ref $Any))...))
    // where N = capture count, M = call_arity + N (the lifted fn's arity).
    // We don't know the call_arity at the type level, so the funcref field
    // is typed as (ref func) — any function reference. The dispatch helper
    // casts it to the correct $FnM at call time.
    let func_ref = ValType::Ref(RefType {
      nullable: true,
      heap_type: HeapType::Abstract { shared: false, ty: AbstractHeapType::Func },
    });
    for &n_cap in closure_captures {
      let mut fields = Vec::with_capacity(1 + n_cap);
      // Field 0: funcref to the lifted function.
      fields.push(FieldType {
        element_type: StorageType::Val(func_ref),
        mutable: false,
      });
      // Fields 1..N: captured values (all (ref $Any)).
      for _ in 0..n_cap {
        fields.push(FieldType {
          element_type: StorageType::Val(any_ref),
          mutable: false,
        });
      }
      types.ty().subtype(&SubType {
        is_final: true,
        supertype_idx: Some(any_idx), // sub $Any
        composite_type: CompositeType {
          inner: CompositeInnerType::Struct(StructType {
            fields: fields.into_boxed_slice(),
          }),
          shared: false,
          descriptor: None,
          describes: None,
        },
      });
      self.idx.types.insert(format!("$Closure{}", n_cap), next_idx);
      next_idx += 1;
    }

    // $ClosureCtorN = (func (param (ref func) (ref $Any)...) (result (ref $Any)))
    // Type for $closure_N constructor functions.
    for &n_cap in closure_captures {
      let mut params = Vec::with_capacity(1 + n_cap);
      params.push(func_ref); // funcref
      for _ in 0..n_cap {
        params.push(any_ref); // captures
      }
      types.ty().subtype(&SubType {
        is_final: true,
        supertype_idx: None,
        composite_type: CompositeType {
          inner: CompositeInnerType::Func(FuncType::new(params, vec![any_ref])),
          shared: false,
          descriptor: None,
          describes: None,
        },
      });
      self.idx.types.insert(format!("$ClosureCtor{}", n_cap), next_idx);
      next_idx += 1;
    }

    // $CallRefOrClosN = (func (param (ref $Any)... (ref $Any)) (result))
    // Type for $call_ref_or_clos_N dispatch functions.
    // Only emitted when closures exist — otherwise calls use direct return_call_ref.
    // Params: N call args + 1 callee (all (ref $Any)), no results (tail call).
    let dispatch_arities = if closure_captures.is_empty() { BTreeSet::new() } else { extra_arities.clone() };
    for &call_arity in &dispatch_arities {
      let params: Vec<ValType> = vec![any_ref; call_arity + 1]; // args + callee
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
      self.idx.types.insert(format!("$CallRefOrClos{}", call_arity), next_idx);
      next_idx += 1;
    }

    self.module.section(&types);
  }

  // -------------------------------------------------------------------------
  // Import section — builtins as imported functions
  // -------------------------------------------------------------------------

  fn emit_imports_from(&mut self, _cps_mod: &CpsModule<'_, '_>, builtins: &BTreeMap<String, usize>) {
    let mut imports = ImportSection::new();
    let mut next_func_idx = 0u32;

    for (name, arity) in builtins {
      let type_idx = self.idx.fn_type_idx(*arity);
      imports.import("env", name, wasm_encoder::EntityType::Function(type_idx));
      self.idx.imports.insert(name.clone(), next_func_idx);
      next_func_idx += 1;
    }

    self.idx.import_count = next_func_idx;

    if !builtins.is_empty() {
      self.module.section(&imports);
    }
  }

  // -------------------------------------------------------------------------
  // Function section — declares function signatures
  // -------------------------------------------------------------------------

  fn emit_functions(&mut self, cps_mod: &CpsModule<'_, '_>, closure_captures: &BTreeSet<usize>, call_arities: &BTreeSet<usize>) {
    let mut functions = FunctionSection::new();

    // CPS-defined functions.
    for (i, func) in cps_mod.funcs.iter().enumerate() {
      let arity = func.params.len();
      let type_idx = self.idx.fn_type_idx(arity);
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
      for (p_idx, (p_id, _)) in func.params.iter().enumerate() {
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

    // $closure_N constructor functions.
    for &n_cap in closure_captures {
      let type_idx = self.idx.type_idx(&format!("$ClosureCtor{}", n_cap));
      functions.function(type_idx);
      // Name matches the old import convention: closure_N where N = total val args (funcref + captures).
      let name = format!("_closure_{}", n_cap + 1);
      self.idx.funcs.insert(name, next_func_idx);
      next_func_idx += 1;
    }

    // $call_ref_or_clos_N dispatch functions — only when closures exist.
    if !closure_captures.is_empty() {
      for &call_arity in call_arities {
        let type_idx = self.idx.type_idx(&format!("$CallRefOrClos{}", call_arity));
        functions.function(type_idx);
        let name = format!("_croc_{}", call_arity);
        self.idx.funcs.insert(name, next_func_idx);
        next_func_idx += 1;
      }
    }

    // __box_func helper: (func (param funcref) (result (ref null $Any)))
    let box_func_type_idx = self.idx.type_idx("$BoxFuncTy");
    functions.function(box_func_type_idx);
    self.idx.funcs.insert("_box_func".into(), next_func_idx);

    self.module.section(&functions);
  }

  // -------------------------------------------------------------------------
  // Global section — module-level fn aliases
  // -------------------------------------------------------------------------

  fn emit_globals(&mut self, cps_mod: &CpsModule<'_, '_>) {
    let mut globals = GlobalSection::new();
    let mut next_global_idx = 0u32;

    for func in &cps_mod.funcs {
      if let Some((alias_id, alias_label)) = &func.alias {
        let arity = func.params.len();
        let fn_type_idx = self.idx.fn_type_idx(arity);
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

  fn emit_exports(&mut self, cps_mod: &CpsModule<'_, '_>) {
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

    self.module.section(&exports);
  }

  // -------------------------------------------------------------------------
  // Code section — function bodies with byte offset tracking
  // -------------------------------------------------------------------------

  fn emit_code(&mut self, cps_mod: &CpsModule<'_, '_>, closure_captures: &BTreeSet<usize>) {
    let mut code = CodeSection::new();

    for (def_idx, func) in cps_mod.funcs.iter().enumerate() {
      let wasm_func = self.emit_func_body(func, def_idx as u32);
      code.function(&wasm_func);
    }

    // $closure_N constructor bodies.
    for &n_cap in closure_captures {
      code.function(&self.emit_closure_ctor(n_cap));
    }

    // $call_ref_or_clos_N dispatch bodies — only when closures exist.
    if !closure_captures.is_empty() {
      let call_arities: Vec<usize> = self.call_arities.iter().copied().collect();
      for call_arity in call_arities {
        code.function(&self.emit_call_ref_or_clos(call_arity, closure_captures));
      }
    }

    // __box_func body: (struct.new $FuncBox (local.get 0))
    {
      let mut f = Function::new(vec![]);
      let funcbox_idx = self.idx.type_idx("$FuncBox");
      f.instruction(&Instruction::LocalGet(0));
      f.instruction(&Instruction::StructNew(funcbox_idx));
      f.instruction(&Instruction::End);
      code.function(&f);
    }

    self.module.section(&code);
  }

  /// Emit $closure_N constructor: takes funcref + N captures, returns $ClosureN struct.
  fn emit_closure_ctor(&self, n_cap: usize) -> Function {
    let mut f = Function::new(vec![]); // no locals beyond params
    // struct.new $ClosureN (local.get 0) (local.get 1) ... (local.get n_cap)
    for i in 0..=(n_cap as u32) {
      f.instruction(&Instruction::LocalGet(i));
    }
    let closure_type_idx = self.idx.type_idx(&format!("$Closure{}", n_cap));
    f.instruction(&Instruction::StructNew(closure_type_idx));
    f.instruction(&Instruction::End);
    f
  }

  /// Emit $call_ref_or_clos_N dispatch: tries each $ClosureK via br_on_cast,
  /// falls through to plain funcref return_call_ref.
  ///
  /// Params: arg_0 .. arg_{N-1}, callee (all (ref $Any)).
  /// The callee is the last param at index N.
  fn emit_call_ref_or_clos(&self, call_arity: usize, closure_captures: &BTreeSet<usize>) -> Function {
    let any_rt = RefType {
      nullable: true,
      heap_type: HeapType::Concrete(self.idx.type_idx("$Any")),
    };
    let any_ref = ValType::Ref(any_rt);
    let callee_param = call_arity as u32; // last param index

    // One local to hold the downcast struct ref — reused across branches.
    let mut f = Function::new(vec![(1, any_ref)]);
    let cast_local = (call_arity + 1) as u32; // first local after params

    // For each $ClosureK, emit a block that tries the cast.
    // (block $not_clos
    //   (br_on_cast_fail $not_clos (ref $Any) (ref $ClosureK) (local.get $callee))
    //   ;; extract captures, push args, return_call_ref lifted fn
    // )
    // ...
    // ;; fallthrough: plain funcref → ref.cast + return_call_ref

    for &n_cap in closure_captures {
      let closure_type_idx = self.idx.type_idx(&format!("$Closure{}", n_cap));
      let lifted_arity = call_arity + n_cap;
      let fn_type_idx = self.idx.fn_type_idx(lifted_arity);

      let closure_rt = RefType {
        nullable: true,
        heap_type: HeapType::Concrete(closure_type_idx),
      };

      // (block $not_clos
      f.instruction(&Instruction::Block(BlockType::Empty));

      // (br_on_cast_fail $not_clos (ref $Any) (ref $ClosureN) (local.get $callee))
      f.instruction(&Instruction::LocalGet(callee_param));
      f.instruction(&Instruction::BrOnCastFail {
        relative_depth: 0,
        from_ref_type: any_rt,
        to_ref_type: closure_rt,
      });

      // Cast succeeded — struct ref is on stack. Store it.
      f.instruction(&Instruction::LocalSet(cast_local));

      // Push captures from struct (fields 1..N).
      for cap_idx in 0..n_cap {
        f.instruction(&Instruction::LocalGet(cast_local));
        f.instruction(&Instruction::StructGet {
          struct_type_index: closure_type_idx,
          field_index: (cap_idx + 1) as u32, // field 0 is funcref
        });
      }

      // Push the original call args.
      for i in 0..call_arity as u32 {
        f.instruction(&Instruction::LocalGet(i));
      }

      // Push funcref from struct field 0, cast to $FnM, call.
      f.instruction(&Instruction::LocalGet(cast_local));
      f.instruction(&Instruction::StructGet {
        struct_type_index: closure_type_idx,
        field_index: 0,
      });
      f.instruction(&Instruction::RefCastNullable(HeapType::Concrete(fn_type_idx)));
      f.instruction(&Instruction::ReturnCallRef(fn_type_idx));

      // End block — cast failed, continue to next closure type.
      f.instruction(&Instruction::End);
    }

    // Fallthrough: plain funcref in $FuncBox. Unbox, cast, call.
    let fn_type_idx = self.idx.fn_type_idx(call_arity);
    let funcbox_idx = self.idx.type_idx("$FuncBox");

    // Push args.
    for i in 0..call_arity as u32 {
      f.instruction(&Instruction::LocalGet(i));
    }

    // Push callee, unbox $FuncBox → funcref, cast to $FnN, call.
    f.instruction(&Instruction::LocalGet(callee_param));
    f.instruction(&Instruction::RefCastNullable(HeapType::Concrete(funcbox_idx)));
    f.instruction(&Instruction::StructGet { struct_type_index: funcbox_idx, field_index: 0 });
    f.instruction(&Instruction::RefCastNullable(HeapType::Concrete(fn_type_idx)));
    f.instruction(&Instruction::ReturnCallRef(fn_type_idx));

    f.instruction(&Instruction::End);
    f
  }

  fn emit_func_body(&mut self, func: &CollectedFn<'_, '_>, def_idx: u32) -> Function {
    let any_ref = ValType::Ref(RefType {
      nullable: true,
      heap_type: HeapType::Concrete(self.idx.type_idx("$Any")),
    });

    // Build local name → param/local index map for this function.
    let mut local_map: HashMap<String, u32> = HashMap::new();
    for (i, (_id, label)) in func.params.iter().enumerate() {
      local_map.insert(label.clone(), i as u32);
    }

    let locals_list = collect_locals(func.body, self.ctx);
    let param_count = func.params.len() as u32;
    for (i, label) in locals_list.iter().enumerate() {
      local_map.insert(label.clone(), param_count + i as u32);
    }

    // Create function with locals (params are implicit, only extra locals declared).
    let local_count = locals_list.len() as u32;
    let locals: Vec<(u32, ValType)> = if local_count > 0 {
      vec![(local_count, any_ref)]
    } else {
      vec![]
    };
    let mut wasm_func = Function::new(locals);

    // Emit body instructions.
    let mut fc = FuncContext {
      func: &mut wasm_func,
      local_map: &local_map,
      emitter_idx: &self.idx,
      ctx: self.ctx,
      raw_mappings: &mut self.raw_mappings,
      def_idx,
      has_closures: !self.closure_captures.is_empty(),
    };
    emit_body(func.body, &mut fc);

    // Every function body must end with `end`.
    wasm_func.instruction(&Instruction::End);

    wasm_func
  }

  // -------------------------------------------------------------------------
  // Name section
  // -------------------------------------------------------------------------

  fn emit_names(&mut self, cps_mod: &CpsModule<'_, '_>, closure_captures: &BTreeSet<usize>, call_arities: &BTreeSet<usize>) {
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
    // Helper function names.
    for &n_cap in closure_captures {
      let name = format!("_closure_{}", n_cap + 1);
      let idx = self.idx.func_idx(&name);
      func_names.append(idx, &name);
    }
    if !closure_captures.is_empty() {
      for &call_arity in call_arities {
        let name = format!("_croc_{}", call_arity);
        let idx = self.idx.func_idx(&name);
        func_names.append(idx, &name);
      }
    }
    // __box_func helper.
    let box_func_idx = self.idx.func_idx("_box_func");
    func_names.append(box_func_idx, "_box_func");
    names.functions(&func_names);

    // Local names per defined function.
    let mut all_locals = IndirectNameMap::new();
    for (def_idx, func) in cps_mod.funcs.iter().enumerate() {
      let func_idx = self.idx.import_count + def_idx as u32;
      let mut local_names = NameMap::new();
      for (i, (_id, label)) in func.params.iter().enumerate() {
        local_names.append(i as u32, label);
      }
      let locals_list = collect_locals(func.body, self.ctx);
      let param_count = func.params.len() as u32;
      for (i, label) in locals_list.iter().enumerate() {
        local_names.append(param_count + i as u32, label);
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
  /// Whether this module has any closures (controls dispatch path).
  has_closures: bool,
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

fn emit_body(expr: &Expr<'_>, fc: &mut FuncContext<'_, '_, '_>) {
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
          // Tail call to continuation: unbox $FuncBox, return_call_ref $Fn1
          let local_label = fc.ctx.label(name.id);
          let local_idx = fc.local_idx(&local_label);
          fc.instr(&Instruction::LocalGet(local_idx));
          let cont_label = fc.ctx.label(*id);
          emit_get(fc, &cont_label);
          let funcbox_idx = fc.emitter_idx.type_idx("$FuncBox");
          fc.instr(&Instruction::RefCastNullable(HeapType::Concrete(funcbox_idx)));
          fc.instr(&Instruction::StructGet { struct_type_index: funcbox_idx, field_index: 0 });
          let fn1_type = fc.emitter_idx.fn_type_idx(1);
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
      // Unbox cond: ref.cast (ref $Num), struct.get $Num 0, f64.ne 0
      emit_val(cond, fc);
      let num_idx = fc.emitter_idx.type_idx("$Num");
      fc.instr(&Instruction::RefCastNonNull(HeapType::Concrete(num_idx)));
      fc.instr(&Instruction::StructGet { struct_type_index: num_idx, field_index: 0 });
      fc.instr(&Instruction::F64Const(0.0_f64.into()));
      fc.instr(&Instruction::F64Ne);

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

fn emit_app(func: &Callable<'_>, args: &[Arg<'_>], expr_id: CpsId, fc: &mut FuncContext<'_, '_, '_>) {
  match func {
    Callable::BuiltIn(op) => emit_builtin(*op, args, expr_id, fc),
    Callable::Val(val) => emit_call(val, args, expr_id, fc),
  }
}

/// Emit a call to a user function or continuation.
/// When closures exist in the module, dispatches through $call_ref_or_clos_N
/// which handles both plain funcrefs and closure structs at runtime.
/// When no closures exist, uses direct return_call_ref.
fn emit_call(func_val: &Val<'_>, args: &[Arg<'_>], expr_id: CpsId, fc: &mut FuncContext<'_, '_, '_>) {
  let (val_args, cont_arg) = split_args(args);
  let total_arity = val_args.len() + if cont_arg.is_some() { 1 } else { 0 };

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

  let has_closures = fc.has_closures;

  if has_closures {
    for arg in val_args {
      emit_arg(arg, fc);
    }
    if let Some(cont) = cont_arg {
      emit_cont(cont, fc);
    }
    emit_val_ref(func_val, fc);

    // Mark the call instruction.
    fc.mark(mark_id);
    let dispatch_name = format!("_croc_{}", total_arity);
    let dispatch_idx = fc.emitter_idx.func_idx(&dispatch_name);
    fc.instr(&Instruction::ReturnCall(dispatch_idx));
  } else {
    for arg in val_args {
      emit_arg(arg, fc);
    }
    if let Some(cont) = cont_arg {
      emit_cont(cont, fc);
    }
    emit_val_ref(func_val, fc);

    let funcbox_idx = fc.emitter_idx.type_idx("$FuncBox");
    fc.instr(&Instruction::RefCastNullable(HeapType::Concrete(funcbox_idx)));
    fc.instr(&Instruction::StructGet { struct_type_index: funcbox_idx, field_index: 0 });
    let type_idx = fc.emitter_idx.fn_type_idx(total_arity);
    fc.instr(&Instruction::RefCastNullable(HeapType::Concrete(type_idx)));
    // Mark the call instruction.
    fc.mark(mark_id);
    fc.instr(&Instruction::ReturnCallRef(type_idx));
  }
}

/// Emit a builtin operation call.
fn emit_builtin(op: BuiltIn, args: &[Arg<'_>], expr_id: CpsId, fc: &mut FuncContext<'_, '_, '_>) {
  if op == BuiltIn::FnClosure {
    let (val_args, cont) = split_args(args);
    let n = val_args.len();
    let closure_name = format!("_closure_{}", n);

    // Emit closure args.
    for arg in val_args {
      emit_arg(arg, fc);
    }

    let closure_idx = fc.emitter_idx.func_idx(&closure_name);
    fc.instr(&Instruction::Call(closure_idx));

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
        // Unbox $FuncBox → funcref.
        let funcbox_idx = fc.emitter_idx.type_idx("$FuncBox");
        fc.instr(&Instruction::RefCastNullable(HeapType::Concrete(funcbox_idx)));
        fc.instr(&Instruction::StructGet { struct_type_index: funcbox_idx, field_index: 0 });
        let fn1_type = fc.emitter_idx.fn_type_idx(1);
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

  let func_idx = fc.emitter_idx.func_idx(fn_name);
  fc.instr(&Instruction::ReturnCall(func_idx));
}

// ---------------------------------------------------------------------------
// Value emission
// ---------------------------------------------------------------------------

/// Emit a value onto the stack (for inline use in expressions).
/// Marks the value's source location before emitting its instructions.
fn emit_val(val: &Val<'_>, fc: &mut FuncContext<'_, '_, '_>) {
  fc.mark(val.id);
  emit_val_inner(val, fc);
}

fn emit_val_inner(val: &Val<'_>, fc: &mut FuncContext<'_, '_, '_>) {
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
      fc.instr(&Instruction::Unreachable);
    }
    ValKind::BuiltIn(_) => {
      fc.instr(&Instruction::Unreachable);
    }
  }
}

/// Emit a value reference — same as emit_val but used for callee position.
fn emit_val_ref(val: &Val<'_>, fc: &mut FuncContext<'_, '_, '_>) {
  emit_val(val, fc);
}

/// Emit a literal value.
fn emit_lit(lit: &Lit<'_>, fc: &mut FuncContext<'_, '_, '_>) {
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
      let v = if *b { 1.0_f64 } else { 0.0_f64 };
      fc.instr(&Instruction::F64Const(v.into()));
      fc.instr(&Instruction::StructNew(num_idx));
    }
    Lit::Str(_) | Lit::Seq | Lit::Rec => {
      // TODO: not yet implemented.
      let any_idx = fc.emitter_idx.type_idx("$Any");
      fc.instr(&Instruction::RefNull(HeapType::Concrete(any_idx)));
    }
  }
}

/// Emit a call argument.
fn emit_arg(arg: &Arg<'_>, fc: &mut FuncContext<'_, '_, '_>) {
  match arg {
    Arg::Val(v) | Arg::Spread(v) => emit_val(v, fc),
    Arg::Cont(cont) => emit_cont(cont, fc),
    Arg::Expr(_) => {
      fc.instr(&Instruction::Unreachable);
    }
  }
}

/// Emit a continuation reference onto the stack.
fn emit_cont(cont: &Cont<'_>, fc: &mut FuncContext<'_, '_, '_>) {
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
/// Emit global.get, ref.func, or local.get depending on what the label refers to.
/// Function references (global.get for fn aliases, ref.func for lifted fns) are
/// boxed in $FuncBox so they can flow through (ref null $Any) slots.
fn emit_get(fc: &mut FuncContext<'_, '_, '_>, label: &str) {
  let funcbox_idx = fc.emitter_idx.type_idx("$FuncBox");
  if fc.emitter_idx.globals.contains_key(label) {
    let idx = fc.emitter_idx.global_idx(label);
    fc.instr(&Instruction::GlobalGet(idx));
    // Global is a funcref — box it for $Any compatibility.
    fc.instr(&Instruction::StructNew(funcbox_idx));
  } else if fc.emitter_idx.funcs.contains_key(label) {
    // Non-global function reference (e.g. lifted continuation) — use ref.func.
    let idx = fc.emitter_idx.func_idx(label);
    fc.instr(&Instruction::RefFunc(idx));
    // Box the funcref for $Any compatibility.
    fc.instr(&Instruction::StructNew(funcbox_idx));
  } else {
    let idx = fc.local_idx(label);
    fc.instr(&Instruction::LocalGet(idx));
  }
}

// ---------------------------------------------------------------------------
// Builtin scanning — collect all builtins referenced in function bodies
// ---------------------------------------------------------------------------

fn scan_builtins(expr: &Expr<'_>, builtins: &mut BTreeMap<String, usize>) {
  match &expr.kind {
    ExprKind::App { func: Callable::BuiltIn(op), args } => {
      if *op == BuiltIn::FnClosure {
        // FnClosure is special: _closure_N is a defined helper, not an import.
        let (val_args, _) = split_args(args);
        let name = format!("_closure_{}", val_args.len());
        let arity = val_args.len();
        builtins.entry(name).or_insert(arity);
        // Scan continuation bodies.
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

/// Scan function bodies for all call arities used by return_call_ref.
/// These may reference $Fn0 (thunks) or other arities not covered by
/// defined functions or builtin imports.
fn scan_call_arities(expr: &Expr<'_>, arities: &mut BTreeSet<usize>) {
  match &expr.kind {
    ExprKind::App { func: Callable::Val(_), args } => {
      let (val_args, cont_arg) = split_args(args);
      let arity = val_args.len() + if cont_arg.is_some() { 1 } else { 0 };
      // return_call_ref uses arity + 1 (funcref on stack).
      // But the type index is for the function being called, which has `arity` params.
      // Actually we just need all arities that appear. The +1 for funcref is implicit.
      arities.insert(arity);
      for arg in args {
        if let Arg::Cont(Cont::Expr { body, .. }) = arg {
          scan_call_arities(body, arities);
        }
      }
    }
    ExprKind::App { func: Callable::BuiltIn(_), args } => {
      for arg in args {
        if let Arg::Cont(Cont::Expr { body, .. }) = arg {
          scan_call_arities(body, arities);
        }
      }
    }
    ExprKind::LetVal { cont, .. } | ExprKind::LetFn { cont, .. } => {
      match cont {
        Cont::Expr { body, .. } => scan_call_arities(body, arities),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::If { then, else_, .. } => {
      scan_call_arities(then, arities);
      scan_call_arities(else_, arities);
    }
  }
  if let ExprKind::LetFn { fn_body, .. } = &expr.kind {
    scan_call_arities(fn_body, arities);
  }
}

/// Scan for ·fn_closure call sites and collect the capture count for each.
/// The capture count is val_args.len() - 1 (first val arg is the funcref).
/// Returns the set of distinct capture counts, used to emit $ClosureN types.
fn scan_closure_captures(expr: &Expr<'_>, captures: &mut BTreeSet<usize>) {
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
  use crate::ast::build_index;
  use crate::parser::parse;
  use crate::passes::cps::transform::lower_module;
  use crate::passes::lifting::lift;
  use crate::passes::wasm::collect;

  fn compile(src: &str) -> EmitResult {
    let r = parse(src).unwrap_or_else(|e| panic!("parse error: {}", e.message));
    let (root, node_count) = crate::passes::partial::apply(r.root, r.node_count)
      .unwrap_or_else(|e| panic!("partial error: {:?}", e));
    let r = crate::ast::ParseResult { root, node_count };
    let ast_index = build_index(&r);
    let scope = crate::passes::scopes::analyse(&r.root, r.node_count as usize, &[]);
    let exprs = match &r.root.kind {
      crate::ast::NodeKind::Module(exprs) => &exprs.items,
      _ => panic!("expected module"),
    };
    let cps = lower_module(exprs, &scope);
    let lifted = lift(cps, &ast_index);

    let ir_ctx = IrCtx::new(&lifted.origin, &ast_index);
    let module = collect::collect(&lifted.root, &ir_ctx);
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
    // Parse back and count locals per function.
    use wasmparser::{Parser, Payload};
    for payload in Parser::new(0).parse_all(&result.wasm) {
      if let Ok(Payload::CodeSectionEntry(body)) = payload {
        let mut local_count = 0u32;
        let locals = body.get_locals_reader().unwrap();
        for group in locals {
          let (count, _ty) = group.unwrap();
          local_count += count;
          eprintln!("  local group: count={}", count);
        }
        eprintln!("total locals: {}", local_count);
        assert_eq!(local_count, 0, "main = fn: 42 should have 0 locals");
      }
    }
  }
}
