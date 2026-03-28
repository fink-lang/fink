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

use std::collections::{BTreeMap, HashMap};

use wasm_encoder::{
  CodeSection, CompositeInnerType, CompositeType, ConstExpr,
  ExportKind, ExportSection, FieldType, FuncType, Function,
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
  e.emit_types(module);
  e.emit_imports(module);
  e.emit_functions(module);
  e.emit_globals(module);
  e.emit_exports(module);
  e.emit_code(module);
  e.emit_names(module);
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
    }
  }

  // -------------------------------------------------------------------------
  // Type section
  // -------------------------------------------------------------------------

  fn emit_types(&mut self, cps_mod: &CpsModule<'_, '_>) {
    let mut types = TypeSection::new();
    let mut next_idx = 0u32;

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
      supertype_idx: Some(0), // $Any
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

    // $FnN for each arity
    let any_ref = ValType::Ref(RefType {
      nullable: true,
      heap_type: HeapType::Concrete(0), // $Any
    });
    for &arity in &cps_mod.arities {
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

    self.module.section(&types);
  }

  // -------------------------------------------------------------------------
  // Import section — builtins as imported functions
  // -------------------------------------------------------------------------

  fn emit_imports(&mut self, cps_mod: &CpsModule<'_, '_>) {
    let mut imports = ImportSection::new();
    let mut next_func_idx = 0u32;

    // Scan all function bodies for builtin references and collect unique names + arities.
    let mut builtins: BTreeMap<String, usize> = BTreeMap::new();
    for func in &cps_mod.funcs {
      scan_builtins(func.body, &mut builtins);
    }

    for (name, arity) in &builtins {
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

  fn emit_functions(&mut self, cps_mod: &CpsModule<'_, '_>) {
    let mut functions = FunctionSection::new();

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
    let mut has_exports = false;

    for func in &cps_mod.funcs {
      if let Some(name) = &func.export_as {
        let func_idx = self.idx.func_idx(&func.label);
        exports.export(name, ExportKind::Func, func_idx);
        has_exports = true;

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

    if has_exports {
      self.module.section(&exports);
    }
  }

  // -------------------------------------------------------------------------
  // Code section — function bodies with byte offset tracking
  // -------------------------------------------------------------------------

  fn emit_code(&mut self, cps_mod: &CpsModule<'_, '_>) {
    let mut code = CodeSection::new();

    for (def_idx, func) in cps_mod.funcs.iter().enumerate() {
      let wasm_func = self.emit_func_body(func, def_idx as u32);
      code.function(&wasm_func);
    }

    self.module.section(&code);
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
    };
    emit_body(func.body, &mut fc);

    // Every function body must end with `end`.
    wasm_func.instruction(&Instruction::End);

    wasm_func
  }

  // -------------------------------------------------------------------------
  // Name section
  // -------------------------------------------------------------------------

  fn emit_names(&mut self, cps_mod: &CpsModule<'_, '_>) {
    let mut names = NameSection::new();

    // Function names (imports + defined).
    let mut func_names = NameMap::new();
    for (name, &idx) in &self.idx.imports {
      func_names.append(idx, name);
    }
    for func in &cps_mod.funcs {
      let idx = self.idx.func_idx(&func.label);
      func_names.append(idx, &func.label);
    }
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
      // Mark with the = operator loc if available.
      let op_loc = fc.ctx.ast_node(expr.id)
        .and_then(|n| match &n.kind {
          crate::ast::NodeKind::Bind { op, .. } => Some(op.loc),
          _ => None,
        });
      if let Some(loc) = op_loc
        && loc.start.line > 0 {
          fc.raw_mappings.push(RawMapping {
            func_def_index: fc.def_idx,
            func_byte_offset: fc.func.byte_len() as u32,
            loc,
          });
        }

      // Emit value, then local.set.
      emit_val(val, fc);
      let local_label = fc.ctx.label(name.id);
      let idx = fc.local_idx(&local_label);
      fc.instr(&Instruction::LocalSet(idx));

      match cont {
        Cont::Expr { body, .. } => emit_body(body, fc),
        Cont::Ref(id) => {
          // Tail call to continuation: return_call_ref $Fn1 cont_ref local_val
          fc.mark(val.id);
          let cont_label = fc.ctx.label(*id);
          emit_get(fc, &cont_label);
          let local_label = fc.ctx.label(name.id);
          let local_idx = fc.local_idx(&local_label);
          fc.instr(&Instruction::LocalGet(local_idx));
          let fn1_type = fc.emitter_idx.fn_type_idx(1);
          fc.instr(&Instruction::ReturnCallRef(fn1_type));
        }
      }
    }

    ExprKind::App { func, args } => {
      // Source mapping: detect cont calls vs user calls vs builtins.
      match func {
        Callable::BuiltIn(_) => {
          if let Some(node) = fc.ctx.ast_node(expr.id) {
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
        }
        Callable::Val(func_val) => {
          let is_cont_call = match &func_val.kind {
            ValKind::ContRef(_) => true,
            ValKind::Ref(Ref::Synth(id)) => matches!(
              fc.ctx.ast_node(*id).map(|n| &n.kind),
              None | Some(crate::ast::NodeKind::Fn { .. })
            ),
            _ => false,
          };
          if is_cont_call {
            let mark_id = args.iter()
              .find_map(|a| if let Arg::Val(v) = a { Some(v.id) } else { None })
              .unwrap_or(expr.id);
            fc.mark(mark_id);
          } else {
            fc.mark(expr.id);
          }
        }
      }
      emit_app(func, args, fc);
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

fn emit_app(func: &Callable<'_>, args: &[Arg<'_>], fc: &mut FuncContext<'_, '_, '_>) {
  match func {
    Callable::BuiltIn(op) => emit_builtin(*op, args, fc),
    Callable::Val(val) => emit_call(val, args, fc),
  }
}

/// Emit a call to a user function or continuation.
fn emit_call(func_val: &Val<'_>, args: &[Arg<'_>], fc: &mut FuncContext<'_, '_, '_>) {
  let (val_args, cont_arg) = split_args(args);
  let total_arity = val_args.len() + if cont_arg.is_some() { 1 } else { 0 };

  // Callee.
  emit_val_ref(func_val, fc);

  // Value args.
  for arg in val_args {
    emit_arg(arg, fc);
  }

  // Continuation arg.
  if let Some(cont) = cont_arg {
    emit_cont(cont, fc);
  }

  let type_idx = fc.emitter_idx.fn_type_idx(total_arity);
  fc.instr(&Instruction::ReturnCallRef(type_idx));
}

/// Emit a builtin operation call.
fn emit_builtin(op: BuiltIn, args: &[Arg<'_>], fc: &mut FuncContext<'_, '_, '_>) {
  if op == BuiltIn::FnClosure {
    let (val_args, cont) = split_args(args);
    let n = val_args.len();
    let closure_name = format!("closure_{}", n);

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
        let fn1_type = fc.emitter_idx.fn_type_idx(1);
        fc.instr(&Instruction::ReturnCallRef(fn1_type));
      }
      None => {
        // Standalone call — result stays on stack (dropped by end).
      }
    }
    return;
  }

  // Regular builtin: return_call $builtin_name args...
  let fn_name = builtin_name(op);
  let (val_args, cont_arg) = split_args(args);

  for arg in val_args {
    emit_arg(arg, fc);
  }
  if let Some(cont) = cont_arg {
    emit_cont(cont, fc);
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
  // Mark every value with its AST source location (mirrors WAT writer's
  // WatExpr::marked(node.loc, inner) pattern).
  fc.mark(val.id);

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
    Arg::Cont(_) | Arg::Expr(_) => {
      // Should not appear at value arg position.
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

/// Emit global.get or local.get depending on whether the label is a global.
fn emit_get(fc: &mut FuncContext<'_, '_, '_>, label: &str) {
  if fc.emitter_idx.globals.contains_key(label) {
    let idx = fc.emitter_idx.global_idx(label);
    fc.instr(&Instruction::GlobalGet(idx));
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
        // FnClosure is special: the import is "closure_N" where N = value arg count.
        let (val_args, _) = split_args(args);
        let name = format!("closure_{}", val_args.len());
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
        // body.range() gives the byte range of this function body in the WASM binary.
        // The range includes the body size LEB128 prefix.
        // Instructions start after locals declaration.
        // We need the offset where the body's instruction bytes begin.
        // get_locals_reader skips past the locals declarations.
        // The function body range starts after the size prefix.
        // We use get_operators_reader to find where instructions start.
        let ops = body.get_operators_reader().expect("valid function body");
        let instr_offset = ops.original_position() as u32;
        func_body_offsets.push(instr_offset);
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
