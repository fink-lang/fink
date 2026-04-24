//! CPS → unlinked wasm IR `Fragment`.
//!
//! Tracer-phase. Grows by demand — each failing fixture in
//! `test_ir.fnk` pulls in exactly the CPS construct it needs.
//!
//! Current coverage:
//! * `main = fn: <lit>` — apply-path tail call (user-cont).
//! * `main = fn: <lit> + <lit>` — direct-style builtin tail call.

use crate::passes::ast::Ast;
use crate::passes::cps::ir::{Arg, Callable, Cont, CpsId, CpsResult, Expr, ExprKind, Lit, ValKind, BuiltIn};
use crate::sourcemap::native::ByteRange;

use super::ir::*;
use super::runtime_contract::{self, Runtime};

/// Look up the source byte range for a CPS node via `cps.origin →
/// ast_id → ast.nodes[id].loc`. Returns `None` when a CPS node has
/// no AST origin (compiler-synthesised temp or helper).
fn origin_of(cps: &CpsResult, ast: &Ast<'_>, id: CpsId) -> Option<ByteRange> {
  let ast_id = (*cps.origin.try_get(id)?)?;
  let loc = ast.nodes.get(ast_id).loc;
  Some(ByteRange::new(loc.start.idx, loc.end.idx))
}

/// Lower a lifted CPS result to an unlinked wasm IR `Fragment`.
pub fn lower(cps: &CpsResult, ast: &Ast<'_>) -> Fragment {
  // Prepass: enumerate every runtime symbol the program uses, then
  // declare imports up front in a canonical order. After this, the
  // rest of lowering just reads handles from `rt`.
  let usage = runtime_contract::scan(cps);
  let mut frag = Fragment::default();
  let rt = runtime_contract::declare(&mut frag, &usage);

  // Walk the lifted CPS root. Expected shape:
  //   App { func: BuiltIn::FinkModule, args: [Cont::Expr { args: [ƒret], body }] }
  let Some((_ret_bind, body)) = extract_fink_module_body(&cps.root) else {
    panic!("ir_lower: unsupported CPS root shape (expected App(FinkModule, [Cont::Expr]))");
  };

  let fink_module = match &body.kind {
    // Apply-path: `App(ContRef(ƒret), [Lit])` — hand one value back
    // to the module's return continuation via `_apply`.
    ExprKind::App { func: Callable::Val(v), args }
      if matches!(v.kind, ValKind::ContRef(_)) =>
    {
      let (lit, lit_id) = extract_first_lit(args)
        .unwrap_or_else(|| panic!("ir_lower: unsupported apply-path args (expected [Lit])"));
      let lit_origin = origin_of(cps, ast, lit_id);
      build_apply_path_body(&mut frag, &rt, &lit, lit_origin)
    }

    // Direct-style builtin: `App(BuiltIn::Add, [Lit, Lit, Cont::Ref(ƒret)])`.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Add), args } => {
      let ((a, a_id), (b, b_id)) = extract_two_lits(args)
        .unwrap_or_else(|| panic!("ir_lower: unsupported op_plus args (expected [Lit, Lit, Cont])"));
      let a_origin = origin_of(cps, ast, a_id);
      let b_origin = origin_of(cps, ast, b_id);
      let app_origin = origin_of(cps, ast, body.id);
      build_op_plus_body(&mut frag, &rt, a, a_origin, b, b_origin, app_origin)
    }

    _ => panic!("ir_lower: unsupported fink_module body shape"),
  };

  // Export fink_module as the module's bring-up entry.
  frag.funcs[fink_module.0 as usize].export = Some("fink_module".into());

  frag
}

// ──────────────────────────────────────────────────────────────────
// Body builders — one per CPS body shape.
// ──────────────────────────────────────────────────────────────────

/// Literal shape at lowering time. Numeric lits box into `$Num`;
/// bool lits box into an `i31ref` (0 = false, 1 = true).
enum LitVal {
  Num(f64),
  Bool(bool),
}

/// Apply-path body: pop `done` from args, box the literal, build the
/// single-element args list, tail-call `_apply`.
///
/// Origins: the boxing instruction maps to the literal's source
/// range. Everything else (bring-up plumbing) has no source origin.
fn build_apply_path_body(
  frag: &mut Fragment,
  rt: &Runtime,
  lit: &LitVal,
  lit_origin: Option<ByteRange>,
) -> FuncSym {
  let l_args_p = LocalIdx(1);
  let l_done   = LocalIdx(2);
  let l_val    = LocalIdx(3);
  let l_list   = LocalIdx(4);

  let i_head = push_call(frag, rt.args_head(), vec![op_local(l_args_p)], Some(l_done));
  let (i_box, val_local_ty) = match lit {
    LitVal::Num(n) => {
      let id = push_struct_new(frag, rt.num(), vec![op_f64(*n)], l_val);
      (id, val_ref(rt.num(), false))
    }
    LitVal::Bool(b) => {
      // i31 = 0 for false, 1 for true. Stored as `(ref null any)` so
      // the args list (which carries anyref elements) can hold it
      // without further boxing.
      let id = push_ref_i31(frag, op_i32(if *b { 1 } else { 0 }), l_val);
      (id, val_anyref(true))
    }
  };
  if let Some(o) = lit_origin { set_origin(frag, i_box, o); }
  let i_nil  = push_call(frag, rt.args_empty(), vec![], Some(l_list));
  let i_cons = push_call(frag, rt.args_prepend(),
    vec![op_local(l_val), op_local(l_list)], Some(l_list));
  let i_app  = push_return_call(frag, rt.apply(),
    vec![op_local(l_list), op_local(l_done)]);

  func(frag, rt.fn2(),
    vec![
      local(val_anyref(true), "_caps"),
      local(val_anyref(true), "_args"),
    ],
    vec![
      local(val_anyref(true), "v_0"),
      local(val_local_ty, "v_1"),
      local(val_anyref(true), "args"),
    ],
    vec![i_head, i_box, i_nil, i_cons, i_app],
    "fink_module",
  )
}

/// Direct-style `op_plus` body: pop `done`, box both operands,
/// tail-call `op_plus(a, b, done)`.
///
/// Origins: each `struct.new` maps to its literal; the `return_call`
/// maps to the `App` (the whole `a + b` expression).
fn build_op_plus_body(
  frag: &mut Fragment,
  rt: &Runtime,
  a: f64, a_origin: Option<ByteRange>,
  b: f64, b_origin: Option<ByteRange>,
  app_origin: Option<ByteRange>,
) -> FuncSym {
  let l_args_p = LocalIdx(1);
  let l_done   = LocalIdx(2);
  let l_a      = LocalIdx(3);
  let l_b      = LocalIdx(4);

  let i_head = push_call(frag, rt.args_head(), vec![op_local(l_args_p)], Some(l_done));
  let i_a    = push_struct_new(frag, rt.num(), vec![op_f64(a)], l_a);
  if let Some(o) = a_origin { set_origin(frag, i_a, o); }
  let i_b    = push_struct_new(frag, rt.num(), vec![op_f64(b)], l_b);
  if let Some(o) = b_origin { set_origin(frag, i_b, o); }
  let i_app  = push_return_call(frag, rt.op_plus(),
    vec![op_local(l_a), op_local(l_b), op_local(l_done)]);
  if let Some(o) = app_origin { set_origin(frag, i_app, o); }

  func(frag, rt.fn2(),
    vec![
      local(val_anyref(true), "_caps"),
      local(val_anyref(true), "_args"),
    ],
    vec![
      local(val_anyref(true), "v_0"),
      local(val_ref(rt.num(), false), "v_1"),
      local(val_ref(rt.num(), false), "v_2"),
    ],
    vec![i_head, i_a, i_b, i_app],
    "fink_module",
  )
}

// ──────────────────────────────────────────────────────────────────
// CPS shape matchers — throw-away helpers while coverage is narrow.
// ──────────────────────────────────────────────────────────────────

/// Match the lifted module root `App(FinkModule, [Cont::Expr])` and
/// return the cont's return-binding id and body expression.
fn extract_fink_module_body(
  root: &Expr,
) -> Option<(crate::passes::cps::ir::CpsId, &Expr)> {
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

/// Extract the first-arg literal (any supported kind) from an `Arg`
/// list, returning its value plus the CpsId of the Val node.
fn extract_first_lit(args: &[Arg]) -> Option<(LitVal, CpsId)> {
  val_lit(args.first()?)
}

/// Extract two numeric-literal args in order, ignoring trailing
/// `Arg::Cont` (continuation). Each return pair is `(f64, CpsId)`.
/// Used by the direct-style `op_plus` path, which requires both
/// operands to be numeric.
fn extract_two_lits(args: &[Arg]) -> Option<((f64, CpsId), (f64, CpsId))> {
  let a = val_lit_num(args.first()?)?;
  let b = val_lit_num(args.get(1)?)?;
  Some((a, b))
}

fn val_lit(arg: &Arg) -> Option<(LitVal, CpsId)> {
  let Arg::Val(v) = arg else { return None };
  let ValKind::Lit(lit) = &v.kind else { return None };
  let lv = match lit {
    Lit::Int(n)     => LitVal::Num(*n as f64),
    Lit::Float(f)   => LitVal::Num(*f),
    Lit::Decimal(f) => LitVal::Num(*f),
    Lit::Bool(b)    => LitVal::Bool(*b),
    _ => return None,
  };
  Some((lv, v.id))
}

fn val_lit_num(arg: &Arg) -> Option<(f64, CpsId)> {
  match val_lit(arg)? {
    (LitVal::Num(f), id) => Some((f, id)),
    _ => None,
  }
}
