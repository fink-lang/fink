//! CPS → unlinked wasm IR `Fragment`.
//!
//! Tracer-phase. Grows by demand — each failing fixture in
//! `test_ir.fnk` pulls in exactly the CPS construct it needs.
//!
//! Current coverage: just enough to lower a module whose body is
//! `App(ContRef(ƒret), [Lit])` — i.e., a single-expression program
//! that hands its literal to the module's return continuation.

use crate::passes::ast::Ast;
use crate::passes::cps::ir::{Arg, Callable, Cont, CpsResult, Expr, ExprKind, Lit, Val, ValKind, BuiltIn};

use super::ir::*;
use super::runtime_contract;

/// Lower a lifted CPS result to an unlinked wasm IR `Fragment`.
pub fn lower(cps: &CpsResult, _ast: &Ast<'_>) -> Fragment {
  // Prepass: enumerate every runtime symbol the program uses, then
  // declare imports up front in a canonical order. After this, the
  // rest of lowering just reads handles from `rt`.
  let usage = runtime_contract::scan(cps);
  let mut frag = Fragment::default();
  let rt = runtime_contract::declare(&mut frag, &usage);

  // Walk the lifted CPS root. Expected shape:
  //   App { func: BuiltIn::FinkModule, args: [Cont::Expr { args: [ƒret], body }] }
  // where `body` is `App { func: Val(ContRef(ƒret)), args: [Lit(42)] }`.
  let Some((_ret_bind, body)) = extract_fink_module_body(&cps.root) else {
    panic!("ir_lower: unsupported CPS root shape (expected App(FinkModule, [Cont::Expr]))");
  };

  let result_lit = extract_ret_lit(body).unwrap_or_else(|| {
    panic!("ir_lower: unsupported fink_module body shape (expected App(ContRef, [Lit]))");
  });

  // Build $fink_module body.
  //   (local.set $v_0 (call $list_head_any (local.get $_args)))
  //   (local.set $v_1 (struct.new $Num (f64.const <lit>)))
  //   (local.set $args (call $list_nil))
  //   (local.set $args (call $list_prepend_any (local.get $v_1) (local.get $args)))
  //   (return_call $_apply (local.get $args) (local.get $v_0))
  //
  // Locals (numeric indices after the two params):
  //   0: $_caps   1: $_args   2: $v_0   3: $v_1   4: $args
  let l_caps   = LocalIdx(0);
  let l_args_p = LocalIdx(1);
  let l_done   = LocalIdx(2);
  let l_val    = LocalIdx(3);
  let l_list   = LocalIdx(4);

  let i_head = push_call(&mut frag, rt.list_head_any(),
    vec![op_local(l_args_p)], Some(l_done));

  let i_box  = push_struct_new(&mut frag, rt.num(),
    vec![op_f64(result_lit)], l_val);

  let i_nil  = push_call(&mut frag, rt.list_nil(),
    vec![], Some(l_list));

  let i_cons = push_call(&mut frag, rt.list_prepend_any(),
    vec![op_local(l_val), op_local(l_list)], Some(l_list));

  let i_app  = push_return_call(&mut frag, rt.apply(),
    vec![op_local(l_list), op_local(l_done)]);

  let _ = l_caps; // reserved for future use (captures)

  let fink_module = func(
    &mut frag,
    rt.fn2(),
    vec![
      local(val_anyref(false), "_caps"),
      local(val_anyref(false), "_args"),
    ],
    vec![
      local(val_anyref(false), "v_0"),
      local(val_ref(rt.num(), false), "v_1"),
      local(val_anyref(false), "args"),
    ],
    vec![i_head, i_box, i_nil, i_cons, i_app],
    "fink_module",
  );
  // Export fink_module as the module's bring-up entry.
  frag.funcs[fink_module.0 as usize].export = Some("fink_module".into());

  frag
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

/// Match `App { func: Val(ContRef(_)), args: [Val(Lit::Int|Float|Decimal)] }`
/// and return the literal's f64 value.
fn extract_ret_lit(body: &Expr) -> Option<f64> {
  let ExprKind::App { func: Callable::Val(func), args } = &body.kind else { return None };
  let Val { kind: ValKind::ContRef(_), .. } = func else { return None };
  let first_arg = args.first()?;
  let Arg::Val(v) = first_arg else { return None };
  let ValKind::Lit(lit) = &v.kind else { return None };
  match lit {
    Lit::Int(n)     => Some(*n as f64),
    Lit::Float(f)   => Some(*f),
    Lit::Decimal(f) => Some(*f),
    _ => None,
  }
}
