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
  Arg, Bind, BindNode, Callable, Cont, CpsId, CpsResult, Expr, ExprKind,
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
pub fn lower(cps: &CpsResult, ast: &Ast<'_>) -> Fragment {
  let usage = runtime_contract::scan(cps);
  let mut frag = Fragment::default();
  let rt = runtime_contract::declare(&mut frag, &usage);

  // CPS root shape: App(FinkModule, [Cont::Expr { args: [ƒret], body }]).
  let Some((ret_bind, module_body)) = extract_fink_module_body(&cps.root) else {
    panic!("ir_lower: unsupported CPS root shape (expected App(FinkModule, [Cont::Expr]))");
  };

  // Lower the module body as the `fink_module` function.
  let fink_module = lower_fn(
    &mut frag, &rt, cps, ast,
    &[],           // no cap params at the module level
    &[ret_bind],   // user param: ƒret
    module_body,
    "fink_module",
  );
  frag.funcs[fink_module.0 as usize].export = Some("fink_module".into());

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
  user_params: &[CpsId],
  body: &Expr,
  display: &str,
) -> FuncSym {
  let mut ctx = FnCtx::new();

  // WASM-level params (always just `$_caps` and `$_args` — the
  // $Fn2 shape).
  let _l_caps = ctx.alloc_param("_caps");
  let l_args_p = ctx.alloc_param("_args");

  // Unpack captures from $_caps into locals.
  for (i, cap_id) in cap_params.iter().enumerate() {
    let name = cps_ident(cps, *cap_id);
    let local = ctx.alloc_local(&name);
    ctx.bind(*cap_id, local);
    // array.get $Captures (ref.cast (ref $Captures) $_caps) <i>
    // Emitted as a Call-in-place... actually we don't have array.get
    // as an InstrKind yet. Sidestep: since today's captures are
    // always anyref, we can read them via a scratch helper — but
    // simpler: emit a direct InstrKind::ArrayGet variant.
    //
    // Implementation note: the ir.rs InstrKind enum doesn't yet have
    // ArrayGet. Until that lands, this code path will fail for any
    // function that actually has captures. LetFn fixtures that need
    // captures stay skip-ir.
    ctx.emit_cap_unpack(*cap_id, i as u32, local);
  }

  // Unpack user params from $_args by walking `args_head`/`args_tail`.
  // For the first param we only need `args_head`. For subsequent
  // params we'd need `args_tail` calls — a TODO for multi-param
  // functions.
  if let Some(first) = user_params.first() {
    let name = cps_ident(cps, *first);
    let local = ctx.alloc_local(&name);
    ctx.bind(*first, local);
    let i = push_call(frag, rt.args_head(), vec![op_local(l_args_p)], Some(local));
    ctx.instrs.push(i);
  }
  if user_params.len() > 1 {
    panic!("ir_lower: multi-user-param fns not yet supported (got {})", user_params.len());
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
  /// Pending capture unpack instructions — emitted at the top of the
  /// function body. We collect them here because `ArrayGet` isn't an
  /// InstrKind yet; a placeholder Unreachable gets substituted with
  /// the real unpack once ir.rs grows the capability.
  cap_unpacks: Vec<CapUnpack>,
}

struct CapUnpack {
  cap_id: CpsId,
  index: u32,
  into: LocalIdx,
}

impl FnCtx {
  fn new() -> Self {
    Self {
      params: Vec::new(),
      locals: Vec::new(),
      instrs: Vec::new(),
      binds: HashMap::new(),
      next_local_idx: 0,
      cap_unpacks: Vec::new(),
    }
  }

  fn alloc_param(&mut self, name: &str) -> LocalIdx {
    let idx = LocalIdx(self.next_local_idx);
    self.next_local_idx += 1;
    self.params.push(local(val_anyref(true), name));
    idx
  }

  fn alloc_local(&mut self, name: &str) -> LocalIdx {
    let idx = LocalIdx(self.next_local_idx);
    self.next_local_idx += 1;
    self.locals.push(local(val_anyref(true), name));
    idx
  }

  fn bind(&mut self, id: CpsId, idx: LocalIdx) {
    self.binds.insert(id, idx);
  }

  fn lookup(&self, id: CpsId) -> LocalIdx {
    *self.binds.get(&id)
      .unwrap_or_else(|| panic!("ir_lower: unbound CpsId {:?}", id))
  }

  fn emit_cap_unpack(&mut self, cap_id: CpsId, index: u32, into: LocalIdx) {
    self.cap_unpacks.push(CapUnpack { cap_id, index, into });
  }
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
      let local = ctx.alloc_local(&cps_ident_for_bind(cps, name));
      ctx.bind(name.id, local);
      let i = emit_val_into(ctx, frag, rt, cps, ast, val, local);
      if let Some(o) = origin_of(cps, ast, name.id) { set_origin(frag, i, o); }
      ctx.instrs.push(i);
      lower_cont(ctx, frag, rt, cps, ast, cont);
    }

    ExprKind::LetFn { name, params, fn_body, cont, .. } => {
      // Collect cap + user params by role.
      let mut cap_ids: Vec<CpsId> = Vec::new();
      let mut user_ids: Vec<CpsId> = Vec::new();
      for p in params {
        let pid = match p {
          Param::Name(b) | Param::Spread(b) => b.id,
        };
        match cps.param_info.try_get(pid).and_then(|o| *o) {
          Some(ParamInfo::Cap(_))  => cap_ids.push(pid),
          Some(ParamInfo::Param(_)) | Some(ParamInfo::Cont) => user_ids.push(pid),
          None => user_ids.push(pid),  // ungilded params treated as user
        }
      }
      // Lift the fn body to a separate Fn2.
      let display = cps_ident_for_bind(cps, name);
      let fn_sym = lower_fn(frag, rt, cps, ast, &cap_ids, &user_ids, fn_body, &display);
      // The LetFn binds `name.id` to a funcref-valued local; we model
      // it as an anyref local holding a `ref.func` funcref. Actual
      // `ref.func` emission happens at the LetVal(FnClosure) site
      // where this funcref is used as the first arg. So we just
      // remember the FuncSym here.
      ctx.fn_sym_for_bind(name.id, fn_sym);
      lower_cont(ctx, frag, rt, cps, ast, cont);
    }

    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Pub), args } => {
      // `·ƒpub val, cont` — val is ignored (pub is a compile-time
      // marker; the actual export happens via module-level metadata
      // which this tracer doesn't thread yet). Just emit the cont
      // body.
      let cont_arg = args.get(1)
        .unwrap_or_else(|| panic!("ir_lower: Pub expects [val, cont]"));
      let Arg::Cont(cont) = cont_arg else {
        panic!("ir_lower: Pub cont arg is not a Cont");
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
        panic!("ir_lower: FnClosure missing fn arg");
      };
      let fn_sym = ctx.lookup_fn_sym(cps_id_of_ref(fn_val));
      // Remaining non-cont args are the captures.
      let cap_operands: Vec<Operand> = non_cont[1..].iter()
        .map(|a| {
          let v = match a {
            Arg::Val(v) => v,
            _ => panic!("ir_lower: FnClosure capture is not a Val"),
          };
          val_as_operand(ctx, v)
        })
        .collect();

      // Allocate local for the new closure's bind.
      let Cont::Expr { args: cont_args, body } = cont else {
        panic!("ir_lower: FnClosure cont is not Expr");
      };
      let bind = cont_args.first().expect("FnClosure cont has no bind");
      let local = ctx.alloc_local(&cps_ident_for_bind(cps, bind));
      ctx.bind(bind.id, local);

      // Emit: local = struct.new $Closure (ref.func fn_sym, array.new_fixed $Captures N cap_ops).
      // For now, captures must be anyref locals already in scope;
      // we don't yet support inlining complex values.
      let i = push_struct_new_closure(frag, rt, fn_sym, cap_operands, local);
      ctx.instrs.push(i);
      lower_expr(ctx, frag, rt, cps, ast, body);
    }

    // Apply-path: callable is a ContRef — tail-call via `_apply`.
    ExprKind::App { func: Callable::Val(v), args }
      if matches!(v.kind, ValKind::ContRef(_)) =>
    {
      // Pass a single-arg args list [operand] to the ContRef's callee.
      let first = args.first()
        .unwrap_or_else(|| panic!("ir_lower: apply-path expects >=1 arg"));
      let op0 = emit_arg_as_operand(ctx, frag, rt, cps, ast, first);
      let cont_id = if let ValKind::ContRef(id) = &v.kind { *id } else { unreachable!() };
      let callee = ctx.lookup(cont_id);

      let l_args_list = ctx.alloc_local("args");
      let i_nil = push_call(frag, rt.args_empty(), vec![], Some(l_args_list));
      ctx.instrs.push(i_nil);
      let i_cons = push_call(frag, rt.args_prepend(),
        vec![op0, op_local(l_args_list)], Some(l_args_list));
      ctx.instrs.push(i_cons);
      let i_app = push_return_call(frag, rt.apply(),
        vec![op_local(l_args_list), op_local(callee)]);
      ctx.instrs.push(i_app);
    }

    // Apply-path via a bound ref (e.g. a closure local): same as
    // ContRef but the callee is the value of a local.
    ExprKind::App { func: Callable::Val(v), args } => {
      let callee_id = cps_id_of_ref(v);
      let callee = ctx.lookup(callee_id);

      // Args: each element goes through args_prepend. For a single
      // arg this is the same as apply-path above.
      let first = args.first()
        .unwrap_or_else(|| panic!("ir_lower: apply-path (Val) expects >=1 arg"));
      let op0 = emit_arg_as_operand(ctx, frag, rt, cps, ast, first);

      let l_args_list = ctx.alloc_local("args");
      let i_nil = push_call(frag, rt.args_empty(), vec![], Some(l_args_list));
      ctx.instrs.push(i_nil);
      let i_cons = push_call(frag, rt.args_prepend(),
        vec![op0, op_local(l_args_list)], Some(l_args_list));
      ctx.instrs.push(i_cons);
      let i_app = push_return_call(frag, rt.apply(),
        vec![op_local(l_args_list), op_local(callee)]);
      ctx.instrs.push(i_app);
    }

    _ => panic!("ir_lower: unsupported expr shape: {:?}", short_kind(&expr.kind)),
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
      let l_args_list = ctx.alloc_local("args");
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
    Arg::Cont(Cont::Ref(id)) => op_local(ctx.lookup(*id)),
    Arg::Val(v) => val_as_operand(ctx, v),
    _ => panic!("ir_lower: operator cont is neither Cont::Ref nor Val (got {:?})", short_arg(cont)),
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
            .unwrap_or_else(|| panic!("ir_lower: unsupported lit {:?}", lit));
          let local = ctx.alloc_local(&format!("v_{}", v.id.0));
          let i = box_lit(frag, rt, &lv, local);
          if let Some(o) = origin_of(cps, ast, v.id) { set_origin(frag, i, o); }
          ctx.instrs.push(i);
          op_local(local)
        }
        ValKind::Ref(r) => op_local(ctx.lookup(ref_cps_id(*r))),
        ValKind::ContRef(id) => op_local(ctx.lookup(*id)),
        ValKind::BuiltIn(_) => panic!("ir_lower: BuiltIn val as arg not supported"),
      }
    }
    _ => panic!("ir_lower: non-Val arg in value position: {:?}", short_arg(arg)),
  }
}

/// Convert a `Val` directly to an `Operand` (for cases where we're
/// sure it's a ref/lit and don't need to emit boxing).
fn val_as_operand(ctx: &FnCtx, v: &Val) -> Operand {
  match &v.kind {
    ValKind::Ref(r) => op_local(ctx.lookup(ref_cps_id(*r))),
    ValKind::ContRef(id) => op_local(ctx.lookup(*id)),
    ValKind::Lit(_) => panic!("val_as_operand: Lit requires boxing — use emit_arg_as_operand"),
    ValKind::BuiltIn(_) => panic!("val_as_operand: BuiltIn not supported"),
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
        .unwrap_or_else(|| panic!("ir_lower: unsupported lit {:?}", lit));
      box_lit(frag, rt, &lv, into)
    }
    ValKind::Ref(r) => {
      let src = ctx.lookup(ref_cps_id(*r));
      push_local_set(frag, into, op_local(src))
    }
    ValKind::ContRef(id) => {
      let src = ctx.lookup(*id);
      push_local_set(frag, into, op_local(src))
    }
    ValKind::BuiltIn(_) => panic!("ir_lower: BuiltIn as LetVal rhs not supported"),
  }
}

// ──────────────────────────────────────────────────────────────────
// Support types + helpers
// ──────────────────────────────────────────────────────────────────

/// Literal shape at lowering time.
enum LitVal {
  Num(f64),
  Bool(bool),
}

impl LitVal {
  fn from_lit(lit: &Lit) -> Option<Self> {
    Some(match lit {
      Lit::Int(n)     => LitVal::Num(*n as f64),
      Lit::Float(f)   => LitVal::Num(*f),
      Lit::Decimal(f) => LitVal::Num(*f),
      Lit::Bool(b)    => LitVal::Bool(*b),
      _ => return None,
    })
  }
}

fn box_lit(frag: &mut Fragment, rt: &Runtime, lit: &LitVal, into: LocalIdx) -> InstrId {
  match lit {
    LitVal::Num(n) => push_struct_new(frag, rt.num(), vec![op_f64(*n)], into),
    LitVal::Bool(b) => push_ref_i31(frag, op_i32(if *b { 1 } else { 0 }), into),
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

fn cps_ident_for_bind(_cps: &CpsResult, b: &BindNode) -> String {
  // Match the old pipeline's naming convention (`$v_<cps_id>`) so
  // test_ir_*.fnk fixtures don't churn on cosmetic local-name diffs
  // when the walker replaces bespoke builders.
  format!("v_{}", b.id.0)
}

fn cps_ident(_cps: &CpsResult, id: CpsId) -> String {
  format!("v_{}", id.0)
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

/// `struct.new $Closure (ref.func $fn, array.new_fixed $Captures N caps...)`
///
/// Currently not implemented — the IR doesn't have `$Closure` /
/// `$Captures` types exposed, and doesn't have `ArrayNewFixed` /
/// `RefFunc-in-struct.new-fields` wired. Emits a placeholder that
/// panics at emit time.
fn push_struct_new_closure(
  frag: &mut Fragment,
  _rt: &Runtime,
  _fn_sym: FuncSym,
  _caps: Vec<Operand>,
  _into: LocalIdx,
) -> InstrId {
  let _ = frag;
  panic!("ir_lower: closure construction not yet wired to IR (needs $Closure/$Captures types + ArrayNewFixed)");
}

// FnCtx extension: track LetFn bindings that are used later in
// `App(FnClosure)` to build a $Closure.
impl FnCtx {
  fn fn_sym_for_bind(&mut self, _id: CpsId, _sym: FuncSym) {
    // Stored in a side-map. For now, panics when consulted since
    // closure emission isn't wired.
  }
  fn lookup_fn_sym(&self, _id: CpsId) -> FuncSym {
    panic!("ir_lower: LetFn binding lookup requires closure emission, not yet wired");
  }
}
