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
  let mut ctx = Ctx::new(&cps.origin, ast_index, resolve, &cps.synth_alias);
  collect_funcs(&cps.root, &mut ctx);
  process_closures(&cps.root, &mut ctx);
  collect_match_arms(&cps.root, &mut ctx);
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
  /// Bind kinds of the value params — Cont-typed params get (ref $Cont) in WASM.
  param_kinds: Vec<Bind>,
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
  synth_alias: &'a PropGraph<CpsId, Option<CpsId>>,
  funcs: Vec<CollectedFn<'a, 'src>>,
  /// (signature, type_index) for compiled fn types.
  /// Signature: Vec<bool> where true = (ref $Cont), false = anyref, for value params.
  /// The trailing cont param is always (ref $Cont) and not included in the signature.
  fn_types: Vec<(Vec<bool>, u32)>,
  /// Maps CpsId → CpsId for LetVal rebindings: `LetVal { name: X, val: Ref(Synth(Y)) }`
  /// records X → Y. Allows `func_index` to follow alias chains.
  /// WORKAROUND: compensates for redundant Synth→Name indirection in CPS
  /// transform fold (see TODO in cps/transform.rs). Remove when fixed there.
  val_alias: std::collections::HashMap<CpsId, CpsId>,
  /// Compile-time match arm info: maps arm result bind CpsId → (matcher_fn_idx, body_fn_idx).
  /// Populated by collect_match_arms after collect_funcs.
  match_arms: std::collections::HashMap<CpsId, (u32, u32)>,
  /// Maps cap param CpsId → fn_idx for module-level FnClosure constructions.
  /// Module-level cap args are always Synth refs to hoisted fns; this map lets
  /// resolve_val_to_func handle cap params without AST traversal.
  cap_param_fn: std::collections::HashMap<CpsId, u32>,
  /// Maps FnClosure result cont param CpsId → hoisted fn index.
  /// After closure lifting, MatchArm Cont::Refs point to closure values (LetVal binds),
  /// which alias to result cont params. This map resolves those to hoisted fns.
  closure_fn: std::collections::HashMap<CpsId, u32>,
  /// Maps hoisted fn cap param CpsId → cap arg source CpsId.
  /// Built by process_closures to trace params back through FnClosure cap indirection.
  /// Used by emit_match_block to resolve arm val params to their match_arms entries.
  param_source: std::collections::HashMap<CpsId, CpsId>,
  /// Maps closure_fn param CpsId → non-stripped cap arg CpsIds for the FnClosure.
  /// When a cont call goes through closure_fn, these cap values must be pushed
  /// before the call result value and the cont.
  closure_caps: std::collections::HashMap<CpsId, Vec<CpsId>>,
}

impl<'a, 'src> Ctx<'a, 'src> {
  fn new(
    origin: &'a PropGraph<CpsId, Option<AstId>>,
    ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
    resolve: &'a ResolveResult,
    synth_alias: &'a PropGraph<CpsId, Option<CpsId>>,
  ) -> Self {
    Self {
      mappings: Vec::new(),
      relative_mappings: Vec::new(),
      origin,
      ast_index,
      resolve,
      synth_alias,
      funcs: Vec::new(),
      fn_types: Vec::new(),
      match_arms: std::collections::HashMap::new(),
      val_alias: std::collections::HashMap::new(),
      cap_param_fn: std::collections::HashMap::new(),
      closure_fn: std::collections::HashMap::new(),
      param_source: std::collections::HashMap::new(),
      closure_caps: std::collections::HashMap::new(),
    }
  }

  /// Get the WASM type index for a function signature.
  /// Signature: Vec<bool> where true = (ref $Cont) param, false = anyref param.
  fn type_for_sig(&self, sig: &[bool]) -> u32 {
    if sig.is_empty() { return TY_FN1; }
    self.fn_types.iter()
      .find(|(s, _)| s == sig)
      .map(|(_, ty)| *ty)
      .unwrap_or(TY_FN1)
  }

  fn func_type(&self, func_idx: usize) -> u32 {
    let sig = self.fn_sig(func_idx);
    self.type_for_sig(&sig)
  }

  /// Compute the signature for a collected function.
  /// Each value param is `true` if Bind::Cont (passed as (ref $Cont)), else `false` (anyref).
  fn fn_sig(&self, func_idx: usize) -> Vec<bool> {
    self.funcs[func_idx].param_kinds.iter().map(|k| *k == Bind::Cont).collect()
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

  /// Like func_index, but also resolves through closure_fn (FnClosure result cont param → hoisted fn).
  /// Use this only for compile-time resolution (e.g. collect_match_arms), not for runtime calls.
  fn func_index_through_closure(&self, id: CpsId) -> Option<u32> {
    if let Some(idx) = self.func_index(id) { return Some(idx); }
    // Direct closure_fn lookup (handles FnClosure Cont::Expr params).
    if let Some(&fn_idx) = self.closure_fn.get(&id) { return Some(fn_idx); }
    // Follow val_alias chain and check closure_fn.
    let mut cur = id;
    for _ in 0..8 {
      if let Some(&target) = self.val_alias.get(&cur) {
        if let Some(&fn_idx) = self.closure_fn.get(&target) {
          return Some(fn_idx);
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
  compute_fn_types(ctx);
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

fn compute_fn_types(ctx: &mut Ctx) {
  let mut sigs: Vec<Vec<bool>> = Vec::new();
  for i in 0..ctx.funcs.len() {
    let sig = ctx.fn_sig(i);
    if !sig.is_empty() && !sigs.contains(&sig) {
      sigs.push(sig);
    }
  }
  sigs.sort();
  ctx.fn_types = sigs.into_iter().enumerate()
    .map(|(i, sig)| (sig, TY_FUNC_START + i as u32))
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
  for (sig, ty_idx) in &ctx.fn_types {
    let arity = sig.len() + 1;
    let has_cont = sig.iter().any(|c| *c);
    let suffix = if has_cont { "c" } else { "" };
    type_names.append(*ty_idx, &format!("Fn{}{}", arity, suffix));
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

  // Per-signature types: each value param is anyref or (ref null $Cont) based on param_kinds.
  // The fn's own cont (last param) stays non-nullable.
  let cont_ref_nullable = ValType::Ref(RefType { nullable: true, heap_type: wasm_encoder::HeapType::Concrete(TY_CONT) });
  for (sig, _) in &ctx.fn_types {
    let mut params: Vec<ValType> = sig.iter().map(|is_cont| {
      if *is_cont { cont_ref_nullable } else { ValType::Ref(RefType::ANYREF) }
    }).collect();
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

/// Walk the root continuation chain to find a module-level FnClosure construction.
/// Returns (hoisted_fn_idx, num_caps) if found.
///
/// Only follows the root LetFn/LetVal continuation bodies — does NOT recurse into
/// fn_body or App args, since those contain inner closures, not the main entry.
/// Static cap params (module-level fn refs) are stripped by collect_funcs, so
/// num_caps reflects only remaining runtime captures (currently always 0).
fn find_main_closure_call(root: &Expr<'_>, ctx: &Ctx) -> Option<(u32, u32)> {
  use crate::passes::cps::ir::BuiltIn;
  let mut expr = root;
  loop {
    match &expr.kind {
      ExprKind::App { func: Callable::BuiltIn(BuiltIn::FnClosure), args } => {
        let fn_val = match args.first()? {
          Arg::Val(v) => v,
          _ => return None,
        };
        let ValKind::Ref(crate::passes::cps::ir::Ref::Synth(synth_id)) = &fn_val.kind else { return None; };
        let fn_idx = ctx.func_index(*synth_id)?;
        let fn_pos = ctx.funcs.iter().position(|f| f.name_id == *synth_id)?;
        let num_caps = ctx.funcs[fn_pos].arity - 1;
        return Some((fn_idx, num_caps));
      }
      ExprKind::LetFn { body: Cont::Expr { body: cont_body, .. }, .. } => {
        expr = cont_body;
      }
      ExprKind::LetVal { body: Cont::Expr { body: cont_body, .. }, .. } => {
        expr = cont_body;
      }
      _ => return None,
    }
  }
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
    // Closure case: static cap params (module-level fn refs) are stripped by
    // collect_funcs. Remaining caps (runtime values) get null placeholders.
    let fn_pos = (main_fn_idx - FN_COMPILED_START) as usize;
    for i in 0..num_caps as usize {
      let is_cont = fn_pos < ctx.funcs.len()
        && i < ctx.funcs[fn_pos].param_kinds.len()
        && ctx.funcs[fn_pos].param_kinds[i] == Bind::Cont;
      if is_cont {
        f.instruction(&Instruction::RefNull(wasm_encoder::HeapType::Concrete(TY_CONT)));
      } else {
        f.instruction(&Instruction::RefNull(wasm_encoder::HeapType::Abstract {
          shared: false,
          ty: wasm_encoder::AbstractHeapType::Any,
        }));
      }
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

  // +1 extra local for temp storage in emit_cont_call_with_anyref via closure_fn.
  let total_extra = extra_count + 1;
  let wasm_locals = vec![(total_extra, ValType::Ref(RefType::ANYREF))];

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

  /// Find a local by synth_alias: if any local's CpsId has a synth_alias
  /// that matches `bind_id`, return that local. Handles the case where a
  /// synth cap param (fresh CpsId) carries a value previously bound at
  /// `bind_id` (original CpsId from before lifting).
  fn local_for_synth_alias(&self, bind_id: CpsId) -> Option<u32> {
    for &(local_cps_id, local_idx) in &self.locals {
      if let Some(Some(alias_target)) = self.ctx.synth_alias.try_get(local_cps_id)
        && *alias_target == bind_id
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
    Callable::BuiltIn(op) => match op {
      Add | Sub | Mul => {
        let (val_args, cont) = split_app_args(args);
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

      MatchBlock => emit_match_block(args, f, fc),
      MatchArm => emit_match_arm(args, f, fc),
      MatchValue => emit_match_value(args, f, fc),

      FnClosure => emit_fn_closure(args, f, fc),

      _ => {
        f.instruction(&Instruction::Unreachable);
      }
    },

    Callable::Val(val) if matches!(&val.kind, ValKind::ContRef(_)) => {
      // Direct cont call: App func=Val(ContRef(id)) — tail-call the cont fn.
      if let ValKind::ContRef(cont_id) = &val.kind {
        if let Some(fn_idx) = fc.ctx.func_index(*cont_id) {
          f.instruction(&Instruction::LocalGet(fc.cont_local));
          f.instruction(&Instruction::ReturnCall(fn_idx));
        } else {
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

// ---------------------------------------------------------------------------
// Match emission
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Match emission — all inlined, no runtime fn values
// ---------------------------------------------------------------------------

/// MatchBlock(scrutinee, arm1_ref, arm2_ref, ..., result_cont)
///
/// The arm refs are vals pointing to MatchArm fn "values" — but these are
/// never used at runtime. Instead, we walk each arm's MatchArm App (found
/// via the collected fn chain) and inline the comparison + body calls.
fn emit_match_block(args: &[Arg<'_>], f: &mut Function, fc: &mut FnCtx) {
  let scrutinee_val = match args.first() {
    Some(Arg::Val(v)) => v,
    _ => { f.instruction(&Instruction::Unreachable); return; }
  };

  // Arm vals: args[1..n-1].
  let arm_vals: Vec<&Val<'_>> = args[1..args.len() - 1].iter().filter_map(|a| match a {
    Arg::Val(v) => Some(v),
    _ => None,
  }).collect();

  let mut if_depth: u32 = 0;
  for arm_val in &arm_vals {
    // Resolve arm val CpsId → look up in match_arms map.
    // After closure lifting, arm val params may be several indirections away from
    // the original MatchArm result. Follow param_source and val_alias chains.
    let arm_id = match &arm_val.kind {
      ValKind::Ref(Ref::Synth(id)) => Some(*id),
      _ => None,
    };
    let arm_info = arm_id.and_then(|id| {
      // Direct lookup.
      if let Some(info) = fc.ctx.match_arms.get(&id) { return Some(info); }
      // Follow param_source chain (cap param → source bind).
      let mut cur = id;
      for _ in 0..8 {
        if let Some(&source) = fc.ctx.param_source.get(&cur) {
          if let Some(info) = fc.ctx.match_arms.get(&source) { return Some(info); }
          // Also follow val_alias from source.
          if let Some(&alias) = fc.ctx.val_alias.get(&source)
            && let Some(info) = fc.ctx.match_arms.get(&alias)
          { return Some(info); }
          cur = source;
        } else {
          break;
        }
      }
      None
    });

    if let Some(&(matcher_fn, body_fn)) = arm_info {
      // Look at the matcher fn's body to determine literal vs wildcard.
      let fn_pos = (matcher_fn - FN_COMPILED_START) as usize;
      if fn_pos < fc.ctx.funcs.len() {
        let matcher_body = fc.ctx.funcs[fn_pos].fn_body;
        match &matcher_body.kind {
          // Literal: MatchValue(scrutinee_ref, expected, ContRef(success), Cont::Ref(fail))
          ExprKind::App { func: Callable::BuiltIn(crate::passes::cps::ir::BuiltIn::MatchValue), args: mv_args } => {
            let mv_vals: Vec<&Val<'_>> = mv_args.iter().filter_map(|a| match a {
              Arg::Val(v) => Some(v),
              _ => None,
            }).collect();
            if mv_vals.len() >= 2 {
              let expected = mv_vals[1];
              // Compare scrutinee == expected.
              emit_val(scrutinee_val, f, fc);
              f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::I31));
              f.instruction(&Instruction::I31GetS);
              emit_val(expected, f, fc);
              f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::I31));
              f.instruction(&Instruction::I31GetS);
              f.instruction(&Instruction::I32Eq);

              f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
              if_depth += 1;
              // Match → call body fn. Body fn may have Cont-typed params
              // (the result cont) plus its own cont param. Push cont for each.
              emit_match_body_call(body_fn, scrutinee_val, fc, f);
              f.instruction(&Instruction::Else);
              continue; // next arm fills the else block
            }
          }
          // Wildcard: always matches → call body directly.
          _ => {
            emit_match_body_call(body_fn, scrutinee_val, fc, f);
            continue;
          }
        }
      }
    }
    f.instruction(&Instruction::Unreachable);
  }

  // Close if/else blocks — last else is match exhaustion.
  if if_depth > 0 {
    f.instruction(&Instruction::Unreachable);
  }
  for _ in 0..if_depth {
    f.instruction(&Instruction::End);
  }
}

/// MatchArm(Cont::Ref(matcher), Cont::Ref(body), Cont::Ref(result))
///
/// Compile-time construct: wraps a matcher + body into an arm value.
/// At runtime, emits null (the arm value is never used — MatchBlock inlines).
/// Passes null to the result cont so the LetVal bind gets a placeholder.
fn emit_match_arm(args: &[Arg<'_>], f: &mut Function, fc: &mut FnCtx) {
  // The last Cont::Ref is the result cont.
  let result_cont = match args.iter().rev().find_map(|a| match a {
    Arg::Cont(Cont::Ref(id)) => Some(*id),
    _ => None,
  }) {
    Some(id) => id,
    None => { f.instruction(&Instruction::Unreachable); return; }
  };

  // Emit null as the "arm value" placeholder.
  f.instruction(&Instruction::RefNull(wasm_encoder::HeapType::Abstract {
    shared: false,
    ty: wasm_encoder::AbstractHeapType::Any,
  }));
  emit_cont_call_with_anyref(result_cont, f, fc);
}

/// MatchValue(scrutinee_ref, expected, ContRef(success), Cont::Ref(fail))
///
/// Compare scrutinee == expected. On match → call success. On fail → call fail.
fn emit_match_value(args: &[Arg<'_>], f: &mut Function, fc: &mut FnCtx) {
  let val_args: Vec<&Val<'_>> = args.iter().filter_map(|a| match a {
    Arg::Val(v) => Some(v),
    _ => None,
  }).collect();
  let fail_cont = match args.last() {
    Some(Arg::Cont(Cont::Ref(id))) => *id,
    _ => { f.instruction(&Instruction::Unreachable); return; }
  };
  if val_args.len() >= 3 {
    let scrutinee = val_args[0];
    let expected = val_args[1];
    let success_val = val_args[2];

    emit_val(scrutinee, f, fc);
    f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::I31));
    f.instruction(&Instruction::I31GetS);
    emit_val(expected, f, fc);
    f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::I31));
    f.instruction(&Instruction::I31GetS);
    f.instruction(&Instruction::I32Eq);

    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    if let ValKind::ContRef(success_id) = &success_val.kind {
      if let Some(fn_idx) = fc.ctx.func_index(*success_id) {
        f.instruction(&Instruction::LocalGet(fc.cont_local));
        f.instruction(&Instruction::ReturnCall(fn_idx));
      } else { f.instruction(&Instruction::Unreachable); }
    } else { f.instruction(&Instruction::Unreachable); }
    f.instruction(&Instruction::Else);
    if let Some(fn_idx) = fc.ctx.func_index(fail_cont) {
      f.instruction(&Instruction::ReturnCall(fn_idx));
    } else { f.instruction(&Instruction::Unreachable); }
    f.instruction(&Instruction::End);
  } else {
    f.instruction(&Instruction::Unreachable);
  }
}

/// FnClosure(hoisted_fn_ref, cap1, cap2, ..., result_cont)
///
/// Resolve the hoisted fn statically. Push cap values from locals (or null for
/// static caps that were stripped). Push null for remaining non-cap params
/// (e.g. match arm values that are never used at runtime). Push cont. Call.
fn emit_fn_closure(args: &[Arg<'_>], f: &mut Function, fc: &mut FnCtx) {
  // Check if the last arg is Cont::Expr (inline value-binding cont).
  // FnClosure Cont::Expr is a compile-time construct: the closure value is never
  // materialized at runtime. All calls are statically resolved. Just emit the body.
  if let Some(Arg::Cont(Cont::Expr { body, .. })) = args.last() {
    emit_expr(body, f, fc);
    return;
  }

  // Cont::Ref case: call the hoisted fn directly with cap args and forwarded cont.
  let hoisted_fn_id = match args.first() {
    Some(Arg::Val(v)) => match &v.kind {
      ValKind::Ref(Ref::Synth(id)) => Some(*id),
      _ => None,
    },
    _ => None,
  };

  let Some(fn_idx) = hoisted_fn_id.and_then(|id| fc.ctx.func_index(id)) else {
    f.instruction(&Instruction::Unreachable);
    return;
  };

  let fn_pos = (fn_idx - FN_COMPILED_START) as usize;
  if fn_pos >= fc.ctx.funcs.len() {
    f.instruction(&Instruction::Unreachable);
    return;
  }

  let arity = fc.ctx.funcs[fn_pos].arity;
  let cap_args = &args[1..args.len().saturating_sub(1)];

  // Push cap values. After static cap stripping, the remaining params start
  // with non-static caps that need runtime values.
  let num_value_params = arity as usize - 1; // subtract cont
  let param_kinds = &fc.ctx.funcs[fn_pos].param_kinds;
  for i in 0..num_value_params {
    if i < cap_args.len() {
      // Cap arg available — emit its value.
      if let Arg::Val(v) = &cap_args[i] {
        emit_val(v, f, fc);
      } else {
        emit_null_for_param(i, param_kinds, f);
      }
    } else {
      // Beyond cap args — push null placeholder typed to match param.
      emit_null_for_param(i, param_kinds, f);
    }
  }

  // Push cont.
  f.instruction(&Instruction::LocalGet(fc.cont_local));
  f.instruction(&Instruction::ReturnCall(fn_idx));
}

/// Emit a null value appropriate for the param at index `i`.
/// Cont params get `ref.null $Cont`; others get `ref.null any`.
fn emit_null_for_param(i: usize, param_kinds: &[Bind], f: &mut Function) {
  let is_cont = i < param_kinds.len() && param_kinds[i] == Bind::Cont;
  if is_cont {
    f.instruction(&Instruction::RefNull(wasm_encoder::HeapType::Concrete(TY_CONT)));
  } else {
    f.instruction(&Instruction::RefNull(wasm_encoder::HeapType::Abstract {
      shared: false, ty: wasm_encoder::AbstractHeapType::Any,
    }));
  }
}

/// Call a match arm body fn from MatchBlock. Pushes appropriate values for each param:
/// - Cap params (in param_source): push the cap value from the enclosing fn's locals
/// - Value params (not in param_source): push scrutinee value
/// - Cont params: push the forwarded cont
/// - The fn's own cont (last): push the forwarded cont
fn emit_match_body_call(fn_idx: u32, scrutinee_val: &Val<'_>, fc: &mut FnCtx, f: &mut Function) {
  let fn_pos = (fn_idx - FN_COMPILED_START) as usize;
  if fn_pos >= fc.ctx.funcs.len() {
    f.instruction(&Instruction::Unreachable);
    return;
  }

  let param_ids = fc.ctx.funcs[fn_pos].param_ids.clone();
  let param_kinds = fc.ctx.funcs[fn_pos].param_kinds.clone();
  for (i, kind) in param_kinds.iter().enumerate() {
    if *kind == Bind::Cont {
      f.instruction(&Instruction::LocalGet(fc.cont_local));
    } else if let Some(&param_id) = param_ids.get(i) {
      // Check if this param is a cap value (from param_source/closure_caps).
      // If so, resolve its source bind and push from a local.
      if let Some(&source_id) = fc.ctx.param_source.get(&param_id) {
        if let Some(local_idx) = fc.local_for(source_id)
          .or_else(|| fc.local_for_by_origin(source_id))
        {
          f.instruction(&Instruction::LocalGet(local_idx));
        } else {
          // Source not available as local — push null placeholder.
          f.instruction(&Instruction::RefNull(wasm_encoder::HeapType::Abstract {
            shared: false, ty: wasm_encoder::AbstractHeapType::Any,
          }));
        }
      } else {
        // Not a cap param — push scrutinee.
        emit_val(scrutinee_val, f, fc);
      }
    } else {
      emit_val(scrutinee_val, f, fc);
    }
  }
  // The fn's own cont param.
  f.instruction(&Instruction::LocalGet(fc.cont_local));
  f.instruction(&Instruction::ReturnCall(fn_idx));
}

fn resolve_val_to_func(val: &Val<'_>, fc: &FnCtx) -> Option<u32> {
  match &val.kind {
    ValKind::Ref(Ref::Synth(id)) => fc.ctx.func_index(*id),
    ValKind::Ref(Ref::Name) => {
      use crate::passes::name_res::Resolution;
      let bind_id = match fc.ctx.resolve.resolution.try_get(val.id) {
        Some(Some(Resolution::Local(id))) => Some(*id),
        Some(Some(Resolution::Captured { bind, .. })) => Some(*bind),
        Some(Some(Resolution::Recursive(id))) => Some(*id),
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
  // Check closure_fn: cont_id might be a FnClosure result binding that maps
  // to a hoisted fn. Push non-stripped cap values, then the result value, then cont.
  if let Some(&fn_idx) = fc.ctx.closure_fn.get(&cont_id) {
    let fn_pos = (fn_idx - FN_COMPILED_START) as usize;
    if fn_pos < fc.ctx.funcs.len() {
      let caps = fc.ctx.closure_caps.get(&cont_id).cloned().unwrap_or_default();
      if !caps.is_empty() {
        // Non-stripped cap values need to go BEFORE the result value on the stack.
        // Save the result value to a temp local, push caps, push result back.
        let result_local = fc.local_count;
        fc.local_count += 1;
        f.instruction(&Instruction::LocalSet(result_local));
        for &cap_id in &caps {
          if let Some(local_idx) = fc.local_for(cap_id)
            .or_else(|| fc.local_for_by_origin(cap_id))
            .or_else(|| fc.local_for_synth_alias(cap_id))
          {
            f.instruction(&Instruction::LocalGet(local_idx));
          } else {
            f.instruction(&Instruction::RefNull(wasm_encoder::HeapType::Abstract {
              shared: false, ty: wasm_encoder::AbstractHeapType::Any,
            }));
          }
        }
        f.instruction(&Instruction::LocalGet(result_local));
      }
      f.instruction(&Instruction::LocalGet(fc.cont_local));
      f.instruction(&Instruction::ReturnCall(fn_idx));
      return;
    }
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
      if let Some(local_idx) = fc.local_for(bind_id)
        .or_else(|| fc.local_for_by_origin(bind_id))
        .or_else(|| fc.local_for_synth_alias(bind_id))
      {
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
      let param_kinds: Vec<Bind> = params.iter().map(|p| match p {
        crate::passes::cps::ir::Param::Name(b) => b.kind,
        crate::passes::cps::ir::Param::Spread(b) => b.kind,
      }).collect();
      ctx.funcs.push(CollectedFn {
        name_id: name.id,
        letval_bind_id,
        bind: name.kind,
        fn_body,
        arity: params.len() as u32 + 1,
        param_ids,
        param_kinds,
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
      // FnClosure processing (cap stripping, closure_fn) deferred to process_closures
      // pass — hoisted fns may not be collected yet at this point in the tree walk.
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

/// Walk the CPS tree to process FnClosure apps after all funcs are collected.
/// - Builds cap_param_fn: maps cap param CpsIds to fn indices for static resolution.
/// - Strips static cap params (module-level fn refs) from hoisted fn signatures.
/// - Builds closure_fn: maps result cont params to hoisted fn indices.
fn process_closures<'a, 'src>(expr: &'a Expr<'src>, ctx: &mut Ctx<'a, 'src>) {
  // Two-phase approach: first collect all resolvable static fn caps (AST origin → fn_idx),
  // then process each FnClosure using the collected set. This handles the case where a cap
  // arg (e.g. `recurse`) is resolvable at one FnClosure site but Unresolved at another —
  // the fn_idx discovered at the resolvable site applies to all matching params.
  let mut known_static_caps: std::collections::HashMap<AstId, u32> = std::collections::HashMap::new();
  collect_static_fn_caps(expr, ctx, &mut known_static_caps);
  process_closures_inner(expr, ctx, &known_static_caps);
}

/// Pre-pass: walk FnClosure Apps, find cap args that resolve to known fn indices.
/// Records (AST origin of cap param → fn_idx) for use by process_closures_inner.
fn collect_static_fn_caps<'a, 'src>(
  expr: &'a Expr<'src>,
  ctx: &Ctx<'a, 'src>,
  out: &mut std::collections::HashMap<AstId, u32>,
) {
  match &expr.kind {
    ExprKind::App { func: Callable::BuiltIn(crate::passes::cps::ir::BuiltIn::FnClosure), args } => {
      if let Some(Arg::Val(fn_val)) = args.first()
        && let ValKind::Ref(Ref::Synth(hoisted_fn_id)) = &fn_val.kind
        && let Some(fn_pos) = ctx.funcs.iter().position(|f| f.name_id == *hoisted_fn_id)
      {
        let cap_args = &args[1..args.len().saturating_sub(1)];
        let fn_params = &ctx.funcs[fn_pos].param_ids;
        for (cap_arg, param_id) in cap_args.iter().zip(fn_params.iter()) {
          if let Arg::Val(cap_val) = cap_arg {
            let bind_id = resolve_cap_arg_bind(cap_val, ctx);
            if let Some(bid) = bind_id
              && let Some(cap_fn_idx) = ctx.func_index(bid)
              && let Some(Some(ast_id)) = ctx.origin.try_get(*param_id)
            {
              out.insert(*ast_id, cap_fn_idx);
            }
          }
        }
      }
      for arg in args {
        match arg {
          Arg::Cont(Cont::Expr { body, .. }) | Arg::Expr(body) => collect_static_fn_caps(body, ctx, out),
          _ => {}
        }
      }
    }
    ExprKind::LetFn { fn_body, body, .. } => {
      collect_static_fn_caps(fn_body, ctx, out);
      if let Cont::Expr { body: cont_body, .. } = body {
        collect_static_fn_caps(cont_body, ctx, out);
      }
    }
    ExprKind::LetVal { body: Cont::Expr { body: cont_body, .. }, .. } => {
      collect_static_fn_caps(cont_body, ctx, out);
    }
    ExprKind::App { args, .. } => {
      for arg in args {
        match arg {
          Arg::Cont(Cont::Expr { body, .. }) | Arg::Expr(body) => collect_static_fn_caps(body, ctx, out),
          _ => {}
        }
      }
    }
    ExprKind::If { then, else_, .. } => {
      collect_static_fn_caps(then, ctx, out);
      collect_static_fn_caps(else_, ctx, out);
    }
    _ => {}
  }
}

/// Resolve a cap arg val to its bind CpsId (for func_index lookup).
fn resolve_cap_arg_bind(cap_val: &Val<'_>, ctx: &Ctx<'_, '_>) -> Option<CpsId> {
  use crate::passes::name_res::Resolution;
  match &cap_val.kind {
    ValKind::Ref(Ref::Synth(id)) => Some(*id),
    ValKind::Ref(Ref::Name) => match ctx.resolve.resolution.try_get(cap_val.id) {
      Some(Some(Resolution::Local(id))) => Some(*id),
      Some(Some(Resolution::Captured { bind, .. })) => Some(*bind),
      Some(Some(Resolution::Recursive(id))) => Some(*id),
      _ => None,
    },
    _ => None,
  }
}

/// Main pass: process FnClosure Apps — strip static caps, build closure_fn/param_source.
fn process_closures_inner<'a, 'src>(
  expr: &'a Expr<'src>,
  ctx: &mut Ctx<'a, 'src>,
  known_static_caps: &std::collections::HashMap<AstId, u32>,
) {
  match &expr.kind {
    ExprKind::App { func: Callable::BuiltIn(crate::passes::cps::ir::BuiltIn::FnClosure), args } => {
      if let Some(Arg::Val(fn_val)) = args.first()
        && let ValKind::Ref(Ref::Synth(hoisted_fn_id)) = &fn_val.kind
        && let Some(fn_pos) = ctx.funcs.iter().position(|f| f.name_id == *hoisted_fn_id)
      {
        let cap_args = &args[1..args.len().saturating_sub(1)];
        let fn_params = ctx.funcs[fn_pos].param_ids.clone();

        // Process cap args: identify static caps, build param_source map.
        let mut static_cap_indices = Vec::new();
        for (i, (cap_arg, param_id)) in cap_args.iter().zip(fn_params.iter()).enumerate() {
          if let Arg::Val(cap_val) = cap_arg {
            let bind_id = resolve_cap_arg_bind(cap_val, ctx);
            // Record param_source: hoisted fn cap param → cap arg source bind.
            if let Some(bid) = bind_id {
              ctx.param_source.insert(*param_id, bid);
            }
            // Check direct resolution first.
            let cap_fn_idx = bind_id.and_then(|bid| ctx.func_index(bid));
            // Fallback: check known_static_caps by AST origin (handles Unresolved sites
            // where the same cap was resolvable at a different FnClosure site).
            let cap_fn_idx = cap_fn_idx.or_else(|| {
              let ast_id = ctx.origin.try_get(*param_id)?.as_ref()?;
              known_static_caps.get(ast_id).copied()
            });
            if let Some(fn_idx) = cap_fn_idx {
              ctx.cap_param_fn.insert(*param_id, fn_idx);
              static_cap_indices.push(i);
            }
          }
        }

        // Strip static cap params from the hoisted fn's signature.
        if !static_cap_indices.is_empty() {
          let cf = &mut ctx.funcs[fn_pos];
          for &i in static_cap_indices.iter().rev() {
            cf.param_ids.remove(i);
            cf.param_kinds.remove(i);
          }
          cf.arity = cf.param_ids.len() as u32 + 1;
        }

        // Record closure_fn: result cont param → hoisted fn index.
        let hoisted_fn_idx = FN_COMPILED_START + fn_pos as u32;
        // Handle both Cont::Ref and Cont::Expr for the result cont.
        let result_cont_param = args.last().and_then(|a| match a {
          Arg::Cont(Cont::Ref(id)) => {
            // Hoisted cont fn: first param binds the closure value.
            ctx.funcs.iter().position(|f| f.name_id == *id)
              .and_then(|pos| ctx.funcs[pos].param_ids.first().copied())
          }
          Arg::Cont(Cont::Expr { args: cont_args, .. }) => {
            // Inline cont: first arg binds the closure value.
            cont_args.first().map(|b| b.id)
          }
          _ => None,
        });
        if let Some(param_id) = result_cont_param {
          ctx.closure_fn.insert(param_id, hoisted_fn_idx);
          // Record non-stripped cap arg bind CpsIds for emit_cont_call to push at call time.
          // Use the resolved bind CpsId (not the val CpsId) so local_for can find the local.
          let non_stripped_caps: Vec<CpsId> = cap_args.iter().enumerate()
            .filter(|(i, _)| !static_cap_indices.contains(i))
            .filter_map(|(_, arg)| match arg {
              Arg::Val(v) => {
                resolve_cap_arg_bind(v, ctx).or(Some(v.id))
              }
              _ => None,
            })
            .collect();
          if !non_stripped_caps.is_empty() {
            ctx.closure_caps.insert(param_id, non_stripped_caps);
          }
        }
      }
      // Recurse into result cont body.
      for arg in args {
        match arg {
          Arg::Cont(Cont::Expr { body, .. }) | Arg::Expr(body) => process_closures_inner(body, ctx, known_static_caps),
          _ => {}
        }
      }
    }
    ExprKind::LetFn { fn_body, body, .. } => {
      process_closures_inner(fn_body, ctx, known_static_caps);
      if let Cont::Expr { body: cont_body, .. } = body {
        process_closures_inner(cont_body, ctx, known_static_caps);
      }
    }
    ExprKind::LetVal { body: Cont::Expr { body: cont_body, .. }, .. } => {
      process_closures_inner(cont_body, ctx, known_static_caps);
    }
    ExprKind::App { args, .. } => {
      for arg in args {
        match arg {
          Arg::Cont(Cont::Expr { body, .. }) | Arg::Expr(body) => process_closures_inner(body, ctx, known_static_caps),
          _ => {}
        }
      }
    }
    ExprKind::If { then, else_, .. } => {
      process_closures_inner(then, ctx, known_static_caps);
      process_closures_inner(else_, ctx, known_static_caps);
    }
    _ => {}
  }
}

/// Walk the CPS tree to find MatchArm Apps and record their matcher/body fn info.
/// Keys the map by the result cont's target fn's param — the CpsId that receives
/// the arm "value" and eventually flows to MatchBlock as an arm ref.
fn collect_match_arms<'a, 'src>(expr: &'a Expr<'src>, ctx: &mut Ctx<'a, 'src>) {
  use crate::passes::cps::ir::BuiltIn;
  match &expr.kind {
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::MatchArm), args } => {
      let cont_refs: Vec<CpsId> = args.iter().filter_map(|a| match a {
        Arg::Cont(Cont::Ref(id)) => Some(*id),
        _ => None,
      }).collect();
      if cont_refs.len() >= 3 {
        let matcher_id = cont_refs[0];
        let body_id = cont_refs[1];
        let result_cont_id = cont_refs[2];
        if let (Some(matcher_fn), Some(body_fn)) = (
          ctx.func_index_through_closure(matcher_id),
          ctx.func_index_through_closure(body_id),
        ) {
          // The result cont fn receives the arm value as its first (non-cap) param.
          // After closure lifting, cap params may be prepended. Find the arm value
          // param by: (1) try direct func_index to find the original cont fn,
          // (2) if that fails (closure-wrapped), use func_index_through_closure
          //     and skip cap params (those in param_source) to find the arm value.
          let fn_idx = ctx.func_index(result_cont_id)
            .or_else(|| ctx.func_index_through_closure(result_cont_id));
          if let Some(fn_idx) = fn_idx {
            let fn_pos = (fn_idx - FN_COMPILED_START) as usize;
            if fn_pos < ctx.funcs.len() {
              // Find first non-cap param: one that's not in param_source.
              let param_id = ctx.funcs[fn_pos].param_ids.iter()
                .find(|id| !ctx.param_source.contains_key(id))
                .or_else(|| ctx.funcs[fn_pos].param_ids.first());
              if let Some(&pid) = param_id {
                ctx.match_arms.insert(pid, (matcher_fn, body_fn));
              }
            }
          }
        }
      }
    }
    ExprKind::LetFn { fn_body, body, .. } => {
      collect_match_arms(fn_body, ctx);
      match body {
        Cont::Expr { body: cont_body, .. } => collect_match_arms(cont_body, ctx),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::LetVal { body: Cont::Expr { body: cont_body, .. }, .. } => {
      collect_match_arms(cont_body, ctx);
    }
    ExprKind::LetVal { .. } => {}
    ExprKind::App { args, .. } => {
      for arg in args {
        match arg {
          Arg::Cont(Cont::Expr { body, .. }) | Arg::Expr(body) => collect_match_arms(body, ctx),
          _ => {}
        }
      }
    }
    ExprKind::If { then, else_, .. } => {
      collect_match_arms(then, ctx);
      collect_match_arms(else_, ctx);
    }
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
