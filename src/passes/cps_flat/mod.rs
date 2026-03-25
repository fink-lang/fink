// cps_flat — flat pretty-printer for lifted CPS IR.
//
// Renders the CPS IR as a sequence of assignments rather than deeply nested
// ·fn/·let applications. Every LetFn and LetVal becomes a flat `name = rhs`
// binding at its enclosing scope's indent level, and the tail App is rendered
// as a bare expression. The result reads much closer to the original source.
//
// Rendering rules:
//   LetFn { name, params, fn_body, body }
//     → emit `name = fn params: <fn_body>` as an assignment
//     → then emit body's statements at the same indent level
//
//   LetVal { name, val, body }
//     → emit `name = val` as an assignment
//     → then emit body's statements at the same indent level
//
//   App { func, args } → bare expression (tail call)
//
// Multiple consecutive LetFn/LetVal that alias the same value are chained:
//   LetFn name=v_1, body: LetVal name=main, val=ref(v_1), body: ...
//   → `main = v_1 = fn ...`
//
// Names: · sigils are stripped (·v_1 → v_1, ·ƒ_3 → f_3, ·op_plus → op_plus).
//
// Output is built as an AST (Module of Bind/Apply nodes) so the existing
// pretty-printer handles indentation and line-breaking.

mod fmt;

use crate::ast::{Node, NodeKind, Exprs};
use crate::lexer::{Loc, Pos, Token, TokenKind};
use crate::passes::cps::ir::{
  Arg, Bind, BindNode, BuiltIn, Callable, Cont, CpsId, Expr, ExprKind,
  Param, Ref, Val, ValKind, Lit,
};
use crate::passes::cps::fmt::Ctx;
use crate::propgraph::PropGraph;

struct FmtCtx<'a, 'src> {
  ctx: &'a Ctx<'a, 'src>,
  bind_kinds: PropGraph<CpsId, Option<Bind>>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn fmt_flat(expr: &Expr<'_>, ctx: &Ctx<'_, '_>) -> String {
  let mut bind_kinds: PropGraph<CpsId, Option<Bind>> =
    PropGraph::with_size(ctx.origin.len(), None);
  collect_binds(expr, &mut bind_kinds);
  let fc = FmtCtx { ctx, bind_kinds };
  let stmts = collect_stmts(expr, &fc);
  let module = Node::new(NodeKind::Module(Exprs { items: stmts, seps: vec![] }), dummy_loc());
  fmt::fmt(&module)
}

/// Walk the CPS tree and record the Bind kind for every BindNode definition.
fn collect_binds(expr: &Expr<'_>, out: &mut PropGraph<CpsId, Option<Bind>>) {
  match &expr.kind {
    ExprKind::LetFn { name, params, fn_body, cont: cont } => {
      out.set(name.id, Some(name.kind));
      for p in params {
        let b = match p { Param::Name(b) | Param::Spread(b) => b };
        out.set(b.id, Some(b.kind));
      }
      collect_binds(fn_body, out);
      collect_binds_cont(cont, out);
    }
    ExprKind::LetVal { name, cont: cont, .. } => {
      out.set(name.id, Some(name.kind));
      collect_binds_cont(cont, out);
    }
    ExprKind::App { args, .. } => {
      for arg in args {
        match arg {
          Arg::Cont(c) => collect_binds_cont(c, out),
          Arg::Expr(e) => collect_binds(e, out),
          _ => {}
        }
      }
    }
    ExprKind::If { then, else_, .. } => {
      collect_binds(then, out);
      collect_binds(else_, out);
    }
    ExprKind::Yield { cont, .. } => collect_binds_cont(cont, out),
  }
}

fn collect_binds_cont(cont: &Cont<'_>, out: &mut PropGraph<CpsId, Option<Bind>>) {
  if let Cont::Expr { args, body } = cont {
    for b in args { out.set(b.id, Some(b.kind)); }
    collect_binds(body, out);
  }
}

// ---------------------------------------------------------------------------
// Loc / token helpers (mirrors cps/fmt.rs)
// ---------------------------------------------------------------------------

fn dummy_loc() -> Loc {
  let p = Pos { idx: 0, line: 1, col: 0 };
  Loc { start: p, end: p }
}

fn tok(src: &'static str, kind: TokenKind) -> Token<'static> {
  Token { kind, loc: dummy_loc(), src }
}

fn sep_tok()    -> Token<'static> { tok(",",  TokenKind::Comma) }
fn eq_tok()     -> Token<'static> { tok("=",  TokenKind::Sep) }
fn col_tok()    -> Token<'static> { tok(":",  TokenKind::Colon) }
fn spread_tok() -> Token<'static> { tok("..", TokenKind::Sep) }
fn lbrack_tok() -> Token<'static> { tok("[",  TokenKind::BracketOpen) }
fn rbrack_tok() -> Token<'static> { tok("]",  TokenKind::BracketClose) }
fn lbrace_tok() -> Token<'static> { tok("{",  TokenKind::Sep) }
fn rbrace_tok() -> Token<'static> { tok("}",  TokenKind::Sep) }

fn ident(s: &str) -> Node<'static> {
  let s: &'static str = Box::leak(s.to_string().into_boxed_str());
  Node::new(NodeKind::Ident(s), dummy_loc())
}

fn bind_node(lhs: Node<'static>, rhs: Node<'static>) -> Node<'static> {
  Node::new(NodeKind::Bind {
    op: eq_tok(),
    lhs: Box::new(lhs),
    rhs: Box::new(rhs),
  }, dummy_loc())
}

fn apply_node(func: Node<'static>, args: Vec<Node<'static>>) -> Node<'static> {
  let n = args.len();
  let seps = (0..n.saturating_sub(1)).map(|_| sep_tok()).collect();
  Node::new(NodeKind::Apply {
    func: Box::new(func),
    args: Exprs { items: args, seps },
  }, dummy_loc())
}

fn fn_node(params: Vec<Node<'static>>, body_stmts: Vec<Node<'static>>) -> Node<'static> {
  let n = params.len();
  let param_seps = (0..n.saturating_sub(1)).map(|_| sep_tok()).collect();
  Node::new(NodeKind::Fn {
    params: Box::new(Node::new(NodeKind::Patterns(Exprs { items: params, seps: param_seps }), dummy_loc())),
    sep: col_tok(),
    body: Exprs { items: body_stmts, seps: vec![] },
  }, dummy_loc())
}

// ---------------------------------------------------------------------------
// Name rendering — strips · sigils for readability
// ---------------------------------------------------------------------------

fn strip_sigil(s: &str) -> &str {
  // Strip leading · (multi-byte UTF-8 char, 2 bytes)
  s.trim_start_matches('·')
}

fn render_bind(bind: &BindNode, fc: &FmtCtx<'_, '_>) -> String {
  match bind.kind {
    Bind::Synth => format!("·v_{}", bind.id.0),
    Bind::Cont  => format!("·f_{}", bind.id.0),
    Bind::Name  => {
      match fc.ctx.origin.try_get(bind.id).and_then(|opt| *opt)
        .and_then(|ast_id| fc.ctx.ast_index.try_get(ast_id))
        .and_then(|opt| *opt)
      {
        Some(node) => match &node.kind {
          NodeKind::Ident(s) => s.to_string(),
          NodeKind::InfixOp { op, .. } => strip_sigil(op.src).to_string(),
          _ => format!("·v_{}", bind.id.0),
        },
        None => format!("·v_{}", bind.id.0),
      }
    }
  }
}

fn render_val(val: &Val<'_>, fc: &FmtCtx<'_, '_>) -> Node<'static> {
  match &val.kind {
    ValKind::Lit(lit) => lit_node(lit),
    ValKind::Ref(Ref::Name) => {
      let name = fc.ctx.origin.try_get(val.id).and_then(|opt| *opt)
        .and_then(|ast_id| fc.ctx.ast_index.try_get(ast_id))
        .and_then(|opt| *opt)
        .and_then(|node| match &node.kind {
          NodeKind::Ident(s) => Some(s.to_string()),
          NodeKind::InfixOp { op, .. } => Some(strip_sigil(op.src).to_string()),
          _ => None,
        })
        .unwrap_or_else(|| format!("·v_{}", val.id.0));
      ident(&name)
    }
    ValKind::Ref(Ref::Synth(bind_id)) => match fc.bind_kinds.try_get(*bind_id).and_then(|o| *o) {
      Some(Bind::Cont) => ident(&format!("·f_{}", bind_id.0)),
      _                => ident(&format!("·v_{}", bind_id.0)),
    },
    ValKind::Panic           => ident("panic"),
    ValKind::ContRef(id)     => ident(&format!("·f_{}", id.0)),
    ValKind::BuiltIn(op)     => ident(&render_builtin_flat(op)),
  }
}

fn lit_node(lit: &Lit<'_>) -> Node<'static> {
  match lit {
    Lit::Bool(b) => Node::new(NodeKind::LitBool(*b), dummy_loc()),
    Lit::Int(n) => {
      let s: &'static str = Box::leak(n.to_string().into_boxed_str());
      Node::new(NodeKind::LitInt(s), dummy_loc())
    }
    Lit::Float(f) => {
      let s: &'static str = Box::leak(f.to_string().into_boxed_str());
      Node::new(NodeKind::LitFloat(s), dummy_loc())
    }
    Lit::Decimal(f) => {
      let s: &'static str = Box::leak(format!("{}d", f).into_boxed_str());
      Node::new(NodeKind::LitDecimal(s), dummy_loc())
    }
    Lit::Str(s) => Node::new(NodeKind::LitStr {
      open: tok("'", TokenKind::StrStart), close: tok("'", TokenKind::StrEnd),
      content: s.to_string(), indent: 0,
    }, dummy_loc()),
    Lit::Seq => Node::new(NodeKind::LitSeq { open: lbrack_tok(), close: rbrack_tok(), items: Exprs::empty() }, dummy_loc()),
    Lit::Rec => Node::new(NodeKind::LitRec { open: lbrace_tok(), close: rbrace_tok(), items: Exprs::empty() }, dummy_loc()),
  }
}

fn render_builtin_flat(op: &BuiltIn) -> String {
  crate::passes::cps::fmt::render_builtin_name(op)
}

// ---------------------------------------------------------------------------
// Cont → argument node for App rendering
// ---------------------------------------------------------------------------

fn render_cont_arg(cont: &Cont<'_>, fc: &FmtCtx<'_, '_>) -> Node<'static> {
  match cont {
    Cont::Ref(id)           => ident(&format!("·f_{}", id.0)),
    Cont::Expr { args, body } => {
      let params: Vec<Node<'static>> = args.iter()
        .map(|b| ident(&render_bind(b, fc)))
        .collect();
      let body_stmts = collect_stmts(body, fc);
      fn_node(params, body_stmts)
    }
  }
}

// ---------------------------------------------------------------------------
// App → flat Node (a bare expression statement)
// ---------------------------------------------------------------------------

fn render_app(func: &Callable<'_>, args: &[Arg<'_>], fc: &FmtCtx<'_, '_>) -> Node<'static> {
  let func_node = match func {
    Callable::Val(v)       => render_val(v, fc),
    Callable::BuiltIn(op)  => ident(&render_builtin_flat(op)),
  };
  if args.is_empty() {
    return apply_node(func_node, vec![ident("_")]);
  }
  let arg_nodes: Vec<Node<'static>> = args.iter().map(|a| match a {
    Arg::Val(v)    => render_val(v, fc),
    Arg::Spread(v) => Node::new(NodeKind::Spread {
      op: spread_tok(),
      inner: Some(Box::new(render_val(v, fc))),
    }, dummy_loc()),
    Arg::Cont(c)   => render_cont_arg(c, fc),
    Arg::Expr(e)   => {
      let stmts = collect_stmts(e, fc);
      fn_node(vec![], stmts)
    }
  }).collect();
  apply_node(func_node, arg_nodes)
}

// ---------------------------------------------------------------------------
// Core: collect a sequence of flat statement nodes from a CPS expression
// ---------------------------------------------------------------------------

fn collect_stmts<'src>(expr: &Expr<'src>, fc: &FmtCtx<'_, '_>) -> Vec<Node<'static>> {
  let mut stmts: Vec<Node<'static>> = vec![];
  collect_into(expr, fc, &mut stmts);
  stmts
}

/// Recursively walk the LetFn/LetVal chain, emitting assignments and then the
/// tail expression. Consecutive LetFn → LetVal aliases are chained as `b = a = fn ...`.
fn collect_into<'src>(expr: &Expr<'src>, fc: &FmtCtx<'_, '_>, out: &mut Vec<Node<'static>>) {
  match &expr.kind {
    ExprKind::LetFn { name, params, fn_body, cont: cont } => {
      let name_str = render_bind(name, fc);

      let fn_params: Vec<Node<'static>> = params.iter().map(|p| match p {
        Param::Name(b)   => ident(&render_bind(b, fc)),
        Param::Spread(b) => Node::new(NodeKind::Spread {
          op: spread_tok(),
          inner: Some(Box::new(ident(&render_bind(b, fc)))),
        }, dummy_loc()),
      }).collect();

      let body_stmts = collect_stmts(fn_body, fc);
      let fn_rhs = fn_node(fn_params, body_stmts);

      let (lhs_node, tail) = chain_lhs(&name_str, cont, fc);
      let bound = outermost_name(&lhs_node).unwrap_or(&name_str).to_string();
      out.push(bind_node(lhs_node, fn_rhs));
      collect_cont_into(tail, &bound, fc, out);
    }

    ExprKind::LetVal { name, val, cont: cont } => {
      let name_str = render_bind(name, fc);
      let val_node = render_val(val, fc);
      let (lhs_node, tail) = chain_lhs(&name_str, cont, fc);
      let bound = outermost_name(&lhs_node).unwrap_or(&name_str).to_string();
      out.push(bind_node(lhs_node, val_node));
      collect_cont_into(tail, &bound, fc, out);
    }

    ExprKind::App { func, args } => {
      // ·fn_closure is a constructor binding: render as `[a, b] = ·fn_closure caps...`
      // (single param: `a = ...`) and recurse into the cont body flat.
      if matches!(func, Callable::BuiltIn(BuiltIn::FnClosure)) {
        if let Some((value_args, Cont::Expr { args: cont_params, body })) = split_trailing_cont(args) {
          let func_node = ident(&render_builtin_flat(&BuiltIn::FnClosure));
          let arg_nodes: Vec<Node<'static>> = value_args.iter().map(|a| match a {
            Arg::Val(v)    => render_val(v, fc),
            Arg::Spread(v) => Node::new(NodeKind::Spread {
              op: spread_tok(),
              inner: Some(Box::new(render_val(v, fc))),
            }, dummy_loc()),
            Arg::Cont(c)   => render_cont_arg(c, fc),
            Arg::Expr(e)   => fn_node(vec![], collect_stmts(e.as_ref(), fc)),
          }).collect();
          let call_node = apply_node(func_node, arg_nodes);
          let lhs = if cont_params.len() == 1 {
            ident(&render_bind(&cont_params[0], fc))
          } else {
            let names: Vec<Node<'static>> = cont_params.iter()
              .map(|b| ident(&render_bind(b, fc)))
              .collect();
            let n = names.len();
            Node::new(NodeKind::LitSeq {
              open: lbrack_tok(), close: rbrack_tok(),
              items: Exprs { items: names, seps: (0..n.saturating_sub(1)).map(|_| sep_tok()).collect() },
            }, dummy_loc())
          };
          out.push(bind_node(lhs, call_node));
          collect_into(body.as_ref(), fc, out);
          return;
        }
      }
      out.push(render_app(func, args, fc));
    }

    ExprKind::If { cond, then, else_ } => {
      let cond_node  = render_val(cond, fc);
      let then_stmts = collect_stmts(then, fc);
      let else_stmts = collect_stmts(else_, fc);
      out.push(apply_node(ident("if"), vec![
        cond_node,
        fn_node(vec![], then_stmts),
        fn_node(vec![], else_stmts),
      ]));
    }

    ExprKind::Yield { value, cont } => {
      let val_node  = render_val(value, fc);
      let cont_node = render_cont_arg(cont, fc);
      out.push(apply_node(ident("·yield"), vec![val_node, cont_node]));
    }
  }
}

/// Emit statements for a `Cont` body — either recurse into the inner Expr,
/// or for `Cont::Ref` emit a tail call passing the last-bound name.
fn collect_cont_into<'src>(cont: &'src Cont<'src>, bound: &str, fc: &FmtCtx<'_, '_>, out: &mut Vec<Node<'static>>) {
  match cont {
    Cont::Expr { body, .. } => collect_into(body, fc, out),
    Cont::Ref(id) => {
      out.push(apply_node(ident(&format!("·f_{}", id.0)), vec![ident(bound)]));
    }
  }
}

/// Extract the leftmost (outermost) name from a possibly-chained lhs node.
/// `alias = name` → "alias"; plain `name` → "name".
fn outermost_name<'a>(node: &'a Node<'static>) -> Option<&'a str> {
  match &node.kind {
    NodeKind::Ident(s) => Some(s),
    NodeKind::Bind { lhs, .. } => outermost_name(lhs),
    _ => None,
  }
}

// ---------------------------------------------------------------------------
// LHS chain builder
//
// If the body is a Cont::Expr whose inner Expr is a LetVal that immediately
// aliases `name` (e.g. the module-level `main = v_1 = fn ...` pattern),
// return a chained `alias = name` lhs node and the aliased binding's body.
// Otherwise return a plain ident lhs and the original body cont.
// ---------------------------------------------------------------------------

fn chain_lhs<'src>(
  name: &str,
  body: &'src Cont<'src>,
  fc: &FmtCtx<'_, '_>,
) -> (Node<'static>, &'src Cont<'src>) {
  let body_expr = match body {
    Cont::Expr { args, body } if args.len() == 1 => body.as_ref(),
    _ => return (ident(name), body),
  };

  if let ExprKind::LetVal { name: alias, val, cont: inner_body } = &body_expr.kind {
    let rendered_val = render_val(val, fc);
    let val_matches = match &rendered_val.kind {
      NodeKind::Ident(s) => *s == name,
      _ => false,
    };

    if val_matches {
      let alias_str = render_bind(alias, fc);
      let chained = bind_node(ident(&alias_str), ident(name));
      return (chained, inner_body);
    }
  }

  (ident(name), body)
}

/// Split args into `(value_args, trailing_cont)` if the last arg is `Arg::Cont`.
/// Returns borrowed slices from the original `args` vec.
fn split_trailing_cont<'a, 'src>(
  args: &'a [Arg<'src>],
) -> Option<(&'a [Arg<'src>], &'a Cont<'src>)> {
  match args.last() {
    Some(Arg::Cont(c)) => Some((&args[..args.len() - 1], c)),
    _ => None,
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use test_macros::include_fink_tests;

  use crate::ast::build_index;
  use crate::parser::parse;
  use crate::passes::cps::fmt::Ctx;
  use crate::passes::cps::transform::lower_expr;
  use crate::passes::closure_lifting::lift_all;
  use super::fmt_flat;

  fn cps_flat(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let cps = lower_expr(&r.root);
        let (lifted, _) = lift_all(cps, &ast_index);
        let ctx = Ctx { origin: &lifted.origin, ast_index: &ast_index, captures: None };
        fmt_flat(&lifted.root, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  include_fink_tests!("src/passes/cps_flat/test_cps_flat.fnk");

}
