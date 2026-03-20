// CPS IR → WASM binary codegen.
//
// Produces a WASM binary (Vec<u8>) with source mappings directly from CPS IR.
// Uses wasm-encoder to build the binary — no intermediate WAT text.
//
// Calling convention:
//   Every Fink function: (param $args (ref $AnyArray)) (param $cont anyref)
//   Continuation call: return_call $__call_closure (cont, result_array)
//   Built-in ops: inlined; result passed to cont
//
// Module layout:
//   Types:     $Any, $AnyArray, $Int, $FinkFn, $FnClosure
//   Imports:   env.print (i32 → void) — temporary debug helper
//   Globals:   $result (mut i32, exported)
//   Functions: $__halt, $__call_closure, compiled fns..., $__main, fink_main
//   Exports:   fink_main, result
//
// Source mapping:
//   Each instruction records (wasm_byte_offset, src_line, src_col) via the
//   CpsId → AstId origin map. Offsets are relative to the code section start;
//   a post-pass converts them to absolute module offsets.

use crate::ast::{AstId, Node as AstNode};
use crate::passes::cps::ir::{
  Arg, Bind, Callable, Cont, CpsId, CpsResult, Expr, ExprKind,
  Lit, Val, ValKind,
};
use crate::passes::name_res::ResolveResult;
use crate::passes::wasm::sourcemap::WasmMapping;
use crate::propgraph::PropGraph;

use wasm_encoder::{
  ArrayType, CodeSection, CompositeInnerType, CompositeType, ExportKind,
  ExportSection, FieldType, FuncType, Function, FunctionSection, GlobalSection,
  GlobalType, Instruction, Module, RefType, StorageType, SubType, TypeSection,
  ValType,
};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Codegen result: WASM binary + source mappings.
pub struct CodegenResult {
  pub wasm: Vec<u8>,
  pub mappings: Vec<WasmMapping>,
}

/// Compile fully-lifted CPS IR to WASM binary with source mappings.
pub fn codegen(
  cps: &CpsResult,
  resolve: &ResolveResult,
  ast_index: &PropGraph<AstId, Option<&AstNode<'_>>>,
) -> CodegenResult {
  let mut ctx = Ctx::new(&cps.origin, ast_index, resolve);

  // Collect all top-level functions from the CPS tree.
  collect_funcs(&cps.root, &mut ctx);

  let wasm = emit_module(&cps.root, &mut ctx);
  CodegenResult { wasm, mappings: ctx.mappings }
}

// ---------------------------------------------------------------------------
// Type indices — fixed layout, order matters
// ---------------------------------------------------------------------------

// Fixed type section indices (must match emission order in emit_types).
const TY_ANY: u32 = 0;          // (sub (struct))
const TY_ANY_ARRAY: u32 = 1;    // (array (mut anyref))
const TY_INT: u32 = 2;          // (sub $Any (struct (field i64)))
const TY_CONT: u32 = 3;         // (func (param anyref)) — continuation type (receives 1 result)
const TY_FN_CLOSURE: u32 = 4;   // (sub $Any (struct (field funcref) (field (ref $AnyArray))))
const TY_VOID: u32 = 5;         // (func) — no params, no results

/// First type index available for per-arity function types.
const TY_FUNC_START: u32 = 6;

// ---------------------------------------------------------------------------
// Function indices
// ---------------------------------------------------------------------------

// No imports for now — all builtins are defined in the module.
const FN_HALT: u32 = 0;              // $__halt
const FN_CALL_CLOSURE: u32 = 1;      // $__call_closure

/// First index available for compiled Fink functions.
const FN_COMPILED_START: u32 = 2;  // after $__halt and $__call_closure

// ---------------------------------------------------------------------------
// Global indices
// ---------------------------------------------------------------------------

const GLOBAL_RESULT: u32 = 0;

// ---------------------------------------------------------------------------
// Collected function
// ---------------------------------------------------------------------------

struct CollectedFn<'a, 'src> {
  name_id: CpsId,
  bind: Bind,
  fn_body: &'a Expr<'src>,
  /// CpsIds of the params (value params), in order.
  param_ids: Vec<CpsId>,
  /// CpsId of the cont param.
  cont_id: CpsId,
  /// Total arity (value params + cont).
  arity: u32,
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// A source mapping recorded during emission: function-relative byte offset.
struct RelativeMapping {
  /// Index of the function (in emission order within the code section).
  func_idx: u32,
  /// Byte offset within the function body.
  offset_in_body: u32,
  /// 0-indexed source line.
  src_line: u32,
  /// 0-indexed source column.
  src_col: u32,
}

struct Ctx<'a, 'src> {
  mappings: Vec<WasmMapping>,
  /// Relative mappings collected during emission — resolved after module.finish().
  relative_mappings: Vec<RelativeMapping>,
  origin: &'a PropGraph<CpsId, Option<AstId>>,
  ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
  resolve: &'a ResolveResult,
  /// Collected top-level functions (LetFn nodes), in order.
  funcs: Vec<CollectedFn<'a, 'src>>,
  /// Map from arity (number of params including cont) to type index.
  /// Populated during type section emission.
  arity_types: Vec<(u32, u32)>,  // (arity, type_index)
}

impl<'a, 'src> Ctx<'a, 'src> {
  fn new(
    origin: &'a PropGraph<CpsId, Option<AstId>>,
    ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
    resolve: &'a ResolveResult,
  ) -> Self {
    Self { mappings: Vec::new(), relative_mappings: Vec::new(), origin, ast_index, resolve, funcs: Vec::new(), arity_types: Vec::new() }
  }

  /// Get the type index for a function with the given arity (total params including cont).
  fn type_for_arity(&self, arity: u32) -> u32 {
    // TY_CONT is arity 1 (just the result value, no cont param)
    if arity == 1 { return TY_CONT; }
    self.arity_types.iter()
      .find(|(a, _)| *a == arity)
      .map(|(_, ty)| *ty)
      .unwrap_or(TY_CONT)  // fallback
  }

  /// Get the type index for a collected function by its index in ctx.funcs.
  fn func_type(&self, func_idx: usize) -> u32 {
    self.type_for_arity(self.funcs[func_idx].arity)
  }

  /// Get the WASM function index for a collected function by CpsId.
  fn func_index(&self, id: CpsId) -> Option<u32> {
    self.funcs.iter().position(|f| f.name_id == id)
      .map(|i| FN_COMPILED_START + i as u32)
  }

  /// Index of $__main (the last defined function before fink_main).
  fn main_fn_index(&self) -> u32 {
    FN_COMPILED_START + self.funcs.len() as u32
  }

  /// Index of fink_main (entry point export).
  fn fink_main_index(&self) -> u32 {
    self.main_fn_index() + 1
  }
}

// ---------------------------------------------------------------------------
// Module emission
// ---------------------------------------------------------------------------

fn emit_module<'a, 'src>(root: &'a Expr<'src>, ctx: &mut Ctx<'a, 'src>) -> Vec<u8> {
  let mut module = Module::new();

  // Compute per-arity types needed.
  compute_arity_types(ctx);

  // Sections must be added in the canonical WASM order.
  emit_types(&mut module, ctx);
  // No imports for now.
  emit_function_section(&mut module, ctx);
  emit_globals(&mut module);
  emit_exports(&mut module, ctx);
  emit_elem_section(&mut module, ctx);
  emit_code_section(root, &mut module, ctx);
  emit_name_section(&mut module, ctx);

  let wasm = module.finish();

  // Resolve relative mappings to absolute WASM byte offsets.
  resolve_mappings(&wasm, ctx);

  wasm
}

/// Scan collected functions and compute unique per-arity func types.
fn compute_arity_types(ctx: &mut Ctx) {
  let mut arities: Vec<u32> = Vec::new();
  for f in &ctx.funcs {
    if f.arity != 1 && !arities.contains(&f.arity) {
      arities.push(f.arity);
    }
  }
  // Also need arity for $__main (same arity as the root, which has 1 cont param = arity 1)
  // and $__halt (arity 1 = TY_CONT). These are covered by the fixed TY_CONT type.
  arities.sort();
  ctx.arity_types = arities.iter().enumerate()
    .map(|(i, &arity)| (arity, TY_FUNC_START + i as u32))
    .collect();
}

// ---------------------------------------------------------------------------
// Source map resolution
// ---------------------------------------------------------------------------

/// Convert function-relative byte offsets to absolute WASM module offsets.
///
/// Uses wasmparser to walk the code section and find each function body's
/// starting offset within the module binary.
fn resolve_mappings(wasm: &[u8], ctx: &mut Ctx) {
  use wasmparser::{Parser, Payload};

  // Parse the binary to find code section function body offsets.
  let mut func_body_offsets: Vec<u32> = Vec::new();

  for payload in Parser::new(0).parse_all(wasm) {
    if let Ok(Payload::CodeSectionEntry(body)) = payload {
      // body.range().start is the absolute offset of the function body
      // within the module binary (after the body size LEB128).
      func_body_offsets.push(body.range().start as u32);
    }
  }

  // Convert relative mappings to absolute WasmMapping entries.
  for rm in &ctx.relative_mappings {
    if let Some(&body_start) = func_body_offsets.get(rm.func_idx as usize) {
      ctx.mappings.push(WasmMapping {
        wasm_offset: body_start + rm.offset_in_body,
        src_line: rm.src_line,
        src_col: rm.src_col,
      });
    }
  }
}

// ---------------------------------------------------------------------------
// Name section (custom section with debug names for functions, types, etc.)
// ---------------------------------------------------------------------------

fn emit_name_section(module: &mut Module, ctx: &Ctx) {
  use wasm_encoder::{NameMap, NameSection};

  let mut names = NameSection::new();

  // Function names
  let mut func_names = NameMap::new();
  func_names.append(FN_HALT, "__halt");
  func_names.append(FN_CALL_CLOSURE, "__call_closure");

  for (i, collected) in ctx.funcs.iter().enumerate() {
    let idx = FN_COMPILED_START + i as u32;
    let name = func_name(collected, ctx);
    func_names.append(idx, &name);
  }

  func_names.append(ctx.main_fn_index(), "__main");
  func_names.append(ctx.fink_main_index(), "fink_main");

  names.functions(&func_names);

  // Type names
  let mut type_names = NameMap::new();
  type_names.append(TY_ANY, "Any");
  type_names.append(TY_ANY_ARRAY, "AnyArray");
  type_names.append(TY_INT, "Int");
  type_names.append(TY_CONT, "Cont");
  type_names.append(TY_FN_CLOSURE, "FnClosure");
  type_names.append(TY_VOID, "void");
  for &(arity, ty_idx) in &ctx.arity_types {
    type_names.append(ty_idx, &format!("Fn{}", arity));
  }
  names.types(&type_names);

  module.section(&names);
}

/// Derive a debug name for a collected function.
fn func_name(collected: &CollectedFn, ctx: &Ctx) -> String {
  use crate::ast::NodeKind;

  match collected.bind {
    Bind::Name => {
      // Look up the source name via origin map → AST node
      if let Some(Some(ast_id)) = ctx.origin.try_get(collected.name_id)
        && let Some(Some(ast_node)) = ctx.ast_index.try_get(*ast_id)
        && let NodeKind::Ident(name) = &ast_node.kind
      {
        return name.to_string();
      }
      format!("name_{}", collected.name_id.0)
    }
    Bind::Synth => format!("v_{}", collected.name_id.0),
    Bind::Cont => format!("k_{}", collected.name_id.0),
  }
}

// ---------------------------------------------------------------------------
// Type section
// ---------------------------------------------------------------------------

fn emit_types(module: &mut Module, ctx: &Ctx) {
  let mut types = TypeSection::new();

  let ct = |inner| CompositeType { inner, shared: false, descriptor: None, describes: None };

  // TY_ANY = 0: (type $Any (sub (struct)))
  types.ty().subtype(&SubType {
    is_final: false,
    supertype_idx: None,
    composite_type: ct(CompositeInnerType::Struct(wasm_encoder::StructType {
      fields: Box::new([]),
    })),
  });

  // TY_ANY_ARRAY = 1: (type $AnyArray (array (mut anyref)))
  types.ty().subtype(&SubType {
    is_final: true,
    supertype_idx: None,
    composite_type: ct(CompositeInnerType::Array(ArrayType(FieldType {
      element_type: StorageType::Val(ValType::Ref(RefType::ANYREF)),
      mutable: true,
    }))),
  });

  // TY_INT = 2: (type $Int (sub $Any (struct (field i64))))
  types.ty().subtype(&SubType {
    is_final: false,
    supertype_idx: Some(TY_ANY),
    composite_type: ct(CompositeInnerType::Struct(wasm_encoder::StructType {
      fields: Box::new([FieldType {
        element_type: StorageType::Val(ValType::I64),
        mutable: false,
      }]),
    })),
  });

  // TY_CONT = 3: (func (param anyref)) — continuation (receives 1 result value)
  types.ty().subtype(&SubType {
    is_final: true,
    supertype_idx: None,
    composite_type: ct(CompositeInnerType::Func(FuncType::new(
      [ValType::Ref(RefType::ANYREF)],
      [],
    ))),
  });

  // TY_FN_CLOSURE = 4: (sub $Any (struct (field funcref) (field (ref $AnyArray))))
  // Uses generic funcref since closure function types vary per arity.
  types.ty().subtype(&SubType {
    is_final: false,
    supertype_idx: Some(TY_ANY),
    composite_type: ct(CompositeInnerType::Struct(wasm_encoder::StructType {
      fields: Box::new([
        FieldType {
          element_type: StorageType::Val(ValType::Ref(RefType::FUNCREF)),
          mutable: false,
        },
        FieldType {
          element_type: StorageType::Val(ValType::Ref(RefType {
            nullable: false,
            heap_type: wasm_encoder::HeapType::Concrete(TY_ANY_ARRAY),
          })),
          mutable: false,
        },
      ]),
    })),
  });

  // TY_VOID = 5: (func) — no params, no results
  types.ty().subtype(&SubType {
    is_final: true,
    supertype_idx: None,
    composite_type: ct(CompositeInnerType::Func(FuncType::new([], []))),
  });

  // Per-arity function types: (func (param anyref * N))
  // N includes the cont param. E.g., arity 2 = (param anyref anyref) = 1 value + 1 cont.
  // Arity 1 is TY_CONT (already emitted). Only emit arity > 1.
  for &(arity, _) in &ctx.arity_types {
    let params: Vec<ValType> = (0..arity).map(|_| ValType::Ref(RefType::ANYREF)).collect();
    types.ty().subtype(&SubType {
      is_final: true,
      supertype_idx: None,
      composite_type: ct(CompositeInnerType::Func(FuncType::new(params, []))),
    });
  }

  module.section(&types);
}

// ---------------------------------------------------------------------------
// Function section (declares type index for each defined function)
// ---------------------------------------------------------------------------

fn emit_function_section(module: &mut Module, ctx: &Ctx) {
  let mut funcs = FunctionSection::new();

  // $__halt — receives 1 value (cont type)
  funcs.function(TY_CONT);
  // $__call_closure — placeholder (unused for now)
  funcs.function(TY_CONT);

  // Compiled Fink functions — each has its own arity-based type
  for (i, _) in ctx.funcs.iter().enumerate() {
    funcs.function(ctx.func_type(i));
  }

  // $__main — receives 1 cont (arity 1 = TY_CONT)
  funcs.function(TY_CONT);

  // fink_main (no params, no results — entry point)
  funcs.function(TY_VOID);

  module.section(&funcs);
}

// ---------------------------------------------------------------------------
// Global section
// ---------------------------------------------------------------------------

fn emit_globals(module: &mut Module) {
  let mut globals = GlobalSection::new();
  globals.global(
    GlobalType { val_type: ValType::I32, mutable: true, shared: false },
    &wasm_encoder::ConstExpr::i32_const(0),
  );
  module.section(&globals);
}

// ---------------------------------------------------------------------------
// Export section
// ---------------------------------------------------------------------------

fn emit_exports(module: &mut Module, ctx: &Ctx) {
  let mut exports = ExportSection::new();
  exports.export("fink_main", ExportKind::Func, ctx.fink_main_index());
  exports.export("result", ExportKind::Global, GLOBAL_RESULT);
  module.section(&exports);
}

// ---------------------------------------------------------------------------
// Element section (declares func refs used by ref.func)
// ---------------------------------------------------------------------------

fn emit_elem_section(module: &mut Module, ctx: &Ctx) {
  use wasm_encoder::{Elements, ElementSection, ElementSegment};

  let mut elems = ElementSection::new();

  // Collect all function indices that are used via ref.func.
  let mut refs = vec![FN_HALT];  // $__halt is always needed

  // Add the main entry function
  let main_fn = find_main_fn_index(&ctx.funcs);
  if let Some(idx) = main_fn {
    refs.push(FN_COMPILED_START + idx as u32);
  } else {
    refs.push(ctx.main_fn_index());
  }

  // Add all compiled functions (they may be referenced via ref.func for closures)
  for (i, _) in ctx.funcs.iter().enumerate() {
    let idx = FN_COMPILED_START + i as u32;
    if !refs.contains(&idx) {
      refs.push(idx);
    }
  }

  elems.segment(ElementSegment {
    mode: wasm_encoder::ElementMode::Declared,
    elements: Elements::Functions(refs.into()),
  });

  module.section(&elems);
}

// ---------------------------------------------------------------------------
// Code section
// ---------------------------------------------------------------------------

fn emit_code_section<'a, 'src>(root: &'a Expr<'src>, module: &mut Module, ctx: &mut Ctx<'a, 'src>) {
  let mut code = CodeSection::new();
  let mut rel = Vec::new();

  // $__halt (code_idx 0)
  code.function(&build_halt());

  // $__call_closure (code_idx 1)
  code.function(&build_call_closure());

  // Compiled Fink functions (code_idx 2..)
  let n_funcs = ctx.funcs.len();
  for i in 0..n_funcs {
    let body = ctx.funcs[i].fn_body;
    let arity = ctx.funcs[i].arity;
    code.function(&build_fink_fn(body, arity, FN_COMPILED_START + i as u32, ctx, &mut rel));
  }

  // $__main — receives 1 cont param (arity 1)
  let main_code_idx = FN_COMPILED_START + n_funcs as u32;
  code.function(&build_fink_fn(root, 1, main_code_idx, ctx, &mut rel));

  ctx.relative_mappings = rel;

  // fink_main — entry point
  code.function(&build_fink_main(root, ctx));

  module.section(&code);
}

// ---------------------------------------------------------------------------
// Built-in: $__halt
// ---------------------------------------------------------------------------

fn build_halt() -> Function {
  // $__halt: (param $result anyref) — receives the result value directly
  let mut f = Function::new([]);

  f.instruction(&Instruction::LocalGet(0));      // $result (anyref)
  f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::I31));
  f.instruction(&Instruction::I31GetS);
  f.instruction(&Instruction::GlobalSet(GLOBAL_RESULT));
  f.instruction(&Instruction::End);
  f
}

// ---------------------------------------------------------------------------
// Built-in: $__call_closure
// ---------------------------------------------------------------------------

fn build_call_closure() -> Function {
  // params: $args (ref $AnyArray), $cont anyref
  // But we also receive a closure as the cont — we need to unpack it.
  // Actually, __call_closure takes (closure, args, cont):
  //   - Extract fn_ref and caps from closure
  //   - Build new_args = caps ++ args (for now: just args, no cap prepending)
  //   - return_call_ref fn_ref (new_args, cont)
  //
  // But our FinkFn type is (ref $AnyArray, anyref) → void.
  // __call_closure has the same signature: it receives (args, cont).
  // args[0] = the closure to call.
  // Wait — that's a different calling convention.
  //
  // Let me re-think. In the WAT version, __call_closure was:
  //   (param $closure anyref) (param $args (ref $AnyArray)) (param $state (ref $State)) (param $cont anyref)
  // But we dropped $state. And our FinkFn type is (args, cont).
  //
  // The cleanest approach: __call_closure is NOT a FinkFn. It's a helper with its
  // own signature: (closure, args, cont). But then we need a separate type.
  //
  // For now, keep it simple: all cont calls go through __call_closure, which
  // has 3 params packed differently. Let's use a simpler approach:
  //
  // All continuations are FnClosure values. To call a cont with a result:
  //   1. Extract fn_ref from closure
  //   2. Build result_args = [result]
  //   3. return_call_ref $FinkFn (result_args, cont_of_cont) using fn_ref
  //
  // But the cont's own cont is unknown here. Actually in pure CPS, the cont
  // IS the final thing — it doesn't have its own cont. The $__halt cont
  // just stores the result.
  //
  // Simplification for now: call the cont's fn_ref with (result_args, null_cont).
  // The cont function ignores the cont param if it's $__halt.

  // __call_closure signature: same as FinkFn — (args, cont)
  // args[0] = value to pass (the result)
  // cont = the FnClosure to call
  //
  // No wait, let me match the WAT version's approach but with 2 params.
  // The caller does:
  //   return_call $__call_closure (closure_anyref) (result_array) ... but that's 3 things.
  //
  // OK let me look at this from the caller's perspective:
  //   LetVal { val: 42, body: Cont::Ref(cont_id) }
  //   → need to pass 42 to the cont bound at cont_id
  //   → the cont is a local variable holding an anyref (FnClosure)
  //   → unpack FnClosure → fn_ref + caps
  //   → build args = caps ++ [i31ref(42)]
  //   → return_call_ref $FinkFn (args, ???)
  //
  // The second param to the called function is its own cont. For a simple
  // cont like $__halt, it doesn't need a cont. For hoisted cont fns,
  // their outer_cont is in the captures.
  //
  // So: return_call_ref $FinkFn (args, ref.null none)
  // The called function gets its real cont from captures if it needs one.

  // Actually, let me just inline the closure call at each call site for now.
  // __call_closure adds complexity. The caller can do:
  //   local.get $cont
  //   ref.cast (ref $FnClosure)
  //   struct.get $FnClosure 0  → fn_ref
  //   struct.get $FnClosure 1  → caps
  //   ... build args ...
  //   return_call_ref $FinkFn (args, ref.null none)
  //
  // But we still need __call_closure for the general case with captures.
  // Let me emit a proper __call_closure that:
  //   1. Takes ($closure: anyref, $args: ref $AnyArray) — NOT FinkFn signature
  //   2. Unpacks closure → fn_ref + caps
  //   3. For now: ignores caps, calls fn_ref(args, ref.null none)

  // Hmm, but then __call_closure can't be type $FinkFn. Let me add a separate type.
  // Actually — let's avoid the type proliferation. Inline the closure dispatch
  // at each call site instead of using __call_closure.

  // For the minimal case (main = fn: 42), the only cont call is:
  //   pass 42 to $cont param → unpack $cont as FnClosure, call fn_ref
  //
  // Let's emit __call_closure as a $FinkFn where:
  //   $args[0] = the closure to call
  //   $args[1..] = the actual args to pass
  //   $cont = the cont to forward (usually ref.null)
  //
  // No, this is getting convoluted. Let me take the simplest approach:
  //
  // Each cont call site inlines the dispatch:
  //   local.get $cont           // anyref
  //   ref.cast (ref $FnClosure)
  //   local.tee $tmp_closure
  //   struct.get 0              // fn_ref
  //   local.set $tmp_fn
  //   ;; build args array with result
  //   array.new_fixed $AnyArray 1 (result_val)
  //   ;; cont's own cont: null (halt doesn't need one)
  //   ref.null none
  //   ;; call
  //   local.get $tmp_fn
  //   return_call_ref $FinkFn

  // This avoids needing __call_closure entirely for now.
  // Emit a dummy __call_closure that just unreachable's — placeholder for later.

  let mut f = Function::new([]);
  f.instruction(&Instruction::Unreachable);
  f.instruction(&Instruction::End);
  f
}

// ---------------------------------------------------------------------------
// Compiled Fink function
// ---------------------------------------------------------------------------

fn build_fink_fn<'a, 'b, 'src>(
  body: &Expr<'_>,
  arity: u32,
  code_idx: u32,
  ctx: &'a Ctx<'b, 'src>,
  rel: &mut Vec<RelativeMapping>,
) -> Function {
  // Function params are all anyref: N value params + 1 cont param.
  // The last param (index arity-1) is the cont.
  let mut f = Function::new([]);
  let cont_local = if arity > 0 { arity - 1 } else { 0 };

  // Build CpsId → local index mapping from collected function params.
  let mut locals = Vec::new();
  if let Some(func_info) = ctx.funcs.iter().find(|cf| {
    // Match by fn_body pointer — the collected fn whose body we're building
    std::ptr::eq(cf.fn_body as *const _, body as *const _)
  }) {
    for (i, &param_id) in func_info.param_ids.iter().enumerate() {
      locals.push((param_id, i as u32));
    }
    locals.push((func_info.cont_id, cont_local));
  }
  // Walk LetVal alias chains: `LetVal { name: x, val: Ref(Synth(param_id)) }`
  // means `x` is an alias for the param. Map x's CpsId to the same local.
  discover_aliases(body, &mut locals);

  let mut fc = FnCtx { local_count: arity, cont_local, locals, code_idx, ctx, rel };
  emit_expr(body, &mut f, &mut fc);
  f.instruction(&Instruction::End);
  f
}

struct FnCtx<'a, 'b, 'src> {
  local_count: u32,
  /// Local index of the continuation parameter (last param).
  cont_local: u32,
  /// Map from CpsId to WASM local index (for params and locals).
  locals: Vec<(CpsId, u32)>,
  /// Index of this function in the code section (0-based, matching code_idx in RelativeMapping).
  code_idx: u32,
  ctx: &'a Ctx<'b, 'src>,
  /// Mutable reference to the relative mappings vec (owned by emit_code_section).
  rel: &'a mut Vec<RelativeMapping>,
}

impl FnCtx<'_, '_, '_> {
  /// Look up the WASM local index for a CpsId.
  fn local_for(&self, id: CpsId) -> Option<u32> {
    self.locals.iter().find(|(cps_id, _)| *cps_id == id).map(|(_, idx)| *idx)
  }

  /// Look up a WASM local by matching AST origin of the target bind_id
  /// against the AST origins of the function's params.
  /// Used as fallback when CpsIds don't match after lifting passes.
  fn local_for_by_origin(&self, bind_id: CpsId) -> Option<u32> {
    // Get the AST origin of the target binding.
    let target_ast_id = self.ctx.origin.try_get(bind_id)?.as_ref()?;
    // Search locals for a param with the same AST origin.
    for &(local_cps_id, local_idx) in &self.locals {
      if let Some(Some(local_ast_id)) = self.ctx.origin.try_get(local_cps_id) {
        if local_ast_id == target_ast_id {
          return Some(local_idx);
        }
      }
    }
    None
  }
}

impl FnCtx<'_, '_, '_> {
  /// Record a source mapping for the current byte offset in the function body.
  fn mark(&mut self, f: &Function, cps_id: CpsId) {
    if let Some(Some(ast_id)) = self.ctx.origin.try_get(cps_id)
      && let Some(Some(ast_node)) = self.ctx.ast_index.try_get(*ast_id)
    {
      self.rel.push(RelativeMapping {
        func_idx: self.code_idx,
        offset_in_body: f.byte_len() as u32,
        src_line: ast_node.loc.start.line,
        src_col: ast_node.loc.start.col,
      });
    }
  }
}

// ---------------------------------------------------------------------------
// Expression emission
// ---------------------------------------------------------------------------

fn emit_expr(expr: &Expr<'_>, f: &mut Function, fc: &mut FnCtx) {
  // Record source mapping for this expression.
  fc.mark(f, expr.id);

  match &expr.kind {
    ExprKind::LetVal { val, body, .. } => {
      match body {
        Cont::Ref(cont_id) => {
          // Pass val to the continuation.
          // The cont is either $cont param or a known function.
          emit_cont_call_with_val(val, *cont_id, f, fc);
        }
        Cont::Expr { body: cont_body, .. } => {
          // Inline continuation — just emit the body.
          emit_expr(cont_body, f, fc);
        }
      }
    }

    ExprKind::LetFn { body, .. } => {
      // The fn was already emitted at top level. Continue with body.
      match body {
        Cont::Expr { body: cont_body, .. } => emit_expr(cont_body, f, fc),
        Cont::Ref(_) => {
          f.instruction(&Instruction::Unreachable);
        }
      }
    }

    ExprKind::App { func, args } => {
      emit_app(func, args, expr.id, f, fc);
    }

    _ => {
      f.instruction(&Instruction::Unreachable);
    }
  }
}

// ---------------------------------------------------------------------------
// App emission
// ---------------------------------------------------------------------------

fn emit_app(func: &Callable<'_>, args: &[Arg<'_>], _expr_id: CpsId, f: &mut Function, fc: &mut FnCtx) {
  use crate::passes::cps::ir::BuiltIn::*;

  match func {
    Callable::BuiltIn(op) => {
      // Separate value args from the trailing cont.
      let (val_args, cont) = split_app_args(args);

      match op {
        // Binary i31ref arithmetic: unwrap, compute, rewrap, pass to cont
        Add | Sub | Mul => {
          emit_arg_val(&val_args[0], f, fc);
          f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::I31));
          f.instruction(&Instruction::I31GetS);
          emit_arg_val(&val_args[1], f, fc);
          f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::I31));
          f.instruction(&Instruction::I31GetS);
          match op {
            Add => f.instruction(&Instruction::I32Add),
            Sub => f.instruction(&Instruction::I32Sub),
            Mul => f.instruction(&Instruction::I32Mul),
            _ => unreachable!(),
          };
          f.instruction(&Instruction::RefI31);
          // Result is on stack as anyref — pass to cont
          emit_cont_call_with_anyref(cont, f, fc);
        }

        _ => {
          f.instruction(&Instruction::Unreachable);
        }
      }
    }

    _ => {
      f.instruction(&Instruction::Unreachable);
    }
  }
}

/// Split App args into (value_args, cont_id).
/// The last Arg::Cont is the result continuation.
fn split_app_args<'a, 'src>(args: &'a [Arg<'src>]) -> (Vec<&'a Arg<'src>>, CpsId) {
  let mut val_args = Vec::new();
  let mut cont_id = None;
  for arg in args.iter().rev() {
    match arg {
      Arg::Cont(Cont::Ref(id)) if cont_id.is_none() => cont_id = Some(*id),
      Arg::Cont(Cont::Expr { .. }) if cont_id.is_none() => {
        // Inline cont — shouldn't happen after cont_lifting
        panic!("unexpected inline cont in App after cont_lifting");
      }
      _ => val_args.push(arg),
    }
  }
  val_args.reverse();
  (val_args, cont_id.expect("App must have a result cont"))
}

/// Emit an Arg as a value onto the WASM stack.
fn emit_arg_val(arg: &Arg<'_>, f: &mut Function, fc: &FnCtx) {
  match arg {
    Arg::Val(val) => emit_val(val, f, fc),
    _ => {
      f.instruction(&Instruction::Unreachable);
    }
  }
}

/// Tail-call a cont with the anyref value already on the stack.
fn emit_cont_call_with_anyref(cont_id: CpsId, f: &mut Function, fc: &FnCtx) {
  // Check if cont_id refers to a known compiled function.
  if let Some(fn_idx) = fc.ctx.func_index(cont_id) {
    // The target function may need additional params beyond the result value.
    // Cont-lifted functions take (value_params..., cont). We need to forward
    // our own cont so the callee can eventually return to it.
    let target_idx = fc.ctx.funcs.iter().position(|cf| cf.name_id == cont_id);
    if let Some(idx) = target_idx {
      let target_arity = fc.ctx.funcs[idx].arity;
      if target_arity > 1 {
        // Target takes (result, cont) — forward our cont.
        f.instruction(&Instruction::LocalGet(fc.cont_local));
        f.instruction(&Instruction::ReturnCall(fn_idx));
        return;
      }
    }
    // Arity 1 (just result) — no cont needed.
    f.instruction(&Instruction::ReturnCall(fn_idx));
    return;
  }

  // Unknown cont — the $cont param. Unpack FnClosure and tail-call.
  // Stack has: [anyref(result)]
  // Need: [anyref(result), (ref $Cont)]
  f.instruction(&Instruction::LocalGet(fc.cont_local));
  f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::Concrete(TY_FN_CLOSURE)));
  f.instruction(&Instruction::StructGet { struct_type_index: TY_FN_CLOSURE, field_index: 0 });
  f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::Concrete(TY_CONT)));
  f.instruction(&Instruction::ReturnCallRef(TY_CONT));
}

// ---------------------------------------------------------------------------
// Continuation call — pass a value to a cont
// ---------------------------------------------------------------------------

/// Emit instructions to pass a single value to a continuation.
///
/// If the cont is a known compiled function, use return_call directly.
/// Otherwise, the cont is the $cont param — call it via return_call_ref.
fn emit_cont_call_with_val(val: &Val<'_>, cont_id: CpsId, f: &mut Function, fc: &FnCtx) {
  // Check if cont_id refers to a known compiled function.
  if let Some(fn_idx) = fc.ctx.func_index(cont_id) {
    // Direct call: push value, return_call
    emit_val(val, f, fc);
    f.instruction(&Instruction::ReturnCall(fn_idx));
    return;
  }

  // Unknown cont — it's the $cont param. Conts are anyref (FnClosure at runtime).
  // Unpack FnClosure → funcref, then call with the value.
  // Cont type is TY_CONT = (func (param anyref)).
  emit_val(val, f, fc);              // push the result value
  f.instruction(&Instruction::LocalGet(fc.cont_local));  // push cont (anyref)
  f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::Concrete(TY_FN_CLOSURE)));
  f.instruction(&Instruction::StructGet { struct_type_index: TY_FN_CLOSURE, field_index: 0 });
  // Stack: [anyref(val), funcref] — cast funcref to (ref $Cont) for return_call_ref
  f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::Concrete(TY_CONT)));
  f.instruction(&Instruction::ReturnCallRef(TY_CONT));
}

// ---------------------------------------------------------------------------
// Value emission
// ---------------------------------------------------------------------------

fn emit_val(val: &Val<'_>, f: &mut Function, fc: &FnCtx) {
  match &val.kind {
    ValKind::Lit(Lit::Int(n)) => {
      f.instruction(&Instruction::I32Const(*n as i32));
      f.instruction(&Instruction::RefI31);
    }
    ValKind::Lit(Lit::Bool(b)) => {
      f.instruction(&Instruction::I32Const(if *b { 1 } else { 0 }));
      f.instruction(&Instruction::RefI31);
    }
    ValKind::Ref(r) => {
      // Reference to a bound value — resolve to bind-site CpsId, then look up WASM local.
      let bind_id = match r {
        crate::passes::cps::ir::Ref::Synth(id) => *id,
        crate::passes::cps::ir::Ref::Name => {
          // Use name resolution to find the bind-site CpsId.
          use crate::passes::name_res::Resolution;
          match fc.ctx.resolve.resolution.try_get(val.id) {
            Some(Some(Resolution::Local(bind_id))) => *bind_id,
            Some(Some(Resolution::Captured { bind, .. })) => *bind,
            _ => val.id,  // fallback
          }
        }
      };
      if let Some(local_idx) = fc.local_for(bind_id) {
        f.instruction(&Instruction::LocalGet(local_idx));
      } else {
        // Fallback: match by AST origin.
        // After lifting, CpsIds change but AST origins are preserved.
        if let Some(local_idx) = fc.local_for_by_origin(bind_id) {
          f.instruction(&Instruction::LocalGet(local_idx));
        } else {
          // TODO: cont_lifting creates params with no origin map entries.
          // Fix: copy origin entries to new params during lifting.
          f.instruction(&Instruction::Unreachable);
        }
      }
    }
    _ => {
      f.instruction(&Instruction::I32Const(0));
      f.instruction(&Instruction::RefI31);
    }
  }
}

// ---------------------------------------------------------------------------
// Entry point: fink_main
// ---------------------------------------------------------------------------

fn build_fink_main(_root: &Expr<'_>, ctx: &Ctx) -> Function {
  // fink_main: no params, no results. Entry point.
  // Calls the $__main function (or the first compiled fn) with a halt closure as cont.
  let mut f = Function::new([]);

  // Find the main function — it takes 1 param (the cont)
  let main_fn_idx = find_main_fn_index(&ctx.funcs)
    .map(|i| FN_COMPILED_START + i as u32)
    .unwrap_or(ctx.main_fn_index());

  // cont: FnClosure { $__halt, [] }
  f.instruction(&Instruction::RefFunc(FN_HALT));
  f.instruction(&Instruction::ArrayNewFixed { array_type_index: TY_ANY_ARRAY, array_size: 0 });
  f.instruction(&Instruction::StructNew(TY_FN_CLOSURE));

  // Call main(cont)
  f.instruction(&Instruction::Call(main_fn_idx));

  f.instruction(&Instruction::End);
  f
}

// ---------------------------------------------------------------------------
// Function collection (walk CPS tree)
// ---------------------------------------------------------------------------

fn collect_funcs<'a, 'src>(expr: &'a Expr<'src>, ctx: &mut Ctx<'a, 'src>) {
  match &expr.kind {
    ExprKind::LetFn { name, params, cont, fn_body, body } => {
      let param_ids: Vec<CpsId> = params.iter().map(|p| match p {
        crate::passes::cps::ir::Param::Name(b) => b.id,
        crate::passes::cps::ir::Param::Spread(b) => b.id,
      }).collect();
      ctx.funcs.push(CollectedFn {
        name_id: name.id,
        bind: name.kind,
        fn_body,
        arity: params.len() as u32 + 1,  // +1 for the cont param
        param_ids,
        cont_id: cont.id,
      });
      // Recurse into fn_body for nested LetFns
      collect_funcs(fn_body, ctx);
      if let Cont::Expr { body: cont_body, .. } = body {
        collect_funcs(cont_body, ctx);
      }
    }
    ExprKind::LetVal { body: Cont::Expr { body: cont_body, .. }, .. } => {
      collect_funcs(cont_body, ctx);
    }
    _ => {}
  }
}

/// Walk a function body's LetVal chain to discover aliases.
/// After cont_lifting, param values are often rebound: `·let ·v_N, fn x: ...`
/// where `·v_N` is Ref(Synth(param_id)). This means `x` (name.id) is an alias
/// for the param at local index of param_id.
fn discover_aliases(expr: &Expr<'_>, locals: &mut Vec<(CpsId, u32)>) {
  let mut current = expr;
  loop {
    match &current.kind {
      ExprKind::LetVal { name, val, body: Cont::Expr { body: cont_body, .. } } => {
        // Check if val is a Ref to an existing local
        if let ValKind::Ref(crate::passes::cps::ir::Ref::Synth(ref_id)) = &val.kind {
          if let Some(local_idx) = locals.iter().find(|(id, _)| id == ref_id).map(|(_, idx)| *idx) {
            locals.push((name.id, local_idx));
          }
        }
        current = cont_body;
      }
      ExprKind::LetFn { body: Cont::Expr { body: cont_body, .. }, .. } => {
        current = cont_body;
      }
      _ => break,
    }
  }
}

/// Find the index (within ctx.funcs) of the root LetFn — the module's `main`.
fn find_main_fn_index(funcs: &[CollectedFn]) -> Option<usize> {
  // The main fn is the first LetFn collected from the root chain.
  // For `main = fn: 42`, it's the outermost LetFn.
  if funcs.is_empty() { None } else { Some(0) }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use wasmtime::{Config, Engine, Module, Store};

  use crate::ast::build_index;
  use crate::parser::parse;
  use crate::passes::closure_lifting::lift_all;
  use crate::passes::cont_lifting::lift;
  use crate::passes::cps::transform::lower_expr;
  use super::codegen;

  fn compile_wasm(src: &str) -> Vec<u8> {
    let r = parse(src).expect("parse failed");
    let ast_index = build_index(&r);
    let cps = lower_expr(&r.root);
    let cps = lift(cps);
    let (lifted, _) = lift_all(cps, &ast_index);
    let lifted = lift(lifted);
    // Re-resolve after all lifting passes so refs map to final params.
    let resolved = crate::passes::name_res::resolve(&lifted.root, &lifted.origin, &ast_index, lifted.origin.len());
    codegen(&lifted, &resolved, &ast_index).wasm
  }

  /// Compile and run Fink source, return the i32 result as a string.
  fn run(src: &str) -> String {
    let wasm = compile_wasm(src);
    exec_wasm(&wasm).to_string()
  }

  fn exec_wasm(wasm: &[u8]) -> i32 {
    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_function_references(true);
    config.wasm_tail_call(true);
    let engine = Engine::new(&config).expect("engine");
    let module = Module::new(&engine, wasm).expect("module");
    let mut store = Store::new(&engine, ());
    let instance = wasmtime::Instance::new(&mut store, &module, &[]).expect("instance");
    let main = instance.get_func(&mut store, "fink_main").expect("fink_main");
    main.call(&mut store, &[], &mut []).expect("call fink_main");
    let result = instance.get_global(&mut store, "result").expect("result");
    match result.get(&mut store) {
      wasmtime::Val::I32(v) => v,
      v => panic!("expected i32 result, got {:?}", v),
    }
  }

  #[test]
  fn source_mappings_produced() {
    let r = parse("main = fn: 42").expect("parse failed");
    let ast_index = build_index(&r);
    let cps = lower_expr(&r.root);
    let cps = lift(cps);
    let (lifted, resolved) = lift_all(cps, &ast_index);
    let lifted = lift(lifted);
    let result = codegen(&lifted, &resolved, &ast_index);

    assert!(!result.mappings.is_empty(), "should produce source mappings");
    // At least one mapping should point to the literal 42 (line 1, col 11, 1-indexed)
    let has_literal = result.mappings.iter().any(|m| m.src_line == 1 && m.src_col == 11);
    assert!(has_literal, "should map to literal 42; got: {:?}", result.mappings);
  }

  test_macros::include_fink_tests!("src/passes/wasm/test_codegen.fnk");
}
