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
  Arg, BindNode, Callable, Cont, CpsId, CpsResult, Expr, ExprKind,
  Lit, Param, ParamInfo, Ref, Val, ValKind, BuiltIn,
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
pub fn lower(cps: &CpsResult, ast: &Ast<'_>, fqn_prefix: &str) -> Fragment {
  let mut usage = runtime_contract::scan(cps);
  // Fn3 / ctx-aware lowering routes user-fn calls through `apply_3`
  // instead of `apply`. Mark the Apply3 runtime symbol so `declare()`
  // sets up the import. The scan already marks Apply (Fn3) — that's
  // harmless here; lower_ctx never emits a call to it.
  usage.mark(runtime_contract::Sym::Apply3);
  usage.mark(runtime_contract::Sym::Fn3);
  // Per-module wrapper synthesised below uses init_module and the
  // closure/captures/str primitives.
  usage.mark(runtime_contract::Sym::ModulesInitModule);
  usage.mark(runtime_contract::Sym::Closure);
  usage.mark(runtime_contract::Sym::Captures);
  usage.mark(runtime_contract::Sym::StrFromData);
  let mut frag = Fragment::default();
  let rt = runtime_contract::declare(&mut frag, &usage);

  // CPS root shape: App(FinkModule, [Cont::Expr { args: [ƒctx, ƒret], body }]).
  let Some((ctx_bind, ret_bind, module_body)) = extract_fink_module_body(&cps.root) else {
    panic!("lower: unsupported CPS root shape (expected App(FinkModule, [Cont::Expr]))");
  };

  // Scan for ·ƒpub apps and pre-allocate one exported (mut anyref)
  // global per exported binding. The Pub arm in lower_expr looks these
  // up by CpsId to emit global.set at the export site.
  let mut pubs: Vec<(CpsId, String)> = Vec::new();
  find_pub_apps(module_body, cps, ast, &mut pubs);
  let mut pub_globals: HashMap<CpsId, (GlobalSym, String)> = HashMap::new();
  for (id, name) in &pubs {
    let qualified = format!("{fqn_prefix}{name}");
    // No WASM-level export. User-binding globals are addressable
    // storage for the registry (`std/modules.fnk:pub`); the host
    // accesses bindings exclusively through the per-module host
    // wrapper export, which routes via `init_module` + the
    // registry. The bare globals stay because lifted closures read
    // them at module-init time (forward-reference machinery).
    let sym = add_global(
      &mut frag,
      val_anyref(true),
      true,
      GlobalInit::RefNull(AbsHeap::Any),
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
  {
    let mut lcx = LowerCtx {
      cps, ast, rt: &rt, frag: &mut frag,
      pub_globals: &pub_globals, fqn_prefix,
      bind_kinds: &bind_kinds,
    };
    let fink_module = lower_fn(
      &mut lcx,
      &[],                 // no cap params at the module level
      &[(ctx_bind, false), (ret_bind, false)], // user params: ƒctx, ƒret
      module_body,
      &module_display,
      &HashMap::new(),    // module body: no enclosing fn_syms
    );
    let FuncSym::Local(_) = fink_module else { panic!("lower: fink_module must be Local"); };
    // No WASM-level export for fink_module — host accesses the module
    // exclusively through the per-module wrapper exported under the
    // canonical FQN. fink_module stays as a bare internal func; the
    // wrapper holds a no-capture closure over it.

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
}

// ──────────────────────────────────────────────────────────────────
// Function lowering
// ──────────────────────────────────────────────────────────────────

/// Lower a CPS function — either the module body or a LetFn'd helper.
///
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

  // Unpack captures from $:caps_param into locals. Emits once:
  //   local.set $:caps_cast (ref.cast (ref $Captures) $:caps_param)
  // then per-capture:
  //   local.set $<cap_name> (array.get $Captures $:caps_cast <i>)
  if !cap_params.is_empty() {
    let caps_cast = ctx.alloc_local_typed(
      ":caps_cast",
      val_ref(lcx.rt.captures(), /*nullable*/ false),
    );
    let i_cast = push_ref_cast_non_null(
      lcx.frag, lcx.rt.captures(), op_local(l_caps_p), caps_cast,
    );
    ctx.instrs.push(i_cast);
    for (i, cap_id) in cap_params.iter().enumerate() {
      let name = cps_ident(lcx.cps, lcx.ast, *cap_id);
      let local = ctx.alloc_local(&name);
      ctx.bind(*cap_id, local);
      let i_get = push_array_get(
        lcx.frag, lcx.rt.captures(),
        op_local(caps_cast), lit_i32(i as i32),
        local,
      );
      ctx.instrs.push(i_get);
    }
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
  if let Some(local) = ctx.binds.get(&id).copied() {
    return op_local(local);
  }
  if let Some(&(gsym, _)) = lcx.pub_globals.get(&id) {
    return op_global(gsym);
  }
  if let Some(fn_sym) = ctx.try_lookup_fn_sym(id) {
    let local = ctx.alloc_local(&format!("v_{}_fn", id.0));
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

    ExprKind::LetFn { name, params, fn_body, cont, .. } => {
      // Collect cap + user params by role. User params carry their
      // spread flag through to lower_fn so the prologue can emit the
      // right `args_head`/`args_tail`/spread sequence.
      let mut cap_ids: Vec<CpsId> = Vec::new();
      let mut user_ids: Vec<(CpsId, bool)> = Vec::new();
      for p in params {
        let (pid, is_spread) = match p {
          Param::Name(b)   => (b.id, false),
          Param::Spread(b) => (b.id, true),
        };
        match lcx.cps.param_info.try_get(pid).and_then(|o| *o) {
          Some(ParamInfo::Cap(_))  => cap_ids.push(pid),
          Some(ParamInfo::Param(_)) | Some(ParamInfo::Cont) => user_ids.push((pid, is_spread)),
          None => user_ids.push((pid, is_spread)),  // ungilded params treated as user
        }
      }
      // Lift the fn body to a separate Fn3. Display name carries the
      // module's FQN prefix so cross-fragment merges stay collision-free.
      let raw_display = cps_ident_for_bind(lcx.cps, lcx.ast, name);
      let display = format!("{}{}", lcx.fqn_prefix, raw_display);
      let fn_sym = lower_fn(
        lcx,
        &cap_ids, &user_ids, fn_body, &display,
        &ctx.fn_syms,
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
      // `·ƒpub val, cont` — register `val` as a module-level export.
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
      let Some(Arg::Val(val)) = args.first() else {
        panic!("lower: Pub expects [val, cont], missing val");
      };
      let id = cps_id_of_ref(val);
      let (gsym, src_name) = lcx.pub_globals.get(&id)
        .cloned()
        .unwrap_or_else(|| panic!("lower: Pub val CpsId {:?} has no pre-allocated global", id));
      let val_local = ctx.lookup(id);

      // 1. Addressable storage.
      let i_set = push_global_set(lcx.frag, gsym, op_local(val_local));
      ctx.instrs.push(i_set);

      // 2. Registry mutation.
      let url_bytes: Vec<u8> = lcx.fqn_prefix.trim_end_matches(':').as_bytes().to_vec();
      let url_local = emit_str_const(lcx, ctx, &url_bytes, ":pub_url");
      let name_local = emit_str_const(lcx, ctx, src_name.as_bytes(), ":pub_name");
      let i_pub = push_call(lcx.frag, lcx.rt.modules_pub(),
        vec![op_local(url_local), op_local(name_local), op_local(val_local)],
        None);
      ctx.instrs.push(i_pub);

      let cont_arg = args.get(1)
        .unwrap_or_else(|| panic!("lower: Pub expects [val, cont]"));
      let Arg::Cont(cont) = cont_arg else {
        panic!("lower: Pub cont arg is not a Cont");
      };
      lower_cont(lcx, ctx, cont);
    }

    ExprKind::App { func: Callable::BuiltIn(b), args } if binary_op_sym(*b).is_some() => {
      let sym = binary_op_sym(*b).unwrap();
      let (a, b_v, cont) = split_binary_args(args);
      let a_op = emit_arg_as_operand(lcx, ctx, a);
      let b_op = emit_arg_as_operand(lcx, ctx, b_v);
      emit_op_tail_call(lcx, ctx, sym, vec![a_op, b_op], cont, expr.id);
    }

    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Not), args } => {
      let (v, cont) = split_unary_args(args);
      let v_op = emit_arg_as_operand(lcx, ctx, v);
      emit_op_tail_call(lcx, ctx, Sym::OpNot, vec![v_op], cont, expr.id);
    }

    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Empty), args } => {
      let (v, cont) = split_unary_args(args);
      let v_op = emit_arg_as_operand(lcx, ctx, v);
      emit_op_tail_call(lcx, ctx, Sym::OpEmpty, vec![v_op], cont, expr.id);
    }

    ExprKind::App { func: Callable::BuiltIn(BuiltIn::RangeFrom), args } => {
      let (v, cont) = split_unary_args(args);
      let v_op = emit_arg_as_operand(lcx, ctx, v);
      emit_op_tail_call(lcx, ctx, Sym::OpRngFrom, vec![v_op], cont, expr.id);
    }

    // Cooperative-multitasking + channels — all `(value, cont)` shape,
    // same as `BuiltIn::Not` / `BuiltIn::Empty`. Side effects happen in
    // the runtime function (queue manipulation, host channel I/O); the
    // user-facing call shape is plain unary.
    ExprKind::App { func: Callable::BuiltIn(b), args }
      if matches!(b, BuiltIn::Yield | BuiltIn::Spawn | BuiltIn::Await
                   | BuiltIn::Channel | BuiltIn::Receive) =>
    {
      let sym = match b {
        BuiltIn::Yield   => Sym::Yield,
        BuiltIn::Spawn   => Sym::Spawn,
        BuiltIn::Await   => Sym::Await,
        BuiltIn::Channel => Sym::Channel,
        BuiltIn::Receive => Sym::Receive,
        _ => unreachable!(),
      };
      let (v, cont) = split_unary_args(args);
      let v_op = emit_arg_as_operand(lcx, ctx, v);
      emit_op_tail_call(lcx, ctx, sym, vec![v_op], cont, expr.id);
    }

    // StrMatch: `(subj, prefix, suffix, fail, succ)` — 5-arg template
    // pattern dispatch. All five are anyref operands at the WASM level
    // (the latter two are continuations resolved as closures).
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::StrMatch), args } => {
      if args.len() != 5 {
        panic!("lower: StrMatch expects 5 args, got {}", args.len());
      }
      let ctx_local = ctx.ctx_local.expect("lower StrMatch: enclosing fn must have :ctx_param");
      let mut ops: Vec<Operand> = vec![op_local(ctx_local)];
      ops.extend(args.iter().map(|a| emit_arg_as_operand(lcx, ctx, a)));
      let i = push_return_call(lcx.frag, lcx.rt.str_match(), ops);
      if let Some(o) = origin_of(lcx.cps, lcx.ast, expr.id) { set_origin(lcx.frag, i, o); }
      set_cps_id(lcx.frag, i, expr.id);
      ctx.instrs.push(i);
    }

    // StrFmt: `(seg_0, seg_1, ..., seg_n, cont)` — build a $VarArgs
    // array from the segments and tail-call $str_fmt(varargs, cont).
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::StrFmt), args } => {
      // Last arg is the cont; the rest are value segments.
      let (cont, segments) = split_last_cont(args);
      let seg_ops: Vec<Operand> = segments.iter()
        .map(|a| emit_arg_as_operand(lcx, ctx, a))
        .collect();
      // Allocate the $VarArgs array.
      let varargs_local = ctx.alloc_local_typed(":varargs",
        val_ref(lcx.rt.varargs(), /*nullable*/ true));
      let i_arr = push_array_new_fixed(lcx.frag, lcx.rt.varargs(), seg_ops, varargs_local);
      ctx.instrs.push(i_arr);
      // Wrap as Arg::Val for emit_op_tail_call's cont handling.
      emit_op_tail_call(lcx, ctx,
        Sym::StrFmt, vec![op_local(varargs_local)], &Arg::Cont(cont.clone()),
        expr.id);
    }

    // SeqPrepend: `(item, seq, cont)` — same call shape as a binary
    // protocol op. Lowers to `return_call $seq_prepend item seq cont`.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::SeqPrepend), args } => {
      let (a, b, cont) = split_binary_args(args);
      let a_op = emit_arg_as_operand(lcx, ctx, a);
      let b_op = emit_arg_as_operand(lcx, ctx, b);
      emit_op_tail_call(lcx, ctx, Sym::SeqPrepend, vec![a_op, b_op], cont, expr.id);
    }

    // SeqConcat: `(a, b, cont)` — same call shape as SeqPrepend. Used
    // for list literals containing a spread (`[..xs, y]`, `[..a, ..b]`).
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::SeqConcat), args } => {
      let (a, b, cont) = split_binary_args(args);
      let a_op = emit_arg_as_operand(lcx, ctx, a);
      let b_op = emit_arg_as_operand(lcx, ctx, b);
      emit_op_tail_call(lcx, ctx, Sym::SeqConcat, vec![a_op, b_op], cont, expr.id);
    }

    // RecMerge: `(dest, src, cont)` — same shape as SeqPrepend.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::RecMerge), args } => {
      let (a, b, cont) = split_binary_args(args);
      let a_op = emit_arg_as_operand(lcx, ctx, a);
      let b_op = emit_arg_as_operand(lcx, ctx, b);
      emit_op_tail_call(lcx, ctx, Sym::RecMerge, vec![a_op, b_op], cont, expr.id);
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
      emit_quaternary(lcx, ctx, target, args, expr.id);
    }
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::RecPop), args } => {
      let target = lcx.rt.rec_pop();
      emit_quaternary(lcx, ctx, target, args, expr.id);
    }

    // Panic: zero-arg sentinel for irrefutable-pattern failure. We
    // emit `unreachable` directly — the runtime panic helper isn't
    // wired through `Sym` yet, and an unreachable trap is acceptable
    // at this level (matches the old emitter's fallback shape).
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Panic), .. } => {
      let i = push_unreachable(lcx.frag);
      ctx.instrs.push(i);
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
          let clo_local = ctx.alloc_local(&format!("v_{}_clo", cont_id.0));
          emit_closure_construction(lcx, ctx, fn_sym, cap_operands, clo_local);

          // Resolve cont; spill if non-local.
          let callee_op = resolve_id_as_operand(lcx, ctx, *cont_id);
          let callee = match callee_op {
            Operand::Local(l) => l,
            other => {
              let local = ctx.alloc_local(&format!("v_{}_callee", cont_id.0));
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
          let local = ctx.alloc_local(&format!("v_{}_callee", cont_id.0));
          let i = push_local_set(lcx.frag, local, other);
          ctx.instrs.push(i);
          local
        }
      };

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
          let local = ctx.alloc_local(&format!("v_{}_callee", callee_id.0));
          let i = push_local_set(lcx.frag, local, other);
          ctx.instrs.push(i);
          local
        }
      };

      let (ctx_op, rest_args) = split_ctx_arg(lcx, ctx, args);
      let l_args_list = build_args_list(lcx, ctx, rest_args);
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

  // Pull the destructure cont (Cont::Ref).
  let cont_id = args.iter().find_map(|a| match a {
    Arg::Cont(Cont::Ref(id)) => Some(*id),
    _ => None,
  }).unwrap_or_else(|| panic!("lower: BuiltIn::Import missing cont"));

  if crate::passes::wasm::compile_package::MIGRATED_STDLIB_FNK.contains(&url) {
    lower_import_user_fragment(lcx, ctx, url, cont_id);
  } else if is_virtual_stdlib_path(url) || url.ends_with(".wat") {
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

    // 1c. Build the $Str key for the field name.
    let l_key = ctx.alloc_local(&format!(":imp_key_{name}"));
    let key_bytes = name.as_bytes();
    let key_sym = intern_data(lcx.frag, key_bytes);
    let i_key = push_call(lcx.frag, lcx.rt.str_from_data(),
      vec![Operand::DataRef { sym: key_sym, len: key_bytes.len() as u32 }],
      Some(l_key));
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
      let l = ctx.alloc_local(&format!("v_{}_callee", cont_id.0));
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
      let l = ctx.alloc_local(&format!("v_{}_callee", cont_id.0));
      let i = push_local_set(lcx.frag, l, other);
      ctx.instrs.push(i);
      l
    }
  };

  // 5. Tail-call `std/modules.fnk:import (url, mod_clos, cont)`. The
  //    runtime helper handles init-once + delivers the producer's
  //    exports rec to the destructure cont.
  let i_imp = push_return_call(lcx.frag, lcx.rt.modules_import(),
    vec![op_local(url_local), op_local(mod_clos_local), op_local(cont_local)]);
  ctx.instrs.push(i_imp);
}

/// Recognise virtual stdlib namespace paths. Today only `std/*.fnk`,
/// but the predicate is named generically so future virtual namespaces
/// (`@fink/meta`, language-version-locked stdlib variants) extend the
/// same matcher.
fn is_virtual_stdlib_path(url: &str) -> bool {
  url.starts_with("std/") && url.ends_with(".fnk")
}


/// Emit a 4-arg primitive with shape `(any, any, any, any) -> ()`.
/// Used by `RecPut(rec, key, val, cont)` and
/// `RecPop(rec, key, fail, succ)`. Each arg is an anyref value; conts
/// among them get resolved through the unified id resolver.
fn emit_quaternary(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  target: FuncSym,
  args: &[Arg],
  app_id: CpsId,
) {
  if args.len() != 4 {
    panic!("lower: 4-arg primitive expects 4 args, got {}", args.len());
  }
  let ctx_local = ctx.ctx_local.expect("emit_quaternary: enclosing fn must have :ctx_param");
  let mut ops: Vec<Operand> = vec![op_local(ctx_local)];
  ops.extend(args.iter().map(|a| emit_arg_as_operand(lcx, ctx, a)));
  let i = push_return_call(lcx.frag, target, ops);
  if let Some(o) = origin_of(lcx.cps, lcx.ast, app_id) { set_origin(lcx.frag, i, o); }
  set_cps_id(lcx.frag, i, app_id);
  ctx.instrs.push(i);
}

/// Emit a `(value, cont, cont)` ternary primitive (IsSeqLike,
/// IsRecLike, SeqPop). The runtime function takes 3 anyref params:
/// the value being tested, plus two continuations resolved as
/// values at this layer.
fn emit_ternary_guard(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  sym: Sym,
  args: &[Arg],
  app_id: CpsId,
) {
  if args.len() != 3 {
    panic!("lower: ternary primitive {:?} expects 3 args, got {}", sym, args.len());
  }
  let val_op = emit_arg_as_operand(lcx, ctx, &args[0]);
  let cont1_op = emit_arg_as_operand(lcx, ctx, &args[1]);
  let cont2_op = emit_arg_as_operand(lcx, ctx, &args[2]);
  let ctx_local = ctx.ctx_local.expect("emit_ternary_guard: enclosing fn must have :ctx_param");
  let i = push_return_call(lcx.frag, lcx.rt.op(sym), vec![op_local(ctx_local), val_op, cont1_op, cont2_op]);
  if let Some(o) = origin_of(lcx.cps, lcx.ast, app_id) { set_origin(lcx.frag, i, o); }
  set_cps_id(lcx.frag, i, app_id);
  ctx.instrs.push(i);
}

fn emit_op_tail_call(
  lcx: &mut LowerCtx<'_>,
  ctx: &mut FnCtx,
  sym: Sym,
  value_operands: Vec<Operand>,
  cont: &Arg,
  app_id: CpsId,
) {
  let cont_op = match cont {
    Arg::Cont(Cont::Ref(id)) => resolve_id_as_operand(lcx, ctx, *id),
    Arg::Val(v) => val_as_operand(lcx, ctx, v),
    _ => panic!("lower: operator cont is neither Cont::Ref nor Val (got {:?})", short_arg(cont)),
  };
  let ctx_local = ctx.ctx_local.expect("emit_op_tail_call: enclosing fn must have :ctx_param");
  let mut operands = vec![op_local(ctx_local)];
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
          let local = ctx.alloc_local(&format!("v_{}", v.id.0));
          let i = box_lit(lcx.frag, lcx.rt, &lv, local);
          if let Some(o) = origin_of(lcx.cps, lcx.ast, v.id) { set_origin(lcx.frag, i, o); }
          ctx.instrs.push(i);
          op_local(local)
        }
        ValKind::Ref(r) => resolve_id_as_operand(lcx, ctx, ref_cps_id(*r)),
        ValKind::ContRef(id) => resolve_id_as_operand(lcx, ctx, *id),
        ValKind::BuiltIn(BuiltIn::Panic) => panic_closure_operand(lcx, ctx),
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
/// function. Used when `BuiltIn::Panic` appears in value position
/// (typically as the fail continuation in pattern-match dispatch
/// generated by the lifting pass).
fn panic_closure_operand(lcx: &mut LowerCtx<'_>, ctx: &mut FnCtx) -> Operand {
  let local = ctx.alloc_local("v_panic_clo");
  let caps_local = ctx.alloc_local_typed(":caps_arg",
    val_ref(lcx.rt.captures(), /*nullable*/ true));
  let i_caps = push_ref_null_concrete(lcx.frag, lcx.rt.captures(), caps_local);
  ctx.instrs.push(i_caps);
  let i_clo = push_struct_new(lcx.frag, lcx.rt.closure(),
    vec![Operand::RefFunc(lcx.rt.panic()), op_local(caps_local)],
    local);
  ctx.instrs.push(i_clo);
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
    ValKind::BuiltIn(BuiltIn::Panic) => panic_closure_operand(lcx, ctx),
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
  let ExprKind::App { func: Callable::BuiltIn(BuiltIn::FinkModule), args } = &root.kind else {
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

fn split_binary_args(args: &[Arg]) -> (&Arg, &Arg, &Arg) {
  (
    args.first().expect("binary op: missing arg 0"),
    args.get(1).expect("binary op: missing arg 1"),
    args.get(2).expect("binary op: missing cont"),
  )
}

fn split_unary_args(args: &[Arg]) -> (&Arg, &Arg) {
  (
    args.first().expect("unary op: missing arg"),
    args.get(1).expect("unary op: missing cont"),
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
  // BindNode carries kind directly, so we don't need bind_kinds here —
  // special-case kinds that don't map to AST origins.
  match b.kind {
    crate::passes::cps::ir::Bind::Ctx => format!(":ctx_{}", b.id.0),
    _ => cps_ident(cps, ast, b.id),
  }
}

/// Derive a display name for a CPS bind/ref. Uses the source ident
/// from the origin map (`{ident}_{id}`) when available, falls back to
/// `v_<id>`. Mirrors `collect.rs::label`. Special bind kinds with no
/// AST origin (e.g. `Bind::Ctx`) get descriptive synth names instead
/// of the generic `v_<id>` fallback.
fn cps_ident(cps: &CpsResult, ast: &Ast<'_>, id: CpsId) -> String {
  let ast_id = cps.origin.try_get(id).and_then(|o| *o);
  match ast_id {
    Some(a) => match &ast.nodes.get(a).kind {
      crate::ast::NodeKind::Ident(s) => format!("{}_{}", s, id.0),
      _ => format!("v_{}", id.0),
    },
    None => format!("v_{}", id.0),
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
  if let Some(Some(crate::passes::cps::ir::Bind::Ctx)) = bind_kinds.try_get(id) {
    return format!(":ctx_{}", id.0);
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
      for arg in args {
        if let Arg::Val(v) = arg
          && let ValKind::Ref(Ref::Synth(id)) = v.kind
        {
          out.push((id, pub_export_name(cps, ast, id)));
        }
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
  }
}

fn short_kind(k: &ExprKind) -> &'static str {
  match k {
    ExprKind::LetVal { .. } => "LetVal",
    ExprKind::LetFn { .. } => "LetFn",
    ExprKind::App { .. } => "App",
    ExprKind::If { .. } => "If",
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
