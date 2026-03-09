// TODO: Add named builder helpers for Expr construction (like cps_fmt.rs has).
//       Each ExprKind variant is currently built inline with verbose struct literal
//       syntax; extracting small fns would make callsites read like a DSL.
//
// AST → compiler-internal CPS IR transform.
//
// Produces `cps::Expr` trees — clean structural IR with no env/state plumbing.
//
// Transform strategy: bottom-up accumulation.
//   lower(node) → (Val, Vec<Binding>)
// where `Binding` is a deferred let-binding. Bindings are woven into an
// Expr chain by `wrap(val, bindings, tail)` which builds right-to-left:
//
//   wrap(v, [LetVal(a,x), LetFn(f,p,b)], Ret(v))
//   → LetVal { name: a, val: x, body: LetFn { name: f, ... body: Ret(v) } }
//
// This avoids the monomorphization explosion that closures-as-continuations
// cause in Rust's type system.

use crate::ast::{CmpPart, Node, NodeKind};
use crate::lexer::Loc;
use crate::transform::cps::{
  Arg, BindName, Expr, ExprKind, Key, KeyKind, Lit, Meta, Name, Param, Prim,
  Val, ValKind,
};

// ---------------------------------------------------------------------------
// Name generator
// ---------------------------------------------------------------------------

pub struct Gen {
  counter: u32,
}

impl Gen {
  pub fn new() -> Self {
    Gen { counter: 0 }
  }

  fn next(&mut self) -> u32 {
    let n = self.counter;
    self.counter += 1;
    n
  }

  pub fn fresh_fn(&mut self) -> BindName<'static> {
    BindName::Gen(self.next())
  }

  pub fn fresh_result(&mut self) -> BindName<'static> {
    BindName::Gen(self.next())
  }

  /// Allocate a cursor index for seq/rec pattern traversal.
  /// The formatter renders this as `·m_N`.
  pub fn fresh_cursor(&mut self) -> u32 {
    self.next()
  }
}

// ---------------------------------------------------------------------------
// Deferred bindings — accumulated bottom-up (full definition below)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ident_val(name: BindName<'_>, loc: Loc) -> Val<'_> {
  Val { kind: ValKind::Ident(name), meta: Meta::at(loc) }
}

fn key_val_name<'src>(name: Name<'src>, loc: Loc) -> Val<'src> {
  Val {
    kind: ValKind::Key(Key { kind: KeyKind::Name(name), resolution: None, meta: Meta::at(loc) }),
    meta: Meta::at(loc),
  }
}

/// Create a Key reference to a BindName that needs loading from scope.
/// Used when a fn param needs to be referenced as a Key val (scope-stored by the runtime).
#[allow(dead_code)]
fn key_val_bind(name: BindName<'_>, loc: Loc) -> Val<'_> {
  Val {
    kind: ValKind::Key(Key { kind: KeyKind::Bind(name), resolution: None, meta: Meta::at(loc) }),
    meta: Meta::at(loc),
  }
}

fn key_val_prim(prim: Prim, loc: Loc) -> Val<'static> {
  Val {
    kind: ValKind::Key(Key { kind: KeyKind::Prim(prim), resolution: None, meta: Meta::at(loc) }),
    meta: Meta::at(loc),
  }
}

fn key_val_op<'src>(op: &'src str, loc: Loc) -> Val<'src> {
  Val {
    kind: ValKind::Key(Key { kind: KeyKind::Op(op), resolution: None, meta: Meta::at(loc) }),
    meta: Meta::at(loc),
  }
}

fn lit_val(lit: Lit<'_>, loc: Loc) -> Val<'_> {
  Val { kind: ValKind::Lit(lit), meta: Meta::at(loc) }
}

/// The `·panic` fail expression — irrefutable pattern failure; no recovery path.
fn panic_expr(loc: Loc) -> Expr<'static> {
  Expr { kind: ExprKind::Panic, meta: Meta::at(loc) }
}

/// A reference to `·ƒ_fail` — used as the fail cont inside match arm bodies.
fn fail_cont_expr(loc: Loc) -> Expr<'static> {
  Expr { kind: ExprKind::FailCont, meta: Meta::at(loc) }
}

fn ret_expr(val: Val<'_>, loc: Loc) -> Expr<'_> {
  Expr { kind: ExprKind::Ret(Box::new(val)), meta: Meta::at(loc) }
}

/// Emit an App node: func(args...) → result; body.
#[allow(dead_code)]
fn app_node<'src>(
  func: Val<'src>,
  args: Vec<Arg<'src>>,
  result: BindName<'src>,
  body: Expr<'src>,
  loc: Loc,
) -> Expr<'src> {
  Expr {
    kind: ExprKind::App { func: Box::new(func), args, result, body: Box::new(body) },
    meta: Meta::at(loc),
  }
}

/// Wrap a plain `Val` as an `Arg::Val`.
#[allow(dead_code)]
fn arg_val(val: Val<'_>) -> Arg<'_> {
  Arg::Val(val)
}

/// Wrap a `Vec<Val>` as `Vec<Arg::Val>` — for internal primitives that never spread.
fn args_val<'src>(vals: Vec<Val<'src>>) -> Vec<Arg<'src>> {
  vals.into_iter().map(Arg::Val).collect()
}

// ---------------------------------------------------------------------------
// Core lowering — returns (value_produced, bindings_accumulated)
// ---------------------------------------------------------------------------

type Lower<'src> = (Val<'src>, Vec<Pending<'src>>);

fn lower<'src>(g: &mut Gen, node: &'src Node<'src>) -> Lower<'src> {
  let loc = node.loc;
  match &node.kind {
    // ---- literals ----
    NodeKind::LitBool(b) => (lit_val(Lit::Bool(*b), loc), vec![]),
    NodeKind::LitInt(s)  => (lit_val(Lit::Int(parse_int(s)), loc), vec![]),
    NodeKind::LitFloat(s) => (lit_val(Lit::Float(parse_float(s)), loc), vec![]),
    NodeKind::LitDecimal(s) => (lit_val(Lit::Decimal(parse_decimal(s)), loc), vec![]),
    NodeKind::LitStr(s) => (lit_val(Lit::Str(s), loc), vec![]),

    // ---- identifier reference — scope lookup ----
    NodeKind::Ident(name) => (key_val_name(name, loc), vec![]),

    // ---- wildcard ----
    NodeKind::Wildcard => (key_val_name("_", loc), vec![]),

    // ---- group ----
    // A plain group `(expr)` is transparent.
    // A block group `(stmt; stmt)` parses to `Group(Fn { params: Patterns([]), body })` —
    // a zero-param closure that must be immediately invoked to produce a value.
    NodeKind::Group(inner) => match &inner.kind {
      NodeKind::Fn { params, body }
        if matches!(&params.kind, NodeKind::Patterns(ps) if ps.is_empty()) =>
      {
        lower_iife(g, params, body, loc)
      }
      _ => lower(g, inner),
    },

    // ---- try: lower transparently for now ----
    NodeKind::Try(inner) => lower(g, inner),

    // ---- bind: `name = rhs` ----
    NodeKind::Bind { lhs, rhs } => lower_bind(g, lhs, rhs, loc),

    // ---- bind-right: `rhs |= lhs` (swap) ----
    NodeKind::BindRight { lhs, rhs } => lower_bind(g, rhs, lhs, loc),

    // ---- fn: `fn params: body` ----
    NodeKind::Fn { params, body } => lower_fn(g, params, body, loc),

    // ---- apply: `func arg1 arg2` ----
    NodeKind::Apply { func, args } => lower_apply(g, func, args, loc),

    // ---- pipe: `a | b | c` == `c (b a)` ----
    NodeKind::Pipe(stages) => lower_pipe(g, stages, loc),

    // ---- infix op: `a + b` ----
    NodeKind::InfixOp { op, lhs, rhs } => lower_infix(g, op, lhs, rhs, loc),

    // ---- unary op: `-a`, `not a` ----
    NodeKind::UnaryOp { op, operand } => lower_unary(g, op, operand, loc),

    // ---- chained cmp: `a < b < c` ----
    NodeKind::ChainedCmp(parts) => lower_chained_cmp(g, parts, loc),

    // ---- member access: `lhs.rhs` ----
    NodeKind::Member { lhs, rhs } => lower_member(g, lhs, rhs, loc),

    // ---- sequence literal ----
    NodeKind::LitSeq(elems) => lower_lit_seq(g, elems, loc),

    // ---- record literal ----
    NodeKind::LitRec(fields) => lower_lit_rec(g, fields, loc),

    // ---- string template ----
    NodeKind::StrTempl(parts) => lower_str_templ(g, parts, loc),

    // ---- raw string template (tagged) ----
    NodeKind::StrRawTempl(parts) => lower_str_raw_templ(g, parts, loc),

    // ---- match ----
    NodeKind::Match { subjects, arms } => lower_match(g, subjects, arms, loc),

    // ---- block: `name params: body` ----
    NodeKind::Block { name, params, body } => lower_block(g, name, params, body, loc),

    // ---- should not appear post-partial-pass ----
    NodeKind::Partial => panic!("Partial should be eliminated before CPS transform"),

    // ---- spread in expression position ----
    NodeKind::Spread(inner) => {
      if let Some(inner) = inner {
        lower(g, inner)
      } else {
        panic!("Bare spread in expression position")
      }
    }

    // ---- structural nodes lowered via their parents ----
    NodeKind::Patterns(_) => panic!("Patterns node lowered via fn/match"),
    NodeKind::Arm { .. }  => panic!("Arm node lowered via lower_match"),
  }
}

/// Lower a sequence of statements and return an Expr for the whole sequence.
/// The last statement's value becomes the return value.
fn lower_stmts<'src>(g: &mut Gen, stmts: &'src [Node<'src>]) -> Expr<'src> {
  assert!(!stmts.is_empty(), "empty statement list");
  let mut all_pending: Vec<Pending<'src>> = vec![];
  let n = stmts.len();
  for (i, stmt) in stmts.iter().enumerate() {
    let is_last = i + 1 == n;
    if is_last {
      let (val, pending) = lower(g, stmt);
      all_pending.extend(pending);
      let tail = ret_expr(val, stmt.loc);
      return wrap(all_pending, tail);
    } else {
      // Statement in non-tail position.
      match &stmt.kind {
        // Bind introduces a name available in subsequent stmts.
        NodeKind::Bind { lhs, rhs } | NodeKind::BindRight { rhs: lhs, lhs: rhs } => {
          let pending = lower_bind_stmt(g, lhs, rhs, stmt.loc);
          all_pending.extend(pending);
        }
        // Any other statement: evaluate for effects, result discarded.
        _ => {
          let (val, pending) = lower(g, stmt);
          all_pending.extend(pending);
          let discard = g.fresh_result();
          all_pending.push(Pending::Val { name: discard, val, loc: stmt.loc });
        }
      }
    }
  }
  unreachable!()
}

// ---------------------------------------------------------------------------
// Bind
// ---------------------------------------------------------------------------

/// Lower a bind statement (not last in a block) — returns the `Pending` that
/// introduces the binding, so subsequent statements can use the name.
fn lower_bind_stmt<'src>(
  g: &mut Gen,
  lhs: &'src Node<'src>,
  rhs: &'src Node<'src>,
  loc: Loc,
) -> Vec<Pending<'src>> {
  let (val, mut pending) = lower(g, rhs);
  match &lhs.kind {
    NodeKind::Wildcard => {
      // _ discards — no store, just evaluate for side effects.
    }
    _ => {
      // All user binds (ident or pattern) are degenerate pattern matches.
      lower_pat_lhs(g, lhs, val, loc, &mut pending);
    }
  }
  pending
}

/// Lower a bind expression (the result IS the bound value — last in block or standalone).
fn lower_bind<'src>(
  g: &mut Gen,
  lhs: &'src Node<'src>,
  rhs: &'src Node<'src>,
  loc: Loc,
) -> Lower<'src> {
  let (val, mut pending) = lower(g, rhs);
  match &lhs.kind {
    NodeKind::Wildcard => {
      // _ discards the value — no store, just evaluate for side effects.
      (val, pending)
    }
    _ => {
      // All user binds (ident or pattern) are degenerate pattern matches.
      // lower_pat_lhs emits MatchLetVal for plain idents, Match* chains for patterns.
      let bound = lower_pat_lhs(g, lhs, val, loc, &mut pending);
      (ident_val(bound, loc), pending)
    }
  }
}

// ---------------------------------------------------------------------------
// Function definition
// ---------------------------------------------------------------------------

fn lower_fn<'src>(
  g: &mut Gen,
  params: &'src Node<'src>,
  body: &'src [Node<'src>],
  loc: Loc,
) -> Lower<'src> {
  let fn_name = g.fresh_fn();
  let (param_names, deferred) = extract_params_with_gen(g, params);
  let fn_body = prepend_pat_binds(deferred, lower_stmts(g, body));
  let pending = vec![Pending::Fn { name: fn_name, params: param_names, fn_body, loc }];
  (ident_val(fn_name, loc), pending)
}

/// Lower a block group `(stmt; stmt)` — immediately-invoked zero-param closure.
/// Defines the closure then emits an App that calls it right away.
fn lower_iife<'src>(
  g: &mut Gen,
  params: &'src Node<'src>,
  body: &'src [Node<'src>],
  loc: Loc,
) -> Lower<'src> {
  let fn_name = g.fresh_fn();
  let (param_names, deferred) = extract_params_with_gen(g, params);
  let fn_body = prepend_pat_binds(deferred, lower_stmts(g, body));
  let result = g.fresh_result();
  let pending = vec![
    Pending::Fn { name: fn_name, params: param_names, fn_body, loc },
    Pending::App { func: ident_val(fn_name, loc), args: args_val(vec![]), result, loc },
  ];
  (ident_val(result, loc), pending)
}

/// Extract params from a fn params node, returning:
/// - the param list (with complex patterns replaced by fresh Gen names)
/// - a list of Pending entries to prepend to the fn body via wrap().
/// Complex destructuring params (e.g. `[1, ..b]`) are desugared to a fresh spread
/// param `·v_N` and a set of Match* pending entries that destructure it.
fn extract_params_with_gen<'src>(
  g: &mut Gen,
  params: &'src Node<'src>,
) -> (Vec<Param<'src>>, Vec<Pending<'src>>) {
  let mut param_list = vec![];
  let mut deferred: Vec<Pending<'src>> = vec![];
  let nodes = match &params.kind {
    NodeKind::Patterns(ps) => ps.as_slice(),
    _ => std::slice::from_ref(params),
  };
  for p in nodes {
    match &p.kind {
      NodeKind::Ident(name) => param_list.push(Param::Name(BindName::User(name))),
      NodeKind::Wildcard => param_list.push(Param::Name(BindName::User("_"))),
      NodeKind::Patterns(ps) => {
        for inner in ps {
          param_list.push(Param::Name(match &inner.kind {
            NodeKind::Ident(name) => BindName::User(name),
            _ => BindName::User("_"),
          }));
        }
      }
      NodeKind::Spread(inner) => {
        let bind_name = match inner.as_deref() {
          Some(Node { kind: NodeKind::Ident(name), .. }) => BindName::User(name),
          _ => BindName::User("_"),
        };
        param_list.push(Param::Spread(bind_name));
      }
      // Complex destructuring param — desugar to a fresh plain param + Match* lowering in body.
      // The param receives a single value (not varargs); destructuring happens inside the fn.
      _ => {
        let param_name = g.fresh_result();
        param_list.push(Param::Name(param_name));
        lower_pat_lhs(g, p, ident_val(param_name, p.loc), p.loc, &mut deferred);
      }
    }
  }
  (param_list, deferred)
}

/// Wrap `body` in Match* nodes for each deferred pattern entry, innermost first.
fn prepend_pat_binds<'src>(deferred: Vec<Pending<'src>>, body: Expr<'src>) -> Expr<'src> {
  wrap(deferred, body)
}

fn extract_params<'src>(params: &'src Node<'src>) -> Vec<Param<'src>> {
  match &params.kind {
    NodeKind::Patterns(ps) => ps.iter().flat_map(|p| extract_param(p)).collect(),
    _ => extract_param(params),
  }
}

fn extract_param<'src>(param: &'src Node<'src>) -> Vec<Param<'src>> {
  match &param.kind {
    NodeKind::Ident(name) => vec![Param::Name(BindName::User(name))],
    NodeKind::Wildcard => vec![Param::Name(BindName::User("_"))],
    NodeKind::Patterns(ps) => ps.iter().flat_map(|p| extract_param(p)).collect(),
    // `..rest` varargs param — trailing spread.
    NodeKind::Spread(inner) => {
      let bind_name = match inner.as_deref() {
        Some(Node { kind: NodeKind::Ident(name), .. }) => BindName::User(name),
        _ => BindName::User("_"),
      };
      vec![Param::Spread(bind_name)]
    }
    // Complex destructuring params (e.g. `fn [a, b]: …`) — not yet implemented.
    _ => vec![],
  }
}

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------

fn lower_apply<'src>(
  g: &mut Gen,
  func: &'src Node<'src>,
  args: &'src [Node<'src>],
  loc: Loc,
) -> Lower<'src> {
  let (func_val, mut pending) = lower(g, func);
  let mut arg_vals = vec![];
  for arg in args {
    let is_spread = matches!(arg.kind, NodeKind::Spread(_));
    let inner = if is_spread {
      if let NodeKind::Spread(Some(inner)) = &arg.kind { inner.as_ref() } else { arg }
    } else {
      arg
    };
    let (av, ap) = lower(g, inner);
    pending.extend(ap);
    arg_vals.push(if is_spread { Arg::Spread(av) } else { Arg::Val(av) });
  }
  let result = g.fresh_result();
  pending.push(Pending::App { func: func_val, args: arg_vals, result, loc });
  (ident_val(result, loc), pending)
}

// ---------------------------------------------------------------------------
// Pipe: `a | b | c` == `c (b a)`
// ---------------------------------------------------------------------------

fn lower_pipe<'src>(g: &mut Gen, stages: &'src [Node<'src>], loc: Loc) -> Lower<'src> {
  assert!(!stages.is_empty(), "empty pipe");
  if stages.len() == 1 {
    return lower(g, &stages[0]);
  }
  // Fold left: head | f | g → g (f head)
  let (mut acc_val, mut pending) = lower(g, &stages[0]);
  for stage in &stages[1..] {
    let (func_val, sp) = lower(g, stage);
    pending.extend(sp);
    let result = g.fresh_result();
    pending.push(Pending::App { func: func_val, args: args_val(vec![acc_val]), result, loc });
    acc_val = ident_val(result, loc);
  }
  (acc_val, pending)
}

// ---------------------------------------------------------------------------
// Infix, unary, chained cmp
// ---------------------------------------------------------------------------

fn lower_infix<'src>(
  g: &mut Gen,
  op: &'src str,
  lhs: &'src Node<'src>,
  rhs: &'src Node<'src>,
  loc: Loc,
) -> Lower<'src> {
  if matches!(op, ".." | "...") {
    return lower_range(g, op, lhs, rhs, loc);
  }
  let (lv, mut pending) = lower(g, lhs);
  let (rv, rp) = lower(g, rhs);
  pending.extend(rp);
  let op_fn = key_val_op(op, loc);
  let result = g.fresh_result();
  pending.push(Pending::App { func: op_fn, args: args_val(vec![lv, rv]), result, loc });
  (ident_val(result, loc), pending)
}

fn lower_unary<'src>(
  g: &mut Gen,
  op: &'src str,
  operand: &'src Node<'src>,
  loc: Loc,
) -> Lower<'src> {
  let (val, mut pending) = lower(g, operand);
  let op_fn = key_val_op(op, loc);
  let result = g.fresh_result();
  pending.push(Pending::App { func: op_fn, args: args_val(vec![val]), result, loc });
  (ident_val(result, loc), pending)
}

fn lower_chained_cmp<'src>(
  g: &mut Gen,
  parts: &'src [CmpPart<'src>],
  loc: Loc,
) -> Lower<'src> {
  // `a < b < c` → `(a < b) and (b < c)`
  // Walk parts: collect Operand/Op pairs and emit pairwise comparisons.
  let mut pending: Vec<Pending<'src>> = vec![];
  let mut operands: Vec<Val<'src>> = vec![];
  let mut ops: Vec<&'src str> = vec![];

  for part in parts {
    match part {
      CmpPart::Operand(node) => {
        let (val, p) = lower(g, node);
        pending.extend(p);
        operands.push(val);
      }
      CmpPart::Op(op) => ops.push(op),
    }
  }

  // Now operands: [a, b, c], ops: [<, <]
  // Emit: cmp0 = a < b; cmp1 = b < c; result = cmp0 and cmp1
  let mut cmp_vals: Vec<Val<'src>> = vec![];
  for (i, op) in ops.iter().enumerate() {
    let lv = operands[i].clone();
    let rv = operands[i + 1].clone();
    let op_fn = key_val_op(op, loc);
    let cmp_result = g.fresh_result();
    pending.push(Pending::App { func: op_fn, args: args_val(vec![lv, rv]), result: cmp_result, loc });
    cmp_vals.push(ident_val(cmp_result, loc));
  }

  // And all comparison results together.
  let mut acc = cmp_vals.remove(0);
  for cv in cmp_vals {
    let and_fn = key_val_op("and", loc);
    let and_result = g.fresh_result();
    pending.push(Pending::App { func: and_fn, args: args_val(vec![acc, cv]), result: and_result, loc });
    acc = ident_val(and_result, loc);
  }
  (acc, pending)
}

// ---------------------------------------------------------------------------
// Range
// ---------------------------------------------------------------------------

fn lower_range<'src>(
  g: &mut Gen,
  op: &'src str,
  start: &'src Node<'src>,
  end: &'src Node<'src>,
  loc: Loc,
) -> Lower<'src> {
  let (sv, mut pending) = lower(g, start);
  let (ev, ep) = lower(g, end);
  pending.extend(ep);
  let range_key = key_val_op(op, loc);
  let result = g.fresh_result();
  pending.push(Pending::App { func: range_key, args: args_val(vec![sv, ev]), result, loc });
  (ident_val(result, loc), pending)
}

// ---------------------------------------------------------------------------
// Member access
// ---------------------------------------------------------------------------

fn lower_member<'src>(
  g: &mut Gen,
  lhs: &'src Node<'src>,
  rhs: &'src Node<'src>,
  loc: Loc,
) -> Lower<'src> {
  let (lv, mut pending) = lower(g, lhs);
  let (rv, rp) = lower(g, rhs);
  pending.extend(rp);
  let dot_fn = key_val_op(".", loc);
  let result = g.fresh_result();
  pending.push(Pending::App { func: dot_fn, args: args_val(vec![lv, rv]), result, loc });
  (ident_val(result, loc), pending)
}

// ---------------------------------------------------------------------------
// Sequence literal: `[a, b, ..c]`
// ---------------------------------------------------------------------------

fn lower_lit_seq<'src>(g: &mut Gen, elems: &'src [Node<'src>], loc: Loc) -> Lower<'src> {
  let mut acc = lit_val(Lit::Seq, loc);
  let mut pending: Vec<Pending<'src>> = vec![];
  for elem in elems {
    let is_spread = matches!(elem.kind, NodeKind::Spread(_));
    let inner = if is_spread {
      if let NodeKind::Spread(Some(inner)) = &elem.kind { inner.as_ref() } else { elem }
    } else {
      elem
    };
    let (ev, ep) = lower(g, inner);
    pending.extend(ep);
    let op_prim = if is_spread { Prim::SeqConcat } else { Prim::SeqAppend };
    let op_fn = key_val_prim(op_prim, loc);
    let result = g.fresh_result();
    pending.push(Pending::App { func: op_fn, args: args_val(vec![acc, ev]), result, loc });
    acc = ident_val(result, loc);
  }
  (acc, pending)
}

// ---------------------------------------------------------------------------
// Record literal: `{a, b: v, ..c}`
// ---------------------------------------------------------------------------

fn lower_lit_rec<'src>(g: &mut Gen, fields: &'src [Node<'src>], loc: Loc) -> Lower<'src> {
  let mut acc = lit_val(Lit::Rec, loc);
  let mut pending: Vec<Pending<'src>> = vec![];
  for field in fields {
    match &field.kind {
      NodeKind::Spread(Some(inner)) => {
        let (sv, sp) = lower(g, inner);
        pending.extend(sp);
        let op_fn = key_val_prim(Prim::RecMerge, loc);
        let result = g.fresh_result();
        pending.push(Pending::App { func: op_fn, args: args_val(vec![acc, sv]), result, loc });
        acc = ident_val(result, loc);
      }
      NodeKind::Bind { lhs, rhs } => {
        if let NodeKind::Ident(key) = &lhs.kind {
          let key_lit = lit_val(Lit::Str(key), field.loc);
          let (fv, fp) = lower(g, rhs);
          pending.extend(fp);
          let op_fn = key_val_prim(Prim::RecPut, loc);
          let result = g.fresh_result();
          pending.push(Pending::App { func: op_fn, args: args_val(vec![acc, key_lit, fv]), result, loc });
          acc = ident_val(result, loc);
        } else {
          // Computed key.
          let (kv, kp) = lower(g, lhs);
          let (fv, fp) = lower(g, rhs);
          pending.extend(kp);
          pending.extend(fp);
          let op_fn = key_val_prim(Prim::RecPut, loc);
          let result = g.fresh_result();
          pending.push(Pending::App { func: op_fn, args: args_val(vec![acc, kv, fv]), result, loc });
          acc = ident_val(result, loc);
        }
      }
      // `{foo: val}` parsed as Arm { lhs: [Ident("foo")], body: [val] }
      NodeKind::Arm { lhs, body } if !lhs.is_empty() => {
        let key_node = &lhs[0];
        let val_node = body.last().expect("arm body empty");
        if let NodeKind::Ident(key) = &key_node.kind {
          let key_lit = lit_val(Lit::Str(key), field.loc);
          let (fv, fp) = lower(g, val_node);
          pending.extend(fp);
          let op_fn = key_val_prim(Prim::RecPut, loc);
          let result = g.fresh_result();
          pending.push(Pending::App { func: op_fn, args: args_val(vec![acc, key_lit, fv]), result, loc });
          acc = ident_val(result, loc);
        } else {
          let (kv, kp) = lower(g, key_node);
          let (fv, fp) = lower(g, val_node);
          pending.extend(kp);
          pending.extend(fp);
          let op_fn = key_val_prim(Prim::RecPut, loc);
          let result = g.fresh_result();
          pending.push(Pending::App { func: op_fn, args: args_val(vec![acc, kv, fv]), result, loc });
          acc = ident_val(result, loc);
        }
      }
      NodeKind::Ident(name) => {
        // Shorthand `{foo}` == `{foo: foo}`
        let key_lit = lit_val(Lit::Str(name), field.loc);
        let id_val = key_val_name(name, field.loc);
        let op_fn = key_val_prim(Prim::RecPut, loc);
        let result = g.fresh_result();
        pending.push(Pending::App { func: op_fn, args: args_val(vec![acc, key_lit, id_val]), result, loc });
        acc = ident_val(result, loc);
      }
      _ => {
        let (fv, fp) = lower(g, field);
        pending.extend(fp);
        let op_fn = key_val_prim(Prim::RecMerge, loc);
        let result = g.fresh_result();
        pending.push(Pending::App { func: op_fn, args: args_val(vec![acc, fv]), result, loc });
        acc = ident_val(result, loc);
      }
    }
  }
  (acc, pending)
}

// ---------------------------------------------------------------------------
// String template: `'hello ${name}'`
// ---------------------------------------------------------------------------

fn lower_str_templ<'src>(g: &mut Gen, parts: &'src [Node<'src>], loc: Loc) -> Lower<'src> {
  let mut pending: Vec<Pending<'src>> = vec![];
  let mut part_vals: Vec<Arg<'src>> = vec![];
  for part in parts {
    let (pv, pp) = lower(g, part);
    pending.extend(pp);
    part_vals.push(Arg::Val(pv));
  }
  let str_fmt_fn = key_val_prim(Prim::StrFmt, loc);
  let result = g.fresh_result();
  pending.push(Pending::App { func: str_fmt_fn, args: part_vals, result, loc });
  (ident_val(result, loc), pending)
}

// ---------------------------------------------------------------------------
// Raw string template (tagged): `tag'...'`
// First element of `parts` is the tag function; rest are string segments.
// ---------------------------------------------------------------------------

fn lower_str_raw_templ<'src>(g: &mut Gen, parts: &'src [Node<'src>], loc: Loc) -> Lower<'src> {
  assert!(!parts.is_empty(), "empty raw string template");
  let (tag_fn, mut pending) = lower(g, &parts[0]);
  let mut part_vals: Vec<Arg<'src>> = vec![];
  for part in &parts[1..] {
    let (pv, pp) = lower(g, part);
    pending.extend(pp);
    part_vals.push(Arg::Val(pv));
  }
  let result = g.fresh_result();
  pending.push(Pending::App { func: tag_fn, args: part_vals, result, loc });
  (ident_val(result, loc), pending)
}

// ---------------------------------------------------------------------------
// Match
// ---------------------------------------------------------------------------

fn lower_match<'src>(
  g: &mut Gen,
  subjects: &'src Node<'src>,
  arms: &'src [Node<'src>],
  loc: Loc,
) -> Lower<'src> {
  let subject_nodes: &[Node<'src>] = match &subjects.kind {
    NodeKind::Patterns(ps) => ps.as_slice(),
    _ => std::slice::from_ref(subjects),
  };
  let mut pending: Vec<Pending<'src>> = vec![];

  let params: Vec<Val<'src>> = subject_nodes.iter().map(|s| {
    let (v, sp) = lower(g, s);
    pending.extend(sp);
    v
  }).collect();
  let arm_params: Vec<BindName<'src>> = params.iter().map(|_| g.fresh_result()).collect();
  let cps_arms: Vec<Expr<'src>> = arms.iter()
    .map(|arm| lower_match_arm(g, arm, &arm_params, loc))
    .collect();
  let result = g.fresh_result();
  pending.push(Pending::MatchBlock { params, arm_params, arms: cps_arms, result, loc });
  let result = result;
  (ident_val(result, loc), pending)
}

fn lower_match_arm<'src>(g: &mut Gen, arm: &'src Node<'src>, arm_params: &[BindName<'src>], _loc: Loc) -> Expr<'src> {
  match &arm.kind {
    NodeKind::Arm { lhs, body } => {
      let loc = arm.loc;
      let lhs_nodes: &[Node<'src>] = match lhs.first().map(|n| &n.kind) {
        Some(NodeKind::Patterns(ps)) => ps.as_slice(),
        _ => lhs.as_slice(),
      };
      let mut arm_pending: Vec<Pending<'src>> = vec![];
      for (pat_node, &param) in lhs_nodes.iter().zip(arm_params.iter()) {
        let scrutinee_val = ident_val(param, loc);
        lower_pat_lhs(g, pat_node, scrutinee_val, loc, &mut arm_pending);
      }
      let arm_tail = lower_stmts(g, body);
      wrap_with_fail(arm_pending, arm_tail, fail_cont_expr)
    }
    _ => panic!("lower_match_arm: expected Arm node"),
  }
}

// ---------------------------------------------------------------------------
// Block: `name params: body`
// ---------------------------------------------------------------------------

fn lower_block<'src>(
  g: &mut Gen,
  name: &'src Node<'src>,
  params: &'src Node<'src>,
  body: &'src [Node<'src>],
  loc: Loc,
) -> Lower<'src> {
  let block_fn_name = g.fresh_fn();
  let param_names = extract_params(params);
  let fn_body = lower_stmts(g, body);
  let (name_val, mut pending) = lower(g, name);
  pending.push(Pending::Fn { name: block_fn_name, params: param_names, fn_body, loc });
  let result = g.fresh_result();
  pending.push(Pending::App {
    func: name_val,
    args: args_val(vec![ident_val(block_fn_name, loc)]),
    result,
    loc,
  });
  (ident_val(result, loc), pending)
}

// ---------------------------------------------------------------------------
// Wrap — builds the Expr chain from Pending bindings
// ---------------------------------------------------------------------------

// Extend Pending to handle App and Match, which need a body (the next expression).
enum Pending<'src> {
  Val { name: BindName<'src>, val: Val<'src>, loc: Loc },
  Fn { name: BindName<'src>, params: Vec<Param<'src>>, fn_body: Expr<'src>, loc: Loc },
  App { func: Val<'src>, args: Vec<Arg<'src>>, result: BindName<'src>, loc: Loc },
  MatchBlock { params: Vec<Val<'src>>, arm_params: Vec<BindName<'src>>, arms: Vec<Expr<'src>>, result: BindName<'src>, loc: Loc },
  /// Pattern-lowered bind — emits MatchLetVal with ·panic as fail cont.
  MatchBind { name: BindName<'src>, val: Val<'src>, loc: Loc },
  /// Pattern-lowered guard check — emits MatchIf with ·panic as fail cont.
  MatchGuard { func: Val<'src>, args: Vec<Val<'src>>, loc: Loc },
  /// Literal equality check — emits MatchValue with ·panic as fail cont.
  MatchValue { val: Val<'src>, lit: Lit<'src>, loc: Loc },
  /// Seq pattern entry — emits MatchSeq with ·panic as fail cont.
  MatchSeq { val: Val<'src>, cursor: u32, loc: Loc },
  /// Pop head from seq — emits MatchNext with ·panic as fail cont.
  MatchNext { val: Val<'src>, cursor: u32, next_cursor: u32, elem: BindName<'src>, loc: Loc },
  /// Seq pattern exhaustion — emits MatchDone with ·panic as fail cont.
  MatchDone { val: Val<'src>, cursor: u32, result: BindName<'src>, loc: Loc },
  /// Assert cursor non-empty — emits MatchNotDone with ·panic as fail cont.
  MatchNotDone { val: Val<'src>, cursor: u32, loc: Loc },
  /// Bind remaining elements — emits MatchRest with ·panic as fail cont.
  MatchRest { val: Val<'src>, cursor: u32, result: BindName<'src>, loc: Loc },
  /// Rec pattern entry — emits MatchRec with ·panic as fail cont.
  MatchRec { val: Val<'src>, cursor: u32, loc: Loc },
  /// Extract named field from rec — emits MatchField with ·panic as fail cont.
  MatchField { val: Val<'src>, cursor: u32, next_cursor: u32, field: &'src str, elem: BindName<'src>, loc: Loc },
}

fn wrap<'src>(bindings: Vec<Pending<'src>>, tail: Expr<'src>) -> Expr<'src> {
  wrap_with_fail(bindings, tail, panic_expr)
}

/// Like `wrap`, but uses `make_fail(loc)` to produce the fail cont for each Match* node.
/// Used for arm bodies inside a MatchBlock, where failure should delegate to `·ƒ_fail`.
fn wrap_with_fail<'src>(
  bindings: Vec<Pending<'src>>,
  tail: Expr<'src>,
  make_fail: fn(Loc) -> Expr<'static>,
) -> Expr<'src> {
  bindings.into_iter().rev().fold(tail, |body, pending| match pending {
    Pending::Val { name, val, loc } => Expr {
      kind: ExprKind::LetVal { name, val: Box::new(val), body: Box::new(body) },
      meta: Meta::at(loc),
    },
    Pending::Fn { name, params, fn_body, loc } => Expr {
      kind: ExprKind::LetFn {
        name,
        params,
        free_vars: vec![],
        fn_body: Box::new(fn_body),
        body: Box::new(body),
      },
      meta: Meta::at(loc),
    },
    Pending::App { func, args, result, loc } => Expr {
      kind: ExprKind::App {
        func: Box::new(func),
        args,
        result,
        body: Box::new(body),
      },
      meta: Meta::at(loc),
    },
    Pending::MatchBlock { params, arm_params, arms, result, loc } => Expr {
      kind: ExprKind::MatchBlock {
        params,
        arm_params,
        fail: Box::new(panic_expr(loc)),
        arms,
        result,
        body: Box::new(body),
      },
      meta: Meta::at(loc),
    },
    Pending::MatchBind { name, val, loc } => Expr {
      kind: ExprKind::MatchLetVal {
        name,
        val: Box::new(val),
        fail: Box::new(make_fail(loc)),
        body: Box::new(body),
      },
      meta: Meta::at(loc),
    },
    Pending::MatchGuard { func, args, loc } => Expr {
      kind: ExprKind::MatchIf {
        func: Box::new(func),
        args,
        fail: Box::new(make_fail(loc)),
        body: Box::new(body),
      },
      meta: Meta::at(loc),
    },
    Pending::MatchValue { val, lit, loc } => Expr {
      kind: ExprKind::MatchValue {
        val: Box::new(val),
        lit,
        fail: Box::new(make_fail(loc)),
        body: Box::new(body),
      },
      meta: Meta::at(loc),
    },
    Pending::MatchSeq { val, cursor, loc } => Expr {
      kind: ExprKind::MatchSeq {
        val: Box::new(val),
        cursor,
        fail: Box::new(make_fail(loc)),
        body: Box::new(body),
      },
      meta: Meta::at(loc),
    },
    Pending::MatchNext { val, cursor, next_cursor, elem, loc } => Expr {
      kind: ExprKind::MatchNext {
        val: Box::new(val),
        cursor,
        next_cursor,
        fail: Box::new(make_fail(loc)),
        elem,
        body: Box::new(body),
      },
      meta: Meta::at(loc),
    },
    Pending::MatchDone { val, cursor, result, loc } => Expr {
      kind: ExprKind::MatchDone {
        val: Box::new(val),
        cursor,
        fail: Box::new(make_fail(loc)),
        result,
        body: Box::new(body),
      },
      meta: Meta::at(loc),
    },
    Pending::MatchNotDone { val, cursor, loc } => Expr {
      kind: ExprKind::MatchNotDone {
        val: Box::new(val),
        cursor,
        fail: Box::new(make_fail(loc)),
        body: Box::new(body),
      },
      meta: Meta::at(loc),
    },
    Pending::MatchRest { val, cursor, result, loc } => Expr {
      kind: ExprKind::MatchRest {
        val: Box::new(val),
        cursor,
        fail: Box::new(make_fail(loc)),
        result,
        body: Box::new(body),
      },
      meta: Meta::at(loc),
    },
    Pending::MatchRec { val, cursor, loc } => Expr {
      kind: ExprKind::MatchRec {
        val: Box::new(val),
        cursor,
        fail: Box::new(make_fail(loc)),
        body: Box::new(body),
      },
      meta: Meta::at(loc),
    },
    Pending::MatchField { val, cursor, next_cursor, field, elem, loc } => Expr {
      kind: ExprKind::MatchField {
        val: Box::new(val),
        cursor,
        next_cursor,
        field,
        fail: Box::new(make_fail(loc)),
        elem,
        body: Box::new(body),
      },
      meta: Meta::at(loc),
    },
  })
}


// ---------------------------------------------------------------------------
// Numeric helpers
// ---------------------------------------------------------------------------

fn parse_int(s: &str) -> i64 {
  s.replace('_', "").parse().unwrap_or(0)
}

fn parse_float(s: &str) -> f64 {
  s.replace('_', "").parse().unwrap_or(0.0)
}

fn parse_decimal(s: &str) -> f64 {
  let s = s.strip_suffix('d').unwrap_or(s);
  s.replace('_', "").parse().unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Lower a top-level block of statements (module body).
pub fn lower_module<'src>(stmts: &'src [Node<'src>]) -> Expr<'src> {
  if stmts.is_empty() {
    let loc = crate::lexer::Loc {
      start: crate::lexer::Pos { idx: 0, line: 1, col: 0 },
      end:   crate::lexer::Pos { idx: 0, line: 1, col: 0 },
    };
    return ret_expr(lit_val(Lit::Seq, loc), loc);
  }
  let mut g = Gen::new();
  lower_stmts(&mut g, stmts)
}

/// Lower a single expression node.
pub fn lower_expr<'src>(node: &'src Node<'src>) -> Expr<'src> {
  let mut g = Gen::new();
  let (val, pending) = lower(&mut g, node);
  let tail = ret_expr(val, node.loc);
  wrap(pending, tail)
}

/// Recursively lower a pattern lhs node, appending Match* pending entries.
/// `val` is the scrutinee already lowered from the rhs.
/// Returns the BindName of the primary binding (used by the caller to construct Ret).
///
/// Implemented: Ident, Wildcard, BindRight, InfixOp (guard + range), Apply (→ MatchGuard predicate),
///              LitInt/Float/Bool/Str (→ MatchValue), LitSeq (plain elems + Spread tail),
///              LitRec (fields + spread variants), Range (→ lower_range + MatchGuard w/ ·op_in).
/// TODO: Apply → MatchApp (after name resolution).
/// TODO(future): StrTempl pattern matching — e.g. `'hello ${name}'` in pattern position;
///               deferred, needs a string-matching primitive (·match_str_prefix or similar).
fn lower_pat_lhs<'src>(
  g: &mut Gen,
  lhs: &'src Node<'src>,
  val: Val<'src>,
  loc: Loc,
  pending: &mut Vec<Pending<'src>>,
) -> BindName<'src> {
  match &lhs.kind {
    // Plain bind: `x = foo`
    NodeKind::Ident(name) => {
      let bind_name = BindName::User(name);
      pending.push(Pending::MatchBind { name: bind_name, val, loc });
      bind_name
    }

    // Wildcard: `_` — no binding; pass the val through as-is for guard args.
    // Val must be an Ident (always true when called from Apply arg lowering).
    NodeKind::Wildcard => {
      match val.kind {
        ValKind::Ident(name) => name,
        _ => panic!("lower_pat_lhs: Wildcard with non-Ident val"),
      }
    }

    // Range pattern: `0..10` or `0...10` — assert val is in range; no binding produced.
    // Evaluates the range as a value, then guards with `·op_in`.
    // Returns val's ident directly (no binding allocation — range is a pure guard).
    NodeKind::InfixOp { op, lhs: start, rhs: end } if matches!(*op, ".." | "...") => {
      let (range_val, rp) = lower_range(g, op, start, end, loc);
      pending.extend(rp);
      let in_fn = key_val_op("in", loc);
      pending.push(Pending::MatchGuard { func: in_fn, args: vec![val.clone(), range_val], loc });
      // Extract the bind name from val: Ident → use directly; Key(Name) → wrap as User.
      // Range is a pure guard; no new binding is allocated.
      match val.kind {
        ValKind::Ident(name)                               => name,
        ValKind::Key(Key { kind: KeyKind::Name(n), .. })  => BindName::User(n),
        _                                                  => g.fresh_result(),
      }
    }

    // Guarded bind: `a > 0 = foo` or `a > 0 or a < 9 = foo`
    // The innermost ident is the binding; the infix is the guard.
    NodeKind::InfixOp { op, lhs: guard_lhs, rhs: guard_rhs } => {
      let bind_name = extract_bind_name(guard_lhs);
      pending.push(Pending::MatchBind { name: bind_name, val, loc });
      let (lv, lp) = lower(g, guard_lhs);
      let (rv, rp) = lower(g, guard_rhs);
      pending.extend(lp);
      pending.extend(rp);
      let op_fn = key_val_op(op, loc);
      pending.push(Pending::MatchGuard { func: op_fn, args: vec![lv, rv], loc });
      bind_name
    }

    // Predicate guard: `is_even y`, `Ok b`, `foo 2, a, 3`
    // In pattern position, Apply args are either:
    //   - Ident/Wildcard — sub-pattern: binds to or discards `val` (the seq element)
    //   - Anything else  — expression: lowered normally and passed as-is to the guard
    // Exactly one arg should be an Ident/Wildcard (the "binding slot"); others are
    // literal/value args. All are assembled in order as arguments to MatchGuard.
    NodeKind::Apply { func, args } => {
      let mut arg_vals: Vec<Val<'src>> = vec![];
      for arg in args.iter() {
        let arg_val = match &arg.kind {
          NodeKind::Ident(_) | NodeKind::Wildcard => {
            let bound = lower_pat_lhs(g, arg, val.clone(), arg.loc, pending);
            ident_val(bound, arg.loc)
          }
          _ => {
            let (v, ap) = lower(g, arg);
            pending.extend(ap);
            v
          }
        };
        arg_vals.push(arg_val);
      }
      let (func_val, func_pending) = lower(g, func);
      pending.extend(func_pending);
      pending.push(Pending::MatchGuard { func: func_val, args: arg_vals, loc });
      g.fresh_result()
    }

    // Literal equality: `1`, `'hello'`, `true` — emits MatchValue; no binding produced.
    // Returns val itself (the scrutinee) as the "result" for the caller — it's a check, not a bind.
    NodeKind::LitInt(s) => {
      pending.push(Pending::MatchValue { val: val.clone(), lit: Lit::Int(parse_int(s)), loc });
      // MatchValue has no result binding; return a fresh slot so the caller can still chain.
      g.fresh_result()
    }
    NodeKind::LitFloat(s) => {
      pending.push(Pending::MatchValue { val: val.clone(), lit: Lit::Float(parse_float(s)), loc });
      g.fresh_result()
    }
    NodeKind::LitBool(b) => {
      pending.push(Pending::MatchValue { val: val.clone(), lit: Lit::Bool(*b), loc });
      g.fresh_result()
    }
    NodeKind::LitStr(s) => {
      pending.push(Pending::MatchValue { val: val.clone(), lit: Lit::Str(s), loc });
      g.fresh_result()
    }

    // Seq pattern: `[] = foo`, `[a, b] = foo`, `[a, []] = foo`, `[head, ..tail] = foo`
    NodeKind::LitSeq(elems) => {
      let seq_cursor = g.fresh_cursor();
      pending.push(Pending::MatchSeq { val: val.clone(), cursor: seq_cursor, loc });
      let mut cur = seq_cursor;
      let mut spread_seen = false;
      for elem_node in elems.iter() {
        match &elem_node.kind {
          // Spread element: `..` (discard non-empty) or `..name` (bind rest)
          NodeKind::Spread(inner) => {
            spread_seen = true;
            match inner {
              None => {
                // `[..]` — assert non-empty, discard rest
                pending.push(Pending::MatchNotDone { val: val.clone(), cursor: cur, loc });
              }
              Some(name_node) => {
                // `[..rest]` — bind remaining elements
                let result = g.fresh_result();
                pending.push(Pending::MatchRest { val: val.clone(), cursor: cur, result, loc });
                // Bind the rest value to the name
                if let NodeKind::Ident(name) = &name_node.kind {
                  pending.push(Pending::MatchBind {
                    name: BindName::User(name),
                    val: ident_val(result, loc),
                    loc,
                  });
                }
              }
            }
            // Spread must be last — stop processing elements
            break;
          }
          // Regular element: extract head, recurse
          _ => {
            let elem = g.fresh_result();
            let next = g.fresh_cursor();
            pending.push(Pending::MatchNext { val: val.clone(), cursor: cur, next_cursor: next, elem, loc });
            cur = next;
            let elem_val = ident_val(elem, loc);
            lower_pat_lhs(g, elem_node, elem_val, elem_node.loc, pending);
          }
        }
      }
      // Only emit MatchDone if no spread consumed the tail
      if spread_seen {
        g.fresh_result()  // placeholder return; no MatchDone
      } else {
        let result = g.fresh_result();
        pending.push(Pending::MatchDone { val, cursor: cur, result, loc });
        result
      }
    }

    // Rec pattern: `{} = foo`, `{x, y} = point`, `{bar, ..rest} = foo`, `{bar, ..{}} = foo`
    // Mirrors LitSeq lowering: open cursor with MatchRec, extract fields with MatchField,
    // close with MatchDone (closed/exact) or leave open (partial/open rest).
    NodeKind::LitRec(fields) => {
      let rec_cursor = g.fresh_cursor();
      pending.push(Pending::MatchRec { val: val.clone(), cursor: rec_cursor, loc });
      let mut cur = rec_cursor;
      let mut _spread_seen = false;
      for field_node in fields.iter() {
        match &field_node.kind {
          // Spread element: `..` (discard non-empty), `..rest` (bind rest), `..{}` (exact close)
          NodeKind::Spread(inner) => {
            _spread_seen = true;
            match inner {
              None => {
                // `{..}` — assert non-empty, discard rest (open partial match)
                pending.push(Pending::MatchNotDone { val: val.clone(), cursor: cur, loc });
              }
              Some(inner_node) => match &inner_node.kind {
                // `{..rest}` — bind remaining fields as a record
                NodeKind::Ident(name) => {
                  let result = g.fresh_result();
                  pending.push(Pending::MatchRest { val: val.clone(), cursor: cur, result, loc });
                  pending.push(Pending::MatchBind {
                    name: BindName::User(name),
                    val: ident_val(result, loc),
                    loc,
                  });
                }
                // `{..{sub_pat}}` — bind rest then destructure as a rec sub-pattern
                NodeKind::LitRec(_) => {
                  let result = g.fresh_result();
                  pending.push(Pending::MatchRest { val: val.clone(), cursor: cur, result, loc });
                  let rest_val = ident_val(result, loc);
                  lower_pat_lhs(g, inner_node, rest_val, inner_node.loc, pending);
                }
                _ => {}
              }
            }
            break;
          }
          // `{x}` shorthand — extract field named x, bind to x
          NodeKind::Ident(name) => {
            let elem = g.fresh_result();
            let next = g.fresh_cursor();
            pending.push(Pending::MatchField {
              val: val.clone(), cursor: cur, next_cursor: next,
              field: name, elem, loc,
            });
            cur = next;
            pending.push(Pending::MatchBind { name: BindName::User(name), val: ident_val(elem, loc), loc });
          }
          // `{x: pat}` — extract field x, lower pat against extracted val
          // Parsed as Bind { lhs: Ident(key), rhs: pat } or Arm { lhs: [Ident(key)], body: [pat] }
          NodeKind::Bind { lhs, rhs: pat_node } => {
            if let NodeKind::Ident(key) = &lhs.kind {
              let elem = g.fresh_result();
              let next = g.fresh_cursor();
              pending.push(Pending::MatchField {
                val: val.clone(), cursor: cur, next_cursor: next,
                field: key, elem, loc,
              });
              cur = next;
              let elem_val = ident_val(elem, loc);
              lower_pat_lhs(g, pat_node, elem_val, pat_node.loc, pending);
            }
          }
          NodeKind::Arm { lhs: arm_lhs, body: arm_body } if !arm_lhs.is_empty() => {
            if let NodeKind::Ident(key) = &arm_lhs[0].kind {
              if let Some(pat_node) = arm_body.last() {
                let elem = g.fresh_result();
                let next = g.fresh_cursor();
                pending.push(Pending::MatchField {
                  val: val.clone(), cursor: cur, next_cursor: next,
                  field: key, elem, loc,
                });
                cur = next;
                let elem_val = ident_val(elem, loc);
                lower_pat_lhs(g, pat_node, elem_val, pat_node.loc, pending);
              }
            }
          }
          _ => {}
        }
      }
      // Emit MatchDone only for `{}` (exact empty match). All other rec patterns
      // are structurally partial — records match even when extra fields are present.
      // Spread-terminated patterns (`..`, `..rest`, `..{}`) also omit MatchDone.
      if fields.is_empty() {
        let result = g.fresh_result();
        pending.push(Pending::MatchDone { val, cursor: cur, result, loc });
        result
      } else {
        g.fresh_result()  // partial match — no cursor exhaustion check
      }
    }

    // Bind-right: `pat |= name` — bind val to `name`, then also destructure as `pat`.
    // e.g. `[b, c] |= d` binds the element as `d` and destructures it as `[b, c]`.
    NodeKind::BindRight { lhs: pat, rhs: name_node } => {
      let bind_name = match &name_node.kind {
        NodeKind::Ident(n) => BindName::User(n),
        _ => panic!("lower_pat_lhs: BindRight rhs must be an Ident"),
      };
      pending.push(Pending::MatchBind { name: bind_name, val: val.clone(), loc });
      lower_pat_lhs(g, pat, val, loc, pending)
    }

    // StrTempl in pattern position is deferred to a future version.
    // It needs a dedicated string-matching primitive (e.g. ·match_str_prefix) not yet designed.
    NodeKind::StrTempl(_) => todo!("lower_pat_lhs: StrTempl pattern matching not yet implemented"),

    _ => todo!("lower_pat_lhs: pattern not yet implemented: {:?}", lhs.kind),
  }
}

/// Extract the BindName from the innermost ident of a guarded pattern lhs.
fn extract_bind_name<'src>(node: &'src Node<'src>) -> BindName<'src> {
  match &node.kind {
    NodeKind::Ident(name) => BindName::User(name),
    NodeKind::InfixOp { lhs, .. } => extract_bind_name(lhs),
    _ => panic!("extract_bind_name: expected ident in pattern lhs, got {:?}", node.kind),
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;
  use crate::parser::parse;
  use crate::transform::cps::ExprKind;

  fn parse_single(src: &str) -> Node<'_> {
    parse(src).expect("parse failed")
  }

  #[test]
  fn lower_lit_int() {
    let src = Box::leak("42".to_string().into_boxed_str());
    let node = parse_single(src);
    let expr = lower_expr(&node);
    assert!(matches!(expr.kind, ExprKind::Ret(_)));
    if let ExprKind::Ret(val) = &expr.kind {
      assert!(matches!(val.kind, ValKind::Lit(Lit::Int(42))));
    }
  }

  #[test]
  fn lower_ident() {
    let src = Box::leak("foo".to_string().into_boxed_str());
    let node = parse_single(src);
    let expr = lower_expr(&node);
    assert!(matches!(expr.kind, ExprKind::Ret(_)));
    if let ExprKind::Ret(val) = &expr.kind {
      assert!(matches!(val.kind, ValKind::Key(_)));
    }
  }

  #[test]
  fn lower_apply_simple() {
    let src = Box::leak("foo bar".to_string().into_boxed_str());
    let node = parse_single(src);
    let expr = lower_expr(&node);
    // foo is a Key, bar is a Key, result is App with Ret inside.
    assert!(matches!(expr.kind, ExprKind::App { .. }));
  }
}
