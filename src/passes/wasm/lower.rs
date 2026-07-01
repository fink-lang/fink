//! CPS → unlinked wasm IR `Fragment`.
//!
//! Tracer-phase walker. Handles the *lifted* CPS shape — all
//! closures are already materialised as `LetFn` + `App(FnClosure)`,
//! so this pass doesn't do free-var analysis.
//!
//! ## Threading model
//!
//! Walker helpers take two contexts side-by-side:
//!
//! * `LowerCtx` — module-scoped. Bundles `cps`, `ast`, `rt`, `frag`,
//!   `pub_globals`, `fqn_prefix`. Constructed once in `lower()` and
//!   threaded as `&mut LowerCtx` through every helper.
//! * `FnCtx` — per-function state (params, locals, instrs, binds,
//!   `fn_syms`). A fresh `FnCtx` is built per `lower_fn` call;
//!   nested fns clone parent `fn_syms` so siblings/ancestors stay
//!   visible while child mutations don't leak back.
//!
//! ## Current coverage:
//! * `main = fn: <lit>` — apply-path via `apply_3` (Lit ∈ {Int, Float,
//!   Decimal, Bool}).
//! * Binary + unary protocol operators (Add..Shr, Rngex/Rngin, In,
//!   NotIn, Get, Not).
//! * `LetVal` — value bindings visible in cont body.
//! * `LetFn` — lifted function definitions (separate WASM funcs).
//! * `App(FnClosure, ...)` — closure construction
//!   (`struct.new $Closure (funcref + captures array)`).
//! * `App(Pub, [val, cont])` — no-op: val ignored, cont body emitted.
//! * `App(Callable::Val(Ref), [..., Cont::Ref(_)])` — closure
//!   dispatch via `apply_3`.
//! * Capture reads: when a cap-param is referenced, emit
//!   `array.get $Captures $_caps <i>`.

use std::collections::HashMap;

use crate::passes::ast::Ast;
use crate::passes::cps::ir::{
  Arg, Bind, BindNode, Callable, Cont, CpsId, CpsResult, Expr, ExprKind,
  Lit, Param, Ref, Val, ValKind, BuiltIn,
};
use crate::sourcemap::native::ByteRange;

use super::ir::*;
use super::runtime_contract::{self, Runtime, Sym};

/// Look up the source byte range for a CPS node.
fn origin_of(cps: &CpsResult, ast: &Ast<'_>, id: CpsId) -> Option<ByteRange> {
  let ast_id = (*cps.origin.try_get(id)?)?;
  let loc = ast.nodes.get(ast_id).loc;
  Some(ByteRange::new(loc.start.idx, loc.end.idx))
}

/// True iff this CpsId's source origin is an `Apply` AST node — i.e. a
/// call the user actually wrote, not a desugar-synthesised apply (pipe
/// expansion, partial application, etc.) whose origin is the surrounding
/// `InfixOp`/`Pipe`/lambda node. Used to gate trace instrumentation so
/// the trace reflects source-level calls.
fn is_source_apply(cps: &CpsResult, ast: &Ast<'_>, id: CpsId) -> bool {
  let Some(Some(ast_id)) = cps.origin.try_get(id) else { return false };
  matches!(ast.nodes.get(*ast_id).kind, crate::ast::NodeKind::Apply { .. })
}

/// True iff this CpsId's source origin is a `Fn` AST node — i.e. a
/// function the user actually wrote, not a desugar-synthesised
/// `CpsFunction` (e.g. the `m_0`/`mp_N` match-block wrappers, whose
/// origin is the surrounding `Match` node). Used to gate trace-frame
/// pushes so the activation stack reflects only source-level functions;
/// synth `CpsFunction`s route through the no-push/inherit path like a
/// `CpsClosure`, keeping push/pop balanced and the trace free of `:0`
/// synth frames.
fn is_source_fn(cps: &CpsResult, ast: &Ast<'_>, id: CpsId) -> bool {
  let Some(Some(ast_id)) = cps.origin.try_get(id) else { return false };
  matches!(ast.nodes.get(*ast_id).kind, crate::ast::NodeKind::Fn { .. })
}

/// Lower a lifted CPS result to an unlinked wasm IR `Fragment`.
///
/// `fqn_prefix` is the module's fully-qualified URL prefix (e.g.
/// `"./sub/foo.fnk:"` — including the trailing colon) used to
/// namespace every emitted symbol's display name. Pass `""` for
/// single-fragment compiles (current default; no namespacing).
/// Multi-fragment package compiles pass each fragment its own prefix
/// so cross-fragment merges are collision-free by construction.
///
/// Phase-4A: prefix is purely a cosmetic / naming concern. The entry
/// function's export name still stays `"fink_module"`; rewiring the
/// body to the `import_module` init-guard shape lands in 4D.
pub fn lower(cps: &CpsResult, ast: &Ast<'_>, fqn_prefix: &str, module_id: ModuleId) -> Fragment {
  let mut usage = runtime_contract::scan(cps);
  // Fn3 / ctx-aware lowering routes user-fn calls through `apply_3`
  // instead of `apply`. Mark the Apply3 runtime symbol so `declare()`
  // sets up the import. The scan already marks Apply (Fn3) — that's
  // harmless here; lower_ctx never emits a call to it.
  usage.mark(runtime_contract::Sym::Apply3);
  usage.mark(runtime_contract::Sym::Fn3);
  // Trace instrumentation (rt/trace.wat): each userland fn body pushes a
  // frame on entry and pops on return; each userland call site marks the
  // current frame's call site.
  usage.mark(runtime_contract::Sym::TracePush);
  usage.mark(runtime_contract::Sym::TraceMark);
  usage.mark(runtime_contract::Sym::TracePop);
  // Each fink_module self-registers its (module_id, url) so trace frames
  // resolve to a source url (rt/modules.wat).
  usage.mark(runtime_contract::Sym::RegisterModule);
  // Per-module wrapper synthesised below uses init_module and the
  // closure/captures/str primitives.
  usage.mark(runtime_contract::Sym::ModulesInitModule);
  usage.mark(runtime_contract::Sym::Closure);
  usage.mark(runtime_contract::Sym::Captures);
  usage.mark(runtime_contract::Sym::Cell);
  usage.mark(runtime_contract::Sym::StrFromData);
  // module_id set here (not after lowering): trace_push / register_module
  // bake the module id as a constant at emit time, so it must be correct
  // during lowering. (compile_package used to set frag.module_id after
  // lower returned, which was too late for those emitted constants.)
  let mut frag = Fragment { module_id, ..Default::default() };
  let rt = runtime_contract::declare(&mut frag, &usage);

  // CPS root shape: App(FinkModule, [Cont::Expr { args: [ƒctx, ƒret], body }]).
  let Some((ctx_bind, ret_bind, module_body)) = extract_fink_module_body(&cps.root) else {
    panic!("lower: unsupported CPS root shape (expected App(FinkModule, [Cont::Expr]))");
  };

  // Scan for ·ƒpub apps and pre-allocate one exported global per
  // exported binding. Pub apps may occur inside hoisted LetFn bodies
  // (lifted conts that pub a captured slot — the slot's actual cell
  // lives at module scope but the pub'd CpsId is the lifted body's
  // local-rebind). Scanning from `cps.root` covers both module body
  // and every hoisted fn body uniformly.
  let mut pubs: Vec<(CpsId, String)> = Vec::new();
  find_pub_apps(&cps.root, cps, ast, &mut pubs);
  let mut pub_globals: HashMap<CpsId, (GlobalSym, String)> = HashMap::new();
  for (id, name) in &pubs {
    let qualified = format!("{fqn_prefix}{name}");
    // No WASM-level export. User-binding globals are addressable
    // storage for the registry (`std/modules.fnk:pub`); the host
    // accesses bindings exclusively through the per-module host
    // wrapper export, which routes via `init_module` + the
    // registry. The bare globals stay because lifted closures read
    // them at module-init time (forward-reference machinery).
    // Slot storage: `(mut (ref null $Cell))`. The Cell is allocated
    // per-slot at LetRec lowering time and stored here; Set updates
    // the Cell's `$value` field rather than overwriting the global.
    let sym = add_global(
      &mut frag,
      val_ref(rt.cell(), /*nullable*/ true),
      true,
      GlobalInit::RefNullConcrete(rt.cell()),
      &qualified,
      None,
    );
    pub_globals.insert(*id, (sym, name.clone()));
  }

  // Lower the module body as the `fink_module` function. Double-colon
  // marks compiler-synth so it can't collide with a user `pub
  // fink_module` in source: `:` is lexer-rejected at the source
  // level, making `<fqn>::fink_module` collision-safe.
  let module_display = format!("{fqn_prefix}:fink_module");
  let bind_kinds = crate::passes::cps::ir::collect_bind_kinds(&cps.root);
  let mut slot_ids: std::collections::HashSet<CpsId> = std::collections::HashSet::new();
  collect_slot_ids(&cps.root, &mut slot_ids);
  // Per-slot wasm global. Pub'd slots reuse their `pub_globals`
  // GlobalSym; non-pub'd slots get a fresh synth-named global so the
  // Cell ref has somewhere to live. The Cell-ref globals are not
  // exported and not registered with the runtime — they're just
  // storage for the slot's mutable reference.
  let mut slot_globals: HashMap<CpsId, GlobalSym> = HashMap::new();
  // Iterate slot ids in deterministic order so the emitted globals
  // appear in the same order across runs (HashSet iteration is
  // unordered).
  let mut sorted_slot_ids: Vec<CpsId> = slot_ids.iter().copied().collect();
  sorted_slot_ids.sort_by_key(|id| id.0);
  for id in &sorted_slot_ids {
    if let Some((g, _)) = pub_globals.get(id) {
      slot_globals.insert(*id, *g);
      continue;
    }
    // Non-pub slot: allocate a private global, named after the slot's
    // source name (or `:v_<id>` for compiler temps) qualified with the
    // module's FQN. No WASM export.
    let raw = crate::passes::cps::ir::collect_bind_kinds(&cps.root);
    let _ = raw;  // dummy to silence unused warning; not used here
    let source = pub_export_name(cps, ast, *id);
    let qualified = format!("{fqn_prefix}:{source}_{}", id.0);
    let sym = add_global(
      &mut frag,
      val_ref(rt.cell(), /*nullable*/ true),
      true,
      GlobalInit::RefNullConcrete(rt.cell()),
      &qualified,
      None,
    );
    slot_globals.insert(*id, sym);
  }
  {
    let mut lcx = LowerCtx {
      cps, ast, rt: &rt, frag: &mut frag,
      pub_globals: &pub_globals, fqn_prefix,
      bind_kinds: &bind_kinds,
      slot_ids: &slot_ids,
      slot_globals: &slot_globals,
    };

    // After hoist, top-level LetFns wrap the FinkModule App. Lower
    // each one as a wasm func and collect their FuncSyms so the
    // module body's Closure construction sites can resolve them by
    // CpsId. We walk the chain pre-order: outer LetFn first.
    let mut top_fn_syms: HashMap<CpsId, FuncSym> = HashMap::new();
    let mut node = &lcx.cps.root;
    while let ExprKind::LetFn { name, params, fn_body, cont, fn_kind, .. } = &node.kind {
      let mut cap_ids: Vec<CpsId> = Vec::new();
      let mut user_ids: Vec<(CpsId, bool)> = Vec::new();
      for p in params {
        let (bind, is_spread) = match p {
          Param::Name(b)   => (b, false),
          Param::Spread(b) => (b, true),
        };
        if matches!(bind.kind, Bind::Caps) {
          cap_ids.push(bind.id);
        } else {
          user_ids.push((bind.id, is_spread));
        }
      }
      let raw_display = cps_ident_for_bind(lcx.cps, lcx.ast, name);
      let display = format!("{}{}", lcx.fqn_prefix, raw_display);
      use crate::passes::cps::ir::CpsFnKind;
      let trace = match fn_kind {
        CpsFnKind::CpsFunction if is_source_fn(lcx.cps, lcx.ast, name.id) =>
          TraceFrame::entry(name.id),
        _ => TraceFrame::cont(None),  // synth/closure: no frame, no enclosing
      };
      let fn_sym = lower_fn(
        &mut lcx,
        &cap_ids, &user_ids, fn_body, &display,
        &top_fn_syms,
        trace,
      );
      top_fn_syms.insert(name.id, fn_sym);
      node = match cont {
        Cont::Expr { body, .. } => body,
        Cont::Ref(_) => panic!("lower: top-level LetFn has Cont::Ref"),
      };
    }

    let fink_module = lower_fn(
      &mut lcx,
      &[],                 // no cap params at the module level
      &[(ctx_bind, false), (ret_bind, false)], // user params: ƒctx, ƒret
      module_body,
      &module_display,
      &top_fn_syms,
      TraceFrame::cont(None),  // module body is synth: no frame, no enclosing
    );
    let FuncSym::Local(_) = fink_module else { panic!("lower: fink_module must be Local"); };
    // No WASM-level export for fink_module — host accesses the module
    // exclusively through the per-module wrapper exported under the
    // canonical FQN. fink_module stays as a bare internal func; the
    // wrapper holds a no-capture closure over it.

    // Self-register (module_id, url) at the top of fink_module so trace
    // frames (which carry module_id) can resolve back to a source url.
    prepend_module_registration(&mut lcx, fink_module);

    // Populate the str->symbol table at module-body startup: one
    // register_symbol(name, id) per interned static field name, so a $Str
    // key (import-rec, dynamic lookup) coerces to its $Symbol.
    prepend_symbol_table(&mut lcx, fink_module);

    // Per-module host-facing wrapper. Exported under the module's
    // canonical FQN so the host can call any module by URL string
    // (`instance.get_func(canonical_url)`). The wrapper composes the
    // module's fink_module with `std/modules.fnk:init_module` —
    // run-once + optional named-export extraction in one call.
    synth_host_wrapper(&mut lcx, fink_module);
  }

  frag
}

/// Synthesise the per-module host-facing wrapper export.
///
/// Each module's wrapper is a Fn3-shaped function exported under the
/// module's canonical FQN (or `"fink_module"` for a fragment with
/// empty `fqn_prefix`, matching the pre-wrapper convention so
/// existing runners keep working). When called by a host, it:
///
/// 1. Takes `:cont` as its sole param — a host-provided anyref
///    continuation that init_module will fire with
///    `(last_expr, exports_rec)`.
/// 2. Builds a no-capture `$Closure` over the module's `fink_module`
///    funcref (funcrefs aren't anyref-compatible; the closure
///    bridges).
/// 3. Materialises the canonical URL as a `$Str` constant — used by
///    `init_module` to key the runtime registry.
/// 4. Tail-calls `std/modules.fnk:init_module(url, mod_clos, cont)`
///    which handles run-once init, then tail-applies cont with
///    `(last_expr, exports_rec)`. Hosts pull named exports out of
///    the rec via `interop/rust.wat:rec_get_by_bytes` (or its JS
///    equivalent).
///
/// The module body itself is Fn3 — init_module's `apply_3` shim
/// synthesises a placeholder ctx (ref.i31 42) and tail-calls the
/// body's Fn3 entry. Once the substrate lands, host-provided ctx
/// flows in through the same channel.
/// Prepend a `register_module(module_id, url)` call to the top of the
/// module's `fink_module` body. `module_id` is the fragment's own id (a
/// compile-time constant); `url` is the canonical url materialised as a
/// `$Str`. This is how the runtime learns module_id → url for resolving
/// trace frames.
fn prepend_module_registration(lcx: &mut LowerCtx<'_>, fink_module: FuncSym) {
  let FuncSym::Local(fi) = fink_module else {
    panic!("prepend_module_registration: fink_module must be Local");
  };
  let fi = fi as usize;

  let canonical_url = lcx.fqn_prefix.trim_end_matches(':').to_string();
  let mid = lit_i32(lcx.frag.module_id.0 as i32);

  // Allocate a fresh local on fink_module for the url $Str. New locals
  // index after params + existing locals.
  let url_idx = LocalIdx(
    (lcx.frag.funcs[fi].params.len() + lcx.frag.funcs[fi].locals.len()) as u32,
  );
  lcx.frag.funcs[fi].locals.push(LocalDecl {
    ty: val_anyref(true),
    display: Some(":mod_reg_url".to_string()),
  });

  // url = from_data(intern(canonical_url)) — same materialisation the
  // host wrapper uses for the registry key.
  let sym = intern_data(lcx.frag, canonical_url.as_bytes());
  let len = canonical_url.len() as u32;
  let i_url = push_call(lcx.frag, lcx.rt.str_from_data(),
    vec![Operand::DataRef { sym, len }], Some(url_idx));

  // register_module(mid, url)
  let i_reg = push_call(lcx.frag, lcx.rt.register_module(),
    vec![mid, op_local(url_idx)], None);

  // Prepend both, url-const first, before the existing body.
  let body = &mut lcx.frag.funcs[fi].body;
  body.insert(0, i_reg);
  body.insert(0, i_url);
}

/// Prepend `register_symbol(name, id)` calls to the module body — one per
/// interned static field name — so the runtime str->symbol table is populated
/// at startup. A $Str key (module-export rec, `r.('foo')`) then coerces to its
/// $Symbol before lookup.
fn prepend_symbol_table(lcx: &mut LowerCtx<'_>, fink_module: FuncSym) {
  let FuncSym::Local(fi) = fink_module else {
    panic!("prepend_symbol_table: fink_module must be Local");
  };
  let fi = fi as usize;

  // Snapshot names — `intern_data` below mutates the fragment. The id is
  // carried by `Operand::SymbolId(name)` and resolved at link.
  let entries: Vec<Vec<u8>> = lcx.frag.symbols.keys().cloned().collect();
  if entries.is_empty() { return; }

  // Build instrs in source order, then prepend the whole block before the
  // body (insert(0, ...) in reverse keeps registration order).
  let base = (lcx.frag.funcs[fi].params.len() + lcx.frag.funcs[fi].locals.len()) as u32;
  let key_idx = LocalIdx(base);
  lcx.frag.funcs[fi].locals.push(LocalDecl {
    ty: val_anyref(true),
    display: Some(":sym_reg_name".to_string()),
  });
  // The symbol word (a compile-time const folded at link); box it as `ref.i31`
  // to pass to register_symbol, which stores it directly.
  let word_idx = LocalIdx(base + 1);
  lcx.frag.funcs[fi].locals.push(LocalDecl {
    ty: val_ref_abs(AbsHeap::I31, /*nullable*/ false),
    display: Some(":sym_reg_word".to_string()),
  });

  let mut block: Vec<InstrId> = Vec::with_capacity(entries.len() * 3);
  for name in &entries {
    let sym = intern_data(lcx.frag, name);
    let len = name.len() as u32;
    let i_name = push_call(lcx.frag, lcx.rt.str_from_data(),
      vec![Operand::DataRef { sym, len }], Some(key_idx));
    // word carried by name; resolved to the folded i31 const at link, same as
    // box_symbol. Box it as ref.i31 for register_symbol's (ref i31) param.
    let i_word = push_ref_i31(lcx.frag, Operand::SymbolId(name.clone()), word_idx);
    let i_reg = push_call(lcx.frag, lcx.rt.register_symbol(),
      vec![op_local(key_idx), op_local(word_idx)], None);
    block.push(i_name);
    block.push(i_word);
    block.push(i_reg);
  }
  let body = &mut lcx.frag.funcs[fi].body;
  for instr in block.into_iter().rev() {
    body.insert(0, instr);
  }
}

fn synth_host_wrapper(
  lcx: &mut LowerCtx<'_>,
  fink_module: FuncSym,
) {
  let canonical_url = lcx.fqn_prefix.trim_end_matches(':').to_string();
  assert!(
    !canonical_url.is_empty(),
    "synth_host_wrapper: fqn_prefix must be non-empty — every \
     fragment needs a real FQN (canonical url for package compiles, \
     `test:` for tests, `repl:` for REPL). The wrapper is exported \
     under canonical FQN so the host can address it.",
  );
  let display = format!("{canonical_url}::host_wrapper");

  // Host-friendly signature: `(cont: anyref) -> ()`. Cont is a fink
  // continuation (`$Closure` over `$Fn3`); init_module fires it with
  // `(last_expr, exports_rec)`. Hosts that want a specific named
  // export do their own lookup against the exports rec via
  // `interop/rust.wat:rec_get_by_bytes`. Host-bridge mechanics
  // (e.g. the Rust runner's i32-cont-id table) live host-side. Sig
  // is declared locally per fragment — same approach as every other
  // lowered fink function (no shared nominal type at this boundary).
  let anyref_n = val_anyref(true);
  let sig = ty_func(
    lcx.frag,
    vec![anyref_n.clone()],
    vec![],
    &format!("{canonical_url}::Fn_host_wrapper"),
  );

  let mut ctx = FnCtx::new(HashMap::new());
  let l_cont_p = ctx.alloc_param(":cont");

  // Build no-capture Fn3-typed closure over fink_module funcref.
  let l_caps_arg = ctx.alloc_local_typed(
    ":caps_arg",
    val_ref(lcx.rt.captures(), /*nullable*/ true),
  );
  let i_caps_null = push_ref_null_concrete(lcx.frag, lcx.rt.captures(), l_caps_arg);
  ctx.instrs.push(i_caps_null);

  let l_mod_clos = ctx.alloc_local(":mod_clos");
  let i_clos = push_struct_new(
    lcx.frag, lcx.rt.closure(),
    vec![Operand::RefFunc(fink_module), op_local(l_caps_arg)],
    l_mod_clos,
  );
  ctx.instrs.push(i_clos);

  // URL constant — the registry key.
  let l_url = emit_str_const(lcx, &mut ctx, canonical_url.as_bytes(), ":url");

  // Tail-call init_module(url, mod_clos, cont). init_module runs the
  // module body (Fn3 — apply shim threads placeholder ctx internally),
  // populates the registry via the body's `pub` calls, then fires
  // cont with `(last_expr, exports_rec)` via apply_3_2_nullable → apply_3.
  let i_init = push_return_call(lcx.frag, lcx.rt.modules_init_module(),
    vec![op_local(l_url), op_local(l_mod_clos), op_local(l_cont_p)]);
  ctx.instrs.push(i_init);

  let sym = func(lcx.frag, sig, ctx.params, ctx.locals, ctx.instrs, &display);
  let FuncSym::Local(i) = sym else { panic!("synth_host_wrapper: func must be Local") };
  lcx.frag.funcs[i as usize].export = Some(canonical_url);
}

// ──────────────────────────────────────────────────────────────────
// Lowering context
// ──────────────────────────────────────────────────────────────────

/// Module-scoped context threaded through the lowering walker.
///
/// Bundles the read-mostly references that every helper in this file
/// otherwise has to take as separate args. `frag` is mutable; the
/// other fields are immutable references valid for the whole lower
/// pass.
///
/// `FnCtx` (the per-function state — locals, instrs, binds) stays
/// separate. The two are passed alongside each other to walker
/// helpers as `(&mut LowerCtx, &mut FnCtx, ...)`.
struct LowerCtx<'a> {
  cps: &'a CpsResult,
  ast: &'a Ast<'a>,
  rt: &'a Runtime,
  frag: &'a mut Fragment,
  /// Module-level exports: CpsId → (pre-allocated GlobalSym, source
  /// binding name). Read by the `Pub` arm in `lower_expr` (to emit
  /// `global.set` and the `std/modules.fnk:pub` registry call) and
  /// by `resolve_id_as_operand` / `emit_val_into` to materialise a
  /// `global.get` operand for cross-fn references to a pub'd
  /// binding. Built once in `lower()` before any FnCtx is created.
  pub_globals: &'a HashMap<CpsId, (GlobalSym, String)>,
  /// FQN prefix for emitted symbol display names. Empty for single-
  /// fragment compiles; `"<canonical_url>:"` for multi-fragment
  /// package compiles. See `lower()` doc.
  fqn_prefix: &'a str,
  /// Bind-kind lookup. Populated once per `to_fragment`. Used to give
  /// special bind kinds (e.g. `Bind::Ctx`) descriptive local names.
  bind_kinds: &'a crate::propgraph::PropGraph<CpsId, Option<crate::passes::cps::ir::Bind>>,
  /// LetRec slot CpsIds. Reads of a slot id auto-unwrap via
  /// `struct.get $Cell.value`; Set stores via `struct.set`. Captures
  /// that close over a slot pass the `(ref $Cell)` itself, so the
  /// captured local stays in `slot_ids` (via convert's Bind::Slot
  /// marker) and the same access path applies inside the lifted
  /// body.
  slot_ids: &'a std::collections::HashSet<CpsId>,
  /// Per-slot wasm global storage. Every module-level LetRec slot
  /// gets one — pub'd slots share their GlobalSym with `pub_globals`;
  /// non-pub'd slots get a synth-named global allocated here.
  slot_globals: &'a HashMap<CpsId, GlobalSym>,
}

// ──────────────────────────────────────────────────────────────────
// Function lowering
// ──────────────────────────────────────────────────────────────────

/// Lower a CPS function — either the module body or a LetFn'd helper.
///
/// Trace identity for a lowered function. `entry = Some(id)` means this
/// is a real userland function entry: push a frame for `id` at the
/// prologue and pops inside it target `id`. `entry = None` means a lifted
/// continuation (CpsClosure) - no push; its ret-cont pops target the
/// *enclosing* userland fn in `enclosing`.
#[derive(Clone, Copy, Default)]
struct TraceFrame {
  entry: Option<CpsId>,
  enclosing: Option<CpsId>,
}

impl TraceFrame {
  /// A real userland function entry, traced under its own id.
  fn entry(id: CpsId) -> Self { Self { entry: Some(id), enclosing: None } }
  /// A lifted continuation: no frame; ret-cont pops target `enclosing`.
  fn cont(enclosing: Option<CpsId>) -> Self { Self { entry: None, enclosing } }
}

/// `cap_params` and `user_params` are the CpsIds of the function's
/// cap-params (read from `$_caps` via `array.get`) and user-params
/// (unpacked from `$_args` via successive `args_head`/`args_tail`).
///
/// Returns the `FuncSym` of the emitted function.
fn lower_fn(
  lcx: &mut LowerCtx<'_>,
  cap_params: &[CpsId],
  user_params: &[(CpsId, bool)],
  body: &Expr,
  display: &str,
  fn_syms: &HashMap<CpsId, FuncSym>,
  trace: TraceFrame,
) -> FuncSym {
  // CRITICAL: `fn_syms` is cloned (not shared) into the child FnCtx.
  // Sibling and ancestor LetFn FuncSyms must be visible to this fn's
  // body, but mutations *inside* this body must not leak back to the
  // parent. Sharing the map here would make child-of-child helpers
  // visible to subsequent siblings of the parent — observable through
  // resolution order in `App(FnClosure)`. Keep the clone literal.
  let mut ctx = FnCtx::new(fn_syms.clone());

  // WASM-level params: `$:caps_param`, `$:ctx_param`, `$:params` —
  // the $Fn3 shape. Ctx is a native wasm value, NOT peeled from the
  // args list. The first user_param whose Bind::Ctx is bound directly
  // to the ctx native param; all other user_params are unpacked from
  // $:params via head/tail as in the Fn3 shape. Colon-prefix is
  // lexer-rejected in Fink source, so these synth names cannot
  // collide with user bindings.
  let l_caps_p = ctx.alloc_param(":caps_param");
  let l_ctx_p = ctx.alloc_param(":ctx_param");
  let l_args_p = ctx.alloc_param(":params");
  ctx.ctx_local = Some(l_ctx_p);

  // Bind the `Bind::Caps` param's CpsId to the native `:caps_param`
  // wasm slot. The lifted body's own `LetCaps` arm destructures the
  // record into local CpsIds — this prologue does NOT pre-unpack
  // captures.
  for &cap_id in cap_params {
    ctx.bind(cap_id, l_caps_p);
  }

  // Bind any Bind::Ctx user_param directly to the native $:ctx_param
  // wasm slot — no head/tail peel from the args list. All other
  // user_params come out of $:params via the standard Fn3-style
  // head/tail walk. There is at most one Bind::Ctx in user_params
  // (slice-2a invariant), and by convention it is the 0th entry.
  use crate::passes::cps::ir::Bind;
  let non_ctx_params: Vec<(CpsId, bool)> = user_params.iter()
    .filter_map(|&(pid, is_spread)| {
      let kind = lcx.bind_kinds.try_get(pid).and_then(|o| *o);
      if matches!(kind, Some(Bind::Ctx)) {
        // Bind ctx CpsId directly to the native ctx param wasm local.
        // Param keeps its synth name `:ctx_param` in the WAT for now;
        // a future pass can rename for display fidelity if needed.
        ctx.bind(pid, l_ctx_p);
        None
      } else {
        Some((pid, is_spread))
      }
    })
    .collect();

  // Unpack non-ctx user params from $:params by walking `args_head`
  // / `args_tail`. Same shape as the Fn3 lowering.
  let n = non_ctx_params.len();
  for (j, &(pid, is_spread)) in non_ctx_params.iter().enumerate() {
    let name = cps_ident_kinded(lcx.cps, lcx.ast, lcx.bind_kinds, pid);
    let local = ctx.alloc_local(&name);
    ctx.bind(pid, local);
    if is_spread {
      let i = push_local_set(lcx.frag, local, op_local(l_args_p));
      ctx.instrs.push(i);
    } else {
      let i = push_call(lcx.frag, lcx.rt.args_head(), vec![op_local(l_args_p)], Some(local));
      ctx.instrs.push(i);
      if j + 1 < n {
        let i = push_call(lcx.frag, lcx.rt.args_tail(), vec![op_local(l_args_p)], Some(l_args_p));
        ctx.instrs.push(i);
      }
    }
  }

  // Trace: a real userland function entry pushes an activation frame
  // stamped with its own identity; pops inside it (when it invokes its
  // return cont) target that frame. A lifted continuation (CpsClosure)
  // does NOT push - it is part of the enclosing function - but its
  // ret-cont invocations must pop the enclosing function's frame, so it
  // inherits the enclosing fn's id for pop targeting.
  ctx.trace_fn_id = trace.entry.or(trace.enclosing);
  if let Some(fn_id) = trace.entry {
    let mid = lit_i32(lcx.frag.module_id.0 as i32);
    let cid = lit_i32(fn_id.0 as i32);
    let i = push_call(lcx.frag, lcx.rt.trace_push(), vec![mid, cid], None);
    ctx.instrs.push(i);
  }

  // Walk the body.
  lower_expr(lcx, &mut ctx, body);

  // Build the function. Sig is the runtime-imported `$Fn3` type so
  // every Fn3-shaped funcref structurally equates to the same nominal
  // type at apply-3-time (the cast wouldn't trap across compile units
  // with locally-declared duplicates, but importing keeps the rendered
  // WAT closer to the runtime ABI).
  func(lcx.frag, lcx.rt.fn3(),
    ctx.params,
    ctx.locals,
    ctx.instrs,
    display,
  )
}

/// Per-function lowering context. Owns locals, their bindings (CpsId →
/// LocalIdx), and the accumulated instruction list.
struct FnCtx {
  params: Vec<LocalDecl>,
  locals: Vec<LocalDecl>,
  instrs: Vec<InstrId>,
  /// Map from CPS bind id → local index.
  binds: HashMap<CpsId, LocalIdx>,
  /// Next local index (params + locals, in WASM local-numbering order).
  next_local_idx: u32,
  /// LetFn bind id → emitted FuncSym. Populated by `LetFn` whenever a
  /// nested fn is lowered; read by `FnClosure` when constructing
  /// `struct.new $Closure (ref.func ...)` and by `emit_val_into` when
  /// a `LetVal(name, Ref(fn_id))` materialises a no-capture closure.
  ///
  /// Inherited from the parent FnCtx (snapshot at child-lower time) so
  /// child fn bodies can resolve cross-fn references to siblings or
  /// ancestors. The lifting pass produces a strict pre-order over
  /// LetFns, so by the time a child is lowered, all enclosing /
  /// preceding-sibling LetFn FuncSyms are already known.
  fn_syms: HashMap<CpsId, FuncSym>,
  /// Native ctx wasm param of this fn (Fn3 calling convention).
  /// Set by `lower_fn` after allocating `:ctx_param`. Used by
  /// `lower_cont` to bind any Bind::Ctx in a Cont::Expr.args list
  /// directly to this slot — ctx is invariant within a fn body, so
  /// every "fresh" ctx CpsId at thread_ctx time aliases to the same
  /// wasm value at runtime.
  ctx_local: Option<LocalIdx>,
  /// This function's own identity for trace instrumentation: the
  /// defining LetFn's CpsId. `Some` for userland source functions (which
  /// push/pop an activation frame); `None` for synth/runtime functions
  /// (module body, host wrapper) which are not traced. Read by the
  /// ret-cont arm to emit `trace_pop(module_id, fn_id)`.
  trace_fn_id: Option<CpsId>,
}

impl FnCtx {
  fn new(fn_syms: HashMap<CpsId, FuncSym>) -> Self {
    Self {
      params: Vec::new(),
      locals: Vec::new(),
      instrs: Vec::new(),
      binds: HashMap::new(),
      next_local_idx: 0,
      fn_syms,
      ctx_local: None,
      trace_fn_id: None,
    }
  }

  fn alloc_param(&mut self, name: &str) -> LocalIdx {
    self.alloc_param_typed(name, val_anyref(true))
  }

  fn alloc_param_typed(&mut self, name: &str, ty: ValType) -> LocalIdx {
    let idx = LocalIdx(self.next_local_idx);
    self.next_local_idx += 1;
    self.params.push(local(ty, name));
    idx
  }

  fn alloc_local(&mut self, name: &str) -> LocalIdx {
    self.alloc_local_typed(name, val_anyref(true))
  }

  /// Allocate a local with a specific value type. Use for synth
  /// scratch locals whose role is a narrower concrete type than
  /// anyref (e.g. `$:caps_cast` and `$:caps_arg` are nullable refs
  /// to `$Captures`). Produces correctly-typed locals so validation
  /// doesn't reject `struct.new`/`array.get` field/element types.
  fn alloc_local_typed(&mut self, name: &str, ty: ValType) -> LocalIdx {
    let idx = LocalIdx(self.next_local_idx);
    self.next_local_idx += 1;
    self.locals.push(local(ty, name));
    idx
  }

  fn bind(&mut self, id: CpsId, idx: LocalIdx) {
    self.binds.insert(id, idx);
  }

  fn lookup(&self, id: CpsId) -> LocalIdx {
    *self.binds.get(&id)
      .unwrap_or_else(|| panic!("lower: unbound CpsId {:?}", id))
  }
}

/// Read a slot's `(ref $Cell)` storage. For module slots: `global.get`
/// the slot's pub global. For captured slots (Bind::Slot in scope):
/// `local.get` the captured Cell ref. Panics for other shapes — they
/// aren't supported yet.
fn slot_cell_ref(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  slot_id: CpsId,
) -> Operand {
  // Captured slot inside a lifted fn: bound to a local that holds the
  // Cell ref directly (no struct.get on the read of the local itself).
  if let Some(local) = ctx.binds.get(&slot_id).copied() {
    return op_local(local);
  }
  if let Some(gsym) = lcx.slot_globals.get(&slot_id) {
    return op_global(*gsym);
  }
  panic!("lower: slot {:?} has no Cell storage (not a slot global, not a captured local)", slot_id);
}

/// Resolve a CpsId to an `Operand`. Three cases, in order:
/// 1. Locally bound — `local.get` of the bound local.
/// 2. Pub'd module-export — `global.get` of the export global.
/// 3. Top-level fn id — materialise a fresh no-capture `$Closure`
///    in a new local and return that local.
/// 4. Otherwise: panic via `ctx.lookup` (preserves diagnostic).
fn resolve_id_as_operand(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  id: CpsId,
) -> Operand {
  // Slot id: read the Cell ref via slot_cell_ref, then auto-unwrap the
  // boxed value via `struct.get $Cell $value`. This makes value-position
  // refs to a slot Just Work — the caller doesn't need to know whether
  // it's reading a regular local or a slot.
  if lcx.slot_ids.contains(&id) {
    let cell_op = slot_cell_ref(lcx, ctx, id);
    let unwrap_local = ctx.alloc_local(&format!(":unwrap_{}", cps_ident(lcx.cps, lcx.ast, id)));
    let i = push_struct_get(lcx.frag, lcx.rt.cell(), 0, cell_op, unwrap_local);
    ctx.instrs.push(i);
    return op_local(unwrap_local);
  }
  if let Some(local) = ctx.binds.get(&id).copied() {
    return op_local(local);
  }
  if let Some(&(gsym, _)) = lcx.pub_globals.get(&id) {
    return op_global(gsym);
  }
  if let Some(fn_sym) = ctx.try_lookup_fn_sym(id) {
    let local = ctx.alloc_local(&format!(":fn_{}", id.0));
    let caps_local = ctx.alloc_local_typed(
      ":caps_arg",
      val_ref(lcx.rt.captures(), /*nullable*/ true),
    );
    let i_caps = push_ref_null_concrete(lcx.frag, lcx.rt.captures(), caps_local);
    ctx.instrs.push(i_caps);
    let i_clo = push_struct_new(
      lcx.frag, lcx.rt.closure(),
      vec![Operand::RefFunc(fn_sym), op_local(caps_local)],
      local,
    );
    ctx.instrs.push(i_clo);
    return op_local(local);
  }
  op_local(ctx.lookup(id))  // panics with `unbound CpsId` diagnostic
}

// ──────────────────────────────────────────────────────────────────
// Expression walker
// ──────────────────────────────────────────────────────────────────

fn lower_expr(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  expr: &Expr,
) {
  match &expr.kind {
    ExprKind::LetVal { name, val, cont } => {
      // LetVal binding a LetRec slot id (e.g. destructure success
      // cont's `let v_15, fn a_0:` where a_0 is a module slot) writes
      // through the cell instead of allocating a fresh local. Reads
      // outside the success arm go via the cell's storage.
      if lcx.slot_ids.contains(&name.id) {
        let cell_op = slot_cell_ref(lcx, ctx, name.id);
        // Box the val into a scratch local if needed, then struct.set
        // on the cell.
        let val_op = match &val.kind {
          ValKind::Lit(lit) => {
            let lv = LitVal::from_lit(lit)
              .unwrap_or_else(|| panic!("lower: unsupported lit {:?}", lit));
            let local = ctx.alloc_local(&cps_ident_for_bind(lcx.cps, lcx.ast, name));
            let i = box_lit(lcx.frag, lcx.rt, &lv, local);
            if let Some(o) = origin_of(lcx.cps, lcx.ast, name.id) { set_origin(lcx.frag, i, o); }
            ctx.instrs.push(i);
            op_local(local)
          }
          _ => val_as_operand(lcx, ctx, val),
        };
        let i_set = push_struct_set(lcx.frag, lcx.rt.cell(), 0, cell_op, val_op);
        set_cps_id(lcx.frag, i_set, expr.id);
        ctx.instrs.push(i_set);
        lower_cont(lcx, ctx, cont);
        return;
      }
      let local = ctx.alloc_local(&cps_ident_for_bind(lcx.cps, lcx.ast, name));
      ctx.bind(name.id, local);
      let i = emit_val_into(lcx, ctx, val, local);
      if let Some(o) = origin_of(lcx.cps, lcx.ast, name.id) { set_origin(lcx.frag, i, o); }
      // Tag the LetVal expr's own id (analyse rule 1 marks any CpsId with
      // a Bind/Apply/etc origin — for a `name = val` shape that's the
      // expr.id, not name.id). Falls back to name.id if expr.id has no
      // origin or its origin AST is not a stop kind.
      set_cps_id(lcx.frag, i, expr.id);
      ctx.instrs.push(i);
      lower_cont(lcx, ctx, cont);
    }

    ExprKind::LetFn { name, params, fn_body, cont, fn_kind, .. } => {
      // Collect cap + user params by role. The `Bind::Caps` param is
      // the ƒcaps record (post closure-conversion); everything else is
      // a user param. User params carry their spread flag so the
      // prologue can emit the right `args_head`/`args_tail`/spread
      // sequence.
      let mut cap_ids: Vec<CpsId> = Vec::new();
      let mut user_ids: Vec<(CpsId, bool)> = Vec::new();
      for p in params {
        let (bind, is_spread) = match p {
          Param::Name(b)   => (b, false),
          Param::Spread(b) => (b, true),
        };
        if matches!(bind.kind, Bind::Caps) {
          cap_ids.push(bind.id);
        } else {
          user_ids.push((bind.id, is_spread));
        }
      }
      // Lift the fn body to a separate Fn3. Display name carries the
      // module's FQN prefix so cross-fragment merges stay collision-free.
      let raw_display = cps_ident_for_bind(lcx.cps, lcx.ast, name);
      let display = format!("{}{}", lcx.fqn_prefix, raw_display);
      // A source-level CpsFunction is a real userland fn (push its own
      // frame); a CpsClosure is a lifted continuation (no push; its
      // ret-cont pops the enclosing fn, inherited via ctx.trace_fn_id).
      // Synth CpsFunctions (e.g. the m_0/mp_N match-block wrappers)
      // inherit too — they are not source-level activations.
      use crate::passes::cps::ir::CpsFnKind;
      let trace = match fn_kind {
        CpsFnKind::CpsFunction if is_source_fn(lcx.cps, lcx.ast, name.id) =>
          TraceFrame::entry(name.id),
        _ => TraceFrame::cont(ctx.trace_fn_id),
      };
      let fn_sym = lower_fn(
        lcx,
        &cap_ids, &user_ids, fn_body, &display,
        &ctx.fn_syms,
        trace,
      );
      // The LetFn binds `name.id` to a funcref-valued local; we model
      // it as an anyref local holding a `ref.func` funcref. Actual
      // `ref.func` emission happens at the LetVal(FnClosure) site
      // where this funcref is used as the first arg. So we just
      // remember the FuncSym here.
      ctx.fn_sym_for_bind(name.id, fn_sym);
      lower_cont(lcx, ctx, cont);
    }

    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Pub), args } => {
      // `·ƒpub ctx, val, cont` — register `val` as a module-level export.
      // After thread_ctx, args[0] is ctx (currently ignored by Pub's
      // side-effects but threaded through uniformly per the design).
      //
      // Two side effects, both inline (no CPS hop):
      //   1. `global.set $<fqn>:<name> val` — addressable storage.
      //   2. `call $std/modules.fnk:pub (<fqn>, <name>, val)` — registers
      //      the binding into the module's exports rec in the runtime
      //      registry, where `import` will read it from.
      //
      // Then descend into the cont body inline.
      //
      // The fqn url is `lcx.fqn_prefix` minus the trailing `:` separator;
      // the source name comes from `pub_globals` alongside the global.
      // Peel the ctx arg (currently unused by Pub).
      let Some(Arg::Val(_ctx_val)) = args.first() else {
        panic!("lower: Pub expects [ctx, val, cont], missing ctx");
      };
      let Some(Arg::Val(val)) = args.get(1) else {
        panic!("lower: Pub expects [ctx, val, cont], missing val");
      };
      let id = cps_id_of_ref(val);
      let (gsym, src_name) = lcx.pub_globals.get(&id)
        .cloned()
        .unwrap_or_else(|| panic!("lower: Pub val CpsId {:?} has no pre-allocated global", id));

      // Unwrap the slot's Cell: `global.get` the slot → `struct.get
      // $Cell $value` to read the boxed value that the preceding Set
      // wrote in. The registry's `pub` takes the value as an anyref,
      // not the Cell ref.
      let val_local = ctx.alloc_local(&format!(":pub_{}", cps_ident(lcx.cps, lcx.ast, id)));
      let i_get = push_struct_get(lcx.frag, lcx.rt.cell(), 0, op_global(gsym), val_local);
      ctx.instrs.push(i_get);
      let url_bytes: Vec<u8> = lcx.fqn_prefix.trim_end_matches(':').as_bytes().to_vec();
      let url_local = emit_str_const(lcx, ctx, &url_bytes, ":pub_url");
      // The export name is a field key, so register it as a $Symbol -- the
      // exports rec is symbol-keyed, matching the consumer's `{x} = import`
      // RecPop and the host's name->symbol-resolved lookup.
      let name_local = ctx.alloc_local(&format!(":pub_name_{}", cps_ident(lcx.cps, lcx.ast, id)));
      let i_name = box_symbol(lcx.frag, lcx.rt, src_name.as_bytes(), name_local);
      ctx.instrs.push(i_name);
      let i_pub = push_call(lcx.frag, lcx.rt.modules_pub(),
        vec![op_local(url_local), op_local(name_local), op_local(val_local)],
        None);
      ctx.instrs.push(i_pub);

      let cont_arg = args.get(2)
        .unwrap_or_else(|| panic!("lower: Pub expects [ctx, val, cont]"));
      let Arg::Cont(cont) = cont_arg else {
        panic!("lower: Pub cont arg is not a Cont");
      };
      lower_cont(lcx, ctx, cont);
    }

    // `op_dot` (Get) is a binary op whose RHS is a record key: a `Lit::Symbol`
    // field key boxes to $Symbol, other keys lower as values. Handle before the
    // generic binary path, which would lower the key via the value path.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Get), args } => {
      let (ctx_a, a, key, cont) = split_binary_args(args);
      let a_op = emit_arg_as_operand(lcx, ctx, a);
      let key_op = emit_key_as_operand(lcx, ctx, key);
      emit_op_tail_call(lcx, ctx, Sym::OpDot, ctx_a, vec![a_op, key_op], cont, expr.id);
    }

    ExprKind::App { func: Callable::BuiltIn(b), args } if binary_op_sym(*b).is_some() => {
      let sym = binary_op_sym(*b).unwrap();
      let (ctx_a, a, b_v, cont) = split_binary_args(args);
      let a_op = emit_arg_as_operand(lcx, ctx, a);
      let b_op = emit_arg_as_operand(lcx, ctx, b_v);
      emit_op_tail_call(lcx, ctx, sym, ctx_a, vec![a_op, b_op], cont, expr.id);
    }

    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Not), args } => {
      let (ctx_a, v, cont) = split_unary_args(args);
      let v_op = emit_arg_as_operand(lcx, ctx, v);
      emit_op_tail_call(lcx, ctx, Sym::OpNot, ctx_a, vec![v_op], cont, expr.id);
    }

    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Empty), args } => {
      let (ctx_a, v, cont) = split_unary_args(args);
      let v_op = emit_arg_as_operand(lcx, ctx, v);
      emit_op_tail_call(lcx, ctx, Sym::OpEmpty, ctx_a, vec![v_op], cont, expr.id);
    }

    ExprKind::App { func: Callable::BuiltIn(BuiltIn::RangeFrom), args } => {
      let (ctx_a, v, cont) = split_unary_args(args);
      let v_op = emit_arg_as_operand(lcx, ctx, v);
      emit_op_tail_call(lcx, ctx, Sym::OpRngFrom, ctx_a, vec![v_op], cont, expr.id);
    }

// StrMatch: `(ctx, subj, prefix, suffix, fail, succ)` — 6-arg
    // template pattern dispatch. After thread_ctx, ctx is args[0].
    // All six are anyref operands at the WASM level (the latter two
    // are continuations resolved as closures).
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::StrMatch), args } => {
      if args.len() != 6 {
        panic!("lower: StrMatch expects 6 args (ctx + 5), got {}", args.len());
      }
      let ops: Vec<Operand> = args.iter().map(|a| emit_arg_as_operand(lcx, ctx, a)).collect();
      let i = push_return_call(lcx.frag, lcx.rt.str_match(), ops);
      if let Some(o) = origin_of(lcx.cps, lcx.ast, expr.id) { set_origin(lcx.frag, i, o); }
      set_cps_id(lcx.frag, i, expr.id);
      ctx.instrs.push(i);
    }

    // StrFmt: `(ctx, seg_0, seg_1, ..., seg_n, cont)` — build a
    // $VarArgs array from the segments and tail-call
    // $str_fmt(ctx, varargs, cont). After cont_lift the cont is an
    // Arg::Val (Ref to a lifted fn); the legacy shape passed it as
    // Arg::Cont.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::StrFmt), args } => {
      // args[0] is ctx, last is cont, segments are everything else.
      let ctx_a = args.first().expect("StrFmt: missing ctx");
      let cont_arg = args.last().expect("StrFmt: missing cont");
      let segments = &args[1..args.len() - 1];
      let seg_ops: Vec<Operand> = segments.iter()
        .map(|a| emit_arg_as_operand(lcx, ctx, a))
        .collect();
      let varargs_local = ctx.alloc_local_typed(":varargs",
        val_ref(lcx.rt.varargs(), /*nullable*/ true));
      let i_arr = push_array_new_fixed(lcx.frag, lcx.rt.varargs(), seg_ops, varargs_local);
      ctx.instrs.push(i_arr);
      emit_op_tail_call(lcx, ctx,
        Sym::StrFmt, ctx_a, vec![op_local(varargs_local)], cont_arg,
        expr.id);
    }

    // SeqPrepend: `(item, seq, cont)` — same call shape as a binary
    // protocol op. Lowers to `return_call $seq_prepend item seq cont`.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::SeqPrepend), args } => {
      let (ctx_a, a, b, cont) = split_binary_args(args);
      let a_op = emit_arg_as_operand(lcx, ctx, a);
      let b_op = emit_arg_as_operand(lcx, ctx, b);
      emit_op_tail_call(lcx, ctx, Sym::SeqPrepend, ctx_a, vec![a_op, b_op], cont, expr.id);
    }

    // NewType: `(ctx, cont)` — no value args. The type-seed constructors take
    // the introspection key (module_id, cps_id) as two i32 constants, supplied
    // here at emit time (like trace_push), NOT as CPS value args. Signature
    // `(ctx, mid i32, cid i32, cont) -> ()`; tail-applies cont with the value.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::NewType), args } => {
      emit_type_seed(lcx, ctx, lcx.rt.new_type(), args, expr.id);
    }

    // TypeSetField: `(ctx, type, key, val, cont)` — same shape as RecPut, with
    // the field-name key at index 2; route via emit_rec_key_op so a $Symbol key
    // boxes correctly.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::TypeSetField), args } => {
      let target = lcx.rt.type_set_field();
      emit_rec_key_op(lcx, ctx, target, args, expr.id);
    }

    // TypePush: `(ctx, type, val, cont)` — append a positional (tuple) field.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::TypePush), args } => {
      let target = lcx.rt.type_push();
      emit_direct_op_call(lcx, ctx, target, args, expr.id);
    }

    // TypeSetNew: `(ctx, type, builder, cont)` — record the type-constructor on a
    // type (mark it generic). Same 2-value + cont shape as TypePush.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::TypeSetNew), args } => {
      let target = lcx.rt.type_set_new();
      emit_direct_op_call(lcx, ctx, target, args, expr.id);
    }

    // TypeInherit: `(ctx, type, base, cont)` — record a `..Base` supertype link.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::TypeInherit), args } => {
      let target = lcx.rt.type_inherit();
      emit_direct_op_call(lcx, ctx, target, args, expr.id);
    }

    // NewUnion: `(ctx, cont)` — mints a `$Union`. Same i32-key seed as NewType.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::NewUnion), args } => {
      emit_type_seed(lcx, ctx, lcx.rt.new_union(), args, expr.id);
    }

    // UnionAdd: `(ctx, union, member, cont)` — add a member type-ref.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::UnionAdd), args } => {
      let target = lcx.rt.union_add();
      emit_direct_op_call(lcx, ctx, target, args, expr.id);
    }

    // NewEnum: `(ctx, cont)` — mints a `$Enum`. Same i32-key seed as NewType.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::NewEnum), args } => {
      emit_type_seed(lcx, ctx, lcx.rt.new_enum(), args, expr.id);
    }

    // EnumAdd: `(ctx, enum, name, member, cont)` — the case name at index 2 is
    // a $Symbol; route via emit_rec_key_op like TypeSetField.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::EnumAdd), args } => {
      let target = lcx.rt.enum_add();
      emit_rec_key_op(lcx, ctx, target, args, expr.id);
    }

    // NewFnType: `(ctx, name, cont)` — mints a `$FnType`. Same name-symbol seed
    // as NewType.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::NewFnType), args } => {
      emit_type_seed(lcx, ctx, lcx.rt.new_fn_type(), args, expr.id);
    }

    // FnTypeParam: `(ctx, fntype, param_type, cont)` — cons-prepend an arg type.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::FnTypeParam), args } => {
      let target = lcx.rt.fn_type_param();
      emit_direct_op_call(lcx, ctx, target, args, expr.id);
    }

    // FnTypeResult: `(ctx, fntype, result_type, cont)` — set the result type.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::FnTypeResult), args } => {
      let target = lcx.rt.fn_type_result();
      emit_direct_op_call(lcx, ctx, target, args, expr.id);
    }

    // SeqConcat: `(a, b, cont)` — same call shape as SeqPrepend. Used
    // for list literals containing a spread (`[..xs, y]`, `[..a, ..b]`).
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::SeqConcat), args } => {
      let (ctx_a, a, b, cont) = split_binary_args(args);
      let a_op = emit_arg_as_operand(lcx, ctx, a);
      let b_op = emit_arg_as_operand(lcx, ctx, b);
      emit_op_tail_call(lcx, ctx, Sym::SeqConcat, ctx_a, vec![a_op, b_op], cont, expr.id);
    }

    // RecMerge: `(dest, src, cont)` — same shape as SeqPrepend.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::RecMerge), args } => {
      let (ctx_a, a, b, cont) = split_binary_args(args);
      let a_op = emit_arg_as_operand(lcx, ctx, a);
      let b_op = emit_arg_as_operand(lcx, ctx, b);
      emit_op_tail_call(lcx, ctx, Sym::RecMerge, ctx_a, vec![a_op, b_op], cont, expr.id);
    }

    // IsSeqLike / IsRecLike: `(val, succ, fail)` — type guard. The
    // succ/fail args are continuations (Cont::Ref or Cont::Expr).
    // We resolve each as an operand (cont = value-like at WASM level)
    // and emit `return_call $<sym> val succ fail`.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::IsSeqLike), args } => {
      emit_ternary_guard(lcx, ctx, Sym::IsSeqLike, args, expr.id);
    }
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::IsRecLike), args } => {
      emit_ternary_guard(lcx, ctx, Sym::IsRecLike, args, expr.id);
    }
    // GuardApply: `(ctx, head, val, succ, fail)` — unified pattern guard.
    // The head is a guard VALUE (a type, a structural protocol type, or a
    // predicate fn). All heads flow uniformly through the runtime $guard_apply,
    // which dispatches on the head. The rec/tuple protocols are materialised as
    // ordinary type values (RecProtocol/TupleProtocol -> their getter funcs in
    // val_as_operand), so there is no structural special-case here.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::GuardApply), args } => {
      let target = lcx.rt.guard_apply();
      emit_direct_op_call(lcx, ctx, target, args, expr.id);
    }
    // SeqPop: `(seq, fail, succ)` — destructure. Both fail and succ
    // are continuations.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::SeqPop), args } => {
      emit_ternary_guard(lcx, ctx, Sym::SeqPop, args, expr.id);
    }
    // SeqPopBack: `(seq, fail, succ)` — destructure from end. Same shape
    // as SeqPop; succ receives `(init, last)` instead of `(head, tail)`.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::SeqPopBack), args } => {
      emit_ternary_guard(lcx, ctx, Sym::SeqPopBack, args, expr.id);
    }
    // RecPut: `(rec, key, val, cont)` — record extension.
    // RecPop: `(rec, key, fail, succ)` — record destructure.
    // Both 4-arg, lowered as `return_call $sym arg0 arg1 arg2 arg3`.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::RecPut), args } => {
      let target = lcx.rt.rec_put();
      emit_rec_key_op(lcx, ctx, target, args, expr.id);
    }
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::RecPop), args } => {
      let target = lcx.rt.rec_pop();
      emit_rec_key_op(lcx, ctx, target, args, expr.id);
    }

    // Panic: tail-position sentinel emitted by lower_match's fail
    // chain and wrap_with_fail's irrefutable-bind path. Calls into the
    // runtime `$panic(reason: i32)` so the host can render a
    // user-facing message; the trailing `unreachable` keeps wasm's
    // validator happy when the function expects a return.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Panic(reason)), .. } => {
      let i = push_call(lcx.frag, lcx.rt.panic(),
        vec![Operand::I32(reason.wire())], None);
      // Tag the panic call with its CpsId so trap::diagnose can resolve a
      // MarkRecord at this PC -- without it the panic has no mark and the
      // trap location falls back to the entry module's line 1.
      set_cps_id(lcx.frag, i, expr.id);
      ctx.instrs.push(i);
      let u = push_unreachable(lcx.frag);
      ctx.instrs.push(u);
    }

    // Module import — `{names..} = import 'url'`.
    //
    // Args: [Val(Lit::Str(url)), Cont(Ref(cont_id))]. The cont takes a
    // single rec value containing the imported names. We:
    //   1. Build the rec via `rec_new` + repeated `_rec_set_field`,
    //      where each value is the result of calling the runtime-side
    //      protocol dispatcher exported as `<url>:<name>`.
    //   2. Tail-apply the cont with `[rec]`.
    //
    // Phase 1 supports only virtual stdlib namespaces — paths starting
    // with `std/` and ending in `.fnk`. User-fragment imports (relative
    // paths, third-party packages) are deferred to multi-module work.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Import), args } => {
      lower_import(lcx, ctx, args);
    }

    // App(BuiltIn::FnClosure, [fn, caps..., cont]) — appears at the
    // call site where a lifted function is combined with its
    // captures. The cont is always Cont::Expr { args: [new_bind], ... }:
    // we allocate a local for new_bind, emit the struct.new $Closure,
    // and recurse into the cont body.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::FnClosure), args } => {
      // Last arg is the cont; earlier args are [fn, cap_0, cap_1, ...].
      let (cont, non_cont) = split_last_cont(args);
      // First non-cont arg is the lifted fn reference.
      let Some(Arg::Val(fn_val)) = non_cont.first() else {
        panic!("lower: FnClosure missing fn arg");
      };
      let fn_sym = ctx.lookup_fn_sym(cps_id_of_ref(fn_val));

      // Self-recursion: if the cont aliases the closure result to a
      // user-bind via `LetVal { val: Ref(bind), name: alias, ... }` AND
      // one of the captures references that alias's CpsId, this is a
      // self-recursive closure. Pre-allocate one local and bind BOTH
      // bind.id and alias.id to it. Then when cap_operands resolves the
      // self-capture CpsId, it gets the (currently-uninitialised) local;
      // the subsequent struct.new $Closure writes the closure value into
      // that local, and the capture slot holds the closure pointer
      // (whose first read is the local that just got the closure).
      // Detect self-recursive closure: cont aliases bind.id to alias_name.id
      // via a LetVal, and one of the captures references alias_name.id.
      // Returns (bind_id, alias_id, self_cap_idx).
      let self_alias_info: Option<(CpsId, CpsId, usize)> = match &cont {
        Cont::Expr { args: cont_args, body } => {
          if let Some(bind) = cont_args.first()
            && let ExprKind::LetVal { name: alias_name, val: alias_val, .. } = &body.kind
            && let ValKind::Ref(r) = &alias_val.kind
            && ref_cps_id(*r) == bind.id
          {
            non_cont[1..].iter().position(|a| match a {
              Arg::Val(v) => match &v.kind {
                ValKind::Ref(rr) => ref_cps_id(*rr) == alias_name.id,
                _ => false,
              },
              _ => false,
            }).map(|i| (bind.id, alias_name.id, i))
          } else {
            None
          }
        }
        _ => None,
      };

      let pre_bound_local: Option<LocalIdx> = self_alias_info.map(|(bind_id, alias_id, _)| {
        let local = ctx.alloc_local(&format!(":self_{}", bind_id.0));
        ctx.bind(bind_id, local);
        ctx.bind(alias_id, local);
        local
      });

      // Remaining non-cont args are the captures.
      let cap_operands: Vec<Operand> = non_cont[1..].iter()
        .map(|a| {
          let v = match a {
            Arg::Val(v) => v,
            _ => panic!("lower: FnClosure capture is not a Val"),
          };
          val_as_operand(lcx, ctx, v)
        })
        .collect();

      let self_cap_idx: Option<usize> = self_alias_info.map(|(_, _, i)| i);

      // FnClosure has two cont shapes:
      // * `Cont::Expr { args: [new_bind], body }` — the closure value
      //   is bound to a local in the parent scope and execution
      //   continues into `body`.
      // * `Cont::Ref(id)` — tail-apply the cont with the closure as a
      //   single arg (`apply_3([closure], cont_local)`).
      match cont {
        Cont::Expr { args: cont_args, body } => {
          let bind = cont_args.first().expect("FnClosure cont has no bind");
          let local = if let Some(l) = pre_bound_local {
            l
          } else {
            let l = ctx.alloc_local(&cps_ident_for_bind(lcx.cps, lcx.ast, bind));
            ctx.bind(bind.id, l);
            l
          };
          emit_closure_construction_inner(lcx, ctx, fn_sym, cap_operands, local, self_cap_idx);
          lower_expr(lcx, ctx, body);
        }
        Cont::Ref(cont_id) => {
          // Build the closure into a fresh anyref local first.
          let clo_local = ctx.alloc_local(&format!(":clo_{}", cont_id.0));
          emit_closure_construction(lcx, ctx, fn_sym, cap_operands, clo_local);

          // Resolve cont; spill if non-local.
          let callee_op = resolve_id_as_operand(lcx, ctx, *cont_id);
          let callee = match callee_op {
            Operand::Local(l) => l,
            other => {
              let local = ctx.alloc_local(&format!(":callee_{}", cont_id.0));
              let i = push_local_set(lcx.frag, local, other);
              ctx.instrs.push(i);
              local
            }
          };

          let l_args = ctx.alloc_local_typed(":args",
            val_ref_abs(AbsHeap::Any, /*nullable*/ false));
          let i_nil = push_call(lcx.frag, lcx.rt.args_empty(), vec![], Some(l_args));
          ctx.instrs.push(i_nil);
          let i_cons = push_call(lcx.frag, lcx.rt.args_prepend(),
            vec![op_local(clo_local), op_local(l_args)], Some(l_args));
          ctx.instrs.push(i_cons);
          let l_ctx = ctx.ctx_local.expect("FnClosure Cont::Ref tail: enclosing fn must have :ctx_param");
          let i_app = push_return_call(lcx.frag, lcx.rt.apply_3(),
            vec![op_local(l_args), op_local(l_ctx), op_local(callee)]);
          ctx.instrs.push(i_app);
        }
      }
    }

    // Apply-path: callable is a ContRef — tail-call the named cont
    // via the Fn3-aware `apply_3` runtime dispatcher.
    // Convention after thread_ctx: args[0] is the ctx Val, the rest
    // (if any) are the cont's result values. Pull ctx out as a native
    // wasm arg; the rest goes through the args list as before.
    ExprKind::App { func: Callable::Val(v), args }
      if matches!(v.kind, ValKind::ContRef(_)) =>
    {
      let cont_id = if let ValKind::ContRef(id) = &v.kind { *id } else { unreachable!() };
      let callee_op = resolve_id_as_operand(lcx, ctx, cont_id);
      let callee = match callee_op {
        Operand::Local(l) => l,
        other => {
          let local = ctx.alloc_local(&format!(":callee_{}", cont_id.0));
          let i = push_local_set(lcx.frag, local, other);
          ctx.instrs.push(i);
          local
        }
      };

      // Trace pop: invoking this fn's return cont is the fn returning -
      // pop its activation frame. Only for userland fns (trace_fn_id Some)
      // and only when the cont being applied is this fn's Ret-kind cont.
      if let Some(fn_id) = ctx.trace_fn_id
        && matches!(
          lcx.bind_kinds.try_get(cont_id).and_then(|b| *b),
          Some(Bind::Cont(crate::passes::cps::ir::ContKind::Ret)))
      {
        let mid = lit_i32(lcx.frag.module_id.0 as i32);
        let cid = lit_i32(fn_id.0 as i32);
        let i = push_call(lcx.frag, lcx.rt.trace_pop(), vec![mid, cid], None);
        ctx.instrs.push(i);
      }

      let (ctx_op, rest_args) = split_ctx_arg(lcx, ctx, args);
      let l_args_list = build_args_list(lcx, ctx, rest_args);
      let i_app = push_return_call(lcx.frag, lcx.rt.apply_3(),
        vec![op_local(l_args_list), ctx_op, op_local(callee)]);
      set_cps_id(lcx.frag, i_app, expr.id);
      ctx.instrs.push(i_app);
    }

    // Apply-path via a bound ref (e.g. a closure local). After
    // thread_ctx the CPS convention for user-fn calls is
    // `args[0] = ctx, args[1] = cont, args[2..] = values`. ctx is
    // pulled out as a native wasm arg; cont + values go through the
    // args list (cont-first) as before.
    ExprKind::App { func: Callable::Val(v), args } => {
      let callee_id = cps_id_of_ref(v);
      let callee_op = resolve_id_as_operand(lcx, ctx, callee_id);
      let callee = match callee_op {
        Operand::Local(l) => l,
        other => {
          let local = ctx.alloc_local(&format!(":callee_{}", callee_id.0));
          let i = push_local_set(lcx.frag, local, other);
          ctx.instrs.push(i);
          local
        }
      };

      let (ctx_op, rest_args) = split_ctx_arg(lcx, ctx, args);
      let l_args_list = build_args_list(lcx, ctx, rest_args);
      mark_call_site(lcx, ctx, expr.id);
      let i_app = push_return_call(lcx.frag, lcx.rt.apply_3(),
        vec![op_local(l_args_list), ctx_op, op_local(callee)]);
      set_cps_id(lcx.frag, i_app, expr.id);
      ctx.instrs.push(i_app);
    }

    // If: cond is a Val (bool — i31ref or literal). Unbox to i32 and
    // branch to one of two recursively-lowered bodies. Match arms
    // always end in a tail-call (`return_call`), so neither branch
    // falls through past the `If` in user code.
    ExprKind::If { cond, then, else_ } => {
      let cond_leaf = match &cond.kind {
        ValKind::Lit(Lit::Bool(b)) => lit_i32(if *b { 1 } else { 0 }),
        ValKind::Ref(r) => {
          let op = resolve_id_as_operand(lcx, ctx, ref_cps_id(*r));
          unbox_anyref(lcx, ctx, op)
        }
        ValKind::ContRef(id) => {
          let op = resolve_id_as_operand(lcx, ctx, *id);
          unbox_anyref(lcx, ctx, op)
        }
        _ => panic!("lower: If cond shape not supported: {:?}", cond.kind),
      };

      // Recursively lower then/else into separate instruction lists by
      // swapping `ctx.instrs` to a fresh empty Vec for each branch.
      let saved = std::mem::take(&mut ctx.instrs);
      lower_expr(lcx, ctx, then);
      let then_body = std::mem::take(&mut ctx.instrs);
      lower_expr(lcx, ctx, else_);
      let else_body = std::mem::replace(&mut ctx.instrs, saved);

      let i_if = push_if(lcx.frag, cond_leaf, then_body, else_body);
      ctx.instrs.push(i_if);
    }

    ExprKind::LetRec { slots, body } => {
      // For each slot, allocate a fresh `$Cell` and store it in the
      // slot's pub global. Non-pub'd slots are not yet supported.
      // The cell starts with `value = null` (ref.null any); a read
      // before the corresponding Set traps via struct.get of a null
      // field, naturally enforcing the "empty slot traps" semantics.
      for slot in slots {
        let gsym = *lcx.slot_globals.get(&slot.id)
          .unwrap_or_else(|| panic!("lower: LetRec slot {:?} has no slot_global registered", slot.id));
        // Allocate cell: `local.set $:cell_<slot> (struct.new $Cell (ref.null any))`.
        // Per-slot suffix keeps local names unique when a LetRec has
        // multiple slots — wasm uses indices at the binary level, but
        // duplicate WAT-display names are annoying for diagnostics.
        let cell_name = format!(":cell_{}", cps_ident_for_bind(lcx.cps, lcx.ast, slot));
        let cell_local = ctx.alloc_local_typed(
          &cell_name,
          val_ref(lcx.rt.cell(), /*nullable*/ true),
        );
        let i_new = push_struct_new(
          lcx.frag, lcx.rt.cell(),
          vec![op_ref_null(AbsHeap::Any)],
          cell_local,
        );
        ctx.instrs.push(i_new);
        // Store into the slot's global.
        let i_gset = push_global_set(lcx.frag, gsym, op_local(cell_local));
        ctx.instrs.push(i_gset);
      }
      lower_expr(lcx, ctx, body);
    }
    ExprKind::Set { name, val, cont } => {
      // Set fills a LetRec slot: `struct.set $Cell $value (cell_ref) (val)`.
      // The cell ref is read from the slot's storage location — module
      // slot = global, captured slot = local (Bind::Slot). Non-pub'd
      // module slots and fn-body slots aren't supported yet.
      let slot_id = name.id;
      let cell_op = slot_cell_ref(lcx, ctx, slot_id);
      let val_op = match &val.kind {
        ValKind::Lit(lit) => {
          let lv = LitVal::from_lit(lit)
            .unwrap_or_else(|| panic!("lower: unsupported lit {:?}", lit));
          let local = ctx.alloc_local(&cps_ident_for_bind(lcx.cps, lcx.ast, name));
          let i = box_lit(lcx.frag, lcx.rt, &lv, local);
          if let Some(o) = origin_of(lcx.cps, lcx.ast, name.id) { set_origin(lcx.frag, i, o); }
          ctx.instrs.push(i);
          op_local(local)
        }
        _ => val_as_operand(lcx, ctx, val),
      };
      let i_set = push_struct_set(lcx.frag, lcx.rt.cell(), 0, cell_op, val_op);
      ctx.instrs.push(i_set);
      // Lower the cont's body.
      if let Cont::Expr { body, .. } = cont {
        lower_expr(lcx, ctx, body);
      }
    }

    // `·closure funcref, {cap: outer_ref, ...}, fn result: cont` —
    // build a `$Closure` from a lifted-fn ref + captured outer values.
    // Mirrors `App(FnClosure)` but reads explicit captures from the
    // structural variant. Self-recursion via captured slot ref is
    // handled naturally — the slot's value is already correct at
    // closure-construction time once the enclosing LetRec/Set chain
    // has filled it.
    // `·letcaps caps_val, fn {bind_0, bind_1, ...}: cont` — destructure
    // the lifted fn's caps record into fresh locals. `caps_val` is a
    // Val::Ref to the ƒcaps param (a non-Cap Param::Name). Emits:
    //   local.set $:caps_cast (ref.cast (ref $Captures) <caps_ref>)
    //   local.set $<bind_i> (array.get $Captures $:caps_cast <i>)
    // for each bind, then descends into the cont body.
    ExprKind::LetCaps { caps, binds, cont } => {
      let caps_op = val_as_operand(lcx, ctx, caps);
      let caps_cast = ctx.alloc_local_typed(
        ":caps_cast",
        val_ref(lcx.rt.captures(), /*nullable*/ false),
      );
      let i_cast = push_ref_cast_non_null(
        lcx.frag, lcx.rt.captures(), caps_op, caps_cast,
      );
      ctx.instrs.push(i_cast);
      for (i, bind) in binds.iter().enumerate() {
        let name = cps_ident_for_bind(lcx.cps, lcx.ast, bind);
        // Bind::Slot locals hold a `(ref null $Cell)`. Captures
        // array entries are anyref, so extract into a scratch anyref
        // and ref.cast into the typed local. Other bind kinds keep
        // their anyref-typed local (Closures, ints etc).
        if matches!(bind.kind, Bind::Slot) {
          let local = ctx.alloc_local_typed(
            &name,
            val_ref(lcx.rt.cell(), /*nullable*/ true),
          );
          ctx.bind(bind.id, local);
          let tmp = ctx.alloc_local_typed(
            &format!("{name}_raw"),
            val_anyref(true),
          );
          let i_get = push_array_get(
            lcx.frag, lcx.rt.captures(),
            op_local(caps_cast), lit_i32(i as i32),
            tmp,
          );
          ctx.instrs.push(i_get);
          let i_cast = push_ref_cast_nullable(
            lcx.frag, lcx.rt.cell(),
            op_local(tmp), local,
          );
          ctx.instrs.push(i_cast);
        } else {
          let local = ctx.alloc_local(&name);
          ctx.bind(bind.id, local);
          let i_get = push_array_get(
            lcx.frag, lcx.rt.captures(),
            op_local(caps_cast), lit_i32(i as i32),
            local,
          );
          ctx.instrs.push(i_get);
        }
      }
      if let Cont::Expr { body, .. } = cont {
        lower_expr(lcx, ctx, body);
      }
    }

    ExprKind::Closure { funcref, captures, cont } => {
      let fn_sym = ctx.lookup_fn_sym(cps_id_of_ref(funcref));
      // Capture-position emission: if the outer is a slot, pass the
      // Cell ref directly (NOT the unwrapped value) so writes through
      // the captured local stay visible to the original LetRec scope.
      let cap_operands: Vec<Operand> = captures.iter()
        .map(|(_, outer_val)| {
          if let ValKind::Ref(r) = &outer_val.kind {
            let id = ref_cps_id(*r);
            if lcx.slot_ids.contains(&id) {
              return slot_cell_ref(lcx, ctx, id);
            }
          }
          val_as_operand(lcx, ctx, outer_val)
        })
        .collect();
      match cont {
        Cont::Expr { args: cont_args, body } => {
          let bind = cont_args.first()
            .expect("Closure cont has no result bind");
          let local = ctx.alloc_local(&cps_ident_for_bind(lcx.cps, lcx.ast, bind));
          ctx.bind(bind.id, local);
          emit_closure_construction(lcx, ctx, fn_sym, cap_operands, local);
          lower_expr(lcx, ctx, body);
        }
        Cont::Ref(_) => {
          panic!("lower: Closure with Cont::Ref not yet supported");
        }
      }
    }

    _ => panic!("lower: unsupported expr shape: {:?}", short_kind(&expr.kind)),
  }
}

/// Emit a continuation. `Cont::Expr` is emitted inline (body + recurse);
/// `Cont::Ref` closes out the function with an apply-dispatch call.
fn lower_cont(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  cont: &Cont,
) {
  match cont {
    Cont::Expr { args, body } => {
      // After thread_ctx, Cont::Expr.args may begin with a Bind::Ctx.
      // Inline cont bodies share the surrounding fn's runtime ctx, so
      // alias the fresh CpsId to the fn's `:ctx_param` local. This keeps
      // any ref to that ctx CpsId resolvable in the body.
      use crate::passes::cps::ir::Bind;
      if let (Some(b), Some(ctx_local)) = (args.first(), ctx.ctx_local)
        && matches!(b.kind, Bind::Ctx)
      {
        ctx.bind(b.id, ctx_local);
      }
      lower_expr(lcx, ctx, body);
    }
    Cont::Ref(id) => {
      // Tail-call the named cont with an empty args list, threading the
      // surrounding fn's ctx as a native wasm arg via apply_3.
      let callee = ctx.lookup(*id);
      let l_args_list = ctx.alloc_local_typed(":args",
        val_ref_abs(AbsHeap::Any, /*nullable*/ false));
      let i_nil = push_call(lcx.frag, lcx.rt.args_empty(), vec![], Some(l_args_list));
      ctx.instrs.push(i_nil);
      let l_ctx = ctx.ctx_local.expect("Cont::Ref tail-call: enclosing fn must have :ctx_param");
      let i_app = push_return_call(lcx.frag, lcx.rt.apply_3(),
        vec![op_local(l_args_list), op_local(l_ctx), op_local(callee)]);
      ctx.instrs.push(i_app);
    }
  }
}

/// Unbox an anyref into an `i32` operand for `If` cond evaluation.
/// Cast anyref → (ref i31), then `i31.get_s` into an i32 local.
fn unbox_anyref(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  op: Operand,
) -> Operand {
  let i31_local = ctx.alloc_local_typed(":cond_i31",
    val_ref_abs(AbsHeap::I31, /*nullable*/ false));
  let i_cast = push_ref_cast_non_null_abs(
    lcx.frag, AbsHeap::I31, op, i31_local);
  ctx.instrs.push(i_cast);
  let i32_local = ctx.alloc_local_typed(":cond_i32", val_i32());
  let i = push_i31_get_s(lcx.frag, op_local(i31_local), i32_local);
  ctx.instrs.push(i);
  op_local(i32_local)
}

// ──────────────────────────────────────────────────────────────────
// Operator / val helpers
// ──────────────────────────────────────────────────────────────────

/// Emit a protocol operator's tail call: `return_call op(a, b, done)`.
/// `operands` is the N value operands (1 for unary, 2 for binary).
/// The cont is either Cont::Ref (whose local is used directly as
/// `done`) or a Cont::Expr (lifted into a closure — not handled here
/// since the lifting pass already produces that as App(FnClosure)
/// ahead of the tail call).
/// Emit `trace_mark(module_id, cps_id)` recording this call site into the
/// current (top) activation frame, immediately before the user-fn dispatch.
/// `cps_id` is the Apply node's id (the call site); `module_id` is the
/// fragment's own id. Updates "where in the current function we are".
///
/// Only userland calls are marked: an Apply is marked iff it has a source
/// origin. Desugar-synthesised applies (pipe expansion, partial
/// application, etc.) have no origin and are skipped, so the trace
/// reflects source-level calls, not the finer post-desugar CPS structure.
fn mark_call_site(lcx: &mut LowerCtx<'_>, ctx: &mut FnCtx, call_site: CpsId) {
  if !is_source_apply(lcx.cps, lcx.ast, call_site) {
    return;
  }
  let module_id = lit_i32(lcx.frag.module_id.0 as i32);
  let cps_id = lit_i32(call_site.0 as i32);
  let i = push_call(lcx.frag, lcx.rt.trace_mark(), vec![module_id, cps_id], None);
  ctx.instrs.push(i);
}

/// Build the `apply_3` args list from a heterogeneous arg sequence.
///
/// Two-phase to keep the locals/instr order stable across changes:
/// Pull out the leading ctx argument from a thread_ctx-augmented args
/// list. After thread_ctx, every Apply with `Callable::Val` has its
/// 0th arg as `Arg::Val(Ref::Synth(ctx_id))`. Returns the materialised
/// ctx operand and the remaining args (cont + user values). Panics if
/// the 0th arg isn't a plain Val — a thread_ctx invariant violation.
fn split_ctx_arg<'a>(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  args: &'a [Arg],
) -> (Operand, &'a [Arg]) {
  match args.first() {
    Some(Arg::Val(v)) => (val_as_operand(lcx, ctx, v), &args[1..]),
    _ => panic!("split_ctx_arg: expected Arg::Val(ctx) at args[0], got {:?}", args.first()),
  }
}

/// 1. **Materialise** every arg into a leaf operand in source order
///    (so any boxing/closure-construction locals appear in their
///    natural declaration position).
/// 2. **Build** the list — alloc `:args`, `args_empty`, then walk the
///    materialised operands in reverse and `args_prepend` (or
///    `args_concat` for spread) onto the running list.
///
/// Returns the local holding the final list.
fn build_args_list(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  args: &[Arg],
) -> LocalIdx {
  enum Materialised { Prepend(Operand), Concat(Operand) }
  let materialised: Vec<Materialised> = args.iter()
    .map(|a| match a {
      Arg::Spread(v) => Materialised::Concat(val_as_operand(lcx, ctx, v)),
      Arg::Cont(Cont::Ref(id)) => Materialised::Prepend(resolve_id_as_operand(lcx, ctx, *id)),
      _ => Materialised::Prepend(emit_arg_as_operand(lcx, ctx, a)),
    })
    .collect();

  let l_args = ctx.alloc_local_typed(":args",
    val_ref_abs(AbsHeap::Any, /*nullable*/ false));
  let i_nil = push_call(lcx.frag, lcx.rt.args_empty(), vec![], Some(l_args));
  ctx.instrs.push(i_nil);
  for m in materialised.into_iter().rev() {
    match m {
      Materialised::Concat(op) => {
        let i = push_call(lcx.frag, lcx.rt.args_concat(),
          vec![op, op_local(l_args)], Some(l_args));
        ctx.instrs.push(i);
      }
      Materialised::Prepend(op) => {
        let i = push_call(lcx.frag, lcx.rt.args_prepend(),
          vec![op, op_local(l_args)], Some(l_args));
        ctx.instrs.push(i);
      }
    }
  }
  l_args
}

/// Emit `BuiltIn::Import` — `{names..} = import 'url'`.
///
/// Args layout: `[Val(LitStr(url)), Cont(Ref(cont_id))]`. The cont
/// takes a single rec value containing the imported names.
///
/// Two lowering paths depending on the URL kind:
///
///   - **Virtual stdlib** (`std/io.fnk` etc.). Each imported name maps
///     to its own per-name accessor in the runtime (`std/io.fnk:stdout`
///     → `interop_io_get_stdout`). Lowering: per-name
///     `<url>:<name>` import-call, build a rec inline, tail-apply
///     destructure cont with the rec.
///
///   - **User fragment** (`./foo.fnk`, `../bar.fnk`, etc.). The whole
///     module is one `<url>:fink_module` function. Lowering: build a
///     no-capture `$Closure` over the producer's `<url>:fink_module`
///     funcref, tail-call `std/modules.fnk:import` with
///     `[url_str, mod_clos, destructure_cont]`. The runtime helper
///     handles init-once + populates the destructure cont with the
///     producer's exports rec.
fn lower_import(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  args: &[Arg],
) {
  // Pull URL bytes from args[0] (Val::Lit::Str).
  let url_bytes = args.iter().find_map(|a| match a {
    Arg::Val(v) => match &v.kind {
      ValKind::Lit(Lit::Str(s)) => Some(s.as_slice()),
      _ => None,
    },
    _ => None,
  }).unwrap_or_else(|| panic!("lower: BuiltIn::Import missing URL arg"));
  let url = std::str::from_utf8(url_bytes)
    .unwrap_or_else(|_| panic!("lower: BuiltIn::Import URL is not valid UTF-8"));

  // Pull the destructure cont id from the LAST arg. After
  // cont_lift, the inline Cont::Expr is lifted into a LetFn and
  // passed as Arg::Val (Ref to the lifted fn).
  let cont_id = match args.last() {
    Some(Arg::Cont(Cont::Ref(id))) => *id,
    Some(Arg::Val(v)) => match &v.kind {
      ValKind::Ref(r) => ref_cps_id(*r),
      ValKind::ContRef(id) => *id,
      _ => panic!("lower: BuiltIn::Import cont arg has unexpected Val shape"),
    },
    other => panic!("lower: BuiltIn::Import missing cont (got {:?})", other),
  };

  // `.wat` runtime modules resolve via the virtual-rec path (per-name
  // runtime accessors). Everything else -- `std/*.fnk` stdlib modules and
  // relative user fragments alike -- is a real compiled fragment.
  if url.ends_with(".wat") {
    lower_import_virtual_stdlib(lcx, ctx, url, cont_id);
  } else {
    lower_import_user_fragment(lcx, ctx, url, cont_id);
  }
}

/// Virtual stdlib path: `import 'std/io.fnk'` etc. Each imported name
/// maps to its own runtime accessor. Build a rec from per-name calls,
/// tail-apply destructure cont.
fn lower_import_virtual_stdlib(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  url: &str,
  cont_id: CpsId,
) {
  // Names come from the pre-collected module_imports table.
  let names = lcx.cps.module_imports.get(url)
    .cloned()
    .unwrap_or_else(|| panic!(
      "lower: BuiltIn::Import for `{url}` has no entries in module_imports"));

  // 1. Build the rec.
  let l_rec = ctx.alloc_local(":imp_rec");
  let i_new = push_call(lcx.frag, lcx.rt.rec_empty(), vec![], Some(l_rec));
  ctx.instrs.push(i_new);

  for name in &names {
    // 1a. Declare the import — `<url>:<name>` with signature `() -> anyref`.
    //     The IR linker resolves this against the runtime's export table
    //     at emit time. Reuses the `Fn_rec_new` sig type (`() -> anyref`).
    let sig = lcx.rt.fn_nil_to_list_sig();
    let target = crate::passes::wasm::ir::import_func(lcx.frag, sig, url, name);

    // 1b. Call the import to get the channel/value into a fresh local.
    let l_val = ctx.alloc_local(&format!(":imp_val_{name}"));
    let i_call = push_call(lcx.frag, target, vec![], Some(l_val));
    ctx.instrs.push(i_call);

    // 1c. Build the $Symbol key for the field name. This rec is destructured
    // by fink (`{io} = import ...`) via RecPop, which keys by $Symbol; the
    // names are static module exports, so they intern like any static key.
    let l_key = ctx.alloc_local(&format!(":imp_key_{name}"));
    let i_key = box_symbol(lcx.frag, lcx.rt, name.as_bytes(), l_key);
    ctx.instrs.push(i_key);

    // 1d. Set the field on the rec.
    let i_set = push_call(lcx.frag, lcx.rt.rec_set_field(),
      vec![op_local(l_rec), op_local(l_key), op_local(l_val)],
      Some(l_rec));
    ctx.instrs.push(i_set);
  }

  // 2. Tail-apply the cont with [rec].
  let cont_op = resolve_id_as_operand(lcx, ctx, cont_id);
  let cont_local = match cont_op {
    Operand::Local(l) => l,
    other => {
      let l = ctx.alloc_local(&format!(":callee_{}", cont_id.0));
      let i = push_local_set(lcx.frag, l, other);
      ctx.instrs.push(i);
      l
    }
  };

  let l_args = ctx.alloc_local_typed(":args",
    val_ref_abs(AbsHeap::Any, /*nullable*/ false));
  let i_nil = push_call(lcx.frag, lcx.rt.args_empty(), vec![], Some(l_args));
  ctx.instrs.push(i_nil);
  let i_cons = push_call(lcx.frag, lcx.rt.args_prepend(),
    vec![op_local(l_rec), op_local(l_args)], Some(l_args));
  ctx.instrs.push(i_cons);
  let l_ctx = ctx.ctx_local.expect("Import wrap-cont tail: enclosing fn must have :ctx_param");
  let i_app = push_return_call(lcx.frag, lcx.rt.apply_3(),
    vec![op_local(l_args), op_local(l_ctx), op_local(cont_local)]);
  ctx.instrs.push(i_app);
}

/// User-fragment path: `import './foo.fnk'`. Tail-call
/// `std/modules.fnk:import` with the URL, a no-capture closure over
/// the producer's `<url>:fink_module`, and the destructure cont.
fn lower_import_user_fragment(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  url: &str,
  cont_id: CpsId,
) {
  // Canonicalise the URL relative to the importing module's URL so
  // the runtime call's `mod_url` arg matches what the producer
  // fragment's `pub` calls write to in the registry. Without this,
  // a nested import like `import './foo.fnk'` from
  // `./test_modules/needs_tiny.fnk` would pass `./foo.fnk` to the
  // runtime, but the producer of `./foo.fnk` was compiled under
  // canonical URL `./test_modules/foo.fnk` and pubs to that key.
  // The two URLs must agree; canonicalising here ensures it.
  let importer_canonical = lcx.fqn_prefix.trim_end_matches(':').to_string();
  let canonical_url = super::compile_package::canonicalise_url(
    &importer_canonical, url,
  );

  // 1. Materialise the canonicalised URL as a `$Str` constant.
  let url_local = emit_str_const(lcx, ctx, canonical_url.as_bytes(), ":imp_url");

  // 2. Declare a func import of the producer's `<canonical_url>:fink_module`,
  //    typed as `$Fn3`. Resolved at emit time by name lookup against
  //    the merged runtime's export table; link::link rewrites it to
  //    the producer's local FuncSym during multi-fragment merge by
  //    matching the canonical URL against producer fragments' display
  //    names.
  let mod_fn_sym = crate::passes::wasm::ir::import_func(
    lcx.frag, lcx.rt.fn3(), &canonical_url, "fink_module");

  // 3. Build a no-capture `$Closure` over that funcref. funcrefs are
  //    not anyref-compatible (disjoint typing hierarchies in WasmGC),
  //    so we wrap to satisfy the std/modules.fnk:import signature
  //    which takes everything as anyref.
  let caps_local = ctx.alloc_local_typed(
    ":imp_caps_arg",
    val_ref(lcx.rt.captures(), /*nullable*/ true),
  );
  let i_caps = push_ref_null_concrete(lcx.frag, lcx.rt.captures(), caps_local);
  ctx.instrs.push(i_caps);

  let mod_clos_local = ctx.alloc_local(":imp_mod_clos");
  let i_clos = push_struct_new(
    lcx.frag, lcx.rt.closure(),
    vec![Operand::RefFunc(mod_fn_sym), op_local(caps_local)],
    mod_clos_local,
  );
  ctx.instrs.push(i_clos);

  // 4. Resolve the destructure cont.
  let cont_op = resolve_id_as_operand(lcx, ctx, cont_id);
  let cont_local = match cont_op {
    Operand::Local(l) => l,
    other => {
      let l = ctx.alloc_local(&format!(":callee_{}", cont_id.0));
      let i = push_local_set(lcx.frag, l, other);
      ctx.instrs.push(i);
      l
    }
  };

  // 5. Tail-call `std/modules.fnk:import (ctx, url, mod_clos, cont)`.
  //    The runtime helper invokes the producer's module body under
  //    the importer's ctx and threads any ctx mutations back to the
  //    destructure cont.
  let ctx_local = ctx.ctx_local.expect("lower_import: enclosing fn must have :ctx_param");
  let i_imp = push_return_call(lcx.frag, lcx.rt.modules_import(),
    vec![op_local(ctx_local), op_local(url_local), op_local(mod_clos_local), op_local(cont_local)]);
  ctx.instrs.push(i_imp);
}


/// Emit a 4-arg primitive with shape `(ctx, any, any, any, any) -> ()`.
/// After thread_ctx the CPS shape is `[ctx, ...4 user args]`; ctx is
/// passed to the runtime as the 0th wasm arg. Used by RecPut and
/// RecPop.
/// Emit a type-seed constructor (`new_type`/`new_union`/`new_enum`). The CPS
/// shape is `(ctx, name, cont)`: the type's `$name` is a `Lit::Symbol` value arg
/// (the declared ident, or the empty symbol for an anonymous `type _`), boxed to
/// a tagged i31 via the symbol path. Runtime sig is `(ctx, name, cont)`.
fn emit_type_seed(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  target: FuncSym,
  args: &[Arg],
  app_id: CpsId,
) {
  let ctx_a = args.first().expect("type seed: missing ctx");
  let name_a = args.get(1).expect("type seed: missing name");
  let cont = args.get(2).expect("type seed: missing cont");
  let ctx_op = emit_arg_as_operand(lcx, ctx, ctx_a);
  let name_op = emit_key_as_operand(lcx, ctx, name_a);
  let cont_op = match cont {
    Arg::Cont(Cont::Ref(id)) => resolve_id_as_operand(lcx, ctx, *id),
    Arg::Val(v) => val_as_operand(lcx, ctx, v),
    _ => panic!("lower: type-seed cont is neither Cont::Ref nor Val"),
  };
  let i = push_return_call(lcx.frag, target, vec![ctx_op, name_op, cont_op]);
  if let Some(o) = origin_of(lcx.cps, lcx.ast, app_id) { set_origin(lcx.frag, i, o); }
  set_cps_id(lcx.frag, i, app_id);
  ctx.instrs.push(i);
}

/// Emit a tail-call to a runtime func resolved by a dedicated getter (not the
/// protocol `op()` table). Every `Arg` maps to an operand in order, so this is
/// arity-agnostic — the CPS shape `[ctx, ...user_args, cont]` flows straight to
/// the runtime func's params. Used by the type-construction accretion ops and
/// `emit_quaternary`.
fn emit_direct_op_call(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  target: FuncSym,
  args: &[Arg],
  app_id: CpsId,
) {
  let ops: Vec<Operand> = args.iter().map(|a| emit_arg_as_operand(lcx, ctx, a)).collect();
  let i = push_return_call(lcx.frag, target, ops);
  if let Some(o) = origin_of(lcx.cps, lcx.ast, app_id) { set_origin(lcx.frag, i, o); }
  set_cps_id(lcx.frag, i, app_id);
  ctx.instrs.push(i);
}

/// Like `emit_direct_op_call`, but the arg at the record-KEY position (index
/// 2 after thread_ctx: `[ctx, rec, key, ...]`) is lowered via
/// `emit_key_as_operand`, so a `Lit::Symbol` field key becomes a `$Symbol`
/// (string / computed keys lower as ordinary values). Used by RecPut / RecPop.
fn emit_rec_key_op(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  target: FuncSym,
  args: &[Arg],
  app_id: CpsId,
) {
  const KEY: usize = 2;
  let ops: Vec<Operand> = args.iter().enumerate()
    .map(|(i, a)| if i == KEY {
      emit_key_as_operand(lcx, ctx, a)
    } else {
      emit_arg_as_operand(lcx, ctx, a)
    })
    .collect();
  let i = push_return_call(lcx.frag, target, ops);
  if let Some(o) = origin_of(lcx.cps, lcx.ast, app_id) { set_origin(lcx.frag, i, o); }
  set_cps_id(lcx.frag, i, app_id);
  ctx.instrs.push(i);
}

/// Emit a `(ctx, value, cont, cont)` ternary primitive (IsSeqLike,
/// IsRecLike, SeqPop). After thread_ctx the CPS shape is
/// `[ctx, val, succ, fail]`; ctx is passed to the runtime as the
/// 0th wasm arg, the rest are the value being tested and two
/// continuations resolved as values at this layer.
fn emit_ternary_guard(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  sym: Sym,
  args: &[Arg],
  app_id: CpsId,
) {
  if args.len() != 4 {
    panic!("lower: ternary primitive {:?} expects 4 args (ctx + val + 2 conts), got {}", sym, args.len());
  }
  let ctx_op = emit_arg_as_operand(lcx, ctx, &args[0]);
  let val_op = emit_arg_as_operand(lcx, ctx, &args[1]);
  let cont1_op = emit_arg_as_operand(lcx, ctx, &args[2]);
  let cont2_op = emit_arg_as_operand(lcx, ctx, &args[3]);
  let _ = ctx;  // ctx_local no longer needed: use the threaded ctx arg directly
  let i = push_return_call(lcx.frag, lcx.rt.op(sym), vec![ctx_op, val_op, cont1_op, cont2_op]);
  if let Some(o) = origin_of(lcx.cps, lcx.ast, app_id) { set_origin(lcx.frag, i, o); }
  set_cps_id(lcx.frag, i, app_id);
  ctx.instrs.push(i);
}

fn emit_op_tail_call(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  sym: Sym,
  ctx_arg: &Arg,
  value_operands: Vec<Operand>,
  cont: &Arg,
  app_id: CpsId,
) {
  let ctx_op = emit_arg_as_operand(lcx, ctx, ctx_arg);
  let cont_op = match cont {
    Arg::Cont(Cont::Ref(id)) => resolve_id_as_operand(lcx, ctx, *id),
    Arg::Val(v) => val_as_operand(lcx, ctx, v),
    _ => panic!("lower: operator cont is neither Cont::Ref nor Val (got {:?})", short_arg(cont)),
  };
  let mut operands = vec![ctx_op];
  operands.extend(value_operands);
  operands.push(cont_op);
  let i = push_return_call(lcx.frag, lcx.rt.op(sym), operands);
  if let Some(o) = origin_of(lcx.cps, lcx.ast, app_id) { set_origin(lcx.frag, i, o); }
  set_cps_id(lcx.frag, i, app_id);
  ctx.instrs.push(i);
}

/// Convert an `Arg` to a leaf `Operand`, allocating locals for
/// non-trivial values (literals get boxed into a fresh local).
fn emit_arg_as_operand(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  arg: &Arg,
) -> Operand {
  match arg {
    Arg::Val(v) => {
      match &v.kind {
        ValKind::Lit(lit) => {
          let lv = LitVal::from_lit(lit)
            .unwrap_or_else(|| panic!("lower: unsupported lit {:?}", lit));
          let local = ctx.alloc_local(&format!(":lit_{}", v.id.0));
          let i = box_lit(lcx.frag, lcx.rt, &lv, local);
          if let Some(o) = origin_of(lcx.cps, lcx.ast, v.id) { set_origin(lcx.frag, i, o); }
          ctx.instrs.push(i);
          op_local(local)
        }
        ValKind::Ref(r) => resolve_id_as_operand(lcx, ctx, ref_cps_id(*r)),
        ValKind::ContRef(id) => resolve_id_as_operand(lcx, ctx, *id),
        ValKind::BuiltIn(BuiltIn::Panic(_)) => panic_closure_operand(lcx, ctx),
        ValKind::BuiltIn(BuiltIn::RecProtocol) => protocol_operand(lcx, ctx, 0),
        ValKind::BuiltIn(BuiltIn::TupleProtocol) => protocol_operand(lcx, ctx, 1),
        ValKind::BuiltIn(b) => panic!("lower: BuiltIn {:?} as arg not supported", b),
      }
    }
    // Cont args appear in builtin calls like IsSeqLike(val, succ, fail)
    // where succ/fail are continuations. Cont::Ref is just an id —
    // resolve it as a closure operand. Cont::Expr is a not currently
    // supported here (would need to materialise an inline lambda).
    Arg::Cont(Cont::Ref(id)) => resolve_id_as_operand(lcx, ctx, *id),
    _ => panic!("lower: non-Val arg in value position: {:?}", short_arg(arg)),
  }
}

/// Materialise a no-capture `$Closure` over the runtime `panic`
/// function. Used when `BuiltIn::Panic(_)` appears in value position
/// (typically as the fail continuation in pattern-match dispatch
/// generated by the lifting pass).
fn panic_closure_operand(lcx: &mut LowerCtx<'_>, ctx: &mut FnCtx) -> Operand {
  let local = ctx.alloc_local(":panic_clo");
  let caps_local = ctx.alloc_local_typed(":caps_arg",
    val_ref(lcx.rt.captures(), /*nullable*/ true));
  let i_caps = push_ref_null_concrete(lcx.frag, lcx.rt.captures(), caps_local);
  ctx.instrs.push(i_caps);
  let i_clo = push_struct_new(lcx.frag, lcx.rt.closure(),
    vec![Operand::RefFunc(lcx.rt.panic_apply()), op_local(caps_local)],
    local);
  ctx.instrs.push(i_clo);
  op_local(local)
}

/// Materialise a built-in protocol guard sentinel for `guard_apply`. Interim
/// representation: a magic i31 (0 = rec-like, 1 = tuple-like) that the runtime
/// guard_apply recognises and routes to is_rec_like / is_seq_like. Replace with
/// real protocol type values once intrinsics/FQN-globals are unified.
fn protocol_operand(lcx: &mut LowerCtx<'_>, ctx: &mut FnCtx, sentinel: i32) -> Operand {
  let local = ctx.alloc_local(":protocol");
  let i = push_ref_i31(lcx.frag, lit_i32(sentinel), local);
  ctx.instrs.push(i);
  op_local(local)
}

/// Convert a `Val` directly to an `Operand` (for cases where we're
/// sure it's a ref/lit and don't need to emit boxing). Routes through
/// `resolve_id_as_operand` so cross-fn refs to module-level Pub'd
/// bindings or top-level lifted fns resolve correctly.
fn val_as_operand(lcx: &mut LowerCtx<'_>, ctx: &mut FnCtx, v: &Val) -> Operand {
  match &v.kind {
    ValKind::Ref(r) => resolve_id_as_operand(lcx, ctx, ref_cps_id(*r)),
    ValKind::ContRef(id) => resolve_id_as_operand(lcx, ctx, *id),
    ValKind::BuiltIn(BuiltIn::Panic(_)) => panic_closure_operand(lcx, ctx),
    ValKind::Lit(_) => panic!("val_as_operand: Lit requires boxing — use emit_arg_as_operand"),
    ValKind::BuiltIn(b) => panic!("val_as_operand: BuiltIn {:?} not supported", b),
  }
}

/// Emit a value into a specific local. Used by LetVal.
fn emit_val_into(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  val: &Val,
  into: LocalIdx,
) -> InstrId {
  match &val.kind {
    ValKind::Lit(lit) => {
      let lv = LitVal::from_lit(lit)
        .unwrap_or_else(|| panic!("lower: unsupported lit {:?}", lit));
      box_lit(lcx.frag, lcx.rt, &lv, into)
    }
    ValKind::Ref(r) => {
      let id = ref_cps_id(*r);
      // Three cases for `LetVal(_, Ref(id))`:
      // 1. Locally bound — copy local-to-local.
      // 2. Pub'd module global — `local.set into (global.get $g)`.
      // 3. Top-level fn id (no captures) — emit `$Closure` directly
      //    into `into` (avoids spilling through a scratch local).
      if let Some(local) = ctx.binds.get(&id).copied() {
        return push_local_set(lcx.frag, into, op_local(local));
      }
      if let Some(&(gsym, _)) = lcx.pub_globals.get(&id) {
        return push_local_set(lcx.frag, into, op_global(gsym));
      }
      if let Some(fn_sym) = ctx.try_lookup_fn_sym(id) {
        let caps_local = ctx.alloc_local_typed(
          ":caps_arg",
          val_ref(lcx.rt.captures(), /*nullable*/ true),
        );
        let i_caps = push_ref_null_concrete(lcx.frag, lcx.rt.captures(), caps_local);
        ctx.instrs.push(i_caps);
        return push_struct_new(
          lcx.frag, lcx.rt.closure(),
          vec![Operand::RefFunc(fn_sym), op_local(caps_local)],
          into,
        );
      }
      let src = ctx.lookup(id);  // panic with diagnostic
      push_local_set(lcx.frag, into, op_local(src))
    }
    ValKind::ContRef(id) => {
      // Cont refs: same three-way resolution shape, but the no-capture
      // closure case shouldn't apply (conts are never bare fn ids).
      if let Some(local) = ctx.binds.get(id).copied() {
        return push_local_set(lcx.frag, into, op_local(local));
      }
      if let Some(&(gsym, _)) = lcx.pub_globals.get(id) {
        return push_local_set(lcx.frag, into, op_global(gsym));
      }
      let src = ctx.lookup(*id);
      push_local_set(lcx.frag, into, op_local(src))
    }
    ValKind::BuiltIn(_) => panic!("lower: BuiltIn as LetVal rhs not supported"),
  }
}

// ──────────────────────────────────────────────────────────────────
// Support types + helpers
// ──────────────────────────────────────────────────────────────────

/// Literal shape at lowering time.
enum LitVal {
  /// Signed integer literal — boxes as `$I64`. All sized signed widths
  /// (i8/i16/i32/i64) collapse to a single i64 carrier; the field stays
  /// f64 today (will narrow to i64 in a later step).
  I64(i64),
  /// Unsigned integer literal — boxes as `$U64`. All unsigned widths
  /// collapse to a single u64 carrier; field stays f64 today.
  U64(u64),
  /// Float literal — boxes as `$F64`. Both f32 and f64 widths share the
  /// f64 carrier today.
  F64(f64),
  /// Decimal literal — boxes as `$Decimal` with `(coeff i64, exp i32)`.
  /// Value semantics: `coeff * 10^exp`. Read paths (formatter, hash,
  /// num.wat conversions) compute the f64 view at use time.
  Decimal { coeff: i64, exp: i32 },
  Bool(bool),
  /// Empty sequence literal `[]` — lowers to `call $args_empty`.
  /// Reuses the `args_empty` runtime function (which is exported as
  /// both `args_empty` and `list_nil` from the same impl).
  EmptySeq,
  /// Empty record literal `{}` — lowers to `call $rec_new`.
  EmptyRec,
  /// String literal. Empty strings are special-cased to `str_empty`;
  /// non-empty strings intern their bytes into `frag.data` and emit
  /// `call $from_data (i32.const offset) (i32.const len)`.
  Str(Vec<u8>),
}

impl LitVal {
  fn from_lit(lit: &Lit) -> Option<Self> {
    use crate::passes::cps::ir::IntWidth;
    Some(match lit {
      Lit::Int { value, width } => match width {
        IntWidth::I8 | IntWidth::I16 | IntWidth::I32 | IntWidth::I64 => LitVal::I64(*value),
        IntWidth::U8 | IntWidth::U16 | IntWidth::U32 | IntWidth::U64 => LitVal::U64(*value as u64),
      },
      Lit::Float { value, .. }   => LitVal::F64(*value),
      Lit::Decimal { coeff, exp } => LitVal::Decimal { coeff: *coeff, exp: *exp },
      Lit::Bool(b)    => LitVal::Bool(*b),
      Lit::Seq        => LitVal::EmptySeq,
      Lit::Rec        => LitVal::EmptyRec,
      Lit::Str(s)     => LitVal::Str(s.clone()),
      // Symbols only appear at record-key positions, lowered via the dedicated
      // `box_symbol` path (`emit_key_as_operand`). Reaching this generic literal
      // path means a symbol leaked into value position -- a bug; return None so
      // the caller panics loudly rather than silently stringifying it.
      Lit::Symbol(_)  => return None,
    })
  }
}

/// Emit a `$Str` constant from raw bytes into a fresh local. Used by
/// the Pub arm to materialise the `<fqn>` and `<name>` arguments to
/// `std/modules.fnk:pub` at every export site.
///
/// Uses `rt.str_from_data()` unconditionally — even for empty bytes — so we
/// don't need `Sym::StrEmpty` declared just for the empty-FQN case.
/// `intern_data(frag, &[])` produces a zero-length data symbol; the
/// resulting `$Str` has len 0 and reads as the empty string.
fn emit_str_const(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  bytes: &[u8],
  display_hint: &str,
) -> LocalIdx {
  let local = ctx.alloc_local(display_hint);
  let sym = intern_data(lcx.frag, bytes);
  let len = bytes.len() as u32;
  let i = push_call(lcx.frag, lcx.rt.str_from_data(),
    vec![Operand::DataRef { sym, len }], Some(local));
  ctx.instrs.push(i);
  local
}

fn box_lit(frag: &mut Fragment, rt: &Runtime, lit: &LitVal, into: LocalIdx) -> InstrId {
  match lit {
    // Integer literals: signed → $I64, unsigned → $U64. Field is still
    // f64 today (subtypes share $Num's slot); the value is converted
    // for storage. Future step narrows the field to i64.
    // Single i64 field — the f64 view was dropped from $Int.
    LitVal::I64(n)  => push_struct_new(frag, rt.i64_(), vec![lit_i64(*n)], into),
    LitVal::U64(n)  => push_struct_new(frag, rt.u64_(), vec![lit_i64(*n as i64)], into),
    LitVal::F64(n)  => push_struct_new(frag, rt.f64_(), vec![lit_f64(*n)], into),
    LitVal::Decimal { coeff, exp } => push_struct_new(frag, rt.decimal_(),
      vec![lit_i64(*coeff), lit_i32(*exp)], into),
    LitVal::Bool(b) => push_ref_i31(frag, lit_i32(if *b { 1 } else { 0 }), into),
    LitVal::EmptySeq => push_call(frag, rt.args_empty(), vec![], Some(into)),
    LitVal::EmptyRec => push_call(frag, rt.rec_empty(), vec![], Some(into)),
    LitVal::Str(bytes) => {
      if bytes.is_empty() {
        push_call(frag, rt.str_empty(), vec![], Some(into))
      } else {
        // Intern the bytes, then emit `call $from_data (data_ref, len)`.
        // `Operand::DataRef` expands to two i32 consts at emit time.
        let sym = intern_data(frag, bytes);
        let len = bytes.len() as u32;
        push_call(frag, rt.str_from_data(),
          vec![Operand::DataRef { sym, len }], Some(into))
      }
    }
  }
}

/// Box a static field name as a symbol: `ref.i31 (i32.const <word>)`, where the
/// word is carried by NAME (`Operand::SymbolId`) until link. The linker merges
/// all names package-wide into one canonical id per name and resolves each to
/// the folded i31 word `(id << 3) | TAG_SYMBOL`. A symbol is a COMPILE-TIME
/// CONSTANT -- the same name anywhere yields the same word, so identity is
/// whole-word ref.eq with no instance table and no allocation.
fn box_symbol(frag: &mut Fragment, _rt: &Runtime, name: &[u8], into: LocalIdx) -> InstrId {
  // Record the name so prepend_symbol_table registers it in the runtime
  // str->symbol table. The value (0) is unused -- the set is name-keyed.
  frag.symbols.insert(name.to_vec(), 0);
  push_ref_i31(frag, Operand::SymbolId(name.to_vec()), into)
}

/// Lower a record-KEY arg. A static string-literal key becomes a `$Symbol`
/// (interned). Any other key (a computed/runtime value) falls back to normal
/// value lowering -- those stay on the generic key path.
fn emit_key_as_operand(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  arg: &Arg,
) -> Operand {
  if let Arg::Val(v) = arg
    && let ValKind::Lit(Lit::Symbol(bytes)) = &v.kind
  {
    let local = ctx.alloc_local(&format!(":sym_{}", v.id.0));
    let i = box_symbol(lcx.frag, lcx.rt, bytes, local);
    if let Some(o) = origin_of(lcx.cps, lcx.ast, v.id) { set_origin(lcx.frag, i, o); }
    ctx.instrs.push(i);
    return op_local(local);
  }
  // Non-symbol keys (string value keys, computed keys) lower as ordinary values.
  emit_arg_as_operand(lcx, ctx, arg)
}

fn binary_op_sym(b: BuiltIn) -> Option<Sym> {
  Some(match b {
    BuiltIn::Add    => Sym::OpPlus,
    BuiltIn::Sub    => Sym::OpMinus,
    BuiltIn::Mul    => Sym::OpMul,
    BuiltIn::Div    => Sym::OpDiv,
    BuiltIn::IntDiv => Sym::OpIntDiv,
    BuiltIn::Mod    => Sym::OpRem,
    BuiltIn::IntMod => Sym::OpIntMod,
    BuiltIn::DivMod => Sym::OpDivMod,
    BuiltIn::Pow    => Sym::OpPow,
    BuiltIn::Eq     => Sym::OpEq,
    BuiltIn::Neq    => Sym::OpNeq,
    BuiltIn::Lt     => Sym::OpLt,
    BuiltIn::Lte    => Sym::OpLte,
    BuiltIn::Gt     => Sym::OpGt,
    BuiltIn::Gte    => Sym::OpGte,
    BuiltIn::Disjoint => Sym::OpDisjoint,
    BuiltIn::And    => Sym::OpAnd,
    BuiltIn::Or     => Sym::OpOr,
    BuiltIn::Xor    => Sym::OpXor,
    BuiltIn::Shl    => Sym::OpShl,
    BuiltIn::Shr    => Sym::OpShr,
    BuiltIn::RotL   => Sym::OpRotL,
    BuiltIn::RotR   => Sym::OpRotR,
    BuiltIn::Range     => Sym::OpRngex,
    BuiltIn::RangeIncl => Sym::OpRngin,
    BuiltIn::In        => Sym::OpIn,
    BuiltIn::NotIn     => Sym::OpNotIn,
    BuiltIn::Get       => Sym::OpDot,
    _ => return None,
  })
}

fn extract_fink_module_body(root: &Expr) -> Option<(CpsId, CpsId, &Expr)> {
  // After hoist, the root is wrapped in a chain of top-level LetFns.
  // Peel through them to find the `App(FinkModule, ...)` at the
  // innermost cont. The lifted fns themselves are emitted separately
  // by the lower_expr LetFn arm during the main module-body walk.
  let mut node = root;
  while let ExprKind::LetFn { cont: Cont::Expr { body, .. }, .. } = &node.kind {
    node = body;
  }
  let ExprKind::App { func: Callable::BuiltIn(BuiltIn::FinkModule), args } = &node.kind else {
    return None;
  };
  let cont_arg = args.first()?;
  let Arg::Cont(Cont::Expr { args: cont_args, body }) = cont_arg else {
    return None;
  };
  // Module body shape: `fn ·ƒctx, ·ƒret: <body>`. Ctx is the 0th param
  // (effect-handler universe context, runtime-injected); ƒret is the
  // host return continuation.
  let ctx_bind = cont_args.first()?;
  let ret_bind = cont_args.get(1)?;
  Some((ctx_bind.id, ret_bind.id, body))
}

/// Split a binary op's args after thread_ctx: `[ctx, a, b, cont]` →
/// `(ctx, a, b, cont)`. The ctx is passed to the runtime op as the
/// 0th wasm arg.
fn split_binary_args(args: &[Arg]) -> (&Arg, &Arg, &Arg, &Arg) {
  (
    args.first().expect("binary op: missing ctx"),
    args.get(1).expect("binary op: missing arg 0"),
    args.get(2).expect("binary op: missing arg 1"),
    args.get(3).expect("binary op: missing cont"),
  )
}

/// Split a unary op's args after thread_ctx: `[ctx, v, cont]` →
/// `(ctx, v, cont)`.
fn split_unary_args(args: &[Arg]) -> (&Arg, &Arg, &Arg) {
  (
    args.first().expect("unary op: missing ctx"),
    args.get(1).expect("unary op: missing arg"),
    args.get(2).expect("unary op: missing cont"),
  )
}

/// Splits args into (last_cont, non_cont_prefix). Panics if the last
/// arg is not a Cont.
fn split_last_cont(args: &[Arg]) -> (&Cont, &[Arg]) {
  let last = args.last().expect("split_last_cont: empty args");
  let Arg::Cont(cont) = last else {
    panic!("split_last_cont: last arg is not Cont");
  };
  (cont, &args[..args.len() - 1])
}

fn cps_id_of_ref(v: &Val) -> CpsId {
  match &v.kind {
    ValKind::Ref(r) => ref_cps_id(*r),
    ValKind::ContRef(id) => *id,
    _ => panic!("cps_id_of_ref: val is not a ref ({:?})", v.kind),
  }
}

fn ref_cps_id(r: Ref) -> CpsId {
  match r {
    Ref::Synth(id) | Ref::Unresolved(id) => id,
  }
}

fn cps_ident_for_bind(cps: &CpsResult, ast: &Ast<'_>, b: &BindNode) -> String {
  use crate::passes::cps::ir::{Bind, ContKind};
  match b.kind {
    Bind::Ctx                  => format!(":ctx_{}", b.id.0),
    Bind::Caps                 => format!(":caps_{}", b.id.0),
    Bind::Slot                 => format!(":slot_{}", b.id.0),
    Bind::Cont(ContKind::Ret)  => format!(":ret_{}", b.id.0),
    Bind::Cont(ContKind::Succ) => format!(":succ_{}", b.id.0),
    Bind::Cont(ContKind::Fail) => format!(":fail_{}", b.id.0),
    Bind::Synth                => format!(":v_{}", b.id.0),
    Bind::SynthName            => cps_ident(cps, ast, b.id),
  }
}

/// Derive a display name for a use-site CpsId. Uses the source ident
/// from the origin map (`{ident}_{id}`) when available; falls back to
/// `:v_<id>` for compiler temps. The colon prefix is lexer-rejected
/// in user source, so it cannot collide with a user binding's name.
fn cps_ident(cps: &CpsResult, ast: &Ast<'_>, id: CpsId) -> String {
  let ast_id = cps.origin.try_get(id).and_then(|o| *o);
  match ast_id {
    Some(a) => match &ast.nodes.get(a).kind {
      crate::ast::NodeKind::Ident(s) => format!("{}_{}", s, id.0),
      _ => format!(":v_{}", id.0),
    },
    None => format!(":v_{}", id.0),
  }
}

/// `cps_ident` with bind-kind awareness. Special-cases `Bind::Ctx` to
/// render as `:ctx_<id>` (matching the CPS-level synth-name convention).
fn cps_ident_kinded(
  cps: &CpsResult,
  ast: &Ast<'_>,
  bind_kinds: &crate::propgraph::PropGraph<CpsId, Option<crate::passes::cps::ir::Bind>>,
  id: CpsId,
) -> String {
  use crate::passes::cps::ir::{Bind, ContKind};
  if let Some(Some(kind)) = bind_kinds.try_get(id) {
    return match kind {
      Bind::Ctx                  => format!(":ctx_{}", id.0),
      Bind::Caps                 => format!(":caps_{}", id.0),
      Bind::Slot                 => format!(":slot_{}", id.0),
      Bind::Cont(ContKind::Ret)  => format!(":ret_{}", id.0),
      Bind::Cont(ContKind::Succ) => format!(":succ_{}", id.0),
      Bind::Cont(ContKind::Fail) => format!(":fail_{}", id.0),
      Bind::Synth                => format!(":v_{}", id.0),
      Bind::SynthName            => cps_ident(cps, ast, id),
    };
  }
  cps_ident(cps, ast, id)
}

/// Recover the user-visible export name for a CpsId via the origin map.
/// Mirrors `collect.rs::export_name`.
fn pub_export_name(cps: &CpsResult, ast: &Ast<'_>, id: CpsId) -> String {
  let ast_id = cps.origin.try_get(id).and_then(|o| *o);
  match ast_id {
    Some(a) => match &ast.nodes.get(a).kind {
      crate::ast::NodeKind::Ident(s) => s.to_string(),
      _ => format!("v_{}", id.0),
    },
    None => format!("v_{}", id.0),
  }
}

/// Walk the IR and collect every LetRec slot CpsId into `out`. These
/// are the ids that lower as `(ref $Cell)`-typed storage cells; reads
/// auto-unwrap via `struct.get $Cell.value`, writes go via
/// `struct.set`. Captures of slots flow the Cell ref unchanged through
/// `Bind::Slot` LetCaps locals — those also land in `out` so the same
/// access path applies inside lifted bodies.
fn collect_slot_ids(expr: &Expr, out: &mut std::collections::HashSet<CpsId>) {
  match &expr.kind {
    ExprKind::LetRec { slots, body } => {
      for s in slots { out.insert(s.id); }
      collect_slot_ids(body, out);
    }
    ExprKind::LetVal { cont, .. } => {
      if let Cont::Expr { body, .. } = cont { collect_slot_ids(body, out); }
    }
    ExprKind::LetFn { fn_body, cont, .. } => {
      collect_slot_ids(fn_body, out);
      if let Cont::Expr { body, .. } = cont { collect_slot_ids(body, out); }
    }
    ExprKind::App { args, .. } => {
      for a in args {
        match a {
          Arg::Cont(Cont::Expr { body, .. }) => collect_slot_ids(body, out),
          Arg::Expr(e) => collect_slot_ids(e, out),
          _ => {}
        }
      }
    }
    ExprKind::If { then, else_, .. } => {
      collect_slot_ids(then, out);
      collect_slot_ids(else_, out);
    }
    ExprKind::Set { cont, .. } => {
      if let Cont::Expr { body, .. } = cont { collect_slot_ids(body, out); }
    }
    ExprKind::Closure { cont, .. } => {
      if let Cont::Expr { body, .. } = cont { collect_slot_ids(body, out); }
    }
    ExprKind::LetCaps { binds, cont, .. } => {
      // Per-bind kind check: Bind::Slot marks a captured-slot local
      // whose value is a Cell ref. Convert (TODO) sets this kind when
      // the outer capture source was a slot.
      for b in binds {
        if matches!(b.kind, Bind::Slot) { out.insert(b.id); }
      }
      if let Cont::Expr { body, .. } = cont { collect_slot_ids(body, out); }
    }
  }
}

/// Scan the CPS tree for every `App(BuiltIn::Pub, [Val, Cont])` and
/// return `(exported CpsId, source name)` pairs in encounter order.
/// Mirrors `collect.rs::find_export_app` (the Pub-only branch).
fn find_pub_apps(
  expr: &Expr,
  cps: &CpsResult,
  ast: &Ast<'_>,
  out: &mut Vec<(CpsId, String)>,
) {
  match &expr.kind {
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Pub), args } => {
      // After thread_ctx: Pub args are [ctx, target_val, cont]. Only
      // the target_val (args[1]) is the exported binding; args[0] is
      // the threaded ctx and args[2] is the continuation.
      if let Some(Arg::Val(v)) = args.get(1)
        && let ValKind::Ref(Ref::Synth(id)) = v.kind
      {
        out.push((id, pub_export_name(cps, ast, id)));
      }
      for arg in args {
        if let Arg::Cont(Cont::Expr { body, .. }) = arg {
          find_pub_apps(body, cps, ast, out);
        }
      }
    }
    ExprKind::App { args, .. } => {
      for arg in args {
        match arg {
          Arg::Cont(Cont::Expr { body, .. }) => find_pub_apps(body, cps, ast, out),
          Arg::Expr(e) => find_pub_apps(e, cps, ast, out),
          _ => {}
        }
      }
    }
    ExprKind::LetFn { fn_body, cont, .. } => {
      find_pub_apps(fn_body, cps, ast, out);
      if let Cont::Expr { body, .. } = cont {
        find_pub_apps(body, cps, ast, out);
      }
    }
    ExprKind::LetVal { cont, .. } => {
      if let Cont::Expr { body, .. } = cont {
        find_pub_apps(body, cps, ast, out);
      }
    }
    ExprKind::If { then, else_, .. } => {
      find_pub_apps(then, cps, ast, out);
      find_pub_apps(else_, cps, ast, out);
    }
    ExprKind::LetRec { body, .. } => {
      find_pub_apps(body, cps, ast, out);
    }
    ExprKind::Set { cont, .. } => {
      if let Cont::Expr { body, .. } = cont {
        find_pub_apps(body, cps, ast, out);
      }
    }
    ExprKind::Closure { cont, .. } => {
      if let Cont::Expr { body, .. } = cont {
        find_pub_apps(body, cps, ast, out);
      }
    }
    ExprKind::LetCaps { cont, .. } => {
      if let Cont::Expr { body, .. } = cont {
        find_pub_apps(body, cps, ast, out);
      }
    }
  }
}

fn short_kind(k: &ExprKind) -> &'static str {
  match k {
    ExprKind::LetVal { .. } => "LetVal",
    ExprKind::LetFn { .. } => "LetFn",
    ExprKind::App { .. } => "App",
    ExprKind::If { .. } => "If",
    ExprKind::LetRec { .. } => "LetRec",
    ExprKind::Set { .. } => "Set",
    ExprKind::Closure { .. } => "Closure",
    ExprKind::LetCaps { .. } => "LetCaps",
  }
}

fn short_arg(a: &Arg) -> &'static str {
  match a {
    Arg::Val(_) => "Val",
    Arg::Spread(_) => "Spread",
    Arg::Cont(_) => "Cont",
    Arg::Expr(_) => "Expr",
  }
}

// ──────────────────────────────────────────────────────────────────
// Closure emission
// ──────────────────────────────────────────────────────────────────

/// Emit the instructions for building a closure:
///   `into = struct.new $Closure (ref.func $fn) (<caps-array>)`
/// where `<caps-array>` is either `ref.null $Captures` (no captures)
/// or `array.new_fixed $Captures N cap_ops` (N > 0). Pushes the
/// instructions onto `ctx.instrs` in order.
fn emit_closure_construction(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  fn_sym: FuncSym,
  cap_operands: Vec<Operand>,
  into: LocalIdx,
) {
  emit_closure_construction_inner(lcx, ctx, fn_sym, cap_operands, into, None)
}

/// Emit closure construction with optional self-capture support. If
/// `self_cap_idx` is Some(i), capture slot `i` is a self-reference to
/// the closure being constructed: we use `array.new_default` + per-slot
/// `array.set` so the self slot can be patched with the closure value
/// after struct.new.
fn emit_closure_construction_inner(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  fn_sym: FuncSym,
  cap_operands: Vec<Operand>,
  into: LocalIdx,
  self_cap_idx: Option<usize>,
) {
  let caps_local = ctx.alloc_local_typed(
    ":caps_arg",
    val_ref(lcx.rt.captures(), /*nullable*/ true),
  );
  if cap_operands.is_empty() {
    let i = push_ref_null_concrete(lcx.frag, lcx.rt.captures(), caps_local);
    ctx.instrs.push(i);
  } else if self_cap_idx.is_some() {
    // Build the array with default-null, then array.set each non-self
    // slot. Self slot is left null; patched after struct.new.
    let i_arr = push_array_new_default(
      lcx.frag, lcx.rt.captures(),
      lit_i32(cap_operands.len() as i32), caps_local,
    );
    ctx.instrs.push(i_arr);
    for (i, op) in cap_operands.iter().enumerate() {
      if self_cap_idx == Some(i) { continue; }
      let i_set = push_array_set(
        lcx.frag, lcx.rt.captures(),
        op_local(caps_local), lit_i32(i as i32), op.clone(),
      );
      ctx.instrs.push(i_set);
    }
  } else {
    let i = push_array_new_fixed(lcx.frag, lcx.rt.captures(), cap_operands.clone(), caps_local);
    ctx.instrs.push(i);
  }

  // struct.new $Closure (ref.func $fn, local.get $:caps_arg).
  let struct_instr = push_struct_new(
    lcx.frag,
    lcx.rt.closure(),
    vec![Operand::RefFunc(fn_sym), op_local(caps_local)],
    into,
  );
  ctx.instrs.push(struct_instr);

  // Patch the self slot with the now-built closure.
  if let Some(idx) = self_cap_idx {
    let i_set = push_array_set(
      lcx.frag, lcx.rt.captures(),
      op_local(caps_local), lit_i32(idx as i32), op_local(into),
    );
    ctx.instrs.push(i_set);
  }
}

// FnCtx extension: track LetFn bindings that are used later in
// `App(FnClosure)` to build a $Closure.
impl FnCtx {
  fn fn_sym_for_bind(&mut self, id: CpsId, sym: FuncSym) {
    self.fn_syms.insert(id, sym);
  }
  fn lookup_fn_sym(&self, id: CpsId) -> FuncSym {
    *self.fn_syms.get(&id)
      .unwrap_or_else(|| panic!("lower: FnClosure references CpsId {:?} with no LetFn sym", id))
  }
  fn try_lookup_fn_sym(&self, id: CpsId) -> Option<FuncSym> {
    self.fn_syms.get(&id).copied()
  }
}
