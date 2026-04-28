// Lower-pass functions thread a lot of context (FnCtx, Fragment, Runtime,
// args, conts, ids) — the >7-arg shape is intentional for inlining and
// cache locality.
#![allow(clippy::too_many_arguments)]

//! CPS → unlinked wasm IR `Fragment`.
//!
//! Tracer-phase walker. Handles the *lifted* CPS shape — all
//! closures are already materialised as `LetFn` + `App(FnClosure)`,
//! so this pass doesn't do free-var analysis.
//!
//! Current coverage:
//! * `main = fn: <lit>` — apply-path via `_apply` (Lit ∈ {Int, Float,
//!   Decimal, Bool}).
//! * Binary + unary protocol operators (Add..Shr, Rngex/Rngin, In,
//!   NotIn, Get, Not).
//! * `LetVal` — value bindings visible in cont body.
//! * `LetFn` — lifted function definitions (separate WASM funcs).
//! * `App(FnClosure, ...)` — closure construction
//!   (`struct.new $Closure (funcref + captures array)`).
//! * `App(Pub, [val, cont])` — no-op: val ignored, cont body emitted.
//! * `App(Callable::Val(Ref), [..., Cont::Ref(_)])` — closure
//!   dispatch via `_apply`.
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
  let usage = runtime_contract::scan(cps);
  let mut frag = Fragment::default();
  let rt = runtime_contract::declare(&mut frag, &usage);

  // CPS root shape: App(FinkModule, [Cont::Expr { args: [ƒret], body }]).
  let Some((ret_bind, module_body)) = extract_fink_module_body(&cps.root) else {
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
    // TODO: drop the WASM-level export once the runner stops
    // consuming user globals via `instance.get_global(name)`. The
    // export is currently the only path for the host to fetch
    // `<entry>:main`. Plan: route via `interop/rust.wat` (host-side
    // registry lookup) — see scratch plan.
    let sym = add_global(
      &mut frag,
      val_anyref(true),
      true,
      GlobalInit::RefNull(AbsHeap::Any),
      &qualified,
      Some(qualified.clone()),
    );
    pub_globals.insert(*id, (sym, name.clone()));
  }

  // Lower the module body as the `fink_module` function.
  let module_display = format!("{fqn_prefix}fink_module");
  let fink_module = lower_fn(
    &mut frag, &rt, cps, ast,
    &[],                 // no cap params at the module level
    &[(ret_bind, false)], // user param: ƒret (not a spread)
    module_body,
    &module_display,
    &pub_globals,
    &HashMap::new(),    // module body: no enclosing fn_syms
    fqn_prefix,         // namespacing prefix for nested LetFn displays
  );
  let FuncSym::Local(i) = fink_module else { panic!("lower: fink_module must be Local"); };
  frag.funcs[i as usize].export = Some("fink_module".into());

  frag
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
  frag: &mut Fragment,
  rt: &Runtime,
  cps: &CpsResult,
  ast: &Ast<'_>,
  cap_params: &[CpsId],
  user_params: &[(CpsId, bool)],
  body: &Expr,
  display: &str,
  pub_globals: &HashMap<CpsId, (GlobalSym, String)>,
  fn_syms: &HashMap<CpsId, FuncSym>,
  fqn_prefix: &str,
) -> FuncSym {
  let mut ctx = FnCtx::new(pub_globals.clone(), fn_syms.clone(), fqn_prefix.to_string());

  // WASM-level params (always just `$:caps_param` and `$:params` —
  // the $Fn2 shape). Colon-prefix is lexer-rejected in Fink source,
  // so these synth names can never collide with user bindings.
  let l_caps_p = ctx.alloc_param(":caps_param");
  let l_args_p = ctx.alloc_param(":params");

  // Unpack captures from $:caps_param into locals. Emits once:
  //   local.set $:caps_cast (ref.cast (ref $Captures) $:caps_param)
  // then per-capture:
  //   local.set $<cap_name> (array.get $Captures $:caps_cast <i>)
  if !cap_params.is_empty() {
    let caps_cast = ctx.alloc_local_typed(
      ":caps_cast",
      val_ref(rt.captures(), /*nullable*/ false),
    );
    let i_cast = push_ref_cast_non_null(
      frag, rt.captures(), op_local(l_caps_p), caps_cast,
    );
    ctx.instrs.push(i_cast);
    for (i, cap_id) in cap_params.iter().enumerate() {
      let name = cps_ident(cps, ast, *cap_id);
      let local = ctx.alloc_local(&name);
      ctx.bind(*cap_id, local);
      let i_get = push_array_get(
        frag, rt.captures(),
        op_local(caps_cast), op_i32(i as i32),
        local,
      );
      ctx.instrs.push(i_get);
    }
  }

  // Unpack user params from $:params by walking `args_head` / `args_tail`.
  //
  // We use $:params itself as the cursor: each peel does
  //   <local> = args_head($:params)
  //   $:params = args_tail($:params)        (skipped after the last peel)
  // overwriting the param slot in place. This mirrors the old emitter's
  // approach (see `emit.rs:1428-1446`) and avoids needing a separate
  // cursor local.
  //
  // A trailing `Spread` param consumes the remaining tail directly: no
  // `args_head`/`args_tail` for it — we just bind its local to the
  // current $:params cursor.
  let n = user_params.len();
  for (j, &(pid, is_spread)) in user_params.iter().enumerate() {
    let name = cps_ident(cps, ast, pid);
    let local = ctx.alloc_local(&name);
    ctx.bind(pid, local);
    if is_spread {
      // Spread takes whatever's left in $:params as-is. No `args_head` —
      // the spread local *is* the residual list. (Spread must be last,
      // enforced by the parser/CPS, so no further peeling follows.)
      let i = push_local_set(frag, local, op_local(l_args_p));
      ctx.instrs.push(i);
    } else {
      let i = push_call(frag, rt.args_head(), vec![op_local(l_args_p)], Some(local));
      ctx.instrs.push(i);
      // Advance the cursor unless this is the last entry (no more peels).
      if j + 1 < n {
        let i = push_call(frag, rt.args_tail(), vec![op_local(l_args_p)], Some(l_args_p));
        ctx.instrs.push(i);
      }
    }
  }

  // Walk the body.
  lower_expr(&mut ctx, frag, rt, cps, ast, body);

  // Build the function.
  func(frag, rt.fn2(),
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
  /// Module-level exports: CpsId → (pre-allocated GlobalSym, source
  /// binding name). Used by the `Pub` arm to emit `global.set` at the
  /// export site, plus the `std/modules.fnk:pub` runtime call which
  /// takes the binding name as a `$Str` arg. Shared across all fns
  /// in a module (cloned on FnCtx construction).
  pub_globals: HashMap<CpsId, (GlobalSym, String)>,
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
  /// FQN prefix for emitted symbol display names. Empty for single-
  /// fragment compiles; `"<canonical_url>:"` for multi-fragment package
  /// compiles. See `lower()` doc.
  fqn_prefix: String,
}

impl FnCtx {
  fn new(
    pub_globals: HashMap<CpsId, (GlobalSym, String)>,
    fn_syms: HashMap<CpsId, FuncSym>,
    fqn_prefix: String,
  ) -> Self {
    Self {
      params: Vec::new(),
      locals: Vec::new(),
      instrs: Vec::new(),
      binds: HashMap::new(),
      next_local_idx: 0,
      pub_globals,
      fn_syms,
      fqn_prefix,
    }
  }

  fn alloc_param(&mut self, name: &str) -> LocalIdx {
    let idx = LocalIdx(self.next_local_idx);
    self.next_local_idx += 1;
    self.params.push(local(val_anyref(true), name));
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
  ctx: &mut FnCtx,
  frag: &mut Fragment,
  rt: &Runtime,
  id: CpsId,
) -> Operand {
  if let Some(local) = ctx.binds.get(&id).copied() {
    return op_local(local);
  }
  if let Some(&(gsym, _)) = ctx.pub_globals.get(&id) {
    return op_global(gsym);
  }
  if let Some(fn_sym) = ctx.try_lookup_fn_sym(id) {
    let local = ctx.alloc_local(&format!("v_{}_fn", id.0));
    let caps_local = ctx.alloc_local_typed(
      ":caps_arg",
      val_ref(rt.captures(), /*nullable*/ true),
    );
    let i_caps = push_ref_null_concrete(frag, rt.captures(), caps_local);
    ctx.instrs.push(i_caps);
    let i_clo = push_struct_new(
      frag, rt.closure(),
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
  ctx: &mut FnCtx,
  frag: &mut Fragment,
  rt: &Runtime,
  cps: &CpsResult,
  ast: &Ast<'_>,
  expr: &Expr,
) {
  match &expr.kind {
    ExprKind::LetVal { name, val, cont } => {
      let local = ctx.alloc_local(&cps_ident_for_bind(cps, ast, name));
      ctx.bind(name.id, local);
      let i = emit_val_into(ctx, frag, rt, cps, ast, val, local);
      if let Some(o) = origin_of(cps, ast, name.id) { set_origin(frag, i, o); }
      ctx.instrs.push(i);
      lower_cont(ctx, frag, rt, cps, ast, cont);
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
        match cps.param_info.try_get(pid).and_then(|o| *o) {
          Some(ParamInfo::Cap(_))  => cap_ids.push(pid),
          Some(ParamInfo::Param(_)) | Some(ParamInfo::Cont) => user_ids.push((pid, is_spread)),
          None => user_ids.push((pid, is_spread)),  // ungilded params treated as user
        }
      }
      // Lift the fn body to a separate Fn2. Display name carries the
      // module's FQN prefix so cross-fragment merges stay collision-free.
      let raw_display = cps_ident_for_bind(cps, ast, name);
      let display = format!("{}{}", ctx.fqn_prefix, raw_display);
      let fn_sym = lower_fn(
        frag, rt, cps, ast,
        &cap_ids, &user_ids, fn_body, &display,
        &ctx.pub_globals, &ctx.fn_syms,
        &ctx.fqn_prefix,
      );
      // The LetFn binds `name.id` to a funcref-valued local; we model
      // it as an anyref local holding a `ref.func` funcref. Actual
      // `ref.func` emission happens at the LetVal(FnClosure) site
      // where this funcref is used as the first arg. So we just
      // remember the FuncSym here.
      ctx.fn_sym_for_bind(name.id, fn_sym);
      lower_cont(ctx, frag, rt, cps, ast, cont);
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
      // The fqn url is `ctx.fqn_prefix` minus the trailing `:` separator;
      // the source name comes from `pub_globals` alongside the global.
      let Some(Arg::Val(val)) = args.first() else {
        panic!("lower: Pub expects [val, cont], missing val");
      };
      let id = cps_id_of_ref(val);
      let (gsym, src_name) = ctx.pub_globals.get(&id)
        .cloned()
        .unwrap_or_else(|| panic!("lower: Pub val CpsId {:?} has no pre-allocated global", id));
      let val_local = ctx.lookup(id);

      // 1. Addressable storage.
      let i_set = push_global_set(frag, gsym, op_local(val_local));
      ctx.instrs.push(i_set);

      // 2. Registry mutation.
      let url_bytes: Vec<u8> = ctx.fqn_prefix.trim_end_matches(':').as_bytes().to_vec();
      let url_local = emit_str_const(ctx, frag, rt, &url_bytes, ":pub_url");
      let name_local = emit_str_const(ctx, frag, rt, src_name.as_bytes(), ":pub_name");
      let i_pub = push_call(frag, rt.modules_pub(),
        vec![op_local(url_local), op_local(name_local), op_local(val_local)],
        None);
      ctx.instrs.push(i_pub);

      let cont_arg = args.get(1)
        .unwrap_or_else(|| panic!("lower: Pub expects [val, cont]"));
      let Arg::Cont(cont) = cont_arg else {
        panic!("lower: Pub cont arg is not a Cont");
      };
      lower_cont(ctx, frag, rt, cps, ast, cont);
    }

    ExprKind::App { func: Callable::BuiltIn(b), args } if binary_op_sym(*b).is_some() => {
      let sym = binary_op_sym(*b).unwrap();
      let (a, b_v, cont) = split_binary_args(args);
      let a_op = emit_arg_as_operand(ctx, frag, rt, cps, ast, a);
      let b_op = emit_arg_as_operand(ctx, frag, rt, cps, ast, b_v);
      emit_op_tail_call(ctx, frag, rt, cps, ast, sym, vec![a_op, b_op], cont, expr.id);
    }

    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Not), args } => {
      let (v, cont) = split_unary_args(args);
      let v_op = emit_arg_as_operand(ctx, frag, rt, cps, ast, v);
      emit_op_tail_call(ctx, frag, rt, cps, ast, Sym::OpNot, vec![v_op], cont, expr.id);
    }

    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Empty), args } => {
      let (v, cont) = split_unary_args(args);
      let v_op = emit_arg_as_operand(ctx, frag, rt, cps, ast, v);
      emit_op_tail_call(ctx, frag, rt, cps, ast, Sym::OpEmpty, vec![v_op], cont, expr.id);
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
      let v_op = emit_arg_as_operand(ctx, frag, rt, cps, ast, v);
      emit_op_tail_call(ctx, frag, rt, cps, ast, sym, vec![v_op], cont, expr.id);
    }

    // StrMatch: `(subj, prefix, suffix, fail, succ)` — 5-arg template
    // pattern dispatch. All five are anyref operands at the WASM level
    // (the latter two are continuations resolved as closures).
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::StrMatch), args } => {
      if args.len() != 5 {
        panic!("lower: StrMatch expects 5 args, got {}", args.len());
      }
      let ops: Vec<Operand> = args.iter()
        .map(|a| emit_arg_as_operand(ctx, frag, rt, cps, ast, a))
        .collect();
      let i = push_return_call(frag, rt.str_match(), ops);
      if let Some(o) = origin_of(cps, ast, expr.id) { set_origin(frag, i, o); }
      ctx.instrs.push(i);
    }

    // StrFmt: `(seg_0, seg_1, ..., seg_n, cont)` — build a $VarArgs
    // array from the segments and tail-call $str_fmt(varargs, cont).
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::StrFmt), args } => {
      // Last arg is the cont; the rest are value segments.
      let (cont, segments) = split_last_cont(args);
      let seg_ops: Vec<Operand> = segments.iter()
        .map(|a| emit_arg_as_operand(ctx, frag, rt, cps, ast, a))
        .collect();
      // Allocate the $VarArgs array.
      let varargs_local = ctx.alloc_local_typed(":varargs",
        val_ref(rt.varargs(), /*nullable*/ true));
      let i_arr = push_array_new_fixed(frag, rt.varargs(), seg_ops, varargs_local);
      ctx.instrs.push(i_arr);
      // Wrap as Arg::Val for emit_op_tail_call's cont handling.
      emit_op_tail_call(ctx, frag, rt, cps, ast,
        Sym::StrFmt, vec![op_local(varargs_local)], &Arg::Cont(cont.clone()),
        expr.id);
    }

    // SeqPrepend: `(item, seq, cont)` — same call shape as a binary
    // protocol op. Lowers to `return_call $seq_prepend item seq cont`.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::SeqPrepend), args } => {
      let (a, b, cont) = split_binary_args(args);
      let a_op = emit_arg_as_operand(ctx, frag, rt, cps, ast, a);
      let b_op = emit_arg_as_operand(ctx, frag, rt, cps, ast, b);
      emit_op_tail_call(ctx, frag, rt, cps, ast, Sym::SeqPrepend, vec![a_op, b_op], cont, expr.id);
    }

    // RecMerge: `(dest, src, cont)` — same shape as SeqPrepend.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::RecMerge), args } => {
      let (a, b, cont) = split_binary_args(args);
      let a_op = emit_arg_as_operand(ctx, frag, rt, cps, ast, a);
      let b_op = emit_arg_as_operand(ctx, frag, rt, cps, ast, b);
      emit_op_tail_call(ctx, frag, rt, cps, ast, Sym::RecMerge, vec![a_op, b_op], cont, expr.id);
    }

    // IsSeqLike / IsRecLike: `(val, succ, fail)` — type guard. The
    // succ/fail args are continuations (Cont::Ref or Cont::Expr).
    // We resolve each as an operand (cont = value-like at WASM level)
    // and emit `return_call $<sym> val succ fail`.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::IsSeqLike), args } => {
      emit_ternary_guard(ctx, frag, rt, cps, ast, Sym::IsSeqLike, args, expr.id);
    }
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::IsRecLike), args } => {
      emit_ternary_guard(ctx, frag, rt, cps, ast, Sym::IsRecLike, args, expr.id);
    }
    // SeqPop: `(seq, fail, succ)` — destructure. Both fail and succ
    // are continuations.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::SeqPop), args } => {
      emit_ternary_guard(ctx, frag, rt, cps, ast, Sym::SeqPop, args, expr.id);
    }
    // RecPut: `(rec, key, val, cont)` — record extension.
    // RecPop: `(rec, key, fail, succ)` — record destructure.
    // Both 4-arg, lowered as `return_call $sym arg0 arg1 arg2 arg3`.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::RecPut), args } => {
      emit_quaternary(ctx, frag, rt, cps, ast, rt.rec_put(), args, expr.id);
    }
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::RecPop), args } => {
      emit_quaternary(ctx, frag, rt, cps, ast, rt.rec_pop(), args, expr.id);
    }

    // Panic: zero-arg sentinel for irrefutable-pattern failure. We
    // emit `unreachable` directly — the runtime panic helper isn't
    // wired through `Sym` yet, and an unreachable trap is acceptable
    // at this level (matches the old emitter's fallback shape).
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Panic), .. } => {
      let i = push_unreachable(frag);
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
      lower_import(ctx, frag, rt, cps, args);
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
      // Remaining non-cont args are the captures.
      let cap_operands: Vec<Operand> = non_cont[1..].iter()
        .map(|a| {
          let v = match a {
            Arg::Val(v) => v,
            _ => panic!("lower: FnClosure capture is not a Val"),
          };
          val_as_operand(ctx, frag, rt, v)
        })
        .collect();

      // FnClosure has two cont shapes:
      // * `Cont::Expr { args: [new_bind], body }` — the closure value
      //   is bound to a local in the parent scope and execution
      //   continues into `body`.
      // * `Cont::Ref(id)` — tail-apply the cont with the closure as a
      //   single arg (`_apply([closure], cont_local)`).
      match cont {
        Cont::Expr { args: cont_args, body } => {
          let bind = cont_args.first().expect("FnClosure cont has no bind");
          let local = ctx.alloc_local(&cps_ident_for_bind(cps, ast, bind));
          ctx.bind(bind.id, local);
          emit_closure_construction(ctx, frag, rt, fn_sym, cap_operands, local);
          lower_expr(ctx, frag, rt, cps, ast, body);
        }
        Cont::Ref(cont_id) => {
          // Build the closure into a fresh anyref local first.
          let clo_local = ctx.alloc_local(&format!("v_{}_clo", cont_id.0));
          emit_closure_construction(ctx, frag, rt, fn_sym, cap_operands, clo_local);

          // Resolve cont; spill if non-local.
          let callee_op = resolve_id_as_operand(ctx, frag, rt, *cont_id);
          let callee = match callee_op {
            Operand::Local(l) => l,
            other => {
              let local = ctx.alloc_local(&format!("v_{}_callee", cont_id.0));
              let i = push_local_set(frag, local, other);
              ctx.instrs.push(i);
              local
            }
          };

          let l_args = ctx.alloc_local(":args");
          let i_nil = push_call(frag, rt.args_empty(), vec![], Some(l_args));
          ctx.instrs.push(i_nil);
          let i_cons = push_call(frag, rt.args_prepend(),
            vec![op_local(clo_local), op_local(l_args)], Some(l_args));
          ctx.instrs.push(i_cons);
          let i_app = push_return_call(frag, rt.apply(),
            vec![op_local(l_args), op_local(callee)]);
          ctx.instrs.push(i_app);
        }
      }
    }

    // Apply-path: callable is a ContRef — tail-call the named cont
    // via `_apply`. Args are pure values (no cont prefix, since the
    // callee IS the cont). Supports 0..N args; reverse-prepends so
    // args[0] lands at the head of the list.
    ExprKind::App { func: Callable::Val(v), args }
      if matches!(v.kind, ValKind::ContRef(_)) =>
    {
      let cont_id = if let ValKind::ContRef(id) = &v.kind { *id } else { unreachable!() };
      // Spill via resolver so cross-fn ContRefs (very rare — usually
      // a `Pub`'d cont, doesn't normally happen, but cheap to support)
      // resolve correctly. Apply expects a local-shaped operand.
      let callee_op = resolve_id_as_operand(ctx, frag, rt, cont_id);
      let callee = match callee_op {
        Operand::Local(l) => l,
        other => {
          let local = ctx.alloc_local(&format!("v_{}_callee", cont_id.0));
          let i = push_local_set(frag, local, other);
          ctx.instrs.push(i);
          local
        }
      };

      let l_args_list = build_args_list(ctx, frag, rt, cps, ast, args);
      let i_app = push_return_call(frag, rt.apply(),
        vec![op_local(l_args_list), op_local(callee)]);
      ctx.instrs.push(i_app);
    }

    // Apply-path via a bound ref (e.g. a closure local). The CPS
    // convention for user-fn calls is `args[0] = cont`, `args[1..] =
    // values` (cont-first). We build the args list with the cont at
    // the head by walking args in reverse and `args_prepend`-ing each
    // onto an initially-empty list.
    ExprKind::App { func: Callable::Val(v), args } => {
      let callee_id = cps_id_of_ref(v);
      // Resolve callee. May be a local, a pub'd global, or a sibling
      // lifted fn (materialised as a no-capture `$Closure`). Apply
      // expects a local-shaped operand, so spill non-local results.
      let callee_op = resolve_id_as_operand(ctx, frag, rt, callee_id);
      let callee = match callee_op {
        Operand::Local(l) => l,
        other => {
          let local = ctx.alloc_local(&format!("v_{}_callee", callee_id.0));
          let i = push_local_set(frag, local, other);
          ctx.instrs.push(i);
          local
        }
      };

      let l_args_list = build_args_list(ctx, frag, rt, cps, ast, args);
      let i_app = push_return_call(frag, rt.apply(),
        vec![op_local(l_args_list), op_local(callee)]);
      ctx.instrs.push(i_app);
    }

    // If: cond is a Val (bool — i31ref or literal). Unbox to i32 and
    // branch to one of two recursively-lowered bodies. Match arms
    // always end in a tail-call (`return_call`), so neither branch
    // falls through past the `If` in user code.
    ExprKind::If { cond, then, else_ } => {
      // Evaluate cond into an i32 leaf. For a bool literal, skip
      // unboxing entirely; for a ref/contref, the value is anyref —
      // ref.cast to i31, then i31.get_s.
      let unbox_anyref = |ctx: &mut FnCtx, frag: &mut Fragment, op: Operand| -> Operand {
        // Cast anyref → (ref i31) into a scratch local of type i31ref.
        let i31_local = ctx.alloc_local_typed(":cond_i31",
          val_ref_abs(AbsHeap::I31, /*nullable*/ false));
        let i_cast = push_ref_cast_non_null_abs(
          frag, AbsHeap::I31, op, i31_local);
        ctx.instrs.push(i_cast);
        // Unbox: i31.get_s into a typed i32 local.
        let i32_local = ctx.alloc_local_typed(":cond_i32", val_i32());
        let i = push_i31_get_s(frag, op_local(i31_local), i32_local);
        ctx.instrs.push(i);
        op_local(i32_local)
      };
      let cond_leaf = match &cond.kind {
        ValKind::Lit(Lit::Bool(b)) => op_i32(if *b { 1 } else { 0 }),
        ValKind::Ref(r) => {
          let op = resolve_id_as_operand(ctx, frag, rt, ref_cps_id(*r));
          unbox_anyref(ctx, frag, op)
        }
        ValKind::ContRef(id) => {
          let op = resolve_id_as_operand(ctx, frag, rt, *id);
          unbox_anyref(ctx, frag, op)
        }
        _ => panic!("lower: If cond shape not supported: {:?}", cond.kind),
      };

      // Recursively lower then/else into separate instruction lists by
      // swapping `ctx.instrs` to a fresh empty Vec for each branch.
      let saved = std::mem::take(&mut ctx.instrs);
      lower_expr(ctx, frag, rt, cps, ast, then);
      let then_body = std::mem::take(&mut ctx.instrs);
      lower_expr(ctx, frag, rt, cps, ast, else_);
      let else_body = std::mem::replace(&mut ctx.instrs, saved);

      let i_if = push_if(frag, cond_leaf, then_body, else_body);
      ctx.instrs.push(i_if);
    }

    _ => panic!("lower: unsupported expr shape: {:?}", short_kind(&expr.kind)),
  }
}

/// Emit a continuation. `Cont::Expr` is emitted inline (body + recurse);
/// `Cont::Ref` closes out the function with an apply-dispatch call.
fn lower_cont(
  ctx: &mut FnCtx,
  frag: &mut Fragment,
  rt: &Runtime,
  cps: &CpsResult,
  ast: &Ast<'_>,
  cont: &Cont,
) {
  match cont {
    Cont::Expr { body, .. } => {
      lower_expr(ctx, frag, rt, cps, ast, body);
    }
    Cont::Ref(id) => {
      // Tail-call the named cont with an empty args list.
      let callee = ctx.lookup(*id);
      let l_args_list = ctx.alloc_local(":args");
      let i_nil = push_call(frag, rt.args_empty(), vec![], Some(l_args_list));
      ctx.instrs.push(i_nil);
      let i_app = push_return_call(frag, rt.apply(),
        vec![op_local(l_args_list), op_local(callee)]);
      ctx.instrs.push(i_app);
    }
  }
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
/// Build the `_apply` args list from a heterogeneous arg sequence.
///
/// Two-phase to keep the locals/instr order stable across changes:
/// 1. **Materialise** every arg into a leaf operand in source order
///    (so any boxing/closure-construction locals appear in their
///    natural declaration position).
/// 2. **Build** the list — alloc `:args`, `args_empty`, then walk the
///    materialised operands in reverse and `args_prepend` (or
///    `args_concat` for spread) onto the running list.
///
/// Returns the local holding the final list.
fn build_args_list(
  ctx: &mut FnCtx,
  frag: &mut Fragment,
  rt: &Runtime,
  cps: &CpsResult,
  ast: &Ast<'_>,
  args: &[Arg],
) -> LocalIdx {
  enum Materialised { Prepend(Operand), Concat(Operand) }
  let materialised: Vec<Materialised> = args.iter()
    .map(|a| match a {
      Arg::Spread(v) => Materialised::Concat(val_as_operand(ctx, frag, rt, v)),
      Arg::Cont(Cont::Ref(id)) => Materialised::Prepend(resolve_id_as_operand(ctx, frag, rt, *id)),
      _ => Materialised::Prepend(emit_arg_as_operand(ctx, frag, rt, cps, ast, a)),
    })
    .collect();

  let l_args = ctx.alloc_local(":args");
  let i_nil = push_call(frag, rt.args_empty(), vec![], Some(l_args));
  ctx.instrs.push(i_nil);
  for m in materialised.into_iter().rev() {
    match m {
      Materialised::Concat(op) => {
        let i = push_call(frag, rt.args_concat(),
          vec![op, op_local(l_args)], Some(l_args));
        ctx.instrs.push(i);
      }
      Materialised::Prepend(op) => {
        let i = push_call(frag, rt.args_prepend(),
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
  ctx: &mut FnCtx,
  frag: &mut Fragment,
  rt: &Runtime,
  cps: &CpsResult,
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

  if is_virtual_stdlib_path(url) {
    lower_import_virtual_stdlib(ctx, frag, rt, cps, url, cont_id);
  } else {
    lower_import_user_fragment(ctx, frag, rt, url, cont_id);
  }
}

/// Virtual stdlib path: `import 'std/io.fnk'` etc. Each imported name
/// maps to its own runtime accessor. Build a rec from per-name calls,
/// tail-apply destructure cont.
fn lower_import_virtual_stdlib(
  ctx: &mut FnCtx,
  frag: &mut Fragment,
  rt: &Runtime,
  cps: &CpsResult,
  url: &str,
  cont_id: CpsId,
) {
  // Names come from the pre-collected module_imports table.
  let names = cps.module_imports.get(url)
    .cloned()
    .unwrap_or_else(|| panic!(
      "lower: BuiltIn::Import for `{url}` has no entries in module_imports"));

  // 1. Build the rec.
  let l_rec = ctx.alloc_local(":imp_rec");
  let i_new = push_call(frag, rt.rec_empty(), vec![], Some(l_rec));
  ctx.instrs.push(i_new);

  for name in &names {
    // 1a. Declare the import — `<url>:<name>` with signature `() -> anyref`.
    //     The IR linker resolves this against the runtime's export table
    //     at emit time. Reuses the `Fn_rec_new` sig type (`() -> anyref`).
    let sig = rt.fn_nil_to_list_sig();
    let target = crate::passes::wasm::ir::import_func(frag, sig, url, name);

    // 1b. Call the import to get the channel/value into a fresh local.
    let l_val = ctx.alloc_local(&format!(":imp_val_{name}"));
    let i_call = push_call(frag, target, vec![], Some(l_val));
    ctx.instrs.push(i_call);

    // 1c. Build the $Str key for the field name.
    let l_key = ctx.alloc_local(&format!(":imp_key_{name}"));
    let key_bytes = name.as_bytes();
    let key_sym = intern_data(frag, key_bytes);
    let i_key = push_call(frag, rt.str_(),
      vec![Operand::DataRef { sym: key_sym, len: key_bytes.len() as u32 }],
      Some(l_key));
    ctx.instrs.push(i_key);

    // 1d. Set the field on the rec.
    let i_set = push_call(frag, rt.rec_set_field(),
      vec![op_local(l_rec), op_local(l_key), op_local(l_val)],
      Some(l_rec));
    ctx.instrs.push(i_set);
  }

  // 2. Tail-apply the cont with [rec].
  let cont_op = resolve_id_as_operand(ctx, frag, rt, cont_id);
  let cont_local = match cont_op {
    Operand::Local(l) => l,
    other => {
      let l = ctx.alloc_local(&format!("v_{}_callee", cont_id.0));
      let i = push_local_set(frag, l, other);
      ctx.instrs.push(i);
      l
    }
  };

  let l_args = ctx.alloc_local(":args");
  let i_nil = push_call(frag, rt.args_empty(), vec![], Some(l_args));
  ctx.instrs.push(i_nil);
  let i_cons = push_call(frag, rt.args_prepend(),
    vec![op_local(l_rec), op_local(l_args)], Some(l_args));
  ctx.instrs.push(i_cons);
  let i_app = push_return_call(frag, rt.apply(),
    vec![op_local(l_args), op_local(cont_local)]);
  ctx.instrs.push(i_app);
}

/// User-fragment path: `import './foo.fnk'`. Tail-call
/// `std/modules.fnk:import` with the URL, a no-capture closure over
/// the producer's `<url>:fink_module`, and the destructure cont.
fn lower_import_user_fragment(
  ctx: &mut FnCtx,
  frag: &mut Fragment,
  rt: &Runtime,
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
  let importer_canonical = ctx.fqn_prefix.trim_end_matches(':').to_string();
  let canonical_url = super::compile_package::canonicalise_url(
    &importer_canonical, url,
  );

  // 1. Materialise the canonicalised URL as a `$Str` constant.
  let url_local = emit_str_const(ctx, frag, rt, canonical_url.as_bytes(), ":imp_url");

  // 2. Declare a func import of the producer's `<canonical_url>:fink_module`,
  //    typed as `$Fn2`. Resolved at emit time by name lookup against
  //    the merged runtime's export table; link::link rewrites it to
  //    the producer's local FuncSym during multi-fragment merge by
  //    matching the canonical URL against producer fragments' display
  //    names.
  let mod_fn_sym = crate::passes::wasm::ir::import_func(
    frag, rt.fn2(), &canonical_url, "fink_module");

  // 3. Build a no-capture `$Closure` over that funcref. funcrefs are
  //    not anyref-compatible (disjoint typing hierarchies in WasmGC),
  //    so we wrap to satisfy the std/modules.fnk:import signature
  //    which takes everything as anyref.
  let caps_local = ctx.alloc_local_typed(
    ":imp_caps_arg",
    val_ref(rt.captures(), /*nullable*/ true),
  );
  let i_caps = push_ref_null_concrete(frag, rt.captures(), caps_local);
  ctx.instrs.push(i_caps);

  let mod_clos_local = ctx.alloc_local(":imp_mod_clos");
  let i_clos = push_struct_new(
    frag, rt.closure(),
    vec![Operand::RefFunc(mod_fn_sym), op_local(caps_local)],
    mod_clos_local,
  );
  ctx.instrs.push(i_clos);

  // 4. Resolve the destructure cont.
  let cont_op = resolve_id_as_operand(ctx, frag, rt, cont_id);
  let cont_local = match cont_op {
    Operand::Local(l) => l,
    other => {
      let l = ctx.alloc_local(&format!("v_{}_callee", cont_id.0));
      let i = push_local_set(frag, l, other);
      ctx.instrs.push(i);
      l
    }
  };

  // 5. Tail-call `std/modules.fnk:import (url, mod_clos, cont)`. The
  //    runtime helper handles init-once + delivers the producer's
  //    exports rec to the destructure cont.
  let i_imp = push_return_call(frag, rt.modules_import(),
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
  ctx: &mut FnCtx,
  frag: &mut Fragment,
  rt: &Runtime,
  cps: &CpsResult,
  ast: &Ast<'_>,
  target: FuncSym,
  args: &[Arg],
  app_id: CpsId,
) {
  if args.len() != 4 {
    panic!("lower: 4-arg primitive expects 4 args, got {}", args.len());
  }
  let ops: Vec<Operand> = args.iter()
    .map(|a| emit_arg_as_operand(ctx, frag, rt, cps, ast, a))
    .collect();
  let i = push_return_call(frag, target, ops);
  if let Some(o) = origin_of(cps, ast, app_id) { set_origin(frag, i, o); }
  ctx.instrs.push(i);
}

/// Emit a `(value, cont, cont)` ternary primitive (IsSeqLike,
/// IsRecLike, SeqPop). The runtime function takes 3 anyref params:
/// the value being tested, plus two continuations resolved as
/// values at this layer.
fn emit_ternary_guard(
  ctx: &mut FnCtx,
  frag: &mut Fragment,
  rt: &Runtime,
  cps: &CpsResult,
  ast: &Ast<'_>,
  sym: Sym,
  args: &[Arg],
  app_id: CpsId,
) {
  if args.len() != 3 {
    panic!("lower: ternary primitive {:?} expects 3 args, got {}", sym, args.len());
  }
  let val_op = emit_arg_as_operand(ctx, frag, rt, cps, ast, &args[0]);
  let cont1_op = emit_arg_as_operand(ctx, frag, rt, cps, ast, &args[1]);
  let cont2_op = emit_arg_as_operand(ctx, frag, rt, cps, ast, &args[2]);
  let i = push_return_call(frag, rt.op(sym), vec![val_op, cont1_op, cont2_op]);
  if let Some(o) = origin_of(cps, ast, app_id) { set_origin(frag, i, o); }
  ctx.instrs.push(i);
}

fn emit_op_tail_call(
  ctx: &mut FnCtx,
  frag: &mut Fragment,
  rt: &Runtime,
  cps: &CpsResult,
  _ast: &Ast<'_>,
  sym: Sym,
  value_operands: Vec<Operand>,
  cont: &Arg,
  app_id: CpsId,
) {
  let cont_op = match cont {
    Arg::Cont(Cont::Ref(id)) => resolve_id_as_operand(ctx, frag, rt, *id),
    Arg::Val(v) => val_as_operand(ctx, frag, rt, v),
    _ => panic!("lower: operator cont is neither Cont::Ref nor Val (got {:?})", short_arg(cont)),
  };
  let mut operands = value_operands;
  operands.push(cont_op);
  let i = push_return_call(frag, rt.op(sym), operands);
  if let Some(o) = origin_of(cps, _ast, app_id) { set_origin(frag, i, o); }
  ctx.instrs.push(i);
}

/// Convert an `Arg` to a leaf `Operand`, allocating locals for
/// non-trivial values (literals get boxed into a fresh local).
fn emit_arg_as_operand(
  ctx: &mut FnCtx,
  frag: &mut Fragment,
  rt: &Runtime,
  cps: &CpsResult,
  ast: &Ast<'_>,
  arg: &Arg,
) -> Operand {
  match arg {
    Arg::Val(v) => {
      match &v.kind {
        ValKind::Lit(lit) => {
          let lv = LitVal::from_lit(lit)
            .unwrap_or_else(|| panic!("lower: unsupported lit {:?}", lit));
          let local = ctx.alloc_local(&format!("v_{}", v.id.0));
          let i = box_lit(frag, rt, &lv, local);
          if let Some(o) = origin_of(cps, ast, v.id) { set_origin(frag, i, o); }
          ctx.instrs.push(i);
          op_local(local)
        }
        ValKind::Ref(r) => resolve_id_as_operand(ctx, frag, rt, ref_cps_id(*r)),
        ValKind::ContRef(id) => resolve_id_as_operand(ctx, frag, rt, *id),
        ValKind::BuiltIn(BuiltIn::Panic) => panic_closure_operand(ctx, frag, rt),
        ValKind::BuiltIn(b) => panic!("lower: BuiltIn {:?} as arg not supported", b),
      }
    }
    // Cont args appear in builtin calls like IsSeqLike(val, succ, fail)
    // where succ/fail are continuations. Cont::Ref is just an id —
    // resolve it as a closure operand. Cont::Expr is a not currently
    // supported here (would need to materialise an inline lambda).
    Arg::Cont(Cont::Ref(id)) => resolve_id_as_operand(ctx, frag, rt, *id),
    _ => panic!("lower: non-Val arg in value position: {:?}", short_arg(arg)),
  }
}

/// Materialise a no-capture `$Closure` over the runtime `panic`
/// function. Used when `BuiltIn::Panic` appears in value position
/// (typically as the fail continuation in pattern-match dispatch
/// generated by the lifting pass).
fn panic_closure_operand(ctx: &mut FnCtx, frag: &mut Fragment, rt: &Runtime) -> Operand {
  let local = ctx.alloc_local("v_panic_clo");
  let caps_local = ctx.alloc_local_typed(":caps_arg",
    val_ref(rt.captures(), /*nullable*/ true));
  let i_caps = push_ref_null_concrete(frag, rt.captures(), caps_local);
  ctx.instrs.push(i_caps);
  let i_clo = push_struct_new(frag, rt.closure(),
    vec![Operand::RefFunc(rt.panic()), op_local(caps_local)],
    local);
  ctx.instrs.push(i_clo);
  op_local(local)
}

/// Convert a `Val` directly to an `Operand` (for cases where we're
/// sure it's a ref/lit and don't need to emit boxing). Routes through
/// `resolve_id_as_operand` so cross-fn refs to module-level Pub'd
/// bindings or top-level lifted fns resolve correctly.
fn val_as_operand(ctx: &mut FnCtx, frag: &mut Fragment, rt: &Runtime, v: &Val) -> Operand {
  match &v.kind {
    ValKind::Ref(r) => resolve_id_as_operand(ctx, frag, rt, ref_cps_id(*r)),
    ValKind::ContRef(id) => resolve_id_as_operand(ctx, frag, rt, *id),
    ValKind::BuiltIn(BuiltIn::Panic) => panic_closure_operand(ctx, frag, rt),
    ValKind::Lit(_) => panic!("val_as_operand: Lit requires boxing — use emit_arg_as_operand"),
    ValKind::BuiltIn(b) => panic!("val_as_operand: BuiltIn {:?} not supported", b),
  }
}

/// Emit a value into a specific local. Used by LetVal.
fn emit_val_into(
  ctx: &mut FnCtx,
  frag: &mut Fragment,
  rt: &Runtime,
  _cps: &CpsResult,
  _ast: &Ast<'_>,
  val: &Val,
  into: LocalIdx,
) -> InstrId {
  match &val.kind {
    ValKind::Lit(lit) => {
      let lv = LitVal::from_lit(lit)
        .unwrap_or_else(|| panic!("lower: unsupported lit {:?}", lit));
      box_lit(frag, rt, &lv, into)
    }
    ValKind::Ref(r) => {
      let id = ref_cps_id(*r);
      // Three cases for `LetVal(_, Ref(id))`:
      // 1. Locally bound — copy local-to-local.
      // 2. Pub'd module global — `local.set into (global.get $g)`.
      // 3. Top-level fn id (no captures) — emit `$Closure` directly
      //    into `into` (avoids spilling through a scratch local).
      if let Some(local) = ctx.binds.get(&id).copied() {
        return push_local_set(frag, into, op_local(local));
      }
      if let Some(&(gsym, _)) = ctx.pub_globals.get(&id) {
        return push_local_set(frag, into, op_global(gsym));
      }
      if let Some(fn_sym) = ctx.try_lookup_fn_sym(id) {
        let caps_local = ctx.alloc_local_typed(
          ":caps_arg",
          val_ref(rt.captures(), /*nullable*/ true),
        );
        let i_caps = push_ref_null_concrete(frag, rt.captures(), caps_local);
        ctx.instrs.push(i_caps);
        return push_struct_new(
          frag, rt.closure(),
          vec![Operand::RefFunc(fn_sym), op_local(caps_local)],
          into,
        );
      }
      let src = ctx.lookup(id);  // panic with diagnostic
      push_local_set(frag, into, op_local(src))
    }
    ValKind::ContRef(id) => {
      // Cont refs: same three-way resolution shape, but the no-capture
      // closure case shouldn't apply (conts are never bare fn ids).
      if let Some(local) = ctx.binds.get(id).copied() {
        return push_local_set(frag, into, op_local(local));
      }
      if let Some(&(gsym, _)) = ctx.pub_globals.get(id) {
        return push_local_set(frag, into, op_global(gsym));
      }
      let src = ctx.lookup(*id);
      push_local_set(frag, into, op_local(src))
    }
    ValKind::BuiltIn(_) => panic!("lower: BuiltIn as LetVal rhs not supported"),
  }
}

// ──────────────────────────────────────────────────────────────────
// Support types + helpers
// ──────────────────────────────────────────────────────────────────

/// Literal shape at lowering time.
enum LitVal {
  Num(f64),
  Bool(bool),
  /// Empty sequence literal `[]` — lowers to `call $args_empty`.
  /// Reuses the `args_empty` runtime function (which is exported as
  /// both `args_empty` and `list_nil` from the same impl).
  EmptySeq,
  /// Empty record literal `{}` — lowers to `call $rec_new`.
  EmptyRec,
  /// String literal. Empty strings are special-cased to `str_empty`;
  /// non-empty strings intern their bytes into `frag.data` and emit
  /// `call $str (i32.const offset) (i32.const len)`.
  Str(Vec<u8>),
}

impl LitVal {
  fn from_lit(lit: &Lit) -> Option<Self> {
    Some(match lit {
      Lit::Int(n)     => LitVal::Num(*n as f64),
      Lit::Float(f)   => LitVal::Num(*f),
      Lit::Decimal(f) => LitVal::Num(*f),
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
/// Uses `rt.str_()` unconditionally — even for empty bytes — so we
/// don't need `Sym::StrEmpty` declared just for the empty-FQN case.
/// `intern_data(frag, &[])` produces a zero-length data symbol; the
/// resulting `$Str` has len 0 and reads as the empty string.
fn emit_str_const(
  ctx: &mut FnCtx,
  frag: &mut Fragment,
  rt: &Runtime,
  bytes: &[u8],
  display_hint: &str,
) -> LocalIdx {
  let local = ctx.alloc_local(display_hint);
  let sym = intern_data(frag, bytes);
  let len = bytes.len() as u32;
  let i = push_call(frag, rt.str_(),
    vec![Operand::DataRef { sym, len }], Some(local));
  ctx.instrs.push(i);
  local
}

fn box_lit(frag: &mut Fragment, rt: &Runtime, lit: &LitVal, into: LocalIdx) -> InstrId {
  match lit {
    LitVal::Num(n) => push_struct_new(frag, rt.num(), vec![op_f64(*n)], into),
    LitVal::Bool(b) => push_ref_i31(frag, op_i32(if *b { 1 } else { 0 }), into),
    LitVal::EmptySeq => push_call(frag, rt.args_empty(), vec![], Some(into)),
    LitVal::EmptyRec => push_call(frag, rt.rec_empty(), vec![], Some(into)),
    LitVal::Str(bytes) => {
      if bytes.is_empty() {
        push_call(frag, rt.str_empty(), vec![], Some(into))
      } else {
        // Intern the bytes, then emit `call $str (data_ref, len)`.
        // `Operand::DataRef` expands to two i32 consts at emit time.
        let sym = intern_data(frag, bytes);
        let len = bytes.len() as u32;
        push_call(frag, rt.str_(),
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
    BuiltIn::Range     => Sym::OpRngex,
    BuiltIn::RangeIncl => Sym::OpRngin,
    BuiltIn::In        => Sym::OpIn,
    BuiltIn::NotIn     => Sym::OpNotIn,
    BuiltIn::Get       => Sym::OpDot,
    _ => return None,
  })
}

fn extract_fink_module_body(root: &Expr) -> Option<(CpsId, &Expr)> {
  let ExprKind::App { func: Callable::BuiltIn(BuiltIn::FinkModule), args } = &root.kind else {
    return None;
  };
  let cont_arg = args.first()?;
  let Arg::Cont(Cont::Expr { args: cont_args, body }) = cont_arg else {
    return None;
  };
  let ret_bind = cont_args.first()?;
  Some((ret_bind.id, body))
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
  cps_ident(cps, ast, b.id)
}

/// Derive a display name for a CPS bind/ref. Uses the source ident
/// from the origin map (`{ident}_{id}`) when available, otherwise
/// falls back to `v_<id>`. Mirrors `collect.rs::label`.
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
  ctx: &mut FnCtx,
  frag: &mut Fragment,
  rt: &Runtime,
  fn_sym: FuncSym,
  cap_operands: Vec<Operand>,
  into: LocalIdx,
) {
  // 1. Build the captures operand — either a null ref or a freshly
  //    allocated array. Local is typed `(ref null $Captures)` so
  //    `struct.new $Closure` validates against its second field's
  //    declared type.
  let caps_local = ctx.alloc_local_typed(
    ":caps_arg",
    val_ref(rt.captures(), /*nullable*/ true),
  );
  let caps_instr = if cap_operands.is_empty() {
    push_ref_null_concrete(frag, rt.captures(), caps_local)
  } else {
    push_array_new_fixed(frag, rt.captures(), cap_operands, caps_local)
  };
  ctx.instrs.push(caps_instr);

  // 2. struct.new $Closure (ref.func $fn, local.get $:caps_arg).
  let struct_instr = push_struct_new(
    frag,
    rt.closure(),
    vec![Operand::RefFunc(fn_sym), op_local(caps_local)],
    into,
  );
  ctx.instrs.push(struct_instr);
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
