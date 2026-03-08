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

#![allow(dead_code, unused_imports)]

use crate::ast::{CmpPart, Node, NodeKind};
use crate::lexer::Loc;
use crate::transform::cps::{
  Arg, Arm, Expr, ExprKind, Key, KeyKind, Lit, Meta, Name, Param, Pat, PatKind, RecField,
  SeqElem, Spread, StrPat, Val, ValKind,
};

// ---------------------------------------------------------------------------
// Name generator
// ---------------------------------------------------------------------------

pub struct Gen {
  counter: usize,
  /// Leaked strings for synthetic names that outlive the source lifetime.
  arena: Vec<String>,
}

impl Gen {
  pub fn new() -> Self {
    Gen { counter: 0, arena: Vec::new() }
  }

  fn alloc(&mut self, s: String) -> &'static str {
    self.arena.push(s);
    Box::leak(self.arena.last().unwrap().clone().into_boxed_str())
  }

  pub fn fresh(&mut self, prefix: &str) -> &'static str {
    let n = self.counter;
    self.counter += 1;
    self.alloc(format!("{}{}", prefix, n))
  }

  pub fn fresh_fn(&mut self) -> &'static str {
    self.fresh("fn_")
  }

  pub fn fresh_result(&mut self) -> &'static str {
    self.fresh("v_")
  }
}

// ---------------------------------------------------------------------------
// Deferred bindings — accumulated bottom-up (full definition below)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ident_val(name: Name<'_>, loc: Loc) -> Val<'_> {
  Val { kind: ValKind::Ident(name), meta: Meta::at(loc) }
}

fn key_val_name<'src>(name: Name<'src>, loc: Loc) -> Val<'src> {
  Val {
    kind: ValKind::Key(Key { kind: KeyKind::Name(name), resolution: None, meta: Meta::at(loc) }),
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

fn ret_expr(val: Val<'_>, loc: Loc) -> Expr<'_> {
  Expr { kind: ExprKind::Ret(Box::new(val)), meta: Meta::at(loc) }
}

/// Emit an App node: func(args...) → result; body.
fn app_node<'src>(
  func: Val<'src>,
  args: Vec<Arg<'src>>,
  result: Name<'src>,
  body: Expr<'src>,
  loc: Loc,
) -> Expr<'src> {
  Expr {
    kind: ExprKind::App { func: Box::new(func), args, result, body: Box::new(body) },
    meta: Meta::at(loc),
  }
}

/// Wrap a plain `Val` as an `Arg::Val`.
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

    // ---- range: `0..10`, `0...10` ----
    NodeKind::Range { op, start, end } => lower_range(g, op, start, end, loc),

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
          let (val, pending) = lower_bind_stmt(g, lhs, rhs, stmt.loc);
          all_pending.extend(pending);
          all_pending.push(val);
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
) -> (Pending<'src>, Vec<Pending<'src>>) {
  let (val, mut pending) = lower(g, rhs);
  let binding = match &lhs.kind {
    NodeKind::Ident(name) => Pending::Val { name, val, loc },
    NodeKind::Wildcard => {
      let discard = g.fresh_result();
      Pending::Val { name: discard, val, loc }
    }
    _ => {
      // Load RHS into a temp so LetPat val is an Ident, avoiding double-loading.
      let tmp = g.fresh_result();
      pending.push(Pending::Val { name: tmp, val, loc });
      let pat = lower_pat(lhs);
      Pending::Pat { pat, val: ident_val(tmp, loc), loc }
    }
  };
  (binding, pending)
}

/// Lower a bind expression (the result IS the bound value — last in block or standalone).
fn lower_bind<'src>(
  g: &mut Gen,
  lhs: &'src Node<'src>,
  rhs: &'src Node<'src>,
  loc: Loc,
) -> Lower<'src> {
  let (val, mut pending) = lower(g, rhs);
  let name = match &lhs.kind {
    NodeKind::Ident(name) => *name,
    NodeKind::Wildcard => {
      let discard = g.fresh_result();
      pending.push(Pending::Val { name: discard, val, loc });
      return (ident_val(discard, loc), pending);
    }
    _ => {
      // Pattern bind — load RHS into a temp, then emit LetPat against the temp.
      // Using a temp (Ident) avoids double-loading the Key in the continuation.
      let tmp = g.fresh_result();
      pending.push(Pending::Val { name: tmp, val, loc });
      let pat = lower_pat(lhs);
      pending.push(Pending::Pat { pat, val: ident_val(tmp, loc), loc });
      return (ident_val(tmp, loc), pending);
    }
  };
  pending.push(Pending::Val { name, val, loc });
  (ident_val(name, loc), pending)
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
  let fn_body = prepend_let_pats(deferred, lower_stmts(g, body), loc);
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
  let fn_body = prepend_let_pats(deferred, lower_stmts(g, body), loc);
  let result = g.fresh_result();
  let pending = vec![
    Pending::Fn { name: fn_name, params: param_names, fn_body, loc },
    Pending::App { func: ident_val(fn_name, loc), args: args_val(vec![]), result, loc },
  ];
  (ident_val(result, loc), pending)
}

/// Extract params from a fn params node, returning:
/// - the param list (with complex patterns replaced by fresh spread names)
/// - a list of (Pat, spread_name) pairs to prepend as LetPat in the fn body
fn extract_params_with_gen<'src>(
  g: &mut Gen,
  params: &'src Node<'src>,
) -> (Vec<Param<'src>>, Vec<(Pat<'src>, Name<'src>)>) {
  let mut param_list = vec![];
  let mut deferred = vec![];
  let nodes = match &params.kind {
    NodeKind::Patterns(ps) => ps.as_slice(),
    _ => std::slice::from_ref(params),
  };
  for p in nodes {
    match &p.kind {
      NodeKind::Ident(name) => param_list.push(Param::Name(name)),
      NodeKind::Wildcard => param_list.push(Param::Name("_")),
      NodeKind::Patterns(ps) => {
        for inner in ps {
          param_list.push(Param::Name(match &inner.kind {
            NodeKind::Ident(name) => name,
            _ => "_",
          }));
        }
      }
      NodeKind::Spread(inner) => {
        let name = match inner.as_deref() {
          Some(Node { kind: NodeKind::Ident(name), .. }) => name,
          _ => "_",
        };
        param_list.push(Param::Spread(name));
      }
      // Complex destructuring param — desugar to fresh spread param + LetPat in body.
      _ => {
        let spread_name = g.fresh("v_");
        param_list.push(Param::Spread(spread_name));
        deferred.push((lower_pat(p), spread_name));
      }
    }
  }
  (param_list, deferred)
}

/// Wrap `body` in `LetPat` nodes for each (pat, spread_name) pair, innermost first.
fn prepend_let_pats<'src>(
  deferred: Vec<(Pat<'src>, Name<'src>)>,
  body: Expr<'src>,
  loc: Loc,
) -> Expr<'src> {
  deferred.into_iter().rev().fold(body, |inner, (pat, spread_name)| Expr {
    kind: ExprKind::LetPat {
      pat: Box::new(pat),
      val: Box::new(key_val_name(spread_name, loc)),
      body: Box::new(inner),
    },
    meta: Meta::at(loc),
  })
}

fn extract_params<'src>(params: &'src Node<'src>) -> Vec<Param<'src>> {
  match &params.kind {
    NodeKind::Patterns(ps) => ps.iter().flat_map(|p| extract_param(p)).collect(),
    _ => extract_param(params),
  }
}

fn extract_param<'src>(param: &'src Node<'src>) -> Vec<Param<'src>> {
  match &param.kind {
    NodeKind::Ident(name) => vec![Param::Name(name)],
    NodeKind::Wildcard => vec![Param::Name("_")],
    NodeKind::Patterns(ps) => ps.iter().flat_map(|p| extract_param(p)).collect(),
    // `..rest` varargs param — trailing spread.
    NodeKind::Spread(inner) => {
      let name = match inner.as_deref() {
        Some(Node { kind: NodeKind::Ident(name), .. }) => name,
        _ => "_",
      };
      vec![Param::Spread(name)]
    }
    // Complex destructuring params — extract all bound names from pattern.
    _ => collect_pat_bindings(&lower_pat(param))
        .into_iter()
        .map(Param::Name)
        .collect(),
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
  let range_fn = if *op == *".." { "range_excl" } else { "range_incl" };
  let range_key = key_val_name(range_fn, loc);
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
    let op_name = if is_spread { "seq_concat" } else { "seq_append" };
    let op_fn = key_val_name(op_name, loc);
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
        let op_fn = key_val_name("rec_merge", loc);
        let result = g.fresh_result();
        pending.push(Pending::App { func: op_fn, args: args_val(vec![acc, sv]), result, loc });
        acc = ident_val(result, loc);
      }
      NodeKind::Bind { lhs, rhs } => {
        if let NodeKind::Ident(key) = &lhs.kind {
          let key_lit = lit_val(Lit::Str(key), field.loc);
          let (fv, fp) = lower(g, rhs);
          pending.extend(fp);
          let op_fn = key_val_name("rec_put", loc);
          let result = g.fresh_result();
          pending.push(Pending::App { func: op_fn, args: args_val(vec![acc, key_lit, fv]), result, loc });
          acc = ident_val(result, loc);
        } else {
          // Computed key.
          let (kv, kp) = lower(g, lhs);
          let (fv, fp) = lower(g, rhs);
          pending.extend(kp);
          pending.extend(fp);
          let op_fn = key_val_name("rec_put", loc);
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
          let op_fn = key_val_name("rec_put", loc);
          let result = g.fresh_result();
          pending.push(Pending::App { func: op_fn, args: args_val(vec![acc, key_lit, fv]), result, loc });
          acc = ident_val(result, loc);
        } else {
          let (kv, kp) = lower(g, key_node);
          let (fv, fp) = lower(g, val_node);
          pending.extend(kp);
          pending.extend(fp);
          let op_fn = key_val_name("rec_put", loc);
          let result = g.fresh_result();
          pending.push(Pending::App { func: op_fn, args: args_val(vec![acc, kv, fv]), result, loc });
          acc = ident_val(result, loc);
        }
      }
      NodeKind::Ident(name) => {
        // Shorthand `{foo}` == `{foo: foo}`
        let key_lit = lit_val(Lit::Str(name), field.loc);
        let id_val = key_val_name(name, field.loc);
        let op_fn = key_val_name("rec_put", loc);
        let result = g.fresh_result();
        pending.push(Pending::App { func: op_fn, args: args_val(vec![acc, key_lit, id_val]), result, loc });
        acc = ident_val(result, loc);
      }
      _ => {
        let (fv, fp) = lower(g, field);
        pending.extend(fp);
        let op_fn = key_val_name("rec_merge", loc);
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
  let str_fmt_fn = key_val_name("str_fmt", loc);
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
  // `subjects` may be a Patterns node wrapping a single subject.
  let subject_node = match &subjects.kind {
    NodeKind::Patterns(ps) if ps.len() == 1 => &ps[0],
    _ => subjects,
  };
  let (scrutinee, pending) = lower(g, subject_node);
  let cps_arms = arms.iter().map(|arm| lower_arm(g, arm)).collect();
  let result = g.fresh_result();
  // Match is a terminal-style node that needs a body; use App-style Pending.
  let mut pending = pending;
  pending.push(Pending::Match { scrutinee, arms: cps_arms, result, loc });
  (ident_val(result, loc), pending)
}

fn lower_arm<'src>(g: &mut Gen, arm: &'src Node<'src>) -> Arm<'src> {
  match &arm.kind {
    NodeKind::Arm { lhs, body } => {
      let loc = arm.loc;
      let pat = if lhs.is_empty() {
        Pat { kind: PatKind::Wildcard, meta: Meta::at(loc) }
      } else {
        lower_pat(&lhs[0])
      };
      let bindings = collect_pat_bindings(&pat);
      let fn_body = lower_stmts(g, body);
      Arm { pattern: pat, bindings, fn_body: Box::new(fn_body), meta: Meta::at(loc) }
    }
    _ => panic!("expected Arm node"),
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
  Val { name: Name<'src>, val: Val<'src>, loc: Loc },
  Pat { pat: Pat<'src>, val: Val<'src>, loc: Loc },
  Fn { name: Name<'src>, params: Vec<Param<'src>>, fn_body: Expr<'src>, loc: Loc },
  App { func: Val<'src>, args: Vec<Arg<'src>>, result: Name<'src>, loc: Loc },
  Match { scrutinee: Val<'src>, arms: Vec<Arm<'src>>, result: Name<'src>, loc: Loc },
}

fn wrap<'src>(bindings: Vec<Pending<'src>>, tail: Expr<'src>) -> Expr<'src> {
  bindings.into_iter().rev().fold(tail, |body, pending| match pending {
    Pending::Val { name, val, loc } => Expr {
      kind: ExprKind::LetVal { name, val: Box::new(val), body: Box::new(body) },
      meta: Meta::at(loc),
    },
    Pending::Pat { pat, val, loc } => Expr {
      kind: ExprKind::LetPat { pat: Box::new(pat), val: Box::new(val), body: Box::new(body) },
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
    Pending::Match { scrutinee, arms, result, loc } => Expr {
      kind: ExprKind::Match {
        scrutinee: Box::new(scrutinee),
        arms,
        result,
        body: Box::new(body),
      },
      meta: Meta::at(loc),
    },
  })
}

// ---------------------------------------------------------------------------
// Pattern lowering: AST Node → cps::Pat
// ---------------------------------------------------------------------------

fn lower_pat<'src>(node: &'src Node<'src>) -> Pat<'src> {
  let loc = node.loc;
  let kind = match &node.kind {
    NodeKind::Wildcard => PatKind::Wildcard,
    NodeKind::Ident(name) => PatKind::Bind(name),
    NodeKind::LitBool(b)  => PatKind::Lit(Lit::Bool(*b)),
    NodeKind::LitInt(s)   => PatKind::Lit(Lit::Int(parse_int(s))),
    NodeKind::LitFloat(s) => PatKind::Lit(Lit::Float(parse_float(s))),
    NodeKind::LitDecimal(s) => PatKind::Lit(Lit::Decimal(parse_decimal(s))),
    NodeKind::LitStr(s)   => PatKind::Lit(Lit::Str(s)),

    NodeKind::LitSeq(elems) => {
      let mut pat_elems = vec![];
      for elem in elems {
        match &elem.kind {
          NodeKind::Spread(inner) => {
            pat_elems.push(SeqElem::Spread(lower_spread(inner.as_deref(), elem.loc)));
          }
          _ => pat_elems.push(SeqElem::Pat(lower_pat(elem))),
        }
      }
      PatKind::Seq { elems: pat_elems, spread: None }
    }

    NodeKind::LitRec(fields) => {
      let mut rec_fields = vec![];
      let mut spread = None;
      for field in fields {
        match &field.kind {
          NodeKind::Spread(inner) => {
            spread = Some(Box::new(lower_spread(inner.as_deref(), field.loc)));
          }
          NodeKind::Bind { lhs, rhs } => {
            if let NodeKind::Ident(key) = &lhs.kind {
              rec_fields.push(RecField {
                key,
                pattern: lower_pat(rhs),
                meta: Meta::at(field.loc),
              });
            }
          }
          NodeKind::Ident(name) => {
            rec_fields.push(RecField {
              key: name,
              pattern: Pat { kind: PatKind::Bind(name), meta: Meta::at(field.loc) },
              meta: Meta::at(field.loc),
            });
          }
          _ => {}
        }
      }
      PatKind::Rec { fields: rec_fields, spread }
    }

    NodeKind::Range { op, start, end } => {
      PatKind::Range { op, start: Box::new(lower_pat(start)), end: Box::new(lower_pat(end)) }
    }

    NodeKind::StrTempl(parts) => {
      let str_pats = parts.iter().map(|p| match &p.kind {
        NodeKind::LitStr(s)   => StrPat::Lit(s),
        NodeKind::Spread(inner) => StrPat::Spread(lower_spread(inner.as_deref(), p.loc)),
        _ => StrPat::Lit(""),
      }).collect();
      PatKind::Str(str_pats)
    }

    // Guard: `pat | guard` — encoded as InfixOp "|"
    NodeKind::InfixOp { op: _, lhs, rhs: _ } => {
      // Treat lhs as the pattern. Guard is a value expression — drop for now
      // (guard lowering needs a value, which requires running the expression lowerer;
      // deferred to semantic pass where guard context is available).
      lower_pat(lhs).kind
    }

    NodeKind::Group(inner) => return lower_pat(inner),

    _ => PatKind::Wildcard,
  };
  Pat { kind, meta: Meta::at(loc) }
}

fn lower_spread<'src>(inner: Option<&'src Node<'src>>, loc: Loc) -> Spread<'src> {
  match inner {
    None => Spread { guard: None, bind: None, name: None, meta: Meta::at(loc) },
    Some(node) => match &node.kind {
      NodeKind::Ident(name) => {
        Spread { guard: None, bind: None, name: Some(name), meta: Meta::at(node.loc) }
      }
      _ => Spread { guard: None, bind: None, name: None, meta: Meta::at(loc) },
    }
  }
}

fn collect_pat_bindings<'src>(pat: &Pat<'src>) -> Vec<Name<'src>> {
  let mut names = vec![];
  collect_into(pat, &mut names);
  names
}

fn collect_into<'src>(pat: &Pat<'src>, names: &mut Vec<Name<'src>>) {
  match &pat.kind {
    PatKind::Wildcard | PatKind::Lit(_) => {}
    PatKind::Bind(name) => names.push(name),
    PatKind::Seq { elems, spread } => {
      for elem in elems {
        match elem {
          SeqElem::Pat(p) => collect_into(p, names),
          SeqElem::Spread(s) => {
            if let Some(n) = s.name { names.push(n); }
            if let Some(n) = s.bind { names.push(n); }
          }
        }
      }
      if let Some(s) = spread {
        if let Some(n) = s.name { names.push(n); }
        if let Some(n) = s.bind { names.push(n); }
      }
    }
    PatKind::Rec { fields, spread } => {
      for f in fields { collect_into(&f.pattern, names); }
      if let Some(s) = spread {
        if let Some(n) = s.name { names.push(n); }
        if let Some(n) = s.bind { names.push(n); }
      }
    }
    PatKind::Str(parts) => {
      for p in parts {
        if let StrPat::Spread(s) = p {
          if let Some(n) = s.name { names.push(n); }
          if let Some(n) = s.bind { names.push(n); }
        }
      }
    }
    PatKind::Range { start, end, .. } => {
      collect_into(start, names);
      collect_into(end, names);
    }
    PatKind::Guard { pat, .. } => collect_into(pat, names),
  }
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
