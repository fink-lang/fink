//! CPS → unlinked wasm IR `Fragment`.
//!
//! Tracer-phase. Grows by demand — each failing fixture in
//! `test_ir.fnk` pulls in exactly the CPS construct it needs.
//!
//! Current coverage:
//! * `main = fn: <lit>` — apply-path tail call (user-cont).
//!   Lit ∈ {Int, Float, Decimal, Bool}.
//! * `main = fn: <lit> <op> <lit>` — direct-style binary protocol
//!   operator. Op ∈ {+, -, *, /, //, %%, ==, !=, <, <=, >, >=,
//!   and, or, xor, <<, >>}.
//! * `main = fn: not <lit>` — direct-style unary protocol operator.

use crate::passes::ast::Ast;
use crate::passes::cps::ir::{Arg, Callable, Cont, CpsId, CpsResult, Expr, ExprKind, Lit, ValKind, BuiltIn};
use crate::sourcemap::native::ByteRange;

use super::ir::*;
use super::runtime_contract::{self, Runtime, Sym};

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

    // Direct-style binary-protocol operator:
    //   `App(BuiltIn::<BinOp>, [Lit, Lit, Cont::Ref(ƒret)])`
    // where <BinOp> ∈ {Add, Sub, Mul, ..., Eq, Lt, And, Or, Shl, ...}.
    ExprKind::App { func: Callable::BuiltIn(b), args } if binary_op_sym(*b).is_some() => {
      let sym = binary_op_sym(*b).unwrap();
      let ((a, a_id), (b_v, b_id)) = extract_two_lits(args)
        .unwrap_or_else(|| panic!(
          "ir_lower: unsupported {:?} args (expected [Lit, Lit, Cont])", b));
      let a_origin = origin_of(cps, ast, a_id);
      let b_origin = origin_of(cps, ast, b_id);
      let app_origin = origin_of(cps, ast, body.id);
      build_op_binary_body(&mut frag, &rt, sym, a, a_origin, b_v, b_origin, app_origin)
    }

    // Direct-style unary-protocol operator:
    //   `App(BuiltIn::Not, [Lit, Cont::Ref(ƒret)])`.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Not), args } => {
      let (lit, lit_id) = extract_first_lit(args)
        .unwrap_or_else(|| panic!("ir_lower: unsupported op_not args (expected [Lit, Cont])"));
      let lit_origin = origin_of(cps, ast, lit_id);
      let app_origin = origin_of(cps, ast, body.id);
      build_op_unary_body(&mut frag, &rt, Sym::OpNot, &lit, lit_origin, app_origin)
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

/// Direct-style binary-protocol operator body (op_plus / op_minus /
/// op_eq / op_and / ...): pop `done`, box both literal operands,
/// tail-call the operator's runtime function with (a, b, done).
///
/// Operands flow through as `anyref` — the runtime's polymorphic
/// dispatcher handles the actual type (Num, Str, Bool, ...) via
/// `br_on_cast`. Boxed numeric literals widen from `(ref $Num)` to
/// `(ref null any)` at the local.set.
///
/// Origins: each boxing instr maps to its literal; the `return_call`
/// maps to the `App` node (the whole expression).
fn build_op_binary_body(
  frag: &mut Fragment,
  rt: &Runtime,
  op: Sym,
  a: LitVal, a_origin: Option<ByteRange>,
  b: LitVal, b_origin: Option<ByteRange>,
  app_origin: Option<ByteRange>,
) -> FuncSym {
  let l_args_p = LocalIdx(1);
  let l_done   = LocalIdx(2);
  let l_a      = LocalIdx(3);
  let l_b      = LocalIdx(4);

  let i_head = push_call(frag, rt.args_head(), vec![op_local(l_args_p)], Some(l_done));
  let i_a = box_lit(frag, rt, &a, l_a);
  if let Some(o) = a_origin { set_origin(frag, i_a, o); }
  let i_b = box_lit(frag, rt, &b, l_b);
  if let Some(o) = b_origin { set_origin(frag, i_b, o); }
  let i_app = push_return_call(frag, rt.op(op),
    vec![op_local(l_a), op_local(l_b), op_local(l_done)]);
  if let Some(o) = app_origin { set_origin(frag, i_app, o); }

  func(frag, rt.fn2(),
    vec![
      local(val_anyref(true), "_caps"),
      local(val_anyref(true), "_args"),
    ],
    vec![
      local(val_anyref(true), "v_0"),
      local(val_anyref(true), "v_1"),
      local(val_anyref(true), "v_2"),
    ],
    vec![i_head, i_a, i_b, i_app],
    "fink_module",
  )
}

/// Direct-style unary-protocol operator body (op_not today).
/// Pops `done`, boxes the single literal operand, tail-calls
/// the operator's runtime function with (val, done).
fn build_op_unary_body(
  frag: &mut Fragment,
  rt: &Runtime,
  op: Sym,
  lit: &LitVal, lit_origin: Option<ByteRange>,
  app_origin: Option<ByteRange>,
) -> FuncSym {
  let l_args_p = LocalIdx(1);
  let l_done   = LocalIdx(2);
  let l_v      = LocalIdx(3);

  let i_head = push_call(frag, rt.args_head(), vec![op_local(l_args_p)], Some(l_done));
  let i_box = box_lit(frag, rt, lit, l_v);
  if let Some(o) = lit_origin { set_origin(frag, i_box, o); }
  let i_app = push_return_call(frag, rt.op(op),
    vec![op_local(l_v), op_local(l_done)]);
  if let Some(o) = app_origin { set_origin(frag, i_app, o); }

  func(frag, rt.fn2(),
    vec![
      local(val_anyref(true), "_caps"),
      local(val_anyref(true), "_args"),
    ],
    vec![
      local(val_anyref(true), "v_0"),
      local(val_anyref(true), "v_1"),
    ],
    vec![i_head, i_box, i_app],
    "fink_module",
  )
}

/// Box a literal into the given local slot. Num → `struct.new $Num`;
/// Bool → `ref.i31 0/1`. Result fits a `(ref null any)` local.
fn box_lit(frag: &mut Fragment, rt: &Runtime, lit: &LitVal, into: LocalIdx) -> InstrId {
  match lit {
    LitVal::Num(n) => push_struct_new(frag, rt.num(), vec![op_f64(*n)], into),
    LitVal::Bool(b) => push_ref_i31(frag, op_i32(if *b { 1 } else { 0 }), into),
  }
}

/// Map a CPS `BuiltIn` to the binary-protocol `Sym` it lowers to.
/// Returns `None` for unary / non-binary ops — those have their own
/// match arms in `lower`.
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

/// Extract two literal args in order, ignoring trailing `Arg::Cont`
/// (continuation). Each return pair is `(LitVal, CpsId)`. Supports
/// mixed kinds — the binary-op body builder handles Num and Bool
/// uniformly (`box_lit`).
fn extract_two_lits(args: &[Arg]) -> Option<((LitVal, CpsId), (LitVal, CpsId))> {
  let a = val_lit(args.first()?)?;
  let b = val_lit(args.get(1)?)?;
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

