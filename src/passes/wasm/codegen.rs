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
  // DEBUG: before process_closures
  for (i, cf) in ctx.funcs.iter().enumerate() {
    eprintln!("  BEFORE fn[{}] arity={} name_id={:?} param_ids={:?} cont_id={:?}", i, cf.arity, cf.name_id, cf.param_ids, cf.cont_id);
  }
  process_closures(&cps.root, &mut ctx);
  resolve_user_names(&cps.root, &mut ctx);
  // DEBUG: after process_closures
  for (i, cf) in ctx.funcs.iter().enumerate() {
    eprintln!("  AFTER fn[{}] arity={} name_id={:?} param_ids={:?} cont_id={:?}", i, cf.arity, cf.name_id, cf.param_ids, cf.cont_id);
  }
  eprintln!("  closure_fn={:?}", ctx.closure_fn);
  // DEBUG: print fn body kinds + FnClosure first arg
  for (i, cf) in ctx.funcs.iter().enumerate() {
    let body_desc = match &cf.fn_body.kind {
      ExprKind::App { func: Callable::BuiltIn(b), args } => {
        let first_val = args.first().and_then(|a| if let Arg::Val(v) = a { Some(v) } else { None });
        let last_cont = args.last().and_then(|a| match a { Arg::Cont(Cont::Ref(id)) => Some(*id), _ => None });
        format!("BuiltIn({:?}) first_val={:?} last_cont={:?}", b,
          first_val.map(|v| match &v.kind { ValKind::Ref(Ref::Synth(id)) => format!("Synth({:?})", id), _ => "other".to_string() }),
          last_cont)
      },
      ExprKind::App { func: Callable::Val(_), .. } => "App(Val)".to_string(),
      ExprKind::LetVal { name, cont, .. } => format!("LetVal(name={:?}, cont={:?})", name.id, match cont { Cont::Ref(id) => format!("Ref({:?})", id), Cont::Expr { .. } => "Expr".to_string() }),
      ExprKind::LetFn { name, .. } => format!("LetFn(name={:?})", name.id),
      ExprKind::If { .. } => "If".to_string(),
    };
    eprintln!("  fn[{}] body: {}", i, body_desc);
  }
  collect_match_arms(&cps.root, &mut ctx);
  let wasm = emit_module(&cps.root, &mut ctx);
  CodegenResult { wasm, mappings: ctx.mappings }
}

// ---------------------------------------------------------------------------
// Type indices — fixed layout, order matters
// ---------------------------------------------------------------------------

const TY_ANY: u32 = 0;      // (sub (struct))
const TY_NUM: u32 = 1;      // (sub $Any (struct (field f64))) — universal number
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
const GLOBAL_CONT_ENV: u32 = 1;

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
  /// User-facing name (from LetVal alias in the LetFn cont), if any.
  /// Named fns are exported from the WASM module.
  user_name: Option<&'src str>,
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
  /// (arity, type_index) for cont_env fn types — no trailing cont param.
  /// Only for cont_env fns with arity >= 2 (arity 1 uses TY_CONT).
  cont_env_types: Vec<(u32, u32)>,
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
  /// Maps cont fn name_id → stripped Bind::Cont capture CpsIds.
  /// These captures are passed via $cont_env global instead of as params,
  /// keeping cont fns callable as $Cont type (single anyref param).
  cont_env_caps: std::collections::HashMap<CpsId, Vec<CpsId>>,
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
      cont_env_types: Vec::new(),
      match_arms: std::collections::HashMap::new(),
      val_alias: std::collections::HashMap::new(),
      cap_param_fn: std::collections::HashMap::new(),
      closure_fn: std::collections::HashMap::new(),
      param_source: std::collections::HashMap::new(),
      closure_caps: std::collections::HashMap::new(),
      cont_env_caps: std::collections::HashMap::new(),
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
    let cf = &self.funcs[func_idx];
    // Cont fns with $cont_env: arity == param_ids.len() (no own cont param).
    if cf.arity == cf.param_ids.len() as u32 {
      if cf.arity <= 1 {
        return TY_CONT;
      }
      // Look up the cont_env type by arity.
      return self.cont_env_types.iter()
        .find(|(a, _)| *a == cf.arity)
        .map(|(_, ty)| *ty)
        .unwrap_or(TY_CONT);
    }
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

  /// Check if a CpsId should be resolved via $cont_env global.
  /// Returns true if any cont_env_caps entry's source matches the given id
  /// (directly or via synth_alias).
  fn is_cont_env(&self, id: CpsId) -> bool {
    for sources in self.cont_env_caps.values() {
      for &source_id in sources {
        if source_id == id { return true; }
        // Check synth_alias: any fn's synth_alias might map to the cont_id.
        if let Some(Some(alias)) = self.synth_alias.try_get(id)
          && *alias == source_id
        { return true; }
        // Reverse: source's synth_alias might equal the query id.
        if let Some(Some(alias)) = self.synth_alias.try_get(source_id)
          && *alias == id
        { return true; }
      }
    }
    false
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
  let mut cont_env_arities: Vec<u32> = Vec::new();
  for i in 0..ctx.funcs.len() {
    let cf = &ctx.funcs[i];
    if cf.arity == cf.param_ids.len() as u32 {
      // Cont_env fn: no trailing cont param.
      if cf.arity >= 2 && !cont_env_arities.contains(&cf.arity) {
        cont_env_arities.push(cf.arity);
      }
    } else {
      let sig = ctx.fn_sig(i);
      if !sig.is_empty() && !sigs.contains(&sig) {
        sigs.push(sig);
      }
    }
  }
  sigs.sort();
  cont_env_arities.sort();
  ctx.fn_types = sigs.into_iter().enumerate()
    .map(|(i, sig)| (sig, TY_FUNC_START + i as u32))
    .collect();
  let ce_start = TY_FUNC_START + ctx.fn_types.len() as u32;
  ctx.cont_env_types = cont_env_arities.into_iter().enumerate()
    .map(|(i, arity)| (arity, ce_start + i as u32))
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
  type_names.append(TY_NUM, "Num");
  type_names.append(TY_CONT, "Cont");
  type_names.append(TY_VOID, "void");
  type_names.append(TY_FN1, "Fn1");
  for (sig, ty_idx) in &ctx.fn_types {
    let arity = sig.len() + 1;
    let has_cont = sig.iter().any(|c| *c);
    let suffix = if has_cont { "c" } else { "" };
    type_names.append(*ty_idx, &format!("Fn{}{}", arity, suffix));
  }
  for &(arity, ty_idx) in &ctx.cont_env_types {
    type_names.append(ty_idx, &format!("CE{}", arity));
  }
  names.types(&type_names);

  module.section(&names);
}

fn func_name(collected: &CollectedFn, ctx: &Ctx) -> String {
  use crate::ast::NodeKind;
  match collected.bind {
    Bind::SynthName => {
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

  // TY_NUM = 1: (sub $Any (struct (field f64))) — universal number type
  types.ty().subtype(&SubType {
    is_final: false,
    supertype_idx: Some(TY_ANY),
    composite_type: ct(CompositeInnerType::Struct(wasm_encoder::StructType {
      fields: Box::new([FieldType {
        element_type: StorageType::Val(ValType::F64),
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

  // Cont_env fn types: (func (param anyref * N)) — no trailing (ref $Cont).
  for &(arity, _) in &ctx.cont_env_types {
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
  // GLOBAL_RESULT: i32 — stores the final result value.
  globals.global(
    GlobalType { val_type: ValType::I32, mutable: true, shared: false },
    &wasm_encoder::ConstExpr::i32_const(0),
  );
  // GLOBAL_CONT_ENV: (ref null $Cont) — stores captured cont values for hoisted cont fns.
  // Cont fns must remain $Cont-compatible (single anyref param), so Bind::Cont captures
  // are stripped from their params and passed via this global instead.
  globals.global(
    GlobalType {
      val_type: ValType::Ref(RefType { nullable: true, heap_type: wasm_encoder::HeapType::Concrete(TY_CONT) }),
      mutable: true,
      shared: false,
    },
    &wasm_encoder::ConstExpr::ref_null(wasm_encoder::HeapType::Concrete(TY_CONT)),
  );
  module.section(&globals);
}

// ---------------------------------------------------------------------------
// Export section
// ---------------------------------------------------------------------------

fn emit_exports(module: &mut Module, ctx: &Ctx) {
  let mut exports = ExportSection::new();
  // Export each named fn by its user name (CPS functions).
  for (i, cf) in ctx.funcs.iter().enumerate() {
    if let Some(name) = cf.user_name {
      exports.export(name, ExportKind::Func, FN_COMPILED_START + i as u32);
    }
  }
  // Test convenience: fink_main calls the fn named "main" with $__halt.
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
  // Unwrap $Num(f64) → f64 → trunc to i32 for result global.
  f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::Concrete(TY_NUM)));
  f.instruction(&Instruction::StructGet { struct_type_index: TY_NUM, field_index: 0 });
  f.instruction(&Instruction::I32TruncF64S);
  f.instruction(&Instruction::GlobalSet(GLOBAL_RESULT));
  f.instruction(&Instruction::End);
  f
}

// ---------------------------------------------------------------------------
// fink_main — test convenience wrapper
// ---------------------------------------------------------------------------

/// Build fink_main: finds the fn named "main" and calls it with $__halt.
fn build_fink_main(_root: &Expr<'_>, ctx: &Ctx) -> Function {
  let mut f = Function::new([]);
  let cont_ref_type = wasm_encoder::HeapType::Concrete(TY_CONT);

  // Find the fn with user_name == "main".
  let main_fn_idx = ctx.funcs.iter().enumerate()
    .find(|(_, cf)| cf.user_name == Some("main"))
    .map(|(i, _)| FN_COMPILED_START + i as u32)
    .unwrap_or(FN_COMPILED_START);

  f.instruction(&Instruction::RefFunc(FN_HALT));
  f.instruction(&Instruction::RefCastNonNull(cont_ref_type));
  f.instruction(&Instruction::ReturnCall(main_fn_idx));
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
  // Detect cont fns: arity == param_ids.len() (no own cont param, uses $cont_env).
  let is_cont_env_fn = ctx.funcs.iter()
    .find(|cf| std::ptr::eq(cf.fn_body as *const _, body as *const _))
    .is_some_and(|cf| cf.arity == cf.param_ids.len() as u32);

  let cont_local = if is_cont_env_fn {
    u32::MAX // sentinel: cont fns read their cont from $cont_env, not a local
  } else if arity > 0 {
    arity - 1
  } else {
    0
  };

  let mut locals = Vec::new();
  // Cont-env fns with $Cont type have 1 WASM param (anyref result) even though
  // arity=0. Map cont_id (the result param) to local 0.
  let wasm_param_count = if is_cont_env_fn && arity <= 1 { 1 } else { arity };
  if let Some(func_info) = ctx.funcs.iter().find(|cf| {
    std::ptr::eq(cf.fn_body as *const _, body as *const _)
  }) {
    for (i, &param_id) in func_info.param_ids.iter().enumerate() {
      locals.push((param_id, i as u32));
    }
    if is_cont_env_fn && arity <= 1 {
      // Cont-env fn with $Cont type: cont_id is the result param at local 0.
      locals.push((func_info.cont_id, 0));
    } else if !is_cont_env_fn {
      locals.push((func_info.cont_id, cont_local));
    }
  }

  let mut extra_locals: Vec<CpsId> = Vec::new();
  collect_letval_locals(body, &mut extra_locals);
  let extra_count = extra_locals.len() as u32;

  let mut local_idx = wasm_param_count;
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
    ExprKind::LetVal { val, cont, .. } => {
      match cont {
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

    ExprKind::LetFn { name, cont, .. } => {
      // The fn body is emitted as a separate WASM function.
      // For now, LetFn bindings are not first-class values (no FnClosure).
      // Just continue into the body cont.
      match cont {
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
            emit_call_with_cont(fn_idx, f, fc);
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
      Add | Sub | Mul | Div | Pow => {
        let (val_args, cont) = split_app_args(args);
        emit_arg_val(val_args[0], f, fc);
        emit_unwrap_num(f);
        emit_arg_val(val_args[1], f, fc);
        emit_unwrap_num(f);
        match op {
          Add => f.instruction(&Instruction::F64Add),
          Sub => f.instruction(&Instruction::F64Sub),
          Mul => f.instruction(&Instruction::F64Mul),
          Div => f.instruction(&Instruction::F64Div),
          // f64 has no built-in pow — use the pattern: exp(ln(a) * b)
          // but WASM doesn't have exp/ln either. For now, unreachable for Pow.
          Pow => f.instruction(&Instruction::Unreachable),
          _ => unreachable!(),
        };
        emit_wrap_num(f);
        emit_cont_call_with_anyref(cont, f, fc);
      }

      IntDiv | Mod | IntMod => {
        let (val_args, cont) = split_app_args(args);
        emit_arg_val(val_args[0], f, fc);
        emit_unwrap_num(f);
        f.instruction(&Instruction::I64TruncF64S);
        emit_arg_val(val_args[1], f, fc);
        emit_unwrap_num(f);
        f.instruction(&Instruction::I64TruncF64S);
        match op {
          IntDiv => f.instruction(&Instruction::I64DivS),
          Mod | IntMod => f.instruction(&Instruction::I64RemS),
          _ => unreachable!(),
        };
        f.instruction(&Instruction::F64ConvertI64S);
        emit_wrap_num(f);
        emit_cont_call_with_anyref(cont, f, fc);
      }

      Eq | Neq | Lt | Lte | Gt | Gte => {
        let (val_args, cont) = split_app_args(args);
        emit_arg_val(val_args[0], f, fc);
        emit_unwrap_num(f);
        emit_arg_val(val_args[1], f, fc);
        emit_unwrap_num(f);
        match op {
          Eq  => f.instruction(&Instruction::F64Eq),
          Neq => f.instruction(&Instruction::F64Ne),
          Lt  => f.instruction(&Instruction::F64Lt),
          Lte => f.instruction(&Instruction::F64Le),
          Gt  => f.instruction(&Instruction::F64Gt),
          Gte => f.instruction(&Instruction::F64Ge),
          _ => unreachable!(),
        };
        // Boolean result: 0.0 or 1.0
        f.instruction(&Instruction::F64ConvertI32S);
        emit_wrap_num(f);
        emit_cont_call_with_anyref(cont, f, fc);
      }

      And | Or | Xor => {
        let (val_args, cont) = split_app_args(args);
        emit_arg_val(val_args[0], f, fc);
        emit_unwrap_num(f);
        f.instruction(&Instruction::I64TruncF64S);
        emit_arg_val(val_args[1], f, fc);
        emit_unwrap_num(f);
        f.instruction(&Instruction::I64TruncF64S);
        match op {
          And => f.instruction(&Instruction::I64And),
          Or  => f.instruction(&Instruction::I64Or),
          Xor => f.instruction(&Instruction::I64Xor),
          _ => unreachable!(),
        };
        f.instruction(&Instruction::F64ConvertI64S);
        emit_wrap_num(f);
        emit_cont_call_with_anyref(cont, f, fc);
      }

      Not => {
        let (val_args, cont) = split_app_args(args);
        emit_arg_val(val_args[0], f, fc);
        emit_unwrap_num(f);
        // not x = if x == 0.0 then 1.0 else 0.0
        f.instruction(&Instruction::F64Const(0.0_f64.into()));
        f.instruction(&Instruction::F64Eq);
        f.instruction(&Instruction::F64ConvertI32S);
        emit_wrap_num(f);
        emit_cont_call_with_anyref(cont, f, fc);
      }

      BitAnd | BitXor | Shl | Shr | RotL | RotR => {
        let (val_args, cont) = split_app_args(args);
        emit_arg_val(val_args[0], f, fc);
        emit_unwrap_num(f);
        f.instruction(&Instruction::I64TruncF64S);
        emit_arg_val(val_args[1], f, fc);
        emit_unwrap_num(f);
        f.instruction(&Instruction::I64TruncF64S);
        match op {
          BitAnd => f.instruction(&Instruction::I64And),
          BitXor => f.instruction(&Instruction::I64Xor),
          Shl    => f.instruction(&Instruction::I64Shl),
          Shr    => f.instruction(&Instruction::I64ShrS),
          RotL   => f.instruction(&Instruction::I64Rotl),
          RotR   => f.instruction(&Instruction::I64Rotr),
          _ => unreachable!(),
        };
        f.instruction(&Instruction::F64ConvertI64S);
        emit_wrap_num(f);
        emit_cont_call_with_anyref(cont, f, fc);
      }

      MatchBlock => emit_match_block(args, f, fc),
      MatchArm => emit_match_arm(args, f, fc),
      MatchValue => emit_match_value(args, f, fc),

      FnClosure => emit_fn_closure(args, f, fc),

      // Module export — no runtime code. The export list is structural
      // (handled by emit_exports). The module-level wrapper function
      // terminates here; codegen emits nothing.
      Export | Import => {}

      _ => {
        f.instruction(&Instruction::Unreachable);
      }
    },

    Callable::Val(val) if matches!(&val.kind, ValKind::ContRef(_)) => {
      // Direct cont call: App func=Val(ContRef(id)) — tail-call the cont fn.
      // Args are value args only (no trailing cont — the cont IS the callee).
      if let ValKind::ContRef(cont_id) = &val.kind {
        for arg in args {
          emit_arg_val(arg, f, fc);
        }
        emit_cont_call_with_anyref(*cont_id, f, fc);
      }
    }

    Callable::Val(val) => {
      let (val_args, cont_id) = split_app_args(args);

      // Resolve the callee to a known compiled function index.
      let target_fn_idx = resolve_val_to_func(val, fc);

      if let Some(fn_idx) = target_fn_idx {
        // Direct call: push value args, push the cont, return_call.
        for arg in &val_args {
          emit_arg_val(arg, f, fc);
        }
        emit_cont_ref(cont_id, f, fc);
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
  eprintln!("  emit_match_block: {} args", args.len());
  for (i, arg) in args.iter().enumerate() {
    eprintln!("    arg[{}]: {:?}", i, match arg {
      Arg::Val(v) => format!("Val({:?})", v.kind),
      Arg::Cont(Cont::Ref(id)) => format!("Cont::Ref({:?})", id),
      Arg::Cont(Cont::Expr { .. }) => "Cont::Expr".to_string(),
      _ => "other".to_string(),
    });
  }
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
    eprintln!("  emit_match_block: arm_id={:?} (param_src={:?}, val_alias={:?})",
      arm_id,
      arm_id.and_then(|id| fc.ctx.param_source.get(&id).copied()),
      arm_id.and_then(|id| fc.ctx.val_alias.get(&id).copied()),
    );
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
              // Compare scrutinee == expected (f64).
              emit_val(scrutinee_val, f, fc);
              emit_unwrap_num(f);
              emit_val(expected, f, fc);
              emit_unwrap_num(f);
              f.instruction(&Instruction::F64Eq);

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
    emit_unwrap_num(f);
    emit_val(expected, f, fc);
    emit_unwrap_num(f);
    f.instruction(&Instruction::F64Eq);

    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    if let ValKind::ContRef(success_id) = &success_val.kind {
      if let Some(fn_idx) = fc.ctx.func_index(*success_id) {
        emit_call_with_cont(fn_idx, f, fc);
      } else { f.instruction(&Instruction::Unreachable); }
    } else { f.instruction(&Instruction::Unreachable); }
    f.instruction(&Instruction::Else);
    if let Some(fn_idx) = fc.ctx.func_index(fail_cont) {
      emit_call_with_cont(fn_idx, f, fc);
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

  let cap_args = &args[1..args.len().saturating_sub(1)];

  // Push cap values. After static cap stripping, the remaining params start
  // with non-static caps that need runtime values.
  // For cont_env fns (arity == param_ids.len()), num_value_params == param_ids.len().
  // For regular fns (arity == param_ids.len() + 1), num_value_params == arity - 1.
  let num_value_params = fc.ctx.funcs[fn_pos].param_ids.len();
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

  // Push cont (or set $cont_env for cont_env fns).
  if is_cont_env_fn(fn_idx, fc.ctx) {
    emit_own_cont(f, fc);
    f.instruction(&Instruction::GlobalSet(GLOBAL_CONT_ENV));
  } else {
    emit_own_cont(f, fc);
  }
  f.instruction(&Instruction::ReturnCall(fn_idx));
}

/// Call a hoisted fn, passing the current continuation.
///
/// For regular fns (arity > 0): push cont as the last arg, return_call.
/// For cont_env fns (arity == param_ids.len()): set $cont_env to the current
/// cont, push ref.null any as the result placeholder, return_call.
fn emit_call_with_cont(fn_idx: u32, f: &mut Function, fc: &FnCtx) {
  if is_cont_env_fn(fn_idx, fc.ctx) {
    emit_own_cont(f, fc);
    f.instruction(&Instruction::GlobalSet(GLOBAL_CONT_ENV));
    f.instruction(&Instruction::RefNull(wasm_encoder::HeapType::Abstract {
      shared: false, ty: wasm_encoder::AbstractHeapType::Any,
    }));
  } else {
    emit_own_cont(f, fc);
  }
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
      emit_own_cont(f, fc);
    } else if let Some(&param_id) = param_ids.get(i) {
      // Check if this param is a cap value (from param_source/closure_caps).
      // If so, resolve its source bind and push from a local.
      if let Some(&source_id) = fc.ctx.param_source.get(&param_id) {
        if let Some(local_idx) = fc.local_for(source_id)
          .or_else(|| fc.local_for_by_origin(source_id))
          .or_else(|| fc.local_for_synth_alias(source_id))
        {
          f.instruction(&Instruction::LocalGet(local_idx));
        } else {
          // Source not found as local — trace through param_source chain.
          // The value might be accessible via an intermediate param that
          // aliases the source through the capture chain.
          let mut found = false;
          let mut cur = source_id;
          for _ in 0..8 {
            if let Some(&next) = fc.ctx.param_source.get(&cur) {
              if let Some(local_idx) = fc.local_for(next)
                .or_else(|| fc.local_for_by_origin(next))
                .or_else(|| fc.local_for_synth_alias(next))
              {
                f.instruction(&Instruction::LocalGet(local_idx));
                found = true;
                break;
              }
              cur = next;
            } else {
              break;
            }
          }
          if !found {
            // Last resort: check closure_caps values for matching origin.
            let source_origin = fc.ctx.origin.try_get(source_id).and_then(|o| *o);
            let mut origin_found = false;
            if let Some(origin) = source_origin {
              for &(local_id, local_idx) in &fc.locals {
                if let Some(Some(local_origin)) = fc.ctx.origin.try_get(local_id)
                  && *local_origin == origin
                {
                  f.instruction(&Instruction::LocalGet(local_idx));
                  origin_found = true;
                  break;
                }
              }
            }
            if !origin_found {
              f.instruction(&Instruction::RefNull(wasm_encoder::HeapType::Abstract {
                shared: false, ty: wasm_encoder::AbstractHeapType::Any,
              }));
            }
          }
        }
      } else {
        // Not a cap param — push scrutinee.
        emit_val(scrutinee_val, f, fc);
      }
    } else {
      emit_val(scrutinee_val, f, fc);
    }
  }
  // The fn's own cont param (or set $cont_env for cont_env fns).
  if is_cont_env_fn(fn_idx, fc.ctx) {
    emit_own_cont(f, fc);
    f.instruction(&Instruction::GlobalSet(GLOBAL_CONT_ENV));
  } else {
    emit_own_cont(f, fc);
  }
  f.instruction(&Instruction::ReturnCall(fn_idx));
}

fn resolve_val_to_func(val: &Val<'_>, fc: &FnCtx) -> Option<u32> {
  let ValKind::Ref(Ref::Synth(id)) = &val.kind else { return None; };
  // Fast path: direct CpsId → func index.
  if let Some(idx) = fc.ctx.func_index(*id) { return Some(idx); }
  // Slow path: follow resolution to get the bind CpsId, then look up.
  use crate::passes::name_res::Resolution;
  let bind_id = match fc.ctx.resolve.resolution.try_get(val.id) {
    Some(Some(Resolution::Local(id))) => Some(*id),
    Some(Some(Resolution::Captured { bind, .. })) => Some(*bind),
    Some(Some(Resolution::Recursive(id))) => Some(*id),
    _ => None,
  };
  let by_id = bind_id.and_then(|id| fc.ctx.func_index(id));
  if by_id.is_some() { return by_id; }
  // cap_param_fn handles refs to cap params in hoisted fn bodies.
  bind_id.and_then(|id| fc.ctx.cap_param_fn.get(&id).copied())
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

/// Push the current function's continuation reference onto the stack.
/// For normal fns: local.get cont_local.
/// For cont_env fns (cont_local == u32::MAX): global.get $cont_env.
/// Check if a function index corresponds to a cont_env fn (reads cont from global).
fn is_cont_env_fn(fn_idx: u32, ctx: &Ctx) -> bool {
  let fn_pos = (fn_idx - FN_COMPILED_START) as usize;
  fn_pos < ctx.funcs.len() && {
    let cf = &ctx.funcs[fn_pos];
    cf.arity == cf.param_ids.len() as u32
  }
}

/// If the target fn is a cont_env fn, set $cont_env global to our cont before calling.
fn maybe_set_cont_env(fn_idx: u32, f: &mut Function, fc: &FnCtx) {
  if is_cont_env_fn(fn_idx, fc.ctx) {
    emit_own_cont(f, fc);
    f.instruction(&Instruction::GlobalSet(GLOBAL_CONT_ENV));
  }
}

/// Push a cont reference onto the stack for a specific cont_id.
/// If cont_id is a known fn (direct or via closure_fn), push ref.func.
/// If cont_id is the fn's own cont, push local.get or global.get.
/// If cont_id is in cont_env, push global.get $cont_env.
fn emit_cont_ref(cont_id: CpsId, f: &mut Function, fc: &FnCtx) {
  // Check if cont_id is a known compiled fn — push ref.func + cast.
  if let Some(fn_idx) = fc.ctx.func_index(cont_id) {
    let fn_pos = (fn_idx - FN_COMPILED_START) as usize;
    if fn_pos < fc.ctx.funcs.len() {
      let is_cef = is_cont_env_fn(fn_idx, fc.ctx);
      if is_cef {
        // Cont_env fn: set $cont_env so it can read our cont, then push ref.func.
        emit_own_cont(f, fc);
        f.instruction(&Instruction::GlobalSet(GLOBAL_CONT_ENV));
      }
      f.instruction(&Instruction::RefFunc(fn_idx));
      f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::Concrete(TY_CONT)));
      return;
    }
  }
  // Check closure_fn.
  if let Some(&fn_idx) = fc.ctx.closure_fn.get(&cont_id) {
    let is_cef = is_cont_env_fn(fn_idx, fc.ctx);
    if is_cef {
      emit_own_cont(f, fc);
      f.instruction(&Instruction::GlobalSet(GLOBAL_CONT_ENV));
    }
    f.instruction(&Instruction::RefFunc(fn_idx));
    f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::Concrete(TY_CONT)));
    return;
  }
  // Check if it's in cont_env.
  if fc.ctx.is_cont_env(cont_id) {
    f.instruction(&Instruction::GlobalGet(GLOBAL_CONT_ENV));
    f.instruction(&Instruction::RefAsNonNull);
    return;
  }
  // Check if we can find it as a local (captured cont param).
  if let Some(local_idx) = fc.local_for(cont_id)
    .or_else(|| fc.local_for_synth_alias(cont_id))
  {
    f.instruction(&Instruction::LocalGet(local_idx));
    return;
  }
  // Fallback: push our own cont.
  emit_own_cont(f, fc);
}

fn emit_own_cont(f: &mut Function, fc: &FnCtx) {
  if fc.cont_local == u32::MAX {
    f.instruction(&Instruction::GlobalGet(GLOBAL_CONT_ENV));
    // Global is (ref null $Cont) — refine to (ref $Cont) for call_ref.
    f.instruction(&Instruction::RefAsNonNull);
  } else {
    f.instruction(&Instruction::LocalGet(fc.cont_local));
  }
}

/// Tail-call a cont with an anyref value already on the stack.
fn emit_cont_call_with_anyref(cont_id: CpsId, f: &mut Function, fc: &mut FnCtx) {
  if let Some(fn_idx) = fc.ctx.func_index(cont_id) {
    let target_idx = fc.ctx.funcs.iter().position(|cf| cf.name_id == cont_id);
    if let Some(idx) = target_idx {
      let cf = &fc.ctx.funcs[idx];
      let target_arity = cf.arity;
      let is_cef = cf.arity == cf.param_ids.len() as u32;
      if is_cef {
        // Cont_env fn: set $cont_env before calling.
        // Result value is already on stack — save to temp, set cont_env, restore.
        let result_local = fc.local_count;
        f.instruction(&Instruction::LocalSet(result_local));
        emit_own_cont(f, fc);
        f.instruction(&Instruction::GlobalSet(GLOBAL_CONT_ENV));
        f.instruction(&Instruction::LocalGet(result_local));
        f.instruction(&Instruction::ReturnCall(fn_idx));
        return;
      }
      if target_arity > 1 {
        emit_own_cont(f, fc);
        f.instruction(&Instruction::ReturnCall(fn_idx));
        return;
      }
    }
    maybe_set_cont_env(fn_idx, f, fc);
    f.instruction(&Instruction::ReturnCall(fn_idx));
    return;
  }
  // Check closure_fn: cont_id might be a FnClosure result binding that maps
  // to a hoisted fn.
  if let Some(&fn_idx) = fc.ctx.closure_fn.get(&cont_id) {
    // If the target is a match_block fn, inline it instead of calling.
    // This keeps cap values from the enclosing scope available for
    // emit_match_body_call (match body fn caps need the enclosing scope).
    let fn_pos_check = (fn_idx - FN_COMPILED_START) as usize;
    if fn_pos_check < fc.ctx.funcs.len()
      && let ExprKind::App { func: Callable::BuiltIn(crate::passes::cps::ir::BuiltIn::MatchBlock), args } = &fc.ctx.funcs[fn_pos_check].fn_body.kind
    {
      // Drop the result value (arm value placeholder — not used, match_block reads arm refs by CpsId).
      f.instruction(&Instruction::Drop);
      // Inline the match_block emission.
      emit_match_block(args, f, fc);
      return;
    }
    let fn_pos = (fn_idx - FN_COMPILED_START) as usize;
    if fn_pos < fc.ctx.funcs.len() {
      let is_cef = is_cont_env_fn(fn_idx, fc.ctx);
      let caps = fc.ctx.closure_caps.get(&cont_id).cloned().unwrap_or_default();
      if !caps.is_empty() || is_cef {
        // Save result value to temp, push caps, push result back.
        let result_local = fc.local_count;
        fc.local_count += 1;
        f.instruction(&Instruction::LocalSet(result_local));
        if is_cef {
          // Set $cont_env before calling — cont_env fn reads cont from global.
          emit_own_cont(f, fc);
          f.instruction(&Instruction::GlobalSet(GLOBAL_CONT_ENV));
        }
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
      if !is_cef {
        emit_own_cont(f, fc);
      }
      f.instruction(&Instruction::ReturnCall(fn_idx));
      return;
    }
  }
  // Unknown cont — check if it's in cont_env.
  if fc.ctx.is_cont_env(cont_id) {
    f.instruction(&Instruction::GlobalGet(GLOBAL_CONT_ENV));
    f.instruction(&Instruction::RefAsNonNull);
    f.instruction(&Instruction::ReturnCallRef(TY_CONT));
    return;
  }
  // Unknown cont — it's the cont param, already typed (ref $Cont).
  emit_own_cont(f, fc);
  f.instruction(&Instruction::ReturnCallRef(TY_CONT));
}

/// Emit a value and tail-call a cont with it.
fn emit_cont_call_with_val(val: &Val<'_>, cont_id: CpsId, f: &mut Function, fc: &mut FnCtx) {
  if let Some(fn_idx) = fc.ctx.func_index(cont_id) {
    maybe_set_cont_env(fn_idx, f, fc);
    emit_val(val, f, fc);
    f.instruction(&Instruction::ReturnCall(fn_idx));
    return;
  }
  // Check if cont_id is in cont_env.
  if fc.ctx.is_cont_env(cont_id) {
    emit_val(val, f, fc);
    f.instruction(&Instruction::GlobalGet(GLOBAL_CONT_ENV));
    f.instruction(&Instruction::RefAsNonNull);
    f.instruction(&Instruction::ReturnCallRef(TY_CONT));
    return;
  }
  // Unknown cont — it's the cont param, already typed (ref $Cont).
  emit_val(val, f, fc);
  emit_own_cont(f, fc);
  f.instruction(&Instruction::ReturnCallRef(TY_CONT));
}

// ---------------------------------------------------------------------------
// Value emission
// ---------------------------------------------------------------------------

/// Unwrap a $Num struct to f64 on the stack.
fn emit_unwrap_num(f: &mut Function) {
  f.instruction(&Instruction::RefCastNonNull(wasm_encoder::HeapType::Concrete(TY_NUM)));
  f.instruction(&Instruction::StructGet { struct_type_index: TY_NUM, field_index: 0 });
}

/// Wrap an f64 on the stack into a $Num struct.
fn emit_wrap_num(f: &mut Function) {
  f.instruction(&Instruction::StructNew(TY_NUM));
}

fn emit_val(val: &Val<'_>, f: &mut Function, fc: &mut FnCtx) {
  fc.mark(f, val.id);

  match &val.kind {
    ValKind::Lit(Lit::Int(n)) => {
      f.instruction(&Instruction::F64Const((*n as f64).into()));
      f.instruction(&Instruction::StructNew(TY_NUM));
    }
    ValKind::Lit(Lit::Float(n)) | ValKind::Lit(Lit::Decimal(n)) => {
      f.instruction(&Instruction::F64Const((*n).into()));
      f.instruction(&Instruction::StructNew(TY_NUM));
    }
    ValKind::Lit(Lit::Bool(b)) => {
      f.instruction(&Instruction::F64Const(if *b { 1.0_f64 } else { 0.0_f64 }.into()));
      f.instruction(&Instruction::StructNew(TY_NUM));
    }
    ValKind::Ref(crate::passes::cps::ir::Ref::Synth(id)) => {
      let bind_id = *id;
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
      f.instruction(&Instruction::F64Const(0.0_f64.into()));
      f.instruction(&Instruction::StructNew(TY_NUM));
    }
  }
}

// ---------------------------------------------------------------------------
// Function collection
// ---------------------------------------------------------------------------

/// Detect the `name = ·v_N = fn ...` pattern: LetFn cont is a LetVal that
/// aliases this fn's name_id back to a user-facing Bind::Name.
/// Returns the user name string if the pattern matches.
fn extract_user_name<'src>(
  fn_name_id: CpsId,
  cont: &Cont<'src>,
  ctx: &Ctx<'_, 'src>,
) -> Option<&'src str> {
  use crate::ast::NodeKind;
  let Cont::Expr { args, body } = cont else { return None; };
  if args.len() != 1 { return None; }
  let ExprKind::LetVal { name, val, .. } = &body.kind else { return None; };
  // val must be Ref::Synth pointing back to the LetFn name.
  let ValKind::Ref(Ref::Synth(target)) = &val.kind else { return None; };
  if *target != fn_name_id { return None; }
  // name must be Bind::SynthName with an AST Ident origin.
  if name.kind != Bind::SynthName { return None; }
  let ast_id = ctx.origin.try_get(name.id)?.as_ref()?;
  let ast_node = ctx.ast_index.try_get(*ast_id)?.as_ref()?;
  match &ast_node.kind {
    NodeKind::Ident(s) => Some(s),
    _ => None,
  }
}

/// Walk the entire IR to find LetVal bindings that assign user names to fns.
/// Handles multi-fn modules where names are bound inside fn bodies, including
/// names bound to FnClosure results (via closure_fn map or val_alias).
fn resolve_user_names<'src>(root: &Expr<'src>, ctx: &mut Ctx<'_, 'src>) {
  let mut names: Vec<(CpsId, &'src str)> = Vec::new();
  collect_name_bindings(root, ctx, &mut names);
  eprintln!("  name_bindings={:?}", names);
  for &(target, name) in &names {
    let origin = ctx.origin.try_get(target).and_then(|o| *o);
    eprintln!("    {} → {:?}, origin={:?}", name, target, origin);
  }
  for (target_id, name) in names {
    // Find the collected fn this target resolves to.
    let fn_pos = ctx.funcs.iter().position(|cf| {
      cf.name_id == target_id || cf.letval_bind_id == Some(target_id)
    }).or_else(|| {
      // Follow val_alias chain.
      let mut cur = target_id;
      for _ in 0..8 {
        if let Some(&alias) = ctx.val_alias.get(&cur) {
          if let Some(pos) = ctx.funcs.iter().position(|cf| cf.name_id == alias) {
            return Some(pos);
          }
          cur = alias;
        } else { break; }
      }
      // Check closure_fn map (direct and through val_alias chain).
      let mut cur = target_id;
      for _ in 0..8 {
        if let Some(&fn_idx) = ctx.closure_fn.get(&cur) {
          let pos = (fn_idx - FN_COMPILED_START) as usize;
          if pos < ctx.funcs.len() { return Some(pos); }
        }
        if let Some(&alias) = ctx.val_alias.get(&cur) {
          cur = alias;
        } else { break; }
      }
      // Check param_source chain (LetVal name → FnClosure result cont arg).
      let mut cur = target_id;
      for _ in 0..8 {
        if let Some(&source) = ctx.param_source.get(&cur) {
          if let Some(&fn_idx) = ctx.closure_fn.get(&source) {
            let pos = (fn_idx - FN_COMPILED_START) as usize;
            if pos < ctx.funcs.len() { return Some(pos); }
          }
          cur = source;
        } else { break; }
      }
      None
    });
    if let Some(pos) = fn_pos
      && ctx.funcs[pos].user_name.is_none() {
        ctx.funcs[pos].user_name = Some(name);
      }
  }
}

/// Check if a FnClosure result value (bound to `closure_bind_id`) gets assigned
/// a user name in the immediate body via LetVal chain.
fn collect_closure_result_names<'src>(
  closure_bind_id: CpsId,
  body: &Expr<'src>,
  ctx: &Ctx<'_, 'src>,
  out: &mut Vec<(CpsId, &'src str)>,
) {
  use crate::ast::NodeKind;
  eprintln!("    collect_closure_result_names: closure_bind={:?}, body_kind={}", closure_bind_id, match &body.kind {
    ExprKind::LetVal { .. } => "LetVal",
    ExprKind::LetFn { .. } => "LetFn",
    ExprKind::App { .. } => "App",
    ExprKind::If { .. } => "If",
  });
  if let ExprKind::LetVal { name, val, cont } = &body.kind {
    eprintln!("      LetVal name={:?}({:?}), val_kind={}, cont_is_expr={}",
      name.id, name.kind,
      match &val.kind { ValKind::Ref(Ref::Synth(id)) => format!("Synth({:?})", id), _ => "other".to_string() },
      matches!(cont, Cont::Expr { .. }));
    // Check if val references the closure bind (directly or through a chain).
    let _val_target = match &val.kind {
      ValKind::Ref(Ref::Synth(id)) => Some(*id),
      _ => None,
    };
    // Check cont args for user names that bind this value.
    if let Cont::Expr { args, body: cont_body } = cont {
      eprintln!("      LetVal cont_args: {:?}", args.iter().map(|a| (a.id, a.kind)).collect::<Vec<_>>());
      for arg in args {
        if arg.kind == Bind::SynthName
          && let Some(user_name) = ctx.origin.try_get(arg.id)
            .and_then(|o| o.as_ref())
            .and_then(|ast_id| ctx.ast_index.try_get(*ast_id))
            .and_then(|n| n.as_ref())
            .and_then(|n| match &n.kind { NodeKind::Ident(s) => Some(*s), _ => None })
          {
            // Map the closure bind id (from FnClosure Cont::Expr arg) to this user name.
            out.push((closure_bind_id, user_name));
          }
      }
      // Recurse into cont body.
      collect_closure_result_names(closure_bind_id, cont_body, ctx, out);
    }
  }
}

/// Recursively collect (target_cps_id, user_name) pairs from LetVal bindings.
fn collect_name_bindings<'src>(
  expr: &Expr<'src>,
  ctx: &Ctx<'_, 'src>,
  out: &mut Vec<(CpsId, &'src str)>,
) {
  use crate::ast::NodeKind;
  match &expr.kind {
    ExprKind::LetVal { name, val, cont } => {
      // Check if this LetVal assigns a user name to a synth target.
      if name.kind == Bind::SynthName
        && let Some(user_name) = ctx.origin.try_get(name.id)
          .and_then(|o| o.as_ref())
          .and_then(|ast_id| ctx.ast_index.try_get(*ast_id))
          .and_then(|n| n.as_ref())
          .and_then(|n| match &n.kind { NodeKind::Ident(s) => Some(*s), _ => None })
          && let ValKind::Ref(Ref::Synth(target)) = &val.kind {
            out.push((*target, user_name));
          }
      // Also check the cont args — they can carry user names too.
      // The cont arg receives the LetVal's value, so map the cont arg CpsId as well.
      if let Cont::Expr { args, body } = cont {
        for arg in args {
          if arg.kind == Bind::SynthName
            && let Some(user_name) = ctx.origin.try_get(arg.id)
              .and_then(|o| o.as_ref())
              .and_then(|ast_id| ctx.ast_index.try_get(*ast_id))
              .and_then(|n| n.as_ref())
              .and_then(|n| match &n.kind { NodeKind::Ident(s) => Some(*s), _ => None })
            {
              // The cont arg's CpsId is what gets bound in scope.
              // Map it directly — resolve_user_names will follow indirections.
              out.push((arg.id, user_name));
              // Also map the LetVal val target if it's a synth ref.
              if let ValKind::Ref(Ref::Synth(target)) = &val.kind {
                out.push((*target, user_name));
              }
            }
        }
        collect_name_bindings(body, ctx, out);
      }
    }
    ExprKind::LetFn { fn_body, cont, .. } => {
      collect_name_bindings(fn_body, ctx, out);
      if let Cont::Expr { body, .. } = cont {
        collect_name_bindings(body, ctx, out);
      }
    }
    ExprKind::App { func, args } => {
      // For FnClosure: the result cont arg receives the closure value.
      // If the body has a LetVal assigning a user name, map the cont arg CpsId too.
      let is_fn_closure = matches!(func, Callable::BuiltIn(crate::passes::cps::ir::BuiltIn::FnClosure));
      if is_fn_closure {
        if let Some(Arg::Cont(Cont::Expr { args: ca, .. })) = args.last() {
          eprintln!("    FOUND FnClosure, cont_args={:?}", ca.iter().map(|b| (b.id, b.kind)).collect::<Vec<_>>());
        }
        if let Some(Arg::Cont(Cont::Expr { args: cont_args, body })) = args.last() {
          // The cont_args[0] CpsId receives the closure value.
          if let Some(cont_bind) = cont_args.first() {
            // Check if the body immediately binds this to a user name via LetVal.
            collect_closure_result_names(cont_bind.id, body, ctx, out);
          }
          collect_name_bindings(body, ctx, out);
          return; // already recursed into body
        }
      }
      for arg in args {
        match arg {
          Arg::Cont(Cont::Expr { body, .. }) | Arg::Expr(body) => {
            collect_name_bindings(body, ctx, out);
          }
          _ => {}
        }
      }
    }
    ExprKind::If { then, else_, .. } => {
      collect_name_bindings(then, ctx, out);
      collect_name_bindings(else_, ctx, out);
    }
  }
}

fn collect_funcs<'a, 'src>(expr: &'a Expr<'src>, ctx: &mut Ctx<'a, 'src>) {
  match &expr.kind {
    ExprKind::LetFn { name, params, fn_body, cont } => {
      // Last param is always the cont (Bind::Cont) — exclude it from
      // param_ids/param_kinds so the cont-env detection invariant holds:
      // cont fns have arity == param_ids.len(), regular fns have arity > param_ids.len().
      let value_params = &params[..params.len() - 1];
      let param_ids: Vec<CpsId> = value_params.iter().map(|p| match p {
        crate::passes::cps::ir::Param::Name(b) => b.id,
        crate::passes::cps::ir::Param::Spread(b) => b.id,
      }).collect();
      // Record the LetVal continuation bind (Bind::Name) if present.
      // name_res resolves user references to this fn via this bind_id, not name_id.
      let letval_bind_id = if let Cont::Expr { args, .. } = cont {
        args.first().map(|b| b.id)
      } else {
        None
      };
      let param_kinds: Vec<Bind> = value_params.iter().map(|p| match p {
        crate::passes::cps::ir::Param::Name(b) => b.kind,
        crate::passes::cps::ir::Param::Spread(b) => b.kind,
      }).collect();
      let cont_id = params.last()
        .map(|p| match p { crate::passes::cps::ir::Param::Name(b) | crate::passes::cps::ir::Param::Spread(b) => b.id })
        .expect("LetFn must have at least one param (cont)");
      // Detect user name: LetFn cont body is LetVal aliasing this fn's name.
      // Pattern: Cont::Expr { body: LetVal { name: <user>, val: Ref::Synth(name.id), .. } }
      let user_name = extract_user_name(name.id, cont, ctx);
      ctx.funcs.push(CollectedFn {
        name_id: name.id,
        letval_bind_id,
        bind: name.kind,
        fn_body,
        arity: params.len() as u32,
        param_ids,
        param_kinds,
        cont_id,
        user_name,
      });
      collect_funcs(fn_body, ctx);
      if let Cont::Expr { body: cont_body, .. } = cont {
        collect_funcs(cont_body, ctx);
      }
    }
    ExprKind::LetVal { name, val, cont: Cont::Expr { args, body: cont_body } } => {
      // Record val_alias for LetVal rebindings: name → synth target.
      // This lets func_index follow chains like `add = <fn_ref>`.
      // Also alias the Cont::Expr args (the scope binds name_res resolves to)
      // so func_index_through_closure works for references resolved via name_res.
      if let ValKind::Ref(Ref::Synth(target)) = &val.kind {
        ctx.val_alias.insert(name.id, *target);
        for arg in args {
          ctx.val_alias.insert(arg.id, *target);
        }
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
    _ => {}
  }
}

/// Walk the CPS tree to process FnClosure apps after all funcs are collected.
/// - Builds cap_param_fn: maps cap param CpsIds to fn indices for static resolution.
/// - Strips static cap params (module-level fn refs) from hoisted fn signatures.
/// - Builds closure_fn: maps result cont params to hoisted fn indices.
fn process_closures<'a, 'src>(expr: &'a Expr<'src>, ctx: &mut Ctx<'a, 'src>) {
  // Phase 1: Populate closure_fn for ALL FnClosure nodes. This must happen first so that
  //   func_index_through_closure can resolve cap args that alias through FnClosure result
  //   binds (e.g. recursive fn closures where the fn ref goes through closure_fn).
  collect_closure_fns(expr, ctx);
  // Phase 2+3: Iterate static cap collection + processing until convergence.
  //   Mutual recursion creates forward references (is-odd bound after is-even's FnClosure).
  //   First pass resolves the backward ref (is-even), populating known_static_caps.
  //   Second pass resolves the forward ref (is-odd) via known_static_caps.
  let mut known_static_caps: std::collections::HashMap<AstId, u32> = std::collections::HashMap::new();
  for _ in 0..8 {
    let prev_count = known_static_caps.len();
    collect_static_fn_caps(expr, ctx, &mut known_static_caps);
    if known_static_caps.len() == prev_count { break; }
  }
  process_closures_inner(expr, ctx, &known_static_caps);
}

/// Phase 1 pre-pass: walk FnClosure Apps and populate closure_fn and param_source
/// entries for ALL FnClosure nodes. This must run before cap resolution so
/// func_index_through_closure and param_source chains work in later phases.
fn collect_closure_fns<'a, 'src>(expr: &'a Expr<'src>, ctx: &mut Ctx<'a, 'src>) {
  use crate::passes::cps::ir::BuiltIn;
  match &expr.kind {
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::FnClosure), args } => {
      if let Some(Arg::Val(fn_val)) = args.first()
        && let ValKind::Ref(Ref::Synth(hoisted_fn_id)) = &fn_val.kind
        && let Some(fn_pos) = ctx.funcs.iter().position(|f| f.name_id == *hoisted_fn_id)
      {
        let hoisted_fn_idx = FN_COMPILED_START + fn_pos as u32;
        // Populate param_source: cap param → cap arg source bind.
        let cap_args = &args[1..args.len().saturating_sub(1)];
        let fn_params = ctx.funcs[fn_pos].param_ids.clone();
        for (cap_arg, param_id) in cap_args.iter().zip(fn_params.iter()) {
          if let Arg::Val(cap_val) = cap_arg
            && let Some(bid) = resolve_cap_arg_bind(cap_val, ctx)
          {
            ctx.param_source.insert(*param_id, bid);
          }
        }
        // Populate closure_fn: result cont param → hoisted fn index.
        let result_cont_param = args.last().and_then(|a| match a {
          Arg::Cont(Cont::Ref(id)) => {
            ctx.funcs.iter().position(|f| f.name_id == *id)
              .and_then(|pos| {
                ctx.funcs[pos].param_ids.first().copied()
                  .or_else(|| if ctx.funcs[pos].arity >= 1 { Some(ctx.funcs[pos].cont_id) } else { None })
              })
          }
          Arg::Cont(Cont::Expr { args: cont_args, .. }) => {
            cont_args.first().map(|b| b.id)
          }
          _ => None,
        });
        if let Some(param_id) = result_cont_param {
          ctx.closure_fn.insert(param_id, hoisted_fn_idx);
        }
      }
      for arg in args {
        match arg {
          Arg::Cont(Cont::Expr { body, .. }) | Arg::Expr(body) => collect_closure_fns(body, ctx),
          _ => {}
        }
      }
    }
    ExprKind::LetFn { fn_body, cont, .. } => {
      collect_closure_fns(fn_body, ctx);
      if let Cont::Expr { body: cont_body, .. } = cont {
        collect_closure_fns(cont_body, ctx);
      }
    }
    ExprKind::LetVal { cont: Cont::Expr { body: cont_body, .. }, .. } => {
      collect_closure_fns(cont_body, ctx);
    }
    ExprKind::App { args, .. } => {
      for arg in args {
        match arg {
          Arg::Cont(Cont::Expr { body, .. }) | Arg::Expr(body) => collect_closure_fns(body, ctx),
          _ => {}
        }
      }
    }
    ExprKind::If { then, else_, .. } => {
      collect_closure_fns(then, ctx);
      collect_closure_fns(else_, ctx);
    }
    _ => {}
  }
}

/// Phase 2 pre-pass: walk FnClosure Apps, find cap args that resolve to known fn indices.
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
            let cap_fn_idx = bind_id.and_then(|bid| {
              ctx.func_index_through_closure(bid).or_else(|| {
                // Follow param_source chain: cap param → original bind → fn index.
                // Handles caps that reference other fns via Local(cap_param).
                let mut cur = bid;
                for _ in 0..8 {
                  if let Some(&source) = ctx.param_source.get(&cur) {
                    if let Some(idx) = ctx.func_index_through_closure(source) {
                      return Some(idx);
                    }
                    cur = source;
                  } else { break; }
                }
                None
              })
            });
            // Fallback 1: check already-discovered static caps by param AST origin.
            let cap_fn_idx = cap_fn_idx.or_else(|| {
              let ast_id = ctx.origin.try_get(*param_id)?.as_ref()?;
              out.get(ast_id).copied()
            });
            // Fallback 2: for Unresolved forward refs (mutual recursion), find a
            // val_alias entry whose key shares the same AST origin as the cap val.
            // The LetVal that binds the forward-referenced fn creates a val_alias
            // entry; matching by AST origin connects the unresolved cap to it.
            let cap_fn_idx = cap_fn_idx.or_else(|| {
              if bind_id.is_some() { return None; } // only for Unresolved
              let cap_ast = ctx.origin.try_get(cap_val.id)?.as_ref()?;
              for (&alias_key, &alias_target) in &ctx.val_alias {
                if let Some(Some(key_ast)) = ctx.origin.try_get(alias_key)
                  && key_ast == cap_ast
                  && let Some(idx) = ctx.func_index_through_closure(alias_target)
                {
                  return Some(idx);
                }
              }
              None
            });
            if let Some(cap_fn_idx) = cap_fn_idx
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
    ExprKind::LetFn { fn_body, cont, .. } => {
      collect_static_fn_caps(fn_body, ctx, out);
      if let Cont::Expr { body: cont_body, .. } = cont {
        collect_static_fn_caps(cont_body, ctx, out);
      }
    }
    ExprKind::LetVal { cont: Cont::Expr { body: cont_body, .. }, .. } => {
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
fn resolve_cap_arg_bind(cap_val: &Val<'_>, _ctx: &Ctx<'_, '_>) -> Option<CpsId> {
  match &cap_val.kind {
    ValKind::Ref(Ref::Synth(id)) => Some(*id),
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
            // Check direct resolution first (includes closure_fn chain).
            let cap_fn_idx = bind_id.and_then(|bid| ctx.func_index_through_closure(bid));
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
          // +1 for the fn's own cont param (still present, not stripped).
          cf.arity = cf.param_ids.len() as u32 + 1;
        }

        // Strip Bind::Cont captures from cont fns → pass via $cont_env global.
        // After static cap stripping, any remaining Bind::Cont params are cont captures
        // that must go through $cont_env to keep the fn $Cont-compatible.
        {
          let cf = &ctx.funcs[fn_pos];
          let cont_cap_indices: Vec<usize> = cf.param_kinds.iter().enumerate()
            .filter(|(_, k)| **k == Bind::Cont)
            .map(|(i, _)| i)
            .collect();
          if !cont_cap_indices.is_empty() {
            // Record the cap arg source CpsIds for the stripped cont caps.
            let cont_cap_sources: Vec<CpsId> = cont_cap_indices.iter()
              .filter_map(|&i| {
                let param_id = ctx.funcs[fn_pos].param_ids[i];
                ctx.param_source.get(&param_id).copied()
              })
              .collect();
            let fn_name_id = ctx.funcs[fn_pos].name_id;
            if !cont_cap_sources.is_empty() {
              ctx.cont_env_caps.insert(fn_name_id, cont_cap_sources);
            }
            // Map post-strip cont_cap_indices back to original cap_args indices.
            // After static cap removal, post-strip index i maps to the (i+skip)-th
            // original index, where skip accounts for removed static caps before it.
            let remaining_original_indices: Vec<usize> = (0..fn_params.len())
              .filter(|i| !static_cap_indices.contains(i))
              .collect();
            for &post_strip_idx in &cont_cap_indices {
              if let Some(&orig_idx) = remaining_original_indices.get(post_strip_idx)
                && !static_cap_indices.contains(&orig_idx)
              {
                static_cap_indices.push(orig_idx);
              }
            }
            let cf = &mut ctx.funcs[fn_pos];
            for &i in cont_cap_indices.iter().rev() {
              cf.param_ids.remove(i);
              cf.param_kinds.remove(i);
            }
            // Arity = param_ids.len() (no +1 for own dead cont).
            // Cont fns read their cont from $cont_env, so their own cont param is unused.
            cf.arity = cf.param_ids.len() as u32;
          }
        }

        // Record closure_fn: result cont param → hoisted fn index.
        let hoisted_fn_idx = FN_COMPILED_START + fn_pos as u32;
        // Handle both Cont::Ref and Cont::Expr for the result cont.
        let result_cont_param = args.last().and_then(|a| match a {
          Arg::Cont(Cont::Ref(id)) => {
            // Hoisted cont fn: first value param binds the closure value.
            // If the fn has no value params (param_ids=[]), fall back to cont_id:
            // for arity-1 fns with no value params, the cont IS the single param
            // and receives the closure value.
            ctx.funcs.iter().position(|f| f.name_id == *id)
              .and_then(|pos| {
                ctx.funcs[pos].param_ids.first().copied()
                  .or_else(|| if ctx.funcs[pos].arity >= 1 { Some(ctx.funcs[pos].cont_id) } else { None })
              })
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
    ExprKind::LetFn { fn_body, cont, .. } => {
      process_closures_inner(fn_body, ctx, known_static_caps);
      if let Cont::Expr { body: cont_body, .. } = cont {
        process_closures_inner(cont_body, ctx, known_static_caps);
      }
    }
    ExprKind::LetVal { cont: Cont::Expr { body: cont_body, .. }, .. } => {
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
          // Resolve result_cont_id → the arm_val key used in MatchBlock.
          //
          // After lifting, the result cont fn is a hoisted fn. Its first value param
          // binds the arm's null placeholder — this CpsId is the arm_val key.
          //
          // When the result cont fn has no value params (cont_env fn with arity=0),
          // follow the fn's body: it's typically a FnClosure that creates the MatchBlock
          // fn. In that case the MatchBlock fn's cont_id is the arm_val key (since
          // the MatchBlock fn is a cont_env fn whose cont_id is mapped to local 0,
          // the same slot used for arm_val references in the MatchBlock body).
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
              } else if ctx.funcs[fn_pos].arity == 0 {
                // No value params: cont_env fn receives arm value as anyref.
                // Its cont_id is mapped to local 0 for emit purposes.
                // Key by cont_id so emit_match_block can find it via direct lookup.
                let cont_id = ctx.funcs[fn_pos].cont_id;
                ctx.match_arms.insert(cont_id, (matcher_fn, body_fn));
                // Also: if the fn's body is FnClosure creating another fn (fn_Y),
                // fn_Y.cont_id is the arm_val key used in MatchBlock (because fn_Y
                // is a cont_env fn called as a continuation from the fn above).
                if let ExprKind::App { func: Callable::BuiltIn(crate::passes::cps::ir::BuiltIn::FnClosure), args: fc_args } = &ctx.funcs[fn_pos].fn_body.kind
                  && let Some(Arg::Val(fn_ref_val)) = fc_args.first()
                  && let ValKind::Ref(Ref::Synth(inner_fn_id)) = &fn_ref_val.kind
                  && let Some(inner_pos) = ctx.funcs.iter().position(|f| f.name_id == *inner_fn_id)
                {
                  let inner_cont_id = ctx.funcs[inner_pos].cont_id;
                  ctx.match_arms.insert(inner_cont_id, (matcher_fn, body_fn));
                }
              }
            }
          }
        }
      }
    }
    ExprKind::LetFn { fn_body, cont, .. } => {
      collect_match_arms(fn_body, ctx);
      match cont {
        Cont::Expr { body: cont_body, .. } => collect_match_arms(cont_body, ctx),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::LetVal { cont: Cont::Expr { body: cont_body, .. }, .. } => {
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
  }
}


fn collect_letval_locals(expr: &Expr<'_>, out: &mut Vec<CpsId>) {
  match &expr.kind {
    ExprKind::LetVal { cont: Cont::Expr { args, body }, .. } => {
      for arg in args { out.push(arg.id); }
      collect_letval_locals(body, out);
    }
    ExprKind::LetVal { cont: Cont::Ref(_), .. } => {}
    ExprKind::LetFn { cont, .. } => {
      if let Cont::Expr { args, body: cont_body } = cont {
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
    ExprKind::If { .. } => {}
  }
}

fn discover_aliases(expr: &Expr<'_>, locals: &mut Vec<(CpsId, u32)>) {
  let mut current = expr;
  loop {
    match &current.kind {
      ExprKind::LetVal { name, val, cont: Cont::Expr { args, body: cont_body } } => {
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
      ExprKind::LetFn { cont: Cont::Expr { args, body: cont_body }, .. } => {
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
  use crate::passes::cps::transform::lower_expr;
  use crate::passes::lifting::lift;
  use crate::passes::name_res;
  use super::codegen;

  fn compile_wasm(src: &str) -> Vec<u8> {
    let r = parse(src).expect("parse failed");
    let ast_index = build_index(&r);
    let scope = crate::passes::scopes::analyse(&r.root, r.node_count as usize, &[]);
    let cps = lower_expr(&r.root, &scope);
    let lifted = lift(cps, &ast_index);
    let node_count = lifted.origin.len();
    let resolved = name_res::resolve(&lifted.root, &lifted.origin, &ast_index, node_count, &lifted.synth_alias);
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
    let scope = crate::passes::scopes::analyse(&r.root, r.node_count as usize, &[]);
    let cps = lower_expr(&r.root, &scope);
    let lifted = lift(cps, &ast_index);
    let node_count = lifted.origin.len();
    let resolved = name_res::resolve(&lifted.root, &lifted.origin, &ast_index, node_count, &lifted.synth_alias);
    let result = codegen(&lifted, &resolved, &ast_index);
    assert!(!result.mappings.is_empty(), "should produce source mappings");
    let has_literal = result.mappings.iter().any(|m| m.src_line == 1 && m.src_col == 11);
    assert!(has_literal, "should map to literal 42; got: {:?}", result.mappings);
  }

  test_macros::include_fink_tests!("src/passes/wasm/test_codegen.fnk");
}
