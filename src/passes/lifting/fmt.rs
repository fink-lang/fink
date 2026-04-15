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
// Output is built as a flat-AST arena (Module of Bind/Apply nodes) so the
// existing pretty-printer ([src/passes/ast/fmt.rs](src/passes/ast/fmt.rs))
// handles indentation and line-breaking. Under the flat-ast refactor this
// uses an `AstBuilder<'static>` to build a transient arena per render.

use crate::ast::{AstBuilder, AstId, NodeKind, Exprs};
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
  let mut b = AstBuilder::new();
  let stmt_ids = collect_stmts(&mut b, expr, &fc);
  let module_id = b.append(
    NodeKind::Module {
      exprs: Exprs { items: stmt_ids.into_boxed_slice(), seps: vec![] },
      // Synthetic: this AST is reconstructed from lifted CPS for display only,
      // the URL isn't meaningful here.
      url: String::new(),
    },
    dummy_loc(),
  );
  let ast = b.finish(module_id);
  crate::passes::ast::fmt::fmt_block(&ast)
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

fn b_ident(b: &mut AstBuilder<'static>, s: &str) -> AstId {
  let s: &'static str = Box::leak(s.to_string().into_boxed_str());
  b.append(NodeKind::Ident(s), dummy_loc())
}

fn b_bind(b: &mut AstBuilder<'static>, lhs: AstId, rhs: AstId) -> AstId {
  b.append(NodeKind::Bind { op: eq_tok(), lhs, rhs }, dummy_loc())
}

fn b_apply(b: &mut AstBuilder<'static>, func: AstId, args: Vec<AstId>) -> AstId {
  let n = args.len();
  let seps = (0..n.saturating_sub(1)).map(|_| sep_tok()).collect();
  b.append(
    NodeKind::Apply { func, args: Exprs { items: args.into_boxed_slice(), seps } },
    dummy_loc(),
  )
}

fn b_fn(b: &mut AstBuilder<'static>, params: Vec<AstId>, body_stmts: Vec<AstId>) -> AstId {
  let n = params.len();
  let param_seps = (0..n.saturating_sub(1)).map(|_| sep_tok()).collect();
  let pats = b.append(
    NodeKind::Patterns(Exprs { items: params.into_boxed_slice(), seps: param_seps }),
    dummy_loc(),
  );
  b.append(
    NodeKind::Fn { params: pats, sep: col_tok(), body: Exprs { items: body_stmts.into_boxed_slice(), seps: vec![] } },
    dummy_loc(),
  )
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
  // Otherwise try source name from origin AST node (looked up through the
  // flat ast arena).
  let ast_id = fc.ctx.origin.try_get(origin_id).and_then(|opt| *opt);
  match ast_id {
    Some(id) => match &fc.ctx.ast.nodes.get(id).kind {
      NodeKind::Ident(s) => format!("·{}_{}", s, param_id.0),
      _ => format!("·v_{}", param_id.0),
    },
    None => format!("·v_{}", param_id.0),
  }
}

fn render_synth_name(cps_id: CpsId, fc: &FmtCtx<'_, '_>) -> String {
  let ast_id = fc.ctx.origin.try_get(cps_id).and_then(|opt| *opt);
  match ast_id {
    Some(id) => match &fc.ctx.ast.nodes.get(id).kind {
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
      Bind::SynthName => render_synth_name(cps_id, fc),
      Bind::Synth => format!("·v_{}", cps_id.0),
    };
  }
  format!("·v_{}", cps_id.0)
}

fn render_unresolved_name(cps_id: CpsId, fc: &FmtCtx<'_, '_>) -> String {
  let ast_id = fc.ctx.origin.try_get(cps_id).and_then(|opt| *opt);
  match ast_id {
    Some(id) => match &fc.ctx.ast.nodes.get(id).kind {
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

/// Render a single param as an AST node id, with spread support.
fn render_param_node(b: &mut AstBuilder<'static>, p: &Param, fc: &FmtCtx<'_, '_>, use_info: bool) -> AstId {
  match p {
    Param::Name(bn) => {
      let name = if use_info { render_param_with_info(bn, fc) } else { render_bind(bn, fc) };
      b_ident(b, &name)
    }
    Param::Spread(bn) => {
      let name = if use_info { render_param_with_info(bn, fc) } else { render_bind(bn, fc) };
      let inner = b_ident(b, &name);
      b.append(
        NodeKind::Spread { op: spread_tok(), inner: Some(inner) },
        dummy_loc(),
      )
    }
  }
}

/// Render function params with grouping when `param_info` is available:
///   `{cap0, cap1}, [param0, param1], cont`
/// Falls back to flat rendering when `param_info` is not set.
fn render_fn_params_grouped(b: &mut AstBuilder<'static>, params: &[Param], fc: &FmtCtx<'_, '_>) -> Vec<AstId> {
  let pi = match fc.ctx.param_info {
    Some(pi) if !pi.is_empty() => pi,
    _ => {
      // No param_info — flat rendering.
      return params.iter().map(|p| render_param_node(b, p, fc, false)).collect();
    }
  };

  let mut caps: Vec<AstId> = Vec::new();
  let mut user_params: Vec<AstId> = Vec::new();

  for p in params {
    let bn = match p { Param::Name(b) | Param::Spread(b) => b };
    let info = pi.try_get(bn.id).and_then(|o| *o);
    match info {
      Some(ParamInfo::Cap(_)) => caps.push(render_param_node(b, p, fc, true)),
      // Conts-first: cont params go in the user params group (inside []).
      Some(ParamInfo::Cont) | Some(ParamInfo::Param(_)) | None => user_params.push(render_param_node(b, p, fc, true)),
    }
  }

  let mut result: Vec<AstId> = Vec::new();

  // Captures: {cap0, cap1}
  if !caps.is_empty() {
    let n = caps.len();
    let id = b.append(
      NodeKind::LitRec {
        open: lbrace_tok(),
        close: rbrace_tok(),
        items: Exprs { items: caps.into_boxed_slice(), seps: (0..n.saturating_sub(1)).map(|_| sep_tok()).collect() },
      },
      dummy_loc(),
    );
    result.push(id);
  }

  // User params: [param0, param1]
  if !user_params.is_empty() {
    let n = user_params.len();
    let id = b.append(
      NodeKind::LitSeq {
        open: lbrack_tok(),
        close: rbrack_tok(),
        items: Exprs { items: user_params.into_boxed_slice(), seps: (0..n.saturating_sub(1)).map(|_| sep_tok()).collect() },
      },
      dummy_loc(),
    );
    result.push(id);
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

fn render_val(b: &mut AstBuilder<'static>, val: &Val, fc: &FmtCtx<'_, '_>) -> AstId {
  match &val.kind {
    ValKind::Lit(lit) => lit_node(b, lit),
    ValKind::Ref(Ref::Synth(bind_id))      => {
      // Only use AST origin names for SynthName binds (source-level names).
      // Plain Synth binds (compiler temps like op_eq results) render as ·v_N.
      let is_synth_name = fc.ctx.bind_kinds
        .and_then(|bk| bk.try_get(*bind_id))
        .and_then(|o| *o)
        .is_some_and(|k| matches!(k, Bind::SynthName));
      if is_synth_name {
        b_ident(b, &render_synth_name(*bind_id, fc))
      } else {
        b_ident(b, &render_synth_fallback(*bind_id, fc))
      }
    }
    ValKind::Ref(Ref::Unresolved(_)) => b_ident(b, &render_unresolved_name(val.id, fc)),
    ValKind::ContRef(id)     => b_ident(b, &render_synth_fallback(*id, fc)),
    ValKind::BuiltIn(op)     => b_ident(b, &render_builtin_flat(op)),
  }
}

fn lit_node(b: &mut AstBuilder<'static>, lit: &Lit) -> AstId {
  match lit {
    Lit::Bool(v) => b.append(NodeKind::LitBool(*v), dummy_loc()),
    Lit::Int(n) => {
      let s: &'static str = Box::leak(n.to_string().into_boxed_str());
      b.append(NodeKind::LitInt(s), dummy_loc())
    }
    Lit::Float(f) => {
      let s: &'static str = Box::leak(f.to_string().into_boxed_str());
      b.append(NodeKind::LitFloat(s), dummy_loc())
    }
    Lit::Decimal(f) => {
      let s: &'static str = Box::leak(format!("{}d", f).into_boxed_str());
      b.append(NodeKind::LitDecimal(s), dummy_loc())
    }
    Lit::Str(s) => b.append(
      NodeKind::LitStr {
        open: tok("'", TokenKind::StrStart),
        close: tok("'", TokenKind::StrEnd),
        content: crate::strings::control_pics_bytes(s),
        indent: 0,
      },
      dummy_loc(),
    ),
    Lit::Seq => b.append(
      NodeKind::LitSeq { open: lbrack_tok(), close: rbrack_tok(), items: Exprs::empty() },
      dummy_loc(),
    ),
    Lit::Rec => b.append(
      NodeKind::LitRec { open: lbrace_tok(), close: rbrace_tok(), items: Exprs::empty() },
      dummy_loc(),
    ),
  }
}

fn render_builtin_flat(op: &BuiltIn) -> String {
  crate::passes::cps::fmt::render_builtin_name(op)
}

// ---------------------------------------------------------------------------
// Cont → argument node for App rendering
// ---------------------------------------------------------------------------

fn render_cont_arg(b: &mut AstBuilder<'static>, cont: &Cont, fc: &FmtCtx<'_, '_>) -> AstId {
  match cont {
    Cont::Ref(id) => b_ident(b, &render_synth_fallback(*id, fc)),
    Cont::Expr { args, body } => {
      let params: Vec<AstId> = args.iter()
        .map(|bn| b_ident(b, &render_bind(bn, fc)))
        .collect();
      let body_stmts = collect_stmts(b, body, fc);
      b_fn(b, params, body_stmts)
    }
  }
}

// ---------------------------------------------------------------------------
// App → flat Node (a bare expression statement)
// ---------------------------------------------------------------------------

fn render_app(b: &mut AstBuilder<'static>, func: &Callable, args: &[Arg], fc: &FmtCtx<'_, '_>) -> AstId {
  let func_id = match func {
    Callable::Val(v)       => render_val(b, v, fc),
    Callable::BuiltIn(op)  => b_ident(b, &render_builtin_flat(op)),
  };
  if args.is_empty() {
    let placeholder = b_ident(b, "_");
    return b_apply(b, func_id, vec![placeholder]);
  }
  let arg_ids: Vec<AstId> = args.iter().map(|a| match a {
    Arg::Val(v)    => render_val(b, v, fc),
    Arg::Spread(v) => {
      let inner = render_val(b, v, fc);
      b.append(
        NodeKind::Spread { op: spread_tok(), inner: Some(inner) },
        dummy_loc(),
      )
    }
    Arg::Cont(c)   => render_cont_arg(b, c, fc),
    Arg::Expr(e)   => {
      let stmts = collect_stmts(b, e, fc);
      b_fn(b, vec![], stmts)
    }
  }).collect();
  b_apply(b, func_id, arg_ids)
}

// ---------------------------------------------------------------------------
// Core: collect a sequence of flat statement nodes from a CPS expression
// ---------------------------------------------------------------------------

fn collect_stmts(b: &mut AstBuilder<'static>, expr: &Expr, fc: &FmtCtx<'_, '_>) -> Vec<AstId> {
  let mut stmts: Vec<AstId> = vec![];
  collect_into(b, expr, fc, &mut stmts);
  stmts
}

/// Recursively walk the LetFn/LetVal chain, emitting assignments and then the
/// tail expression. Consecutive LetFn → LetVal aliases are chained as `b = a = fn ...`.
fn collect_into(b: &mut AstBuilder<'static>, expr: &Expr, fc: &FmtCtx<'_, '_>, out: &mut Vec<AstId>) {
  match &expr.kind {
    ExprKind::LetFn { name, params, fn_body, cont, .. } => {
      let name_str = render_bind(name, fc);

      let fn_params = render_fn_params_grouped(b, params, fc);

      let body_stmts = collect_stmts(b, fn_body, fc);
      let fn_rhs = b_fn(b, fn_params, body_stmts);

      let (lhs_id, tail) = chain_lhs(b, &name_str, cont, fc);
      let bound = outermost_name(b, lhs_id).unwrap_or_else(|| name_str.clone());
      let bind_id = b_bind(b, lhs_id, fn_rhs);
      out.push(bind_id);
      collect_cont_into(b, tail, &bound, fc, out);
    }

    ExprKind::LetVal { name, val, cont } => {
      let name_str = render_bind(name, fc);
      let val_id = render_val(b, val, fc);
      let (lhs_id, tail) = chain_lhs(b, &name_str, cont, fc);
      let bound = outermost_name(b, lhs_id).unwrap_or_else(|| name_str.clone());
      let bind_id = b_bind(b, lhs_id, val_id);
      out.push(bind_id);
      collect_cont_into(b, tail, &bound, fc, out);
    }

    ExprKind::App { func, args } => {
      // ·fn_closure is a constructor binding: render as `[a, b] = ·fn_closure caps...`
      // (single param: `a = ...`) and recurse into the cont body flat.
      if matches!(func, Callable::BuiltIn(BuiltIn::FnClosure))
        && let Some((value_args, Cont::Expr { args: cont_params, body })) = split_trailing_cont(args) {
          let func_id = b_ident(b, &render_builtin_flat(&BuiltIn::FnClosure));
          let arg_ids: Vec<AstId> = value_args.iter().map(|a| match a {
            Arg::Val(v) => render_val(b, v, fc),
            Arg::Spread(v) => {
              let inner = render_val(b, v, fc);
              b.append(NodeKind::Spread { op: spread_tok(), inner: Some(inner) }, dummy_loc())
            }
            Arg::Cont(c) => render_cont_arg(b, c, fc),
            Arg::Expr(e) => {
              let stmts = collect_stmts(b, e.as_ref(), fc);
              b_fn(b, vec![], stmts)
            }
          }).collect();
          let call_id = b_apply(b, func_id, arg_ids);
          let lhs_id = if cont_params.len() == 1 {
            b_ident(b, &render_bind(&cont_params[0], fc))
          } else {
            let names: Vec<AstId> = cont_params.iter()
              .map(|bn| b_ident(b, &render_bind(bn, fc)))
              .collect();
            let n = names.len();
            b.append(
              NodeKind::LitSeq {
                open: lbrack_tok(),
                close: rbrack_tok(),
                items: Exprs { items: names.into_boxed_slice(), seps: (0..n.saturating_sub(1)).map(|_| sep_tok()).collect() },
              },
              dummy_loc(),
            )
          };
          let bind_id = b_bind(b, lhs_id, call_id);
          out.push(bind_id);
          collect_into(b, body.as_ref(), fc, out);
          return;
        }
      // ·ƒpub is a side-effect statement: render as `·ƒpub val` then
      // continue with the cont body as sequential statements.
      if matches!(func, Callable::BuiltIn(BuiltIn::Pub))
        && let Some((value_args, Cont::Expr { body, .. })) = split_trailing_cont(args) {
          let func_id = b_ident(b, &render_builtin_flat(&BuiltIn::Pub));
          let arg_ids: Vec<AstId> = value_args.iter().map(|a| match a {
            Arg::Val(v) => render_val(b, v, fc),
            _ => b_ident(b, "_"),
          }).collect();
          let app_id = b_apply(b, func_id, arg_ids);
          out.push(app_id);
          collect_into(b, body.as_ref(), fc, out);
          return;
        }
      let app_id = render_app(b, func, args, fc);
      out.push(app_id);
    }

    ExprKind::If { cond, then, else_ } => {
      let cond_id = render_val(b, cond, fc);
      let then_stmts = collect_stmts(b, then, fc);
      let else_stmts = collect_stmts(b, else_, fc);
      let then_fn = b_fn(b, vec![], then_stmts);
      let else_fn = b_fn(b, vec![], else_stmts);
      let if_keyword = b_ident(b, "·if");
      let app_id = b_apply(b, if_keyword, vec![cond_id, then_fn, else_fn]);
      out.push(app_id);
    }
  }
}

/// Emit statements for a `Cont` body — either recurse into the inner Expr,
/// or for `Cont::Ref` emit a tail call passing the last-bound name.
fn collect_cont_into(b: &mut AstBuilder<'static>, cont: &Cont, bound: &str, fc: &FmtCtx<'_, '_>, out: &mut Vec<AstId>) {
  match cont {
    Cont::Expr { body, .. } => collect_into(b, body, fc, out),
    Cont::Ref(id) => {
      let cont_id = b_ident(b, &render_synth_fallback(*id, fc));
      let bound_id = b_ident(b, bound);
      let app_id = b_apply(b, cont_id, vec![bound_id]);
      out.push(app_id);
    }
  }
}

/// Extract the leftmost (outermost) name from a possibly-chained lhs node.
/// `alias = name` → "alias"; plain `name` → "name".
fn outermost_name(b: &AstBuilder<'static>, id: AstId) -> Option<String> {
  match &b.read(id).kind {
    NodeKind::Ident(s) => Some(s.to_string()),
    NodeKind::Bind { lhs, .. } => {
      let lhs = *lhs;
      outermost_name(b, lhs)
    }
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
  b: &mut AstBuilder<'static>,
  name: &str,
  body: &'src Cont,
  fc: &FmtCtx<'_, '_>,
) -> (AstId, &'src Cont) {
  let body_expr = match body {
    Cont::Expr { args, body } if args.len() == 1 => body.as_ref(),
    _ => return (b_ident(b, name), body),
  };

  if let ExprKind::LetVal { name: alias, val, cont: inner_body } = &body_expr.kind {
    // We need to compare the rendered value to `name`. Since we render val
    // into the arena and then check, just check the val's kind for a Ref
    // matching the previous name pattern.
    let val_matches = match &val.kind {
      ValKind::Ref(Ref::Synth(bind_id)) => {
        // The value is a Ref::Synth — render its name and compare to `name`.
        let is_synth_name = fc.ctx.bind_kinds
          .and_then(|bk| bk.try_get(*bind_id))
          .and_then(|o| *o)
          .is_some_and(|k| matches!(k, Bind::SynthName));
        let rendered = if is_synth_name {
          render_synth_name(*bind_id, fc)
        } else {
          render_synth_fallback(*bind_id, fc)
        };
        rendered == name
      }
      _ => false,
    };

    if val_matches {
      let alias_str = render_bind(alias, fc);
      let alias_id = b_ident(b, &alias_str);
      let name_id = b_ident(b, name);
      let chained = b_bind(b, alias_id, name_id);
      return (chained, inner_body);
    }
  }

  (b_ident(b, name), body)
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
