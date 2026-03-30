// Match lowering pass — rewrites Match* builtins into primitive CPS.
//
// Runs after the CPS transform, before lifting. At this point the IR is
// nested (not yet flat), and closures don't exist — everything is lexically
// scoped LetFn/LetVal/App/If.
//
// ## What this pass does
//
// Rewrites every `App { BuiltIn::MatchBlock, ... }` (and its constituent
// MatchArm, MatchValue, MatchIf, etc.) into chains of If/LetFn/App nodes
// using only primitives the codegen already handles.
//
// After this pass, no Match* builtins remain in the IR. The lifting pass
// then handles capture analysis and closure creation as usual.
//
// ## Emitted structure
//
// Every match expression, regardless of complexity, produces the same
// three-part structure:
//
//   mb_N = fn ..binds, cont:    -- match body: receives bindings, calls cont with result
//     cont <result_expr>
//
//   mp_N = fn subj, succ, fail: -- match pattern: tests subject, dispatches
//     if <test>,
//       fn: mb_N ..binds, succ
//       fn: fail _
//
//   m_0 = fn subj, cont:        -- match block: wires the fail chain
//     mp_1 subj, cont,
//       fn: mp_2 subj, cont,
//         fn: panic _
//
//   m_0 <subject>, <outer_cont>
//
// ### Naming conventions
//
//   mb_N  — match body for arm N
//   mp_N  — match pattern for arm N
//   m_0   — match block entry point (orchestrator)
//
// ### Calling conventions
//
//   mp_N(subj, succ, fail)  — succ-first, like `if then else`
//   mb_N(..binds, cont)     — bindings then continuation
//   m_0(subj, cont)         — subject then result continuation
//
// The succ/fail order matches `if` (true branch first, false branch second).
// Previously the CPS transform used fail/succ order to make inline
// continuation nesting read better, but with the flat structure that
// motivation is gone.
//
// ### Consistency over cleverness
//
// The same mb_N/mp_N/m_0 structure is emitted for every match — a single-arm
// literal match and a multi-arm guarded match with bindings produce the same
// shape. This keeps the lowering pass mechanical and predictable. The
// optimizer (wasm-opt) handles inlining and simplification.
//
// ## Matcher invariant: temps only, no bindings
//
// Matchers work exclusively with synthetic temp values (Bind::Synth).
// No named bindings are created inside the matcher — if a pattern fails,
// nothing should have been bound in scope. The matcher extracts and tests
// using temps, then on success forwards the temp values to succ.
// The body's params (mb_N) give temps their user-visible names.
//
// This means:
//   - The fail path is clean: nothing to undo, no partial bindings.
//   - The match_lower pass sees pure test-and-branch logic.
//   - Name binding is the body's responsibility, not the matcher's.
//
// ## Pattern lowering
//
// Each pattern type becomes a condition tested in mp_N:
//
//   Literal:   `if subj == lit`     (op_eq comparison)
//   Guard:     `if <guard_expr>`    (e.g. subj > 0)
//   Wildcard:  always succeeds      (mp_N directly calls succ with subj)
//   Variable:  always succeeds      (forward subj via succ, body names it)
//   Or-guard:  `if a or b`          (short-circuit or of sub-patterns)
//
// Sequence and record patterns will thread cursor/field state through
// the mp_N function. The m_0 wrapper provides a place for setup/teardown
// of iteration state when needed.
//
// ## Bool match is the base case
//
// All pattern tests bottom out at `match <bool>: true: ...; false: ...`,
// which the CPS transform emits as `ExprKind::If` directly. No recursion
// through the lowering pass — bool match is handled by CPS, everything
// else is handled here.
//
// ## Design: naive first, optimize later
//
// The initial implementation emits a linear fail chain (test arms in
// source order). This can later be replaced with a decision-tree optimizer
// (Maranget-style) that reorders tests to minimize redundant comparisons,
// detects exhaustiveness, and shares common subtrees — all producing the
// same CPS primitives.
//
// ## Pipeline position
//
//   source → parse → partial → scopes → CPS → **match_lower** → lifting → collect → emit
//

use crate::propgraph::PropGraph;
use crate::ast::AstId;
use crate::passes::cps::ir::*;

/// Allocator for creating new CPS nodes during lowering.
struct Alloc {
  origin: PropGraph<CpsId, Option<AstId>>,
}

impl Alloc {
  fn next(&mut self, ast_origin: Option<AstId>) -> CpsId {
    self.origin.push(ast_origin)
  }

  fn bind(&mut self, kind: Bind, ast_origin: Option<AstId>) -> BindNode {
    let id = self.next(ast_origin);
    BindNode { id, kind }
  }

  fn synth_bind(&mut self) -> BindNode {
    self.bind(Bind::Synth, None)
  }

  fn val<'src>(&mut self, kind: ValKind<'src>, ast_origin: Option<AstId>) -> Val<'src> {
    let id = self.next(ast_origin);
    Val { id, kind }
  }

  fn expr<'src>(&mut self, kind: ExprKind<'src>, ast_origin: Option<AstId>) -> Expr<'src> {
    let id = self.next(ast_origin);
    Expr { id, kind }
  }
}

/// Collected arm data extracted from a MatchArm chain.
struct Arm<'src> {
  matcher: Cont<'src>,
  body: Cont<'src>,
}

/// Lower all Match* builtins in the CPS IR into primitive operations.
pub fn lower(cps: CpsResult) -> CpsResult {
  let mut alloc = Alloc { origin: cps.origin };
  let root = lower_expr(cps.root, &mut alloc);
  CpsResult {
    root,
    origin: alloc.origin,
    bind_to_cps: cps.bind_to_cps,
    synth_alias: cps.synth_alias,
  }
}

/// Recursively walk the CPS tree, rewriting MatchArm/MatchBlock chains.
fn lower_expr<'src>(expr: Expr<'src>, alloc: &mut Alloc) -> Expr<'src> {
  let id = expr.id;
  match expr.kind {
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::MatchArm), args } => {
      lower_match_arm_chain(id, args, alloc)
    }

    // MatchBlock without a preceding MatchArm chain — shouldn't happen in
    // well-formed CPS, but handle gracefully by passing through.
    ExprKind::App { func: func @ Callable::BuiltIn(BuiltIn::MatchBlock), args } => {
      Expr { id, kind: ExprKind::App { func, args: lower_args(args, alloc) } }
    }

    // MatchIf: call the op, then If on the bool result.
    // Args: [Val(op), Val(arg0), ..., Val(fail), Cont(succ)]
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::MatchIf), args } => {
      lower_match_if(id, args, alloc)
    }

    // Recurse into all other expression types.
    ExprKind::LetVal { name, val, cont } => {
      Expr { id, kind: ExprKind::LetVal { name, val, cont: lower_cont(cont, alloc) } }
    }
    ExprKind::LetFn { name, params, fn_body, cont } => {
      Expr { id, kind: ExprKind::LetFn {
        name,
        params,
        fn_body: Box::new(lower_expr(*fn_body, alloc)),
        cont: lower_cont(cont, alloc),
      }}
    }
    ExprKind::App { func, args } => {
      Expr { id, kind: ExprKind::App { func, args: lower_args(args, alloc) } }
    }
    ExprKind::If { cond, then, else_ } => {
      Expr { id, kind: ExprKind::If {
        cond,
        then: Box::new(lower_expr(*then, alloc)),
        else_: Box::new(lower_expr(*else_, alloc)),
      }}
    }
  }
}

fn lower_cont<'src>(cont: Cont<'src>, alloc: &mut Alloc) -> Cont<'src> {
  match cont {
    Cont::Ref(_) => cont,
    Cont::Expr { args, body } => Cont::Expr {
      args,
      body: Box::new(lower_expr(*body, alloc)),
    },
  }
}

fn lower_args<'src>(args: Vec<Arg<'src>>, alloc: &mut Alloc) -> Vec<Arg<'src>> {
  args.into_iter().map(|arg| match arg {
    Arg::Cont(cont) => Arg::Cont(lower_cont(cont, alloc)),
    Arg::Expr(expr) => Arg::Expr(Box::new(lower_expr(*expr, alloc))),
    other => other,
  }).collect()
}

/// Process a MatchArm node: collect the chain of arms, find the MatchBlock
/// at the end, and emit the m_0/mp_N/mb_N structure.
fn lower_match_arm_chain<'src>(
  expr_id: CpsId,
  args: Vec<Arg<'src>>,
  alloc: &mut Alloc,
) -> Expr<'src> {
  // Collect the first arm from this MatchArm node.
  let mut arms: Vec<Arm<'src>> = Vec::new();
  let (matcher, body, arm_cont) = extract_match_arm_args(args);
  arms.push(Arm { matcher, body });

  // Follow the arm_cont chain to collect remaining arms and find MatchBlock.
  let (subjects, result_cont) = collect_arm_chain(arm_cont, &mut arms);

  // Emit the lowered structure.
  emit_match_block(expr_id, subjects, arms, result_cont, alloc)
}

/// Extract matcher, body, and arm_cont from MatchArm args.
///
/// MatchArm args: [Cont(matcher), Cont(body), Cont(arm_cont)]
fn extract_match_arm_args<'src>(mut args: Vec<Arg<'src>>) -> (Cont<'src>, Cont<'src>, Cont<'src>) {
  assert_eq!(args.len(), 3, "MatchArm expects exactly 3 args");
  let arm_cont = match args.pop().unwrap() { Arg::Cont(c) => c, _ => panic!("MatchArm arg 2: expected Cont") };
  let body = match args.pop().unwrap() { Arg::Cont(c) => c, _ => panic!("MatchArm arg 1: expected Cont") };
  let matcher = match args.pop().unwrap() { Arg::Cont(c) => c, _ => panic!("MatchArm arg 0: expected Cont") };
  (matcher, body, arm_cont)
}

/// Walk the arm_cont chain, collecting arms until we hit MatchBlock.
/// Returns the MatchBlock's subject values and result continuation.
fn collect_arm_chain<'src>(
  cont: Cont<'src>,
  arms: &mut Vec<Arm<'src>>,
) -> (Vec<Val<'src>>, Cont<'src>) {
  match cont {
    Cont::Expr { args: _, body } => {
      match body.kind {
        // Another MatchArm — collect it and continue.
        ExprKind::App { func: Callable::BuiltIn(BuiltIn::MatchArm), args } => {
          let (matcher, body, arm_cont) = extract_match_arm_args(args);
          arms.push(Arm { matcher, body });
          collect_arm_chain(arm_cont, arms)
        }
        // MatchBlock — end of chain. Extract subjects and result cont.
        ExprKind::App { func: Callable::BuiltIn(BuiltIn::MatchBlock), args } => {
          extract_match_block_args(args)
        }
        _ => panic!("collect_arm_chain: expected MatchArm or MatchBlock, got other expr"),
      }
    }
    Cont::Ref(_) => panic!("collect_arm_chain: expected Cont::Expr, got Cont::Ref"),
  }
}

/// Extract subject values and result continuation from MatchBlock args.
///
/// MatchBlock args: [Val(subj_0), ..., Val(arm_0), ..., Cont(result)]
/// The subjects come first, then arm refs, then the result cont.
/// We know how many arms there are from the collected arms vec,
/// but the subjects/arms are mixed Val args. The last arg is always Cont.
fn extract_match_block_args<'src>(
  args: Vec<Arg<'src>>,
) -> (Vec<Val<'src>>, Cont<'src>) {
  let mut vals: Vec<Val<'src>> = Vec::new();
  let mut result_cont = None;

  for arg in args {
    match arg {
      Arg::Val(v) => vals.push(v),
      Arg::Cont(c) => { result_cont = Some(c); break; }
      _ => panic!("extract_match_block_args: unexpected arg type"),
    }
  }

  let result_cont = result_cont.expect("MatchBlock must have a result Cont");

  // The vals contain [subjects..., arm_refs...].
  // Arm refs are the values produced by MatchArm nodes — they're Ref(Synth)
  // pointing at the arm binds. The subjects are the actual match subjects.
  // For now we assume 1 subject (the first val) and the rest are arm refs.
  // TODO: multi-subject match support.
  let subjects = if vals.is_empty() {
    vec![]
  } else {
    vec![vals.remove(0)]
  };
  // Remaining vals are arm refs — we don't need them since we already
  // collected the arms structurally from the MatchArm chain.

  (subjects, result_cont)
}

/// Emit the lowered match block structure:
///
///   LetFn mp_N (matcher for each arm)
///   LetFn mb_N (body for each arm)
///   -- fail chain: mp_1 subj, succ, fn: mp_2 subj, succ, fn: panic
fn emit_match_block<'src>(
  _expr_id: CpsId,
  subjects: Vec<Val<'src>>,
  arms: Vec<Arm<'src>>,
  result_cont: Cont<'src>,
  alloc: &mut Alloc,
) -> Expr<'src> {
  // For now, single-subject only.
  let subject = subjects.into_iter().next().expect("match must have at least one subject");

  // Build the fail chain from last arm to first.
  // The innermost fail is `panic`.
  let panic_val = alloc.val(ValKind::Panic, None);
  let mut fail_expr: Expr<'src> = alloc.expr(
    ExprKind::App {
      func: Callable::Val(panic_val),
      args: vec![],
    },
    None,
  );

  // Process arms in reverse to build the nested fail chain.
  for arm in arms.into_iter().rev() {
    fail_expr = emit_arm_try(subject.clone(), arm, result_cont.clone(), fail_expr, alloc);
  }

  // The result is the outermost try expression.
  // It needs to be lowered recursively since the matcher/body may contain Match* nodes.
  lower_expr(fail_expr, alloc)
}

/// Emit a single arm try: call the matcher with (subj, succ, fail).
///
/// The matcher is an inline Cont::Expr with params [subj_bind, fail_bind, succ_bind].
/// We need to:
///   1. Define the body as a LetFn (mb_N)
///   2. Inline the matcher, substituting its params with our values
///
/// For now, we emit the matcher and body as LetFn definitions, then call
/// the matcher with the subject, a succ cont that calls the body, and the
/// fail cont that tries the next arm.
fn emit_arm_try<'src>(
  subject: Val<'src>,
  arm: Arm<'src>,
  result_cont: Cont<'src>,
  fail_body: Expr<'src>,
  alloc: &mut Alloc,
) -> Expr<'src> {
  // Define mb_N (body function).
  let mb_name = alloc.synth_bind();
  let mb_id = mb_name.id;
  let mb_fn_body = cont_to_fn_body(arm.body);

  // Define mp_N (matcher function).
  let mp_name = alloc.synth_bind();
  let mp_id = mp_name.id;
  let mp_fn_body = rewrite_matcher(arm.matcher, alloc);

  // Build the call: mp_N(subj, succ, fail)
  // succ = fn ..binds: mb_N(..binds, result_cont)
  // fail = fn: <fail_body> (try next arm or panic)
  let mp_ref = alloc.val(ValKind::Ref(Ref::Synth(mp_id)), None);
  let mb_ref = alloc.val(ValKind::Ref(Ref::Synth(mb_id)), None);

  // Build succ continuation that bridges matcher → body.
  let succ_cont = build_succ_cont(mb_ref, result_cont, &mb_fn_body.0, alloc);

  // Build fail continuation: thunk that runs the next arm's try.
  let fail_cont = Cont::Expr {
    args: vec![],
    body: Box::new(fail_body),
  };

  // Call mp_N(subj, fail, succ)
  // The CPS transform creates matcher params as [subj, fail, succ].
  // TODO: switch to [subj, succ, fail] order (design decision) — requires
  // rewriting matcher bodies to swap fail/succ param positions.
  let call_mp = alloc.expr(
    ExprKind::App {
      func: Callable::Val(mp_ref),
      args: vec![Arg::Val(subject), Arg::Cont(fail_cont), Arg::Cont(succ_cont)],
    },
    None,
  );

  // Wrap: LetFn mb_N = ... in LetFn mp_N = ... in call_mp
  let with_mp = alloc.expr(
    ExprKind::LetFn {
      name: mp_name,
      params: mp_fn_body.0,
      fn_body: Box::new(mp_fn_body.1),
      cont: Cont::Expr { args: vec![], body: Box::new(call_mp) },
    },
    None,
  );

  alloc.expr(
    ExprKind::LetFn {
      name: mb_name,
      params: mb_fn_body.0,
      fn_body: Box::new(mb_fn_body.1),
      cont: Cont::Expr { args: vec![], body: Box::new(with_mp) },
    },
    None,
  )
}

/// Convert a Cont (from MatchArm's matcher or body) into (params, body) for a LetFn.
fn cont_to_fn_body<'src>(cont: Cont<'src>) -> (Vec<Param>, Expr<'src>) {
  match cont {
    Cont::Expr { args, body } => {
      let params = args.into_iter().map(Param::Name).collect();
      (params, *body)
    }
    Cont::Ref(_) => panic!("cont_to_fn_body: expected Cont::Expr, got Cont::Ref"),
  }
}

/// Rewrite matcher: the CPS transform emits matcher with params [subj, fail, succ].
/// We keep it as-is since the caller will pass args in the right order.
fn rewrite_matcher<'src>(matcher: Cont<'src>, _alloc: &mut Alloc) -> (Vec<Param>, Expr<'src>) {
  cont_to_fn_body(matcher)
}

/// Build a succ continuation that calls mb_N with bindings and result_cont.
///
/// The matcher calls succ with the bound values (or `_` for literals).
/// The body (mb_N) takes [..binds, block_cont].
/// Succ receives the matcher's output binds and forwards them + result_cont
/// to the body function.
fn build_succ_cont<'src>(
  mb_ref: Val<'src>,
  result_cont: Cont<'src>,
  body_params: &[Param],
  alloc: &mut Alloc,
) -> Cont<'src> {
  // The succ cont receives the same bindings the body expects (minus the
  // block_cont param, which is the result_cont we provide).
  //
  // body params: [bind_0, ..., bind_N, block_cont]
  // succ params: [bind_0, ..., bind_N]  (from matcher)
  //
  // succ body: mb_N(bind_0, ..., bind_N, result_cont)

  // Create fresh binds for succ's params (matching body's bind params).
  // The last body param is block_cont — not passed by matcher.
  let n_binds = if body_params.is_empty() { 0 } else { body_params.len() - 1 };
  let succ_params: Vec<BindNode> = (0..n_binds)
    .map(|_| alloc.synth_bind())
    .collect();

  // Build args for calling mb_N: forward binds + result_cont.
  let mut call_args: Vec<Arg<'src>> = succ_params.iter()
    .map(|p| Arg::Val(alloc.val(ValKind::Ref(Ref::Synth(p.id)), None)))
    .collect();
  call_args.push(Arg::Cont(result_cont));

  let call_body = alloc.expr(
    ExprKind::App {
      func: Callable::Val(mb_ref),
      args: call_args,
    },
    None,
  );

  Cont::Expr {
    args: succ_params,
    body: Box::new(call_body),
  }
}

/// Lower MatchIf into: call the op with args, then If on the bool result.
///
/// Input: `match_if op, arg0, ..., argN, fail, succ`
///   - op is a BuiltIn value (e.g. op_gt, op_or)
///   - args are the operands
///   - fail is a Val (Panic or ContRef)
///   - succ is a Cont (inline continuation)
///
/// Output:
///   op(arg0, ..., argN, fn result:
///     if result
///       then: <succ body>
///       else: fail _)
fn lower_match_if<'src>(
  _expr_id: CpsId,
  args: Vec<Arg<'src>>,
  alloc: &mut Alloc,
) -> Expr<'src> {
  // Parse args: [Val(op), Val(arg0), ..., Val(fail), Cont(succ)]
  let mut vals: Vec<Val<'src>> = Vec::new();
  let mut succ_cont = None;

  for arg in args {
    match arg {
      Arg::Val(v) => vals.push(v),
      Arg::Cont(c) => { succ_cont = Some(c); break; }
      _ => panic!("lower_match_if: unexpected arg type"),
    }
  }

  let succ_cont = succ_cont.expect("MatchIf must have a succ Cont");

  // vals = [op, arg0, ..., argN, fail]
  // op is first, fail is last, middle are operands.
  assert!(vals.len() >= 2, "MatchIf needs at least op + fail");
  let op_val = vals.remove(0);
  let fail_val = vals.pop().unwrap();
  let op_args = vals; // remaining are the operands

  // Build the succ branch expression from the succ continuation.
  let succ_expr = match succ_cont {
    Cont::Expr { args: succ_args, body } => {
      // The succ cont typically has no args (fn: succ _) or args for bindings.
      // We need to wrap it as an expression.
      if succ_args.is_empty() {
        lower_expr(*body, alloc)
      } else {
        // Has bind args — wrap as a LetVal chain? For now just lower the body.
        // The args are bind params that receive values from the succ call.
        // In match_if, succ is called with `_` (wildcard), so args are typically empty.
        lower_expr(*body, alloc)
      }
    }
    Cont::Ref(id) => {
      // Tail call to the continuation — wrap as App.
      let cont_val = alloc.val(ValKind::ContRef(id), None);
      alloc.expr(
        ExprKind::App { func: Callable::Val(cont_val), args: vec![] },
        None,
      )
    }
  };

  // Build the fail branch: call the fail continuation.
  let fail_expr = match &fail_val.kind {
    ValKind::Panic => {
      // Unreachable — emit App(panic).
      alloc.expr(
        ExprKind::App { func: Callable::Val(fail_val), args: vec![] },
        None,
      )
    }
    ValKind::ContRef(id) => {
      // Call the fail continuation (thunk — no args).
      let cont_val = alloc.val(ValKind::ContRef(*id), None);
      alloc.expr(
        ExprKind::App { func: Callable::Val(cont_val), args: vec![] },
        None,
      )
    }
    _ => {
      // Fail is a regular value ref (e.g. a fail param from a matcher).
      // Call it as a thunk.
      alloc.expr(
        ExprKind::App { func: Callable::Val(fail_val), args: vec![] },
        None,
      )
    }
  };

  // Build: op(args..., fn result: if result then succ else fail)
  let result_bind = alloc.synth_bind();
  let result_ref = alloc.val(ValKind::Ref(Ref::Synth(result_bind.id)), None);

  let if_expr = alloc.expr(
    ExprKind::If {
      cond: Box::new(result_ref),
      then: Box::new(succ_expr),
      else_: Box::new(fail_expr),
    },
    None,
  );

  // Call the op as a builtin with the operand args + continuation that receives the bool.
  let op_builtin = match op_val.kind {
    ValKind::BuiltIn(op) => op,
    _ => panic!("lower_match_if: op is not a BuiltIn"),
  };

  let mut call_args: Vec<Arg<'src>> = op_args.into_iter().map(Arg::Val).collect();
  call_args.push(Arg::Cont(Cont::Expr {
    args: vec![result_bind],
    body: Box::new(if_expr),
  }));

  alloc.expr(
    ExprKind::App {
      func: Callable::BuiltIn(op_builtin),
      args: call_args,
    },
    None,
  )
}

#[cfg(test)]
mod tests {
  use crate::parser::parse;
  use crate::ast::{build_index, NodeKind};
  use crate::passes::cps::fmt::Ctx;
  use crate::passes::scopes;
  use crate::passes::cps::transform::lower_module;

  fn match_lower(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let (root, node_count) = crate::passes::partial::apply(r.root, r.node_count)
          .unwrap_or_else(|e| panic!("partial pass failed: {:?}", e));
        let r = crate::ast::ParseResult { root, node_count };
        let ast_index = build_index(&r);
        let exprs = match &r.root.kind {
          NodeKind::Module(exprs) => exprs.items.as_slice(),
          _ => std::slice::from_ref(&r.root),
        };
        let scope = scopes::analyse(&r.root, r.node_count as usize, &[]);
        let cps = lower_module(exprs, &scope);
        let result = super::lower(cps);
        let ctx = Ctx { origin: &result.origin, ast_index: &ast_index, captures: None };
        crate::passes::cps::fmt::fmt_with(&result.root, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  test_macros::include_fink_tests!("src/passes/match_lower/test_match_lower.fnk");
}
