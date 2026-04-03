// Flat pretty-printer for lifted CPS IR.
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

use crate::ast::{Node, NodeKind, Exprs};
use crate::lexer::{Loc, Pos, Token, TokenKind};
use crate::passes::cps::ir::{
  Arg, Bind, BindNode, BuiltIn, Callable, Cont, ContKind, CpsId, Expr, ExprKind,
  Param, ParamInfo, Ref, Val, ValKind, Lit,
};
use crate::passes::cps::fmt::Ctx;

struct FmtCtx<'a, 'src> {
  ctx: &'a Ctx<'a, 'src>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn fmt_flat(expr: &Expr, ctx: &Ctx<'_, '_>) -> String {
  let fc = FmtCtx { ctx };
  let stmts = collect_stmts(expr, &fc);
  let module = Node::new(NodeKind::Module(Exprs { items: stmts, seps: vec![] }), dummy_loc());
  fmt_ast(&module)
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
// Name rendering
// ---------------------------------------------------------------------------

/// Render a capture param: use the origin CpsId to recover the source name
/// (e.g. `a`), but pair it with the param's own CpsId for the suffix so body
/// refs match. Falls back to `·v_{param_id}` for synthetic origins.
fn render_cap_name(param_id: CpsId, origin_id: CpsId, fc: &FmtCtx<'_, '_>) -> String {
  // First check if the param itself is a cont — use semantic name.
  if let Some(bk) = fc.ctx.bind_kinds
    && let Some(Some(Bind::Cont(ck))) = bk.try_get(param_id) {
      return match ck {
        ContKind::Ret  => format!("·ƒret_{}", param_id.0),
        ContKind::Succ => format!("·ƒsucc_{}", param_id.0),
        ContKind::Fail => format!("·ƒfail_{}", param_id.0),
      };
  }
  // Otherwise try source name from origin AST node.
  match fc.ctx.origin.try_get(origin_id).and_then(|opt| *opt)
    .and_then(|ast_id| fc.ctx.ast_index.try_get(ast_id))
    .and_then(|opt| *opt)
  {
    Some(node) => match &node.kind {
      NodeKind::Ident(s) => format!("·{}_{}", s, param_id.0),
      _ => format!("·v_{}", param_id.0),
    },
    None => format!("·v_{}", param_id.0),
  }
}

fn render_synth_name(cps_id: CpsId, fc: &FmtCtx<'_, '_>) -> String {
  match fc.ctx.origin.try_get(cps_id).and_then(|opt| *opt)
    .and_then(|ast_id| fc.ctx.ast_index.try_get(ast_id))
    .and_then(|opt| *opt)
  {
    Some(node) => match &node.kind {
      NodeKind::Ident(s) => format!("·{}_{}", s, cps_id.0),
      _ => render_synth_fallback(cps_id, fc),
    },
    None => render_synth_fallback(cps_id, fc),
  }
}

/// Render a compiler-generated node with no AST origin.
/// Checks bind_kinds for cont semantic names, falls back to ·v_N.
fn render_synth_fallback(cps_id: CpsId, fc: &FmtCtx<'_, '_>) -> String {
  if let Some(bk) = fc.ctx.bind_kinds
    && let Some(Some(kind)) = bk.try_get(cps_id) {
      return match kind {
        Bind::Cont(ContKind::Ret)  => format!("·ƒret_{}", cps_id.0),
        Bind::Cont(ContKind::Succ) => format!("·ƒsucc_{}", cps_id.0),
        Bind::Cont(ContKind::Fail) => format!("·ƒfail_{}", cps_id.0),
        _ => format!("·v_{}", cps_id.0),
      };
  }
  format!("·v_{}", cps_id.0)
}

fn render_unresolved_name(cps_id: CpsId, fc: &FmtCtx<'_, '_>) -> String {
  match fc.ctx.origin.try_get(cps_id).and_then(|opt| *opt)
    .and_then(|ast_id| fc.ctx.ast_index.try_get(ast_id))
    .and_then(|opt| *opt)
  {
    Some(node) => match &node.kind {
      NodeKind::Ident(s) => format!("·∅{}", s),
      // SynthIdent should always be resolved — if we get here, origin tracking is broken
      NodeKind::SynthIdent(n) => format!("·⚠$_{}", n),
      _ => format!("·⚠_{}", cps_id.0),
    },
    None => format!("·⚠_{}", cps_id.0),
  }
}

fn render_bind(bind: &BindNode, fc: &FmtCtx<'_, '_>) -> String {
  match bind.kind {
    Bind::SynthName => render_synth_name(bind.id, fc),
    Bind::Synth => format!("·v_{}", bind.id.0),
    Bind::Cont(ContKind::Ret)  => format!("·ƒret_{}", bind.id.0),
    Bind::Cont(ContKind::Succ) => format!("·ƒsucc_{}", bind.id.0),
    Bind::Cont(ContKind::Fail) => format!("·ƒfail_{}", bind.id.0),
  }
}

/// Render a single param as an AST node, with spread support.
fn render_param_node(p: &Param, fc: &FmtCtx<'_, '_>, use_info: bool) -> Node<'static> {
  match p {
    Param::Name(b) => {
      let name = if use_info { render_param_with_info(b, fc) } else { render_bind(b, fc) };
      ident(&name)
    }
    Param::Spread(b) => {
      let name = if use_info { render_param_with_info(b, fc) } else { render_bind(b, fc) };
      Node::new(NodeKind::Spread {
        op: spread_tok(),
        inner: Some(Box::new(ident(&name))),
      }, dummy_loc())
    }
  }
}

/// Render function params with grouping when `param_info` is available:
///   `{cap0, cap1}, [param0, param1], cont`
/// Falls back to flat rendering when `param_info` is not set.
fn render_fn_params_grouped(params: &[Param], fc: &FmtCtx<'_, '_>) -> Vec<Node<'static>> {
  let pi = match fc.ctx.param_info {
    Some(pi) if !pi.is_empty() => pi,
    _ => {
      // No param_info — flat rendering.
      return params.iter().map(|p| render_param_node(p, fc, false)).collect();
    }
  };

  let mut caps: Vec<Node<'static>> = Vec::new();
  let mut user_params: Vec<Node<'static>> = Vec::new();

  for p in params {
    let b = match p { Param::Name(b) | Param::Spread(b) => b };
    let info = pi.try_get(b.id).and_then(|o| *o);
    match info {
      Some(ParamInfo::Cap(_)) => caps.push(render_param_node(p, fc, true)),
      // Conts-first: cont params go in the user params group (inside []).
      Some(ParamInfo::Cont) | Some(ParamInfo::Param(_)) | None => user_params.push(render_param_node(p, fc, true)),
    }
  }

  let mut result: Vec<Node<'static>> = Vec::new();

  // Captures: {cap0, cap1}
  if !caps.is_empty() {
    let n = caps.len();
    result.push(Node::new(NodeKind::LitRec {
      open: lbrace_tok(), close: rbrace_tok(),
      items: Exprs { items: caps, seps: (0..n.saturating_sub(1)).map(|_| sep_tok()).collect() },
    }, dummy_loc()));
  }

  // User params: [param0, param1]
  if !user_params.is_empty() {
    let n = user_params.len();
    result.push(Node::new(NodeKind::LitSeq {
      open: lbrack_tok(), close: rbrack_tok(),
      items: Exprs { items: user_params, seps: (0..n.saturating_sub(1)).map(|_| sep_tok()).collect() },
    }, dummy_loc()));
  }

  result
}

/// Render a param node using `param_info` when available.
/// Captures are rendered using the original binding's source name (via origin CpsId).
fn render_param_with_info(bind: &BindNode, fc: &FmtCtx<'_, '_>) -> String {
  if let Some(pi) = fc.ctx.param_info
    && let Some(Some(info)) = pi.try_get(bind.id)
  {
    return match info {
      ParamInfo::Cap(origin) => {
        // Try to recover the source name from the origin's AST node,
        // but use the param's own CpsId for the numeric suffix so it
        // matches body refs. Falls back to ·v_{id} for synthetic origins.
        render_cap_name(bind.id, *origin, fc)
      }
      ParamInfo::Param(_) => render_bind(bind, fc),
      ParamInfo::Cont => render_bind(bind, fc),
    };
  }
  render_bind(bind, fc)
}

fn render_val(val: &Val, fc: &FmtCtx<'_, '_>) -> Node<'static> {
  match &val.kind {
    ValKind::Lit(lit) => lit_node(lit),
    ValKind::Ref(Ref::Synth(bind_id))      => ident(&render_synth_name(*bind_id, fc)),
    ValKind::Ref(Ref::Unresolved(_)) => ident(&render_unresolved_name(val.id, fc)),
    ValKind::Panic           => ident("panic"),
    ValKind::ContRef(id)     => ident(&render_synth_fallback(*id, fc)),
    ValKind::BuiltIn(op)     => ident(&render_builtin_flat(op)),
  }
}

fn lit_node(lit: &Lit) -> Node<'static> {
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
      content: crate::strings::control_pics(s), indent: 0,
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

fn render_cont_arg(cont: &Cont, fc: &FmtCtx<'_, '_>) -> Node<'static> {
  match cont {
    Cont::Ref(id)           => ident(&render_synth_fallback(*id, fc)),
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

fn render_app(func: &Callable, args: &[Arg], fc: &FmtCtx<'_, '_>) -> Node<'static> {
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

fn collect_stmts(expr: &Expr, fc: &FmtCtx<'_, '_>) -> Vec<Node<'static>> {
  let mut stmts: Vec<Node<'static>> = vec![];
  collect_into(expr, fc, &mut stmts);
  stmts
}

/// Recursively walk the LetFn/LetVal chain, emitting assignments and then the
/// tail expression. Consecutive LetFn → LetVal aliases are chained as `b = a = fn ...`.
fn collect_into(expr: &Expr, fc: &FmtCtx<'_, '_>, out: &mut Vec<Node<'static>>) {
  match &expr.kind {
    ExprKind::LetFn { name, params, fn_body, cont, .. } => {
      let name_str = render_bind(name, fc);

      let fn_params = render_fn_params_grouped(params, fc);

      let body_stmts = collect_stmts(fn_body, fc);
      let fn_rhs = fn_node(fn_params, body_stmts);

      let (lhs_node, tail) = chain_lhs(&name_str, cont, fc);
      let bound = outermost_name(&lhs_node).unwrap_or(&name_str).to_string();
      out.push(bind_node(lhs_node, fn_rhs));
      collect_cont_into(tail, &bound, fc, out);
    }

    ExprKind::LetVal { name, val, cont } => {
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
      if matches!(func, Callable::BuiltIn(BuiltIn::FnClosure))
        && let Some((value_args, Cont::Expr { args: cont_params, body })) = split_trailing_cont(args) {
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
  }
}

/// Emit statements for a `Cont` body — either recurse into the inner Expr,
/// or for `Cont::Ref` emit a tail call passing the last-bound name.
fn collect_cont_into(cont: &Cont, bound: &str, fc: &FmtCtx<'_, '_>, out: &mut Vec<Node<'static>>) {
  match cont {
    Cont::Expr { body, .. } => collect_into(body, fc, out),
    Cont::Ref(id) => {
      out.push(apply_node(ident(&format!("·v_{}", id.0)), vec![ident(bound)]));
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
  body: &'src Cont,
  fc: &FmtCtx<'_, '_>,
) -> (Node<'static>, &'src Cont) {
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
fn split_trailing_cont(
  args: &[Arg],
) -> Option<(&[Arg], &Cont)> {
  match args.last() {
    Some(Arg::Cont(c)) => Some((&args[..args.len() - 1], c)),
    _ => None,
  }
}

// ---------------------------------------------------------------------------
// AST pretty-printer
//
// Copy of ast::fmt with adjustments for flat CPS output:
//   - Module items separated by blank lines (makes top-level bindings readable)
//   - Source map tracking removed (not needed for debug output)
// ---------------------------------------------------------------------------

use crate::ast::CmpPart;

fn fmt_ast(node: &Node) -> String {
  let mut out = String::new();
  fmt_node(node, &mut out, 0);
  out
}

fn ind(out: &mut String, depth: usize) {
  for _ in 0..depth {
    out.push_str("  ");
  }
}

fn is_fn(node: &Node) -> bool {
  matches!(node.kind, NodeKind::Fn { .. })
}

fn is_multiline(node: &Node) -> bool {
  match &node.kind {
    NodeKind::LitStr { open, content, .. } => open.src == "\":" || content.contains('\n'),
    NodeKind::StrRawTempl { open, .. } => open.src == "\":",
    NodeKind::Fn { body, .. } => body.items.len() > 1 || body.items.first().is_some_and(|b| !is_inline_expr(b)),
    NodeKind::Match { .. } | NodeKind::Block { .. } => true,
    NodeKind::Apply { args, .. } => args.items.iter().any(|a| is_multiline(a) || is_fn(a)),
    NodeKind::Pipe(exprs) => exprs.items.iter().any(|e| is_multiline(e)),
    _ => false,
  }
}

fn is_atom(node: &Node) -> bool {
  match &node.kind {
    NodeKind::LitStr { content, .. } => !content.contains('\n'),
    _ => matches!(
      node.kind,
      NodeKind::LitBool(_)
        | NodeKind::LitInt(_)
        | NodeKind::LitFloat(_)
        | NodeKind::LitDecimal(_)
        | NodeKind::Ident(_)
        | NodeKind::SynthIdent(_)
    ),
  }
}

fn fmt_node(node: &Node, out: &mut String, depth: usize) {
  match &node.kind {
    NodeKind::LitBool(v) => out.push_str(if *v { "true" } else { "false" }),
    NodeKind::LitInt(s) => out.push_str(s),
    NodeKind::LitFloat(s) => out.push_str(s),
    NodeKind::LitDecimal(s) => out.push_str(s),
    NodeKind::LitStr { open, close: _, content: s, .. } => {
      if open.src == "\":" {
        let content = s.trim_end_matches('\n');
        out.push_str("\":");
        for line in content.split('\n') {
          out.push('\n');
          ind(out, depth + 1);
          out.push_str(line);
        }
      } else {
        out.push('\'');
        if s.contains('\n') {
          for (i, line) in s.split('\n').enumerate() {
            if i > 0 { out.push('\n'); ind(out, depth + 1); }
            out.push_str(line);
          }
        } else {
          out.push_str(s);
        }
        out.push('\'');
      }
    }
    NodeKind::LitSeq { items, .. } if items.items.is_empty() => out.push_str("[]"),
    NodeKind::LitSeq { items, .. } => {
      out.push('[');
      for (i, child) in items.items.iter().enumerate() {
        if i > 0 { out.push_str(", "); }
        fmt_node(child, out, depth);
      }
      out.push(']');
    }
    NodeKind::LitRec { items, .. } if items.items.is_empty() => out.push_str("{}"),
    NodeKind::LitRec { items, .. } => {
      out.push('{');
      for (i, child) in items.items.iter().enumerate() {
        if i > 0 { out.push_str(", "); }
        fmt_node(child, out, depth);
      }
      out.push('}');
    }
    NodeKind::StrRawTempl { .. } | NodeKind::StrTempl { .. } => {
      // Not produced by cps_flat — emit placeholder
      out.push_str("<templ>");
    }
    NodeKind::Ident(s) => out.push_str(s),
    NodeKind::SynthIdent(n) => out.push_str(&format!("·$_{n}")),
    NodeKind::Spread { inner, .. } => {
      out.push_str("..");
      if let Some(n) = inner { fmt_node(n, out, depth); }
    }
    NodeKind::Bind { lhs, rhs, .. } => {
      fmt_node(lhs, out, depth);
      out.push_str(" = ");
      fmt_node(rhs, out, depth);
    }
    NodeKind::Apply { func, args } => fmt_apply(func, &args.items, out, depth),
    NodeKind::Module(exprs) => {
      for (i, child) in exprs.items.iter().enumerate() {
        if i > 0 { out.push_str("\n\n"); ind(out, depth); }
        fmt_node(child, out, depth);
      }
    }
    NodeKind::Fn { params, sep, body } => fmt_fn(params, sep, &body.items, out, depth),
    NodeKind::Patterns(exprs) => {
      for (i, child) in exprs.items.iter().enumerate() {
        if i == 0 { out.push(' '); } else { out.push_str(", "); }
        fmt_node(child, out, depth);
      }
    }
    NodeKind::UnaryOp { op, operand } => {
      out.push_str(op.src);
      if !op.src.starts_with('-') { out.push(' '); }
      fmt_node(operand, out, depth);
    }
    NodeKind::InfixOp { op, lhs, rhs } => {
      fmt_node(lhs, out, depth);
      out.push(' ');
      out.push_str(op.src);
      out.push(' ');
      fmt_node(rhs, out, depth);
    }
    NodeKind::ChainedCmp(parts) => {
      for part in parts.iter() {
        match part {
          CmpPart::Operand(n) => fmt_node(n, out, depth),
          CmpPart::Op(tok) => { out.push(' '); out.push_str(tok.src); out.push(' '); }
        }
      }
    }
    NodeKind::Member { lhs, rhs, .. } => {
      fmt_node(lhs, out, depth);
      out.push('.');
      fmt_node(rhs, out, depth);
    }
    NodeKind::Group { inner, .. } => {
      out.push('(');
      fmt_node(inner, out, depth);
      out.push(')');
    }
    NodeKind::Partial => out.push('?'),
    NodeKind::Wildcard => out.push('_'),
    NodeKind::Token(s) => out.push_str(s),
    NodeKind::BindRight { lhs, rhs, .. } => {
      fmt_node(lhs, out, depth);
      out.push_str(" |= ");
      fmt_node(rhs, out, depth);
    }
    NodeKind::Pipe(exprs) => {
      let multiline = exprs.items.iter().any(|e| is_multiline(e));
      for (i, child) in exprs.items.iter().enumerate() {
        if i > 0 {
          if multiline { out.push('\n'); ind(out, depth); out.push_str("| "); }
          else { out.push_str(" | "); }
        }
        fmt_node(child, out, depth);
      }
    }
    NodeKind::Match { subjects, arms, .. } => {
      out.push_str("match ");
      for (i, subj) in subjects.items.iter().enumerate() {
        if i > 0 { out.push_str(", "); }
        fmt_node(subj, out, depth);
      }
      out.push(':');
      for arm in &arms.items {
        out.push('\n'); ind(out, depth + 1);
        fmt_node(arm, out, depth + 1);
      }
    }
    NodeKind::Arm { lhs, body, .. } => {
      fmt_node(lhs, out, depth);
      out.push(':');
      fmt_body(&body.items, out, depth, true);
    }
    NodeKind::Try(inner) => { out.push_str("try "); fmt_node(inner, out, depth); }
    NodeKind::Yield(inner) => { out.push_str("yield "); fmt_node(inner, out, depth); }
    NodeKind::Block { name, params, body, .. } => {
      fmt_node(name, out, depth);
      out.push(' ');
      fmt_node(params, out, depth);
      out.push(':');
      fmt_body(&body.items, out, depth, true);
    }
  }
}

fn is_complex_arg(node: &Node) -> bool {
  match &node.kind {
    NodeKind::Apply { args, .. } => args.items.iter().any(is_fn),
    _ => false,
  }
}

fn fmt_apply(func: &Node, args: &[Node], out: &mut String, depth: usize) {
  fmt_node(func, out, depth);

  let trailing_start = args.iter().rposition(|a| !is_fn(a) && !is_complex_arg(a))
    .map(|i| i + 1).unwrap_or(0);
  let (plain, trailing) = args.split_at(trailing_start);

  for (i, arg) in plain.iter().enumerate() {
    if i == 0 { out.push(' '); } else { out.push_str(", "); }
    fmt_node(arg, out, depth);
  }

  if trailing.is_empty() { return; }

  if trailing.len() == 1 && is_fn(&trailing[0])
    && let NodeKind::Fn { params, sep, body } = &trailing[0].kind {
      if plain.is_empty() { out.push(' '); } else { out.push_str(", "); }
      fmt_fn_with_inline(params, sep, &body.items, out, depth, false);
      return;
  }

  if !plain.is_empty() { out.push(','); }
  for arg in trailing {
    out.push('\n'); ind(out, depth + 1);
    if let NodeKind::Fn { params, sep, body } = &arg.kind {
      fmt_fn_with_inline(params, sep, &body.items, out, depth + 1, true);
    } else {
      fmt_node(arg, out, depth + 1);
    }
  }
}

fn fmt_fn(params: &Node, sep: &Token, body: &[Node], out: &mut String, depth: usize) {
  fmt_fn_with_inline(params, sep, body, out, depth, true);
}

fn fmt_fn_with_inline(params: &Node, sep: &Token, body: &[Node], out: &mut String, depth: usize, allow_apply_inline: bool) {
  let inline = body.len() == 1 && if allow_apply_inline {
    is_inline_expr(&body[0])
  } else {
    is_inline_single_trailing(&body[0])
  };
  if inline {
    fmt_fn_params(params, out);
    out.push_str(": ");
    fmt_node(&body[0], out, depth);
  } else {
    fmt_fn_params(params, out);
    out.push(':');
    fmt_body(body, out, depth, allow_apply_inline);
  }
  let _ = sep; // sep token not needed — we always emit ":"
}

fn is_inline_expr(node: &Node) -> bool {
  if is_multiline(node) { return false; }
  match &node.kind {
    NodeKind::Apply { .. } => false,
    _ => is_atom(node),
  }
}

fn is_inline_single_trailing(node: &Node) -> bool {
  is_atom(node)
}

fn fmt_fn_params(params: &Node, out: &mut String) {
  out.push_str("fn");
  if let NodeKind::Patterns(exprs) = &params.kind {
    for (i, child) in exprs.items.iter().enumerate() {
      if i == 0 { out.push(' '); } else { out.push_str(", "); }
      fmt_node(child, out, 0);
    }
  } else {
    out.push(' ');
    fmt_node(params, out, 0);
  }
}

fn fmt_body(body: &[Node], out: &mut String, depth: usize, allow_apply_inline: bool) {
  if body.len() == 1 {
    let inline = if allow_apply_inline { is_inline_expr(&body[0]) } else { is_inline_single_trailing(&body[0]) };
    if inline {
      out.push(' ');
      fmt_node(&body[0], out, depth);
      return;
    }
  }
  for stmt in body {
    out.push('\n'); ind(out, depth + 1);
    fmt_node(stmt, out, depth + 1);
  }
}
