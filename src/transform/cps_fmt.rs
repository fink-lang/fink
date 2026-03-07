// CpsExpr → Node → Fink source pretty-printer
//
// Two formatters:
//   `fmt`           — standard output; ignores `captures`, renders Load calls verbatim
//   `fmt_annotated` — debug output after free-var analysis; renders capture destructures
//                     on closure fn env params and suppresses Load calls for captured names

use std::collections::HashSet;
use crate::ast::{self, Node, NodeKind};
use crate::lexer::{Loc, Pos};
use super::cps::{CpsExpr, CpsFn, CpsKey, CpsNode, CpsParam, CpsVal};

pub fn fmt(cps_node: &CpsNode<'_>) -> String {
  ast::fmt::fmt(&to_node(&cps_node.expr))
}

pub fn fmt_annotated(cps_node: &CpsNode<'_>) -> String {
  ast::fmt::fmt(&to_node_ann(&cps_node.expr, &HashSet::new()))
}

// ---------------------------------------------------------------------------
// dummy loc — used for reconstructed AST nodes in the formatter
// ---------------------------------------------------------------------------

fn loc() -> Loc {
  let p = Pos { idx: 0, line: 1, col: 0 };
  Loc { start: p, end: p }
}

fn node(kind: NodeKind<'static>) -> Node<'static> {
  Node::new(kind, loc())
}

// ---------------------------------------------------------------------------
// helpers to build common AST shapes
// ---------------------------------------------------------------------------

fn ident(s: &str) -> Node<'static> {
  // Safety: s is always a literal or leaked string in this module
  let s: &'static str = Box::leak(s.to_string().into_boxed_str());
  node(NodeKind::Ident(s))
}

fn apply(func: Node<'static>, args: Vec<Node<'static>>) -> Node<'static> {
  node(NodeKind::Apply { func: Box::new(func), args })
}

fn patterns(params: Vec<Node<'static>>) -> Node<'static> {
  node(NodeKind::Patterns(params))
}

fn fn_node(params: Node<'static>, body: Vec<Node<'static>>) -> Node<'static> {
  node(NodeKind::Fn { params: Box::new(params), body })
}

fn tagged(tag: &str, s: &str) -> Node<'static> {
  let str_node = node(NodeKind::StrRawTempl(vec![
    node(NodeKind::LitStr(s.to_string()))
  ]));
  apply(ident(tag), vec![str_node])
}

fn id_tag(s: &str) -> Node<'static> { tagged("id", s) }
fn op_tag(s: &str) -> Node<'static> { tagged("op", s) }
fn str_raw_node(s: &str) -> Node<'static> { tagged("str_raw", s) }

// ---------------------------------------------------------------------------
// CpsParam → Node (fn parameter)
// ---------------------------------------------------------------------------

fn param_to_node(p: &CpsParam) -> Node<'static> {
  match p {
    CpsParam::Ident(s) => ident(s),
    CpsParam::Spread(s) => node(NodeKind::Spread(Some(Box::new(ident(s))))),
    CpsParam::Wildcard => node(NodeKind::Wildcard),
  }
}

// ---------------------------------------------------------------------------
// CpsVal → Node
// ---------------------------------------------------------------------------

fn val_to_node(v: &CpsVal) -> Node<'static> {
  match v {
    CpsVal::Bool(b) => node(NodeKind::LitBool(*b)),
    CpsVal::Int(s) => { let s: &'static str = Box::leak(s.to_string().into_boxed_str()); node(NodeKind::LitInt(s)) }
    CpsVal::Float(s) => { let s: &'static str = Box::leak(s.to_string().into_boxed_str()); node(NodeKind::LitFloat(s)) }
    CpsVal::Decimal(s) => { let s: &'static str = Box::leak(s.to_string().into_boxed_str()); node(NodeKind::LitDecimal(s)) }
    CpsVal::Str(s) => node(NodeKind::LitStr(s.clone())),
    CpsVal::StrRaw(s) => str_raw_node(s),
    CpsVal::EmptySeq => node(NodeKind::LitSeq(vec![])),
    CpsVal::EmptyRec => node(NodeKind::LitRec(vec![])),
    CpsVal::Ident(s) => ident(s),
    CpsVal::Id(s) => id_tag(s),
    CpsVal::Op(s) => op_tag(s),
    CpsVal::Spread(s) => node(NodeKind::Spread(Some(Box::new(ident(s))))),
    CpsVal::Wildcard => node(NodeKind::Wildcard),
    CpsVal::Fn(f) => fn_to_node(f),
  }
}

// ---------------------------------------------------------------------------
// CpsKey → Node
// ---------------------------------------------------------------------------

fn key_to_node(k: &CpsKey) -> Node<'static> {
  match k {
    CpsKey::Id(s, _) => id_tag(s),
    CpsKey::Op(s, _) => op_tag(s),
  }
}

// ---------------------------------------------------------------------------
// CpsFn → Node (Fn AST node)
// ---------------------------------------------------------------------------

fn fn_to_node(f: &CpsFn) -> Node<'static> {
  let params = patterns(f.params.iter().map(param_to_node).collect());
  let body = vec![to_node(&f.body)];
  fn_node(params, body)
}

// ---------------------------------------------------------------------------
// CpsExpr → Node
// ---------------------------------------------------------------------------

pub fn to_node(expr: &CpsExpr) -> Node<'static> {
  match expr {
    CpsExpr::Store { env, key, val, cont } => {
      apply(ident("store"), vec![
        ident(env),
        id_tag(key),
        val_to_node(val),
        fn_to_node(cont),
      ])
    }

    CpsExpr::Load { env, key, cont } => {
      apply(ident("load"), vec![
        ident(env),
        key_to_node(key),
        fn_to_node(cont),
      ])
    }

    CpsExpr::Apply { func, args, state, cont } => {
      let mut apply_args = vec![val_to_node(func)];
      apply_args.extend(args.iter().map(val_to_node));
      apply_args.push(ident(state));
      apply_args.push(val_to_node(cont));
      apply(ident("apply"), apply_args)
    }

    CpsExpr::Closure { env, func, cont } => {
      apply(ident("closure"), vec![
        ident(env),
        fn_to_node(func),
        fn_to_node(cont),
      ])
    }

    CpsExpr::Scope { env, inner, cont } => {
      apply(ident("scope"), vec![
        ident(env),
        fn_to_node(inner),
        fn_to_node(cont),
      ])
    }

    CpsExpr::SeqAppend { seq, val, state, cont } => {
      apply(ident("seq_append"), vec![
        val_to_node(seq),
        val_to_node(val),
        ident(state),
        fn_to_node(cont),
      ])
    }

    CpsExpr::SeqConcat { seq, other, state, cont } => {
      apply(ident("seq_concat"), vec![
        val_to_node(seq),
        val_to_node(other),
        ident(state),
        fn_to_node(cont),
      ])
    }

    CpsExpr::RecPut { rec, key, val, state, cont } => {
      apply(ident("rec_put"), vec![
        val_to_node(rec),
        id_tag(key),
        val_to_node(val),
        ident(state),
        fn_to_node(cont),
      ])
    }

    CpsExpr::RecMerge { rec, other, state, cont } => {
      apply(ident("rec_merge"), vec![
        val_to_node(rec),
        val_to_node(other),
        ident(state),
        fn_to_node(cont),
      ])
    }

    CpsExpr::RangeExcl { start, end, state, cont } => {
      apply(ident("range_excl"), vec![
        val_to_node(start),
        val_to_node(end),
        ident(state),
        fn_to_node(cont),
      ])
    }

    CpsExpr::RangeIncl { start, end, state, cont } => {
      apply(ident("range_incl"), vec![
        val_to_node(start),
        val_to_node(end),
        ident(state),
        fn_to_node(cont),
      ])
    }

    CpsExpr::Err { res, state, err_cont, ok_cont } => {
      apply(ident("err"), vec![
        val_to_node(res),
        ident(state),
        fn_to_node(err_cont),
        fn_to_node(ok_cont),
      ])
    }

    CpsExpr::If { cond, then_cont, else_cont } => {
      apply(ident("if"), vec![
        val_to_node(cond),
        fn_to_node(then_cont),
        fn_to_node(else_cont),
      ])
    }

    CpsExpr::Panic { message, state } => {
      apply(ident("panic"), vec![
        val_to_node(message),
        ident(state),
      ])
    }

    CpsExpr::MatchBind { val, state, arm, fail, cont } => {
      apply(ident("match_bind"), vec![
        val_to_node(val),
        ident(state),
        fn_to_node(arm),
        fn_to_node(fail),
        fn_to_node(cont),
      ])
    }

    CpsExpr::MatchBlock { vals, state, branches, fail, cont } => {
      let mut args: Vec<Node> = vals.iter().map(val_to_node).collect();
      args.push(ident(state));
      args.extend(branches.iter().map(to_node));
      args.push(fn_to_node(fail));
      args.push(fn_to_node(cont));
      apply(ident("match_block"), args)
    }

    CpsExpr::MatchBranch { env, arm } => {
      apply(ident("match_branch"), vec![
        ident(env),
        fn_to_node(arm),
      ])
    }

    CpsExpr::SeqMatcher { val, state, cont, fail } => {
      apply(ident("seq_matcher"), vec![
        val_to_node(val),
        ident(state),
        fn_to_node(cont),
        fn_to_node(fail),
      ])
    }

    CpsExpr::RecMatcher { val, state, cont, fail } => {
      apply(ident("rec_matcher"), vec![
        val_to_node(val),
        ident(state),
        fn_to_node(cont),
        fn_to_node(fail),
      ])
    }

    CpsExpr::MatchPopAt { matcher, index, state, cont, fail } => {
      // index as integer literal — we need a string representation
      // We store index as usize; render as a static-lifetime str isn't possible,
      // but we can use LitStr for numeric display (it prints without quotes in fmt...
      // actually no). Better: allocate the string and leak it for display purposes.
      // Since this is a formatter for debug output, leaking a handful of small strings is fine.
      let index_str: &'static str = Box::leak(index.to_string().into_boxed_str());
      apply(ident("match_pop_at"), vec![
        val_to_node(matcher),
        node(NodeKind::LitInt(index_str)),
        ident(state),
        fn_to_node(cont),
        fn_to_node(fail),
      ])
    }

    CpsExpr::MatchPopField { matcher, key, state, cont, fail } => {
      apply(ident("match_pop_field"), vec![
        val_to_node(matcher),
        id_tag(key),
        ident(state),
        fn_to_node(cont),
        fn_to_node(fail),
      ])
    }

    CpsExpr::MatchDone { matcher, state, non_empty, empty } => {
      apply(ident("match_done"), vec![
        val_to_node(matcher),
        ident(state),
        fn_to_node(non_empty),
        fn_to_node(empty),
      ])
    }

    CpsExpr::MatchRest { matcher, state, cont } => {
      apply(ident("match_rest"), vec![
        val_to_node(matcher),
        ident(state),
        fn_to_node(cont),
      ])
    }

    CpsExpr::MatchLen { matcher, len, state, ok, fail } => {
      let len_str: &'static str = Box::leak(len.to_string().into_boxed_str());
      apply(ident("match_len"), vec![
        val_to_node(matcher),
        node(NodeKind::LitInt(len_str)),
        ident(state),
        fn_to_node(ok),
        fn_to_node(fail),
      ])
    }

    CpsExpr::TailCall { cont, args } => {
      if args.is_empty() {
        val_to_node(cont)
      } else {
        apply(val_to_node(cont), args.iter().map(val_to_node).collect())
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Annotated formatter — renders capture destructures, suppresses captured Loads
// ---------------------------------------------------------------------------

/// Render `CpsExpr` to a Node, using `captured` as the set of names that are
/// available via the current closure's destructured env param.
/// `Load` nodes whose key is in `captured` are suppressed (the name comes
/// from the destructure, not a runtime load call).
fn to_node_ann<'a>(expr: &CpsExpr<'a>, captured: &HashSet<&str>) -> Node<'static> {
  match expr {
    // Suppress Load when the bound name is in the current capture set.
    CpsExpr::Load { env, key, cont } => {
      // The bound name is the continuation's first param (e.g. `·foo` or `op_plus`).
      let bound_name = match cont.params.first() {
        Some(super::cps::CpsParam::Ident(s)) => *s,
        _ => "",
      };
      if !bound_name.is_empty() && captured.contains(bound_name) {
        // Skip this Load — emit the continuation body directly.
        to_node_ann(&cont.body, captured)
      } else {
        // Render as a normal load call.
        apply(ident("load"), vec![
          ident(env),
          match key { CpsKey::Id(s, _) => id_tag(s), CpsKey::Op(s, _) => op_tag(s) },
          fn_to_node_ann(cont, captured),
        ])
      }
    }

    // Closure: render func with its captures as the env-param destructure.
    CpsExpr::Closure { env, func, cont } => {
      apply(ident("closure"), vec![
        ident(env),
        fn_to_node_closure_func(func),
        fn_to_node_ann(cont, captured),
      ])
    }

    // All other variants: recurse with the same captured set (no new closure boundary).
    CpsExpr::Store { env, key, val, cont } => {
      apply(ident("store"), vec![
        ident(env),
        id_tag(key),
        val_to_node(val),
        fn_to_node_ann(cont, captured),
      ])
    }

    CpsExpr::Apply { func, args, state, cont } => {
      let mut apply_args = vec![val_to_node(func)];
      apply_args.extend(args.iter().map(val_to_node));
      apply_args.push(ident(state));
      apply_args.push(val_to_node_ann(cont, captured));
      apply(ident("apply"), apply_args)
    }

    CpsExpr::Scope { env, inner, cont } => {
      apply(ident("scope"), vec![
        ident(env),
        fn_to_node_ann(inner, captured),
        fn_to_node_ann(cont, captured),
      ])
    }

    CpsExpr::SeqAppend { seq, val, state, cont } => {
      apply(ident("seq_append"), vec![
        val_to_node(seq), val_to_node(val), ident(state),
        fn_to_node_ann(cont, captured),
      ])
    }

    CpsExpr::SeqConcat { seq, other, state, cont } => {
      apply(ident("seq_concat"), vec![
        val_to_node(seq), val_to_node(other), ident(state),
        fn_to_node_ann(cont, captured),
      ])
    }

    CpsExpr::RecPut { rec, key, val, state, cont } => {
      apply(ident("rec_put"), vec![
        val_to_node(rec), id_tag(key), val_to_node(val), ident(state),
        fn_to_node_ann(cont, captured),
      ])
    }

    CpsExpr::RecMerge { rec, other, state, cont } => {
      apply(ident("rec_merge"), vec![
        val_to_node(rec), val_to_node(other), ident(state),
        fn_to_node_ann(cont, captured),
      ])
    }

    CpsExpr::RangeExcl { start, end, state, cont } => {
      apply(ident("range_excl"), vec![
        val_to_node(start), val_to_node(end), ident(state),
        fn_to_node_ann(cont, captured),
      ])
    }

    CpsExpr::RangeIncl { start, end, state, cont } => {
      apply(ident("range_incl"), vec![
        val_to_node(start), val_to_node(end), ident(state),
        fn_to_node_ann(cont, captured),
      ])
    }

    CpsExpr::Err { res, state, err_cont, ok_cont } => {
      apply(ident("err"), vec![
        val_to_node(res), ident(state),
        fn_to_node_ann(err_cont, captured),
        fn_to_node_ann(ok_cont, captured),
      ])
    }

    CpsExpr::If { cond, then_cont, else_cont } => {
      apply(ident("if"), vec![
        val_to_node(cond),
        fn_to_node_ann(then_cont, captured),
        fn_to_node_ann(else_cont, captured),
      ])
    }

    CpsExpr::Panic { message, state } => {
      apply(ident("panic"), vec![val_to_node(message), ident(state)])
    }

    CpsExpr::MatchBind { val, state, arm, fail, cont } => {
      apply(ident("match_bind"), vec![
        val_to_node(val), ident(state),
        fn_to_node_ann(arm, captured),
        fn_to_node_ann(fail, captured),
        fn_to_node_ann(cont, captured),
      ])
    }

    CpsExpr::MatchBlock { vals, state, branches, fail, cont } => {
      let mut args: Vec<Node> = vals.iter().map(val_to_node).collect();
      args.push(ident(state));
      args.extend(branches.iter().map(|b| to_node_ann(b, captured)));
      args.push(fn_to_node_ann(fail, captured));
      args.push(fn_to_node_ann(cont, captured));
      apply(ident("match_block"), args)
    }

    CpsExpr::MatchBranch { env, arm } => {
      apply(ident("match_branch"), vec![
        ident(env),
        fn_to_node_ann(arm, captured),
      ])
    }

    CpsExpr::SeqMatcher { val, state, cont, fail } => {
      apply(ident("seq_matcher"), vec![
        val_to_node(val), ident(state),
        fn_to_node_ann(cont, captured),
        fn_to_node_ann(fail, captured),
      ])
    }

    CpsExpr::RecMatcher { val, state, cont, fail } => {
      apply(ident("rec_matcher"), vec![
        val_to_node(val), ident(state),
        fn_to_node_ann(cont, captured),
        fn_to_node_ann(fail, captured),
      ])
    }

    CpsExpr::MatchPopAt { matcher, index, state, cont, fail } => {
      let index_str: &'static str = Box::leak(index.to_string().into_boxed_str());
      apply(ident("match_pop_at"), vec![
        val_to_node(matcher),
        node(NodeKind::LitInt(index_str)),
        ident(state),
        fn_to_node_ann(cont, captured),
        fn_to_node_ann(fail, captured),
      ])
    }

    CpsExpr::MatchPopField { matcher, key, state, cont, fail } => {
      apply(ident("match_pop_field"), vec![
        val_to_node(matcher), id_tag(key), ident(state),
        fn_to_node_ann(cont, captured),
        fn_to_node_ann(fail, captured),
      ])
    }

    CpsExpr::MatchDone { matcher, state, non_empty, empty } => {
      apply(ident("match_done"), vec![
        val_to_node(matcher), ident(state),
        fn_to_node_ann(non_empty, captured),
        fn_to_node_ann(empty, captured),
      ])
    }

    CpsExpr::MatchRest { matcher, state, cont } => {
      apply(ident("match_rest"), vec![
        val_to_node(matcher), ident(state),
        fn_to_node_ann(cont, captured),
      ])
    }

    CpsExpr::MatchLen { matcher, len, state, ok, fail } => {
      let len_str: &'static str = Box::leak(len.to_string().into_boxed_str());
      apply(ident("match_len"), vec![
        val_to_node(matcher),
        node(NodeKind::LitInt(len_str)),
        ident(state),
        fn_to_node_ann(ok, captured),
        fn_to_node_ann(fail, captured),
      ])
    }

    CpsExpr::TailCall { cont, args } => {
      if args.is_empty() {
        val_to_node(cont)
      } else {
        apply(val_to_node(cont), args.iter().map(val_to_node).collect())
      }
    }
  }
}

/// Render a `CpsFn` using the parent scope's capture set (non-closure fn).
fn fn_to_node_ann<'a>(f: &CpsFn<'a>, captured: &HashSet<&str>) -> Node<'static> {
  let params = patterns(f.params.iter().map(param_to_node).collect());
  let body = vec![to_node_ann(&f.body, captured)];
  fn_node(params, body)
}

/// Render a `CpsVal`, recursing into inline `Fn` with the parent capture set.
fn val_to_node_ann<'a>(v: &CpsVal<'a>, captured: &HashSet<&str>) -> Node<'static> {
  match v {
    CpsVal::Fn(f) => fn_to_node_ann(f, captured),
    other => val_to_node(other),
  }
}

/// Render a closure's `func` CpsFn: replaces the `env` param with a
/// destructure pattern `{..env, ·x, op_plus}` when `captures` is non-empty.
/// The body is rendered with the closure's own capture set active.
fn fn_to_node_closure_func(f: &CpsFn) -> Node<'static> {
  // Build the capture set for suppressing Loads inside this closure body.
  let captured: HashSet<&str> = f.captures.iter().copied().collect();

  // Build the params list, replacing the `env` param with the destructure.
  let params: Vec<Node<'static>> = f.params.iter().map(|p| match p {
    CpsParam::Ident("env") if !f.captures.is_empty() => {
      // {..env, ·x, op_plus, ...} — rest-first record destructure
      let mut fields: Vec<Node<'static>> = Vec::new();
      // ..env spread
      fields.push(node(NodeKind::Spread(Some(Box::new(ident("env"))))));
      // each captured name
      for &name in &f.captures {
        fields.push(ident(name));
      }
      node(NodeKind::LitRec(fields))
    }
    other => param_to_node(other),
  }).collect();

  let body = vec![to_node_ann(&f.body, &captured)];
  fn_node(patterns(params), body)
}
