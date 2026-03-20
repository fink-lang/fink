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
  _resolve: &ResolveResult,
  ast_index: &PropGraph<AstId, Option<&AstNode<'_>>>,
) -> CodegenResult {
  let mut ctx = Ctx::new(&cps.origin, ast_index);

  // Collect all top-level functions from the CPS tree.
  collect_funcs(&cps.root, &mut ctx);

  let wasm = emit_module(&cps.root, &ctx);
  CodegenResult { wasm, mappings: ctx.mappings }
}

// ---------------------------------------------------------------------------
// Type indices — fixed layout, order matters
// ---------------------------------------------------------------------------

// Type section indices (must match emission order in emit_types).
const TY_ANY: u32 = 0;          // (sub (struct))
const TY_ANY_ARRAY: u32 = 1;    // (array (mut anyref))
const TY_INT: u32 = 2;          // (sub $Any (struct (field i64)))
const TY_FINK_FN: u32 = 3;      // (func (param (ref $AnyArray)) (param anyref))
const TY_FN_CLOSURE: u32 = 4;   // (sub $Any (struct (field (ref $FinkFn)) (field (ref $AnyArray))))
const TY_VOID: u32 = 5;         // (func) — no params, no results

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
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

struct Ctx<'a, 'src> {
  mappings: Vec<WasmMapping>,
  origin: &'a PropGraph<CpsId, Option<AstId>>,
  ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
  /// Collected top-level functions (LetFn nodes), in order.
  funcs: Vec<CollectedFn<'a, 'src>>,
}

impl<'a, 'src> Ctx<'a, 'src> {
  fn new(
    origin: &'a PropGraph<CpsId, Option<AstId>>,
    ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
  ) -> Self {
    Self { mappings: Vec::new(), origin, ast_index, funcs: Vec::new() }
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

fn emit_module(root: &Expr<'_>, ctx: &Ctx) -> Vec<u8> {
  let mut module = Module::new();

  // Sections must be added in the canonical WASM order.
  emit_types(&mut module);
  // No imports for now.
  emit_function_section(&mut module, ctx);
  emit_globals(&mut module);
  emit_exports(&mut module, ctx);
  emit_elem_section(&mut module, ctx);
  emit_code_section(root, &mut module, ctx);

  module.finish()
}

// ---------------------------------------------------------------------------
// Type section
// ---------------------------------------------------------------------------

fn emit_types(module: &mut Module) {
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

  // TY_FINK_FN = 3: (type $FinkFn (func (param (ref $AnyArray)) (param anyref)))
  types.ty().subtype(&SubType {
    is_final: true,
    supertype_idx: None,
    composite_type: ct(CompositeInnerType::Func(FuncType::new(
      [ValType::Ref(RefType { nullable: false, heap_type: wasm_encoder::HeapType::Concrete(TY_ANY_ARRAY) }), ValType::Ref(RefType::ANYREF)],
      [],
    ))),
  });

  // TY_FN_CLOSURE = 4: (sub $Any (struct (field (ref $FinkFn)) (field (ref $AnyArray))))
  types.ty().subtype(&SubType {
    is_final: false,
    supertype_idx: Some(TY_ANY),
    composite_type: ct(CompositeInnerType::Struct(wasm_encoder::StructType {
      fields: Box::new([
        FieldType {
          element_type: StorageType::Val(ValType::Ref(RefType {
            nullable: false,
            heap_type: wasm_encoder::HeapType::Concrete(TY_FINK_FN),
          })),
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

  module.section(&types);
}

// ---------------------------------------------------------------------------
// Function section (declares type index for each defined function)
// ---------------------------------------------------------------------------

fn emit_function_section(module: &mut Module, ctx: &Ctx) {
  let mut funcs = FunctionSection::new();

  // $__halt
  funcs.function(TY_FINK_FN);
  // $__call_closure
  funcs.function(TY_FINK_FN);

  // Compiled Fink functions
  for _ in &ctx.funcs {
    funcs.function(TY_FINK_FN);
  }

  // $__main
  funcs.function(TY_FINK_FN);

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

fn emit_code_section(root: &Expr<'_>, module: &mut Module, ctx: &Ctx) {
  let mut code = CodeSection::new();

  // $__halt
  code.function(&build_halt());

  // $__call_closure
  code.function(&build_call_closure());

  // Compiled Fink functions
  for collected in &ctx.funcs {
    code.function(&build_fink_fn(collected.fn_body, ctx));
  }

  // $__main
  code.function(&build_fink_fn(root, ctx));

  // fink_main — entry point
  code.function(&build_fink_main(root, ctx));

  module.section(&code);
}

// ---------------------------------------------------------------------------
// Built-in: $__halt
// ---------------------------------------------------------------------------

fn build_halt() -> Function {
  let mut f = Function::new([(1, ValType::Ref(RefType::ANYREF))]);

  // global.set $result (i31.get_s (ref.cast i31ref (array.get $AnyArray (local.get $args) (i32.const 0))))
  // Stack order: push args, push 0, array.get, ref.cast, i31.get_s, global.set
  f.instruction(&Instruction::LocalGet(0));      // $args
  f.instruction(&Instruction::I32Const(0));       // index 0
  f.instruction(&Instruction::ArrayGet(TY_ANY_ARRAY));
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

fn build_fink_fn(body: &Expr<'_>, ctx: &Ctx) -> Function {
  // All Fink functions: (param $args (ref $AnyArray)) (param $cont anyref)
  // Locals: we'll need temporaries for closure unpacking etc.
  let mut locals = vec![];
  let mut local_count: u32 = 2; // $args=0, $cont=1

  // Pre-scan for needed locals
  let needs_closure_tmp = true; // always need these for cont calls
  if needs_closure_tmp {
    locals.push((1, ValType::Ref(RefType { nullable: false, heap_type: wasm_encoder::HeapType::Concrete(TY_FN_CLOSURE) })));
    // local 2 = $tmp_closure
    locals.push((1, ValType::Ref(RefType { nullable: false, heap_type: wasm_encoder::HeapType::Concrete(TY_FINK_FN) })));
    // local 3 = $tmp_fn
    local_count += 2;
  }

  let mut f = Function::new(locals);
  let mut fc = FnCtx { local_count, ctx };
  emit_expr(body, &mut f, &mut fc);
  f.instruction(&Instruction::End);
  f
}

struct FnCtx<'a, 'b, 'src> {
  local_count: u32,
  ctx: &'a Ctx<'b, 'src>,
}

// ---------------------------------------------------------------------------
// Expression emission
// ---------------------------------------------------------------------------

fn emit_expr(expr: &Expr<'_>, f: &mut Function, fc: &mut FnCtx) {
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

    ExprKind::App { .. } => {
      // TODO: App codegen
      f.instruction(&Instruction::Unreachable);
    }

    _ => {
      f.instruction(&Instruction::Unreachable);
    }
  }
}

// ---------------------------------------------------------------------------
// Continuation call — pass a value to a cont
// ---------------------------------------------------------------------------

/// Emit instructions to pass a single value to a continuation.
///
/// If the cont is the $cont param (local 1), unpack it as FnClosure and tail-call.
/// If the cont is a known compiled function, use return_call directly.
fn emit_cont_call_with_val(val: &Val<'_>, cont_id: CpsId, f: &mut Function, fc: &FnCtx) {
  // Check if cont_id refers to a known compiled function.
  if let Some(fn_idx) = fc.ctx.func_index(cont_id) {
    // Direct call: build args array, ref.null for cont param, return_call
    emit_val(val, f);
    f.instruction(&Instruction::ArrayNewFixed { array_type_index: TY_ANY_ARRAY, array_size: 1 });
    f.instruction(&Instruction::RefNull(wasm_encoder::HeapType::Abstract { shared: false, ty: wasm_encoder::AbstractHeapType::None }));
    f.instruction(&Instruction::ReturnCall(fn_idx));
    return;
  }

  // Unknown cont — it's the $cont param (local 1). Unpack FnClosure and tail-call.
  // local 2 = $tmp_closure, local 3 = $tmp_fn
  f.instruction(&Instruction::LocalGet(1));       // $cont (anyref)
  f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::Concrete(TY_FN_CLOSURE)));
  f.instruction(&Instruction::LocalTee(2));       // $tmp_closure
  f.instruction(&Instruction::StructGet { struct_type_index: TY_FN_CLOSURE, field_index: 0 });
  f.instruction(&Instruction::LocalSet(3));       // $tmp_fn = fn_ref

  // Build args array: [val]
  emit_val(val, f);
  f.instruction(&Instruction::ArrayNewFixed { array_type_index: TY_ANY_ARRAY, array_size: 1 });

  // Cont's own cont: ref.null (halt doesn't use it; hoisted conts get it from captures)
  f.instruction(&Instruction::RefNull(wasm_encoder::HeapType::Abstract { shared: false, ty: wasm_encoder::AbstractHeapType::None }));

  // Tail-call via fn_ref
  f.instruction(&Instruction::LocalGet(3));       // $tmp_fn
  f.instruction(&Instruction::ReturnCallRef(TY_FINK_FN));
}

// ---------------------------------------------------------------------------
// Value emission
// ---------------------------------------------------------------------------

fn emit_val(val: &Val<'_>, f: &mut Function) {
  match &val.kind {
    ValKind::Lit(Lit::Int(n)) => {
      // Small ints → i31ref
      f.instruction(&Instruction::I32Const(*n as i32));
      f.instruction(&Instruction::RefI31);
    }
    ValKind::Lit(Lit::Bool(b)) => {
      f.instruction(&Instruction::I32Const(if *b { 1 } else { 0 }));
      f.instruction(&Instruction::RefI31);
    }
    _ => {
      // Placeholder for other value kinds
      f.instruction(&Instruction::I32Const(0));
      f.instruction(&Instruction::RefI31);
    }
  }
}

// ---------------------------------------------------------------------------
// Entry point: fink_main
// ---------------------------------------------------------------------------

fn build_fink_main(root: &Expr<'_>, ctx: &Ctx) -> Function {
  // fink_main is (param $args (ref $AnyArray)) (param $cont anyref) — same type as FinkFn
  // but exported and called with no meaningful args.
  //
  // It calls the compiled main fn with:
  //   args = empty array
  //   cont = FnClosure { $__halt, [] }
  let mut f = Function::new([]);

  // Find the main function
  let main_fn_idx = find_main_fn_index(&ctx.funcs)
    .map(|i| FN_COMPILED_START + i as u32)
    .unwrap_or(ctx.main_fn_index());

  // args: empty array
  f.instruction(&Instruction::ArrayNewFixed { array_type_index: TY_ANY_ARRAY, array_size: 0 });

  // cont: FnClosure { $__halt, [] }
  f.instruction(&Instruction::RefFunc(FN_HALT));
  f.instruction(&Instruction::ArrayNewFixed { array_type_index: TY_ANY_ARRAY, array_size: 0 });
  f.instruction(&Instruction::StructNew(TY_FN_CLOSURE));

  // Call main
  f.instruction(&Instruction::Call(main_fn_idx));

  f.instruction(&Instruction::End);
  f
}

// ---------------------------------------------------------------------------
// Function collection (walk CPS tree)
// ---------------------------------------------------------------------------

fn collect_funcs<'a, 'src>(expr: &'a Expr<'src>, ctx: &mut Ctx<'a, 'src>) {
  match &expr.kind {
    ExprKind::LetFn { name, fn_body, body, .. } => {
      ctx.funcs.push(CollectedFn {
        name_id: name.id,
        bind: name.kind,
        fn_body,
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
    let (lifted, resolved) = lift_all(cps, &ast_index);
    let lifted = lift(lifted);
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

  test_macros::include_fink_tests!("src/passes/wasm/test_codegen.fnk");
}
