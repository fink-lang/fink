// CPS IR → WASM binary codegen.
//
// Produces a WASM binary (Vec<u8>) with source mappings directly from CPS IR.
// Uses wasm-encoder to build the binary — no intermediate WAT text.
//
// Calling convention:
//   Every Fink function: (param anyref * N) where last param is the cont.
//   Cont param holds a (ref $Cont) funcref at runtime.
//   All cont calls are direct return_call or return_call_ref $Cont.
//   Built-in ops are inlined; result passed to cont.
//
// Module layout:
//   Types:     $Any, $Int, $Cont, $void, per-arity $FnN
//   Globals:   $result (mut i32, exported)
//   Functions: $__halt, compiled fns..., fink_main
//   Exports:   fink_main, result
//
// Source mapping:
//   Each instruction records (wasm_byte_offset, src_line, src_col) via the
//   CpsId → AstId origin map. Offsets are relative to the code section start;
//   a post-pass converts them to absolute module offsets.
//
// NOTE: FnClosure / caps array deferred until first real closure is needed.

use crate::ast::{AstId, Node as AstNode};
use crate::passes::cps::ir::{
  Arg, Bind, Callable, Cont, CpsId, CpsResult, Expr, ExprKind,
  Lit, Ref, Val, ValKind,
};
use crate::passes::name_res::ResolveResult;
use crate::passes::wasm::sourcemap::WasmMapping;
use crate::propgraph::PropGraph;

use wasm_encoder::{
  CodeSection, CompositeInnerType, CompositeType, ExportKind,
  ExportSection, FieldType, FuncType, Function, GlobalSection,
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
  collect_funcs(&cps.root, &mut ctx);
  let wasm = emit_module(&cps.root, &mut ctx);
  CodegenResult { wasm, mappings: ctx.mappings }
}

// ---------------------------------------------------------------------------
// Type indices — fixed layout, order matters
// ---------------------------------------------------------------------------

const TY_ANY: u32 = 0;      // (sub (struct))
const TY_INT: u32 = 1;      // (sub $Any (struct (field i64)))
const TY_CONT: u32 = 2;     // (func (param anyref)) — $__halt type: receives result value
const TY_VOID: u32 = 3;     // (func) — entry point type
const TY_FN1: u32 = 4;      // (func (param (ref $Cont))) — arity-1 compiled fn: receives cont

/// First type index available for per-arity function types (arity >= 2).
const TY_FUNC_START: u32 = 5;

// ---------------------------------------------------------------------------
// Function indices
// ---------------------------------------------------------------------------

const FN_HALT: u32 = 0;
const FN_COMPILED_START: u32 = 1;  // compiled fns start after $__halt

// ---------------------------------------------------------------------------
// Global indices
// ---------------------------------------------------------------------------

const GLOBAL_RESULT: u32 = 0;

// ---------------------------------------------------------------------------
// Collected function
// ---------------------------------------------------------------------------

struct CollectedFn<'a, 'src> {
  /// CpsId of the LetFn name bind (Bind::Synth, origin=Fn AST node).
  name_id: CpsId,
  /// CpsId of the LetVal continuation bind (Bind::Name, origin=Ident AST node), if any.
  /// name_res resolves references to this fn via this id (it's what goes into scope).
  letval_bind_id: Option<CpsId>,
  bind: Bind,
  fn_body: &'a Expr<'src>,
  /// CpsIds of the value params, in order.
  param_ids: Vec<CpsId>,
  /// CpsId of the cont param.
  cont_id: CpsId,
  /// Total arity (value params + 1 cont).
  arity: u32,
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

struct RelativeMapping {
  func_idx: u32,
  offset_in_body: u32,
  src_line: u32,
  src_col: u32,
}

struct Ctx<'a, 'src> {
  mappings: Vec<WasmMapping>,
  relative_mappings: Vec<RelativeMapping>,
  origin: &'a PropGraph<CpsId, Option<AstId>>,
  ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
  resolve: &'a ResolveResult,
  funcs: Vec<CollectedFn<'a, 'src>>,
  /// (arity, type_index) for arity >= 2.
  arity_types: Vec<(u32, u32)>,
  /// Maps CpsId → CpsId for LetVal rebindings: `LetVal { name: X, val: Ref(Synth(Y)) }`
  /// records X → Y. Allows `func_index` to follow alias chains.
  /// WORKAROUND: compensates for redundant Synth→Name indirection in CPS
  /// transform fold (see TODO in cps/transform.rs). Remove when fixed there.
  val_alias: std::collections::HashMap<CpsId, CpsId>,
  /// Maps cap param CpsId → fn_idx for module-level FnClosure constructions.
  /// Module-level cap args are always Synth refs to hoisted fns; this map lets
  /// resolve_val_to_func handle cap params without AST traversal.
  cap_param_fn: std::collections::HashMap<CpsId, u32>,
}

impl<'a, 'src> Ctx<'a, 'src> {
  fn new(
    origin: &'a PropGraph<CpsId, Option<AstId>>,
    ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
    resolve: &'a ResolveResult,
  ) -> Self {
    Self {
      mappings: Vec::new(),
      relative_mappings: Vec::new(),
      origin,
      ast_index,
      resolve,
      funcs: Vec::new(),
      arity_types: Vec::new(),
      val_alias: std::collections::HashMap::new(),
      cap_param_fn: std::collections::HashMap::new(),
    }
  }

  fn type_for_arity(&self, arity: u32) -> u32 {
    if arity == 1 { return TY_FN1; }
    self.arity_types.iter()
      .find(|(a, _)| *a == arity)
      .map(|(_, ty)| *ty)
      .unwrap_or(TY_FN1)
  }

  fn func_type(&self, func_idx: usize) -> u32 {
    self.type_for_arity(self.funcs[func_idx].arity)
  }

  fn func_index(&self, id: CpsId) -> Option<u32> {
    // Direct match on name_id or letval_bind_id.
    if let Some(pos) = self.funcs.iter().position(|f| f.name_id == id || f.letval_bind_id == Some(id)) {
      return Some(FN_COMPILED_START + pos as u32);
    }
    // Follow val_alias chain: LetVal rebindings like `add = <fn_ref>`.
    let mut cur = id;
    for _ in 0..8 {
      if let Some(&target) = self.val_alias.get(&cur) {
        if let Some(pos) = self.funcs.iter().position(|f| f.name_id == target || f.letval_bind_id == Some(target)) {
          return Some(FN_COMPILED_START + pos as u32);
        }
        cur = target;
      } else {
        break;
      }
    }
    None
  }

  fn fink_main_index(&self) -> u32 {
    FN_COMPILED_START + self.funcs.len() as u32
  }
}

// ---------------------------------------------------------------------------
// Module emission
// ---------------------------------------------------------------------------

fn emit_module<'a, 'src>(root: &'a Expr<'src>, ctx: &mut Ctx<'a, 'src>) -> Vec<u8> {
  let mut module = Module::new();
  compute_arity_types(ctx);
  emit_types(&mut module, ctx);
  emit_function_section(&mut module, ctx);
  emit_globals(&mut module);
  emit_exports(&mut module, ctx);
  emit_elem_section(&mut module, ctx);
  emit_code_section(root, &mut module, ctx);
  emit_name_section(&mut module, ctx);
  let wasm = module.finish();
  resolve_mappings(&wasm, ctx);
  wasm
}

fn compute_arity_types(ctx: &mut Ctx) {
  let mut arities: Vec<u32> = Vec::new();
  for f in &ctx.funcs {
    if f.arity >= 2 && !arities.contains(&f.arity) {
      arities.push(f.arity);
    }
  }
  arities.sort();
  ctx.arity_types = arities.iter().enumerate()
    .map(|(i, &arity)| (arity, TY_FUNC_START + i as u32))
    .collect();
}

// ---------------------------------------------------------------------------
// Source map resolution
// ---------------------------------------------------------------------------

fn resolve_mappings(wasm: &[u8], ctx: &mut Ctx) {
  use wasmparser::{Parser, Payload};
  let mut func_body_offsets: Vec<u32> = Vec::new();
  for payload in Parser::new(0).parse_all(wasm) {
    if let Ok(Payload::CodeSectionEntry(body)) = payload {
      func_body_offsets.push(body.range().start as u32);
    }
  }
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
// Name section
// ---------------------------------------------------------------------------

fn emit_name_section(module: &mut Module, ctx: &Ctx) {
  use wasm_encoder::{NameMap, NameSection};
  let mut names = NameSection::new();

  let mut func_names = NameMap::new();
  func_names.append(FN_HALT, "__halt");
  for (i, cf) in ctx.funcs.iter().enumerate() {
    func_names.append(FN_COMPILED_START + i as u32, &func_name(cf, ctx));
  }
  func_names.append(ctx.fink_main_index(), "fink_main");
  names.functions(&func_names);

  let mut type_names = NameMap::new();
  type_names.append(TY_ANY, "Any");
  type_names.append(TY_INT, "Int");
  type_names.append(TY_CONT, "Cont");
  type_names.append(TY_VOID, "void");
  type_names.append(TY_FN1, "Fn1");
  for &(arity, ty_idx) in &ctx.arity_types {
    type_names.append(ty_idx, &format!("Fn{}", arity));
  }
  names.types(&type_names);

  module.section(&names);
}

fn func_name(collected: &CollectedFn, ctx: &Ctx) -> String {
  use crate::ast::NodeKind;
  match collected.bind {
    Bind::Name => {
      if let Some(Some(ast_id)) = ctx.origin.try_get(collected.name_id)
        && let Some(Some(ast_node)) = ctx.ast_index.try_get(*ast_id)
        && let NodeKind::Ident(name) = &ast_node.kind
      {
        return name.to_string();
      }
      format!("name_{}", collected.name_id.0)
    }
    Bind::Synth => format!("v_{}", collected.name_id.0),
    Bind::Cont  => format!("k_{}", collected.name_id.0),
  }
}

// ---------------------------------------------------------------------------
// Type section
// ---------------------------------------------------------------------------

fn emit_types(module: &mut Module, ctx: &Ctx) {
  let mut types = TypeSection::new();
  let ct = |inner| CompositeType { inner, shared: false, descriptor: None, describes: None };

  // TY_ANY = 0
  types.ty().subtype(&SubType {
    is_final: false,
    supertype_idx: None,
    composite_type: ct(CompositeInnerType::Struct(wasm_encoder::StructType {
      fields: Box::new([]),
    })),
  });

  // TY_INT = 1: (sub $Any (struct (field i64)))
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

  // TY_CONT = 2: (func (param anyref))
  // The cont receives a result value (anyref). The cont itself is passed as (ref $Cont).
  types.ty().subtype(&SubType {
    is_final: true,
    supertype_idx: None,
    composite_type: ct(CompositeInnerType::Func(FuncType::new(
      [ValType::Ref(RefType::ANYREF)],
      [],
    ))),
  });

  // TY_VOID = 3: (func)
  types.ty().subtype(&SubType {
    is_final: true,
    supertype_idx: None,
    composite_type: ct(CompositeInnerType::Func(FuncType::new([], []))),
  });

  // TY_FN1 = 4: (func (param (ref $Cont))) — arity-1 compiled fn receives a cont
  let cont_ref = ValType::Ref(RefType { nullable: false, heap_type: wasm_encoder::HeapType::Concrete(TY_CONT) });
  types.ty().subtype(&SubType {
    is_final: true,
    supertype_idx: None,
    composite_type: ct(CompositeInnerType::Func(FuncType::new(
      [cont_ref],
      [],
    ))),
  });

  // Per-arity types (arity >= 2): (func (param anyref)*(N-1) (param (ref $Cont)))
  for &(arity, _) in &ctx.arity_types {
    let mut params: Vec<ValType> = (0..arity - 1).map(|_| ValType::Ref(RefType::ANYREF)).collect();
    params.push(cont_ref);
    types.ty().subtype(&SubType {
      is_final: true,
      supertype_idx: None,
      composite_type: ct(CompositeInnerType::Func(FuncType::new(params, []))),
    });
  }

  module.section(&types);
}

// ---------------------------------------------------------------------------
// Function section
// ---------------------------------------------------------------------------

fn emit_function_section(module: &mut Module, ctx: &Ctx) {
  let mut funcs = wasm_encoder::FunctionSection::new();
  funcs.function(TY_CONT);  // $__halt
  for (i, _) in ctx.funcs.iter().enumerate() {
    funcs.function(ctx.func_type(i));
  }
  funcs.function(TY_VOID);  // fink_main
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
// Element section
// ---------------------------------------------------------------------------

fn emit_elem_section(module: &mut Module, ctx: &Ctx) {
  use wasm_encoder::{Elements, ElementSection, ElementSegment};
  let mut elems = ElementSection::new();

  // $__halt is used via ref.func in fink_main.
  let mut refs = vec![FN_HALT];
  for (i, _) in ctx.funcs.iter().enumerate() {
    refs.push(FN_COMPILED_START + i as u32);
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

  code.function(&build_halt());

  let n_funcs = ctx.funcs.len();
  for i in 0..n_funcs {
    let body = ctx.funcs[i].fn_body;
    let arity = ctx.funcs[i].arity;
    code.function(&build_fink_fn(body, arity, FN_COMPILED_START + i as u32, ctx, &mut rel));
  }

  ctx.relative_mappings = rel;

  code.function(&build_fink_main(root, ctx));

  module.section(&code);
}

// ---------------------------------------------------------------------------
// $__halt
// ---------------------------------------------------------------------------

fn build_halt() -> Function {
  let mut f = Function::new([]);
  f.instruction(&Instruction::LocalGet(0));
  f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::I31));
  f.instruction(&Instruction::I31GetS);
  f.instruction(&Instruction::GlobalSet(GLOBAL_RESULT));
  f.instruction(&Instruction::End);
  f
}

// ---------------------------------------------------------------------------
// fink_main — entry point
// ---------------------------------------------------------------------------

/// Walk the root to find a module-level FnClosure construction.
/// Returns (hoisted_fn_idx, num_caps) if found.
///
/// Module-level FnClosure caps are always module-level LetFn refs, which codegen
/// resolves statically via func_index. The cap values themselves are never needed
/// at the call site — fink_main passes `ref.null any` placeholders (funcref cannot
/// be cast to anyref in WasmGC). The count is derived from the hoisted fn's arity.
fn find_main_closure_call(root: &Expr<'_>, ctx: &Ctx) -> Option<(u32, u32)> {
  use crate::passes::cps::ir::BuiltIn;

  find_in_expr(root, ctx, &|expr, ctx| {
    let ExprKind::App { func: Callable::BuiltIn(BuiltIn::FnClosure), args } = &expr.kind else {
      return None;
    };
    // args = [hoisted_fn_ref, cap1, ..., Cont::Ref(result_cont)]
    let fn_val = match args.first()? {
      Arg::Val(v) => v,
      _ => return None,
    };
    let fn_idx = match &fn_val.kind {
      ValKind::Ref(crate::passes::cps::ir::Ref::Synth(id)) => ctx.func_index(*id)?,
      _ => return None,
    };
    // num_caps = hoisted fn arity - 1 (subtract cont param). No need to resolve cap identities.
    let ValKind::Ref(crate::passes::cps::ir::Ref::Synth(synth_id)) = &fn_val.kind else { return None; };
    let fn_pos = ctx.funcs.iter().position(|f| f.name_id == *synth_id)?;
    let num_caps = ctx.funcs[fn_pos].arity - 1;
    Some((fn_idx, num_caps))
  })
}

/// Recursively search an expression for a FnClosure App matching `pred`.
fn find_in_expr<'a, 'src, T>(
  expr: &'a Expr<'src>,
  ctx: &Ctx,
  pred: &impl Fn(&'a Expr<'src>, &Ctx) -> Option<T>,
) -> Option<T> {
  if let Some(result) = pred(expr, ctx) {
    return Some(result);
  }
  match &expr.kind {
    ExprKind::LetFn { fn_body, body, .. } => {
      if let Some(r) = find_in_expr(fn_body, ctx, pred) { return Some(r); }
      if let Cont::Expr { body: cont_body, .. } = body {
        return find_in_expr(cont_body, ctx, pred);
      }
    }
    ExprKind::LetVal { body: Cont::Expr { body: cont_body, .. }, .. } => {
      return find_in_expr(cont_body, ctx, pred);
    }
    ExprKind::App { args, .. } => {
      for arg in args {
        match arg {
          Arg::Cont(Cont::Expr { body, .. }) | Arg::Expr(body) => {
            if let Some(r) = find_in_expr(body, ctx, pred) { return Some(r); }
          }
          _ => {}
        }
      }
    }
    ExprKind::If { then, else_, .. } => {
      if let Some(r) = find_in_expr(then, ctx, pred) { return Some(r); }
      return find_in_expr(else_, ctx, pred);
    }
    _ => {}
  }
  None
}

fn build_fink_main(root: &Expr<'_>, ctx: &Ctx) -> Function {
  // fink_main: no params, no results.
  // Two cases:
  // 1. main is a plain arity-1 fn (no captures): call it directly with $__halt.
  // 2. main is a closure (FnClosure): find the hoisted impl + cap fn indices,
  //    push cap funcrefs as anyref + $__halt, return_call the hoisted impl.
  let mut f = Function::new([]);
  let cont_ref_type = wasm_encoder::HeapType::Concrete(TY_CONT);

  if let Some((main_fn_idx, num_caps)) = find_main_closure_call(root, ctx) {
    // Closure case: module-level caps are always resolved statically by the callee via
    // func_index — pass ref.null any placeholders (funcref can't be cast to anyref in WasmGC).
    for _ in 0..num_caps {
      f.instruction(&Instruction::RefNull(wasm_encoder::HeapType::Abstract {
        shared: false,
        ty: wasm_encoder::AbstractHeapType::Any,
      }));
    }
    f.instruction(&Instruction::RefFunc(FN_HALT));
    f.instruction(&Instruction::RefCastNonNull(cont_ref_type));
    f.instruction(&Instruction::ReturnCall(main_fn_idx));
  } else {
    // Simple case: find first arity-1 fn and call it with $__halt.
    let entry_idx = find_arity1_entry(root, ctx).unwrap_or(FN_COMPILED_START);
    f.instruction(&Instruction::RefFunc(FN_HALT));
    f.instruction(&Instruction::RefCastNonNull(cont_ref_type));
    f.instruction(&Instruction::ReturnCall(entry_idx));
  }
  f.instruction(&Instruction::End);
  f
}

/// Walk the root LetFn chain to find the first arity-1 fn (simple module init fn).
fn find_arity1_entry(root: &Expr<'_>, ctx: &Ctx) -> Option<u32> {
  let mut expr = root;
  loop {
    if let ExprKind::LetFn { name, body, .. } = &expr.kind {
      if let Some(fn_idx) = ctx.func_index(name.id) {
        let fn_pos = ctx.funcs.iter().position(|f| f.name_id == name.id)?;
        if ctx.funcs[fn_pos].arity == 1 {
          return Some(fn_idx);
        }
      }
      match body {
        Cont::Expr { body: cont_body, .. } => expr = cont_body,
        Cont::Ref(_) => return None,
      }
    } else {
      return None;
    }
  }
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
  let cont_local = if arity > 0 { arity - 1 } else { 0 };

  let mut locals = Vec::new();
  if let Some(func_info) = ctx.funcs.iter().find(|cf| {
    std::ptr::eq(cf.fn_body as *const _, body as *const _)
  }) {
    for (i, &param_id) in func_info.param_ids.iter().enumerate() {
      locals.push((param_id, i as u32));
    }
    locals.push((func_info.cont_id, cont_local));
  }

  let mut extra_locals: Vec<CpsId> = Vec::new();
  collect_letval_locals(body, &mut extra_locals);
  let extra_count = extra_locals.len() as u32;

  let mut local_idx = arity;
  for cps_id in &extra_locals {
    locals.push((*cps_id, local_idx));
    local_idx += 1;
  }

  discover_aliases(body, &mut locals);

  let wasm_locals = if extra_count > 0 {
    vec![(extra_count, ValType::Ref(RefType::ANYREF))]
  } else {
    vec![]
  };

  let mut f = Function::new(wasm_locals);
  let mut fc = FnCtx { local_count: local_idx, cont_local, locals, code_idx, ctx, rel };
  emit_expr(body, &mut f, &mut fc);
  f.instruction(&Instruction::End);
  f
}

struct FnCtx<'a, 'b, 'src> {
  #[allow(dead_code)]
  local_count: u32,
  cont_local: u32,
  locals: Vec<(CpsId, u32)>,
  code_idx: u32,
  ctx: &'a Ctx<'b, 'src>,
  rel: &'a mut Vec<RelativeMapping>,
}

impl FnCtx<'_, '_, '_> {
  fn local_for(&self, id: CpsId) -> Option<u32> {
    self.locals.iter().find(|(cps_id, _)| *cps_id == id).map(|(_, idx)| *idx)
  }

  fn local_for_by_origin(&self, bind_id: CpsId) -> Option<u32> {
    let target_ast_id = self.ctx.origin.try_get(bind_id)?.as_ref()?;
    for &(local_cps_id, local_idx) in &self.locals {
      if let Some(Some(local_ast_id)) = self.ctx.origin.try_get(local_cps_id)
        && local_ast_id == target_ast_id
      {
        return Some(local_idx);
      }
    }
    None
  }

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
  fc.mark(f, expr.id);

  match &expr.kind {
    ExprKind::LetVal { val, body, .. } => {
      match body {
        Cont::Ref(cont_id) => {
          emit_cont_call_with_val(val, *cont_id, f, fc);
        }
        Cont::Expr { args, body: cont_body, .. } => {
          emit_val(val, f, fc);
          if let Some(bind) = args.first() {
            if let Some(local_idx) = fc.local_for(bind.id) {
              f.instruction(&Instruction::LocalSet(local_idx));
            } else {
              f.instruction(&Instruction::Drop);
            }
          } else {
            f.instruction(&Instruction::Drop);
          }
          emit_expr(cont_body, f, fc);
        }
      }
    }

    ExprKind::LetFn { name, body, .. } => {
      // The fn body is emitted as a separate WASM function.
      // For now, LetFn bindings are not first-class values (no FnClosure).
      // Just continue into the body cont.
      match body {
        Cont::Expr { body: cont_body, .. } => {
          // The cont arg would hold the fn value — skip it, emit body directly.
          // TODO: when fn values are needed, construct a closure here.
          let _ = fc.ctx.func_index(name.id);  // ensure fn is collected
          emit_expr(cont_body, f, fc);
        }
        Cont::Ref(cont_id) => {
          // The fn value is being passed to cont_id. Since all calls are static,
          // treat this as calling the fn directly with the forwarded cont.
          // This handles the module-init pattern: last LetFn passes itself to module cont.
          if let Some(fn_idx) = fc.ctx.func_index(name.id) {
            f.instruction(&Instruction::LocalGet(fc.cont_local));
            f.instruction(&Instruction::ReturnCall(fn_idx));
          } else {
            emit_cont_call_with_anyref(*cont_id, f, fc);
          }
        }
      }
    }

    ExprKind::App { func, args } => {
      emit_app(func, args, f, fc);
    }

    _ => {
      f.instruction(&Instruction::Unreachable);
    }
  }
}

// ---------------------------------------------------------------------------
// App emission
// ---------------------------------------------------------------------------

fn emit_app(func: &Callable<'_>, args: &[Arg<'_>], f: &mut Function, fc: &mut FnCtx) {
  use crate::passes::cps::ir::BuiltIn::*;

  match func {
    Callable::BuiltIn(op) => {
      let (val_args, cont) = split_app_args(args);

      match op {
        Add | Sub | Mul => {
          emit_arg_val(val_args[0], f, fc);
          f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::I31));
          f.instruction(&Instruction::I31GetS);
          emit_arg_val(val_args[1], f, fc);
          f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::I31));
          f.instruction(&Instruction::I31GetS);
          match op {
            Add => f.instruction(&Instruction::I32Add),
            Sub => f.instruction(&Instruction::I32Sub),
            Mul => f.instruction(&Instruction::I32Mul),
            _ => unreachable!(),
          };
          f.instruction(&Instruction::RefI31);
          emit_cont_call_with_anyref(cont, f, fc);
        }

        _ => {
          f.instruction(&Instruction::Unreachable);
        }
      }
    }

    Callable::Val(val) => {
      let (val_args, cont_id) = split_app_args(args);

      // Resolve the callee to a known compiled function index.
      let target_fn_idx = resolve_val_to_func(val, fc);

      if let Some(fn_idx) = target_fn_idx {
        // Direct call: push value args, push our cont, return_call.
        for arg in &val_args {
          emit_arg_val(arg, f, fc);
        }
        f.instruction(&Instruction::LocalGet(fc.cont_local));
        f.instruction(&Instruction::ReturnCall(fn_idx));
      } else {
        // Callee is a runtime value — not supported yet (no closures).
        let _ = cont_id;
        f.instruction(&Instruction::Unreachable);
      }
    }
  }
}

fn resolve_val_to_func(val: &Val<'_>, fc: &FnCtx) -> Option<u32> {
  match &val.kind {
    ValKind::Ref(Ref::Synth(id)) => fc.ctx.func_index(*id),
    ValKind::Ref(Ref::Name) => {
      use crate::passes::name_res::Resolution;
      let bind_id = match fc.ctx.resolve.resolution.try_get(val.id) {
        Some(Some(Resolution::Local(id))) => Some(*id),
        Some(Some(Resolution::Captured { bind, .. })) => Some(*bind),
        _ => None,
      };
      // func_index checks both name_id (Bind::Synth, origin=Fn) and letval_bind_id
      // (Bind::Name, origin=Ident) — handles direct refs to module-level fns.
      let by_id = bind_id.and_then(|id| fc.ctx.func_index(id));
      if by_id.is_some() { return by_id; }
      // cap_param_fn handles refs to cap params in hoisted fn bodies — the cap param
      // CpsId is mapped to the fn index of the captured function at FnClosure build time.
      bind_id.and_then(|id| fc.ctx.cap_param_fn.get(&id).copied())
    }
    _ => None,
  }
}

fn split_app_args<'a, 'src>(args: &'a [Arg<'src>]) -> (Vec<&'a Arg<'src>>, CpsId) {
  let mut val_args = Vec::new();
  let mut cont_id = None;
  for arg in args.iter().rev() {
    match arg {
      Arg::Cont(Cont::Ref(id)) if cont_id.is_none() => cont_id = Some(*id),
      Arg::Cont(Cont::Expr { .. }) if cont_id.is_none() => {
        panic!("unexpected inline cont in App after cont_lifting");
      }
      _ => val_args.push(arg),
    }
  }
  val_args.reverse();
  (val_args, cont_id.expect("App must have a result cont"))
}

fn emit_arg_val(arg: &Arg<'_>, f: &mut Function, fc: &mut FnCtx) {
  match arg {
    Arg::Val(val) => emit_val(val, f, fc),
    _ => { f.instruction(&Instruction::Unreachable); }
  }
}

/// Tail-call a cont with an anyref value already on the stack.
fn emit_cont_call_with_anyref(cont_id: CpsId, f: &mut Function, fc: &mut FnCtx) {
  if let Some(fn_idx) = fc.ctx.func_index(cont_id) {
    let target_idx = fc.ctx.funcs.iter().position(|cf| cf.name_id == cont_id);
    if let Some(idx) = target_idx {
      let target_arity = fc.ctx.funcs[idx].arity;
      if target_arity > 1 {
        f.instruction(&Instruction::LocalGet(fc.cont_local));
        f.instruction(&Instruction::ReturnCall(fn_idx));
        return;
      }
    }
    f.instruction(&Instruction::ReturnCall(fn_idx));
    return;
  }
  // Unknown cont — it's the cont param, already typed (ref $Cont).
  f.instruction(&Instruction::LocalGet(fc.cont_local));
  f.instruction(&Instruction::ReturnCallRef(TY_CONT));
}

/// Emit a value and tail-call a cont with it.
fn emit_cont_call_with_val(val: &Val<'_>, cont_id: CpsId, f: &mut Function, fc: &mut FnCtx) {
  if let Some(fn_idx) = fc.ctx.func_index(cont_id) {
    emit_val(val, f, fc);
    f.instruction(&Instruction::ReturnCall(fn_idx));
    return;
  }
  // Unknown cont — it's the cont param, already typed (ref $Cont).
  emit_val(val, f, fc);
  f.instruction(&Instruction::LocalGet(fc.cont_local));
  f.instruction(&Instruction::ReturnCallRef(TY_CONT));
}

// ---------------------------------------------------------------------------
// Value emission
// ---------------------------------------------------------------------------

fn emit_val(val: &Val<'_>, f: &mut Function, fc: &mut FnCtx) {
  fc.mark(f, val.id);

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
      let bind_id = match r {
        crate::passes::cps::ir::Ref::Synth(id) => *id,
        crate::passes::cps::ir::Ref::Name => {
          use crate::passes::name_res::Resolution;
          match fc.ctx.resolve.resolution.try_get(val.id) {
            Some(Some(Resolution::Local(bind_id))) => *bind_id,
            Some(Some(Resolution::Captured { bind, .. })) => *bind,
            _ => val.id,
          }
        }
      };
      if let Some(local_idx) = fc.local_for(bind_id) {
        f.instruction(&Instruction::LocalGet(local_idx));
      } else if let Some(local_idx) = fc.local_for_by_origin(bind_id) {
        f.instruction(&Instruction::LocalGet(local_idx));
      } else {
        f.instruction(&Instruction::Unreachable);
      }
    }
    _ => {
      f.instruction(&Instruction::I32Const(0));
      f.instruction(&Instruction::RefI31);
    }
  }
}

// ---------------------------------------------------------------------------
// Function collection
// ---------------------------------------------------------------------------

fn collect_funcs<'a, 'src>(expr: &'a Expr<'src>, ctx: &mut Ctx<'a, 'src>) {
  match &expr.kind {
    ExprKind::LetFn { name, params, cont, fn_body, body } => {
      let param_ids: Vec<CpsId> = params.iter().map(|p| match p {
        crate::passes::cps::ir::Param::Name(b) => b.id,
        crate::passes::cps::ir::Param::Spread(b) => b.id,
      }).collect();
      // Record the LetVal continuation bind (Bind::Name) if present.
      // name_res resolves user references to this fn via this bind_id, not name_id.
      let letval_bind_id = if let Cont::Expr { args, .. } = body {
        args.first().map(|b| b.id)
      } else {
        None
      };
      ctx.funcs.push(CollectedFn {
        name_id: name.id,
        letval_bind_id,
        bind: name.kind,
        fn_body,
        arity: params.len() as u32 + 1,
        param_ids,
        cont_id: cont.id,
      });
      collect_funcs(fn_body, ctx);
      if let Cont::Expr { body: cont_body, .. } = body {
        collect_funcs(cont_body, ctx);
      }
    }
    ExprKind::LetVal { name, val, body: Cont::Expr { body: cont_body, .. } } => {
      // Record val_alias for LetVal rebindings: name → synth target.
      // This lets func_index follow chains like `add = <fn_ref>`.
      if let ValKind::Ref(Ref::Synth(target)) = &val.kind {
        ctx.val_alias.insert(name.id, *target);
      }
      collect_funcs(cont_body, ctx);
    }
    ExprKind::App { func: Callable::BuiltIn(crate::passes::cps::ir::BuiltIn::FnClosure), args } => {
      // FnClosure(hoisted_fn_ref, cap1, cap2, ..., result_cont)
      // Map cap param CpsIds of the hoisted fn to their fn indices so that
      // resolve_val_to_func can handle cap param refs inside hoisted fn bodies
      // without any AST traversal.
      if let Some(Arg::Val(fn_val)) = args.first()
        && let ValKind::Ref(Ref::Synth(hoisted_fn_id)) = &fn_val.kind
        && let Some(fn_pos) = ctx.funcs.iter().position(|f| f.name_id == *hoisted_fn_id)
      {
        // cap args occupy args[1..n-1] (last arg is the result cont).
        let cap_args = &args[1..args.len().saturating_sub(1)];
        let fn_params = ctx.funcs[fn_pos].param_ids.clone();
        for (cap_arg, param_id) in cap_args.iter().zip(fn_params.iter()) {
          if let Arg::Val(cap_val) = cap_arg {
            // Cap args are Ref::Name; resolve them to their bind CpsId.
            use crate::passes::name_res::Resolution;
            let bind_id = match &cap_val.kind {
              ValKind::Ref(Ref::Synth(id)) => Some(*id),
              ValKind::Ref(Ref::Name) => match ctx.resolve.resolution.try_get(cap_val.id) {
                Some(Some(Resolution::Local(id))) => Some(*id),
                Some(Some(Resolution::Captured { bind, .. })) => Some(*bind),
                _ => None,
              },
              _ => None,
            };
            if let Some(bid) = bind_id
              && let Some(cap_fn_idx) = ctx.func_index(bid)
            {
              ctx.cap_param_fn.insert(*param_id, cap_fn_idx);
            }
          }
        }
      }
      // Recurse into the result cont body (contains the LetVal that binds the closure value).
      for arg in args {
        match arg {
          Arg::Cont(Cont::Expr { body, .. }) => collect_funcs(body, ctx),
          Arg::Expr(e) => collect_funcs(e, ctx),
          _ => {}
        }
      }
    }
    ExprKind::App { args, .. } => {
      for arg in args {
        match arg {
          Arg::Cont(Cont::Expr { body, .. }) => collect_funcs(body, ctx),
          Arg::Expr(e) => collect_funcs(e, ctx),
          _ => {}
        }
      }
    }
    ExprKind::If { then, else_, .. } => {
      collect_funcs(then, ctx);
      collect_funcs(else_, ctx);
    }
    ExprKind::Yield { cont: Cont::Expr { body, .. }, .. } => collect_funcs(body, ctx),
    _ => {}
  }
}

/// Pre-scan a function body for LetVal bindings that need WASM locals.
fn collect_letval_locals(expr: &Expr<'_>, out: &mut Vec<CpsId>) {
  match &expr.kind {
    ExprKind::LetVal { body: Cont::Expr { args, body }, .. } => {
      for arg in args { out.push(arg.id); }
      collect_letval_locals(body, out);
    }
    ExprKind::LetVal { body: Cont::Ref(_), .. } => {}
    ExprKind::LetFn { body, .. } => {
      if let Cont::Expr { args, body: cont_body } = body {
        for arg in args { out.push(arg.id); }
        collect_letval_locals(cont_body, out);
      }
    }
    ExprKind::App { args, .. } => {
      for arg in args {
        if let Arg::Cont(Cont::Expr { args: bind_args, body }) = arg {
          for b in bind_args { out.push(b.id); }
          collect_letval_locals(body, out);
        }
      }
    }
    ExprKind::If { .. } | ExprKind::Yield { .. } => {}
  }
}

fn discover_aliases(expr: &Expr<'_>, locals: &mut Vec<(CpsId, u32)>) {
  let mut current = expr;
  loop {
    match &current.kind {
      ExprKind::LetVal { name, val, body: Cont::Expr { args, body: cont_body } } => {
        if let ValKind::Ref(crate::passes::cps::ir::Ref::Synth(ref_id)) = &val.kind
          && let Some(local_idx) = locals.iter().find(|(id, _)| id == ref_id).map(|(_, idx)| *idx)
        {
          locals.push((name.id, local_idx));
        }
        if let Some(arg) = args.first()
          && let Some(local_idx) = locals.iter().find(|(id, _)| *id == arg.id).map(|(_, idx)| *idx)
        {
          locals.push((name.id, local_idx));
        }
        current = cont_body;
      }
      ExprKind::LetFn { body: Cont::Expr { args, body: cont_body }, .. } => {
        if let ExprKind::LetVal { name, .. } = &cont_body.kind
          && let Some(arg) = args.first()
          && let Some(local_idx) = locals.iter().find(|(id, _)| *id == arg.id).map(|(_, idx)| *idx)
        {
          locals.push((name.id, local_idx));
        }
        current = cont_body;
      }
      _ => break,
    }
  }
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
  use crate::passes::cps::transform::lower_expr;
  use super::codegen;

  fn compile_wasm(src: &str) -> Vec<u8> {
    let r = parse(src).expect("parse failed");
    let ast_index = build_index(&r);
    let cps = lower_expr(&r.root);
    let (lifted, resolved) = lift_all(cps, &ast_index);
    codegen(&lifted, &resolved, &ast_index).wasm
  }

  fn run(src: &str) -> String {
    exec_wasm(&compile_wasm(src)).to_string()
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
    let (lifted, resolved) = lift_all(cps, &ast_index);
    let result = codegen(&lifted, &resolved, &ast_index);
    assert!(!result.mappings.is_empty(), "should produce source mappings");
    let has_literal = result.mappings.iter().any(|m| m.src_line == 1 && m.src_col == 11);
    assert!(has_literal, "should map to literal 42; got: {:?}", result.mappings);
  }

  test_macros::include_fink_tests!("src/passes/wasm/test_codegen.fnk");
}
