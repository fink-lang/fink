// CpsExpr → Node → Fink source pretty-printer



use crate::ast::{self, Node, NodeKind};
use crate::lexer::{Loc, Pos};
use super::cps::{CpsExpr, CpsFn, CpsKey, CpsNode, CpsParam, CpsVal};

pub fn fmt(cps_node: &CpsNode<'_>) -> String {
  ast::fmt::fmt(&to_node(&cps_node.expr))
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
    CpsKey::Id(s) => id_tag(s),
    CpsKey::Op(s) => op_tag(s),
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
