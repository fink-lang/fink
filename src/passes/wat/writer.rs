// WAT text generator from lifted CPS IR.
//
// Produces a WAT module (s-expression text) from fully-lifted CPS IR.
// The output is a fragment — builtin functions are referenced by name but not
// defined. The module is suitable for snapshot testing and playground use.
//
// ## Calling convention
//
// Every Fink function — including builtins — is CPS: the last parameter is
// always the continuation. Builtins never return values directly; they call
// their continuation with the result. There are no direct `call` instructions
// that return values — all calls are CPS tail calls via return_call_ref.
//
// Every Fink function: (param (ref $Any) * N) where the last param is the cont.
// All tail calls use return_call_ref with the matching $FnN type, inline form:
//   (return_call_ref $FnN (local.get $callee) (local.get $arg) ...)
//
// ## Type layout
//
// $Any   — (sub (struct))                     — GC root, tagged union base
// $Num   — (sub $Any (struct (field f64)))    — boxed number (f64 unified)
// $FnN   — (func (param (ref $Any) * N))      — function type of arity N (N >= 1)
//
// Per-arity func types are collected from the IR and emitted in the type
// section. $Fn1 doubles as the continuation type.
//
// ## Two-pass structure
//
// 1. collect_funcs  — walk the top-level LetFn/LetVal chain; gather CollectedFn
//                     entries and the exports list from the terminal App.
// 2. emit_*         — type section, then one (func ...) per entry, then exports.
//
// ## Builtins
//
// BuiltIn ops are emitted as (call $builtin_<name> ...) — referenced but not
// defined. The environment (test harness / runtime) is expected to provide them.

use crate::ast::{AstId, Node as AstNode, NodeKind};
use crate::passes::cps::ir::{
  Arg, BuiltIn, Callable, Cont, CpsId, CpsResult, Expr, ExprKind,
  Lit, Ref, Val, ValKind,
};
use crate::passes::wasm::collect::{
  self, CollectedFn, IrCtx, Module,
  builtin_name, collect_locals, split_args,
};
use crate::propgraph::PropGraph;
use crate::sourcemap::{MappedWriter, SourceMap};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Generate WAT text from fully-lifted CPS IR.
pub fn emit(
  cps: &CpsResult,
  ast_index: &PropGraph<AstId, Option<&AstNode<'_>>>,
) -> String {
  let mut w = MappedWriter::new();
  let ir_ctx = IrCtx::new(&cps.origin, ast_index);
  let module = collect::collect(&cps.root, &ir_ctx);
  let ctx = Ctx::new(ir_ctx.with_globals(module.globals.clone()));
  emit_module(&module, &ctx, &mut w);
  w.finish_string()
}

/// Generate WAT text with source map.
pub fn emit_mapped(
  cps: &CpsResult,
  ast_index: &PropGraph<AstId, Option<&AstNode<'_>>>,
  source_name: &str,
) -> (String, SourceMap) {
  let mut w = MappedWriter::new();
  let ir_ctx = IrCtx::new(&cps.origin, ast_index);
  let module = collect::collect(&cps.root, &ir_ctx);
  let ctx = Ctx::new(ir_ctx.with_globals(module.globals.clone()));
  emit_module(&module, &ctx, &mut w);
  w.finish(source_name)
}

/// Generate WAT text with source map and embedded source content.
pub fn emit_mapped_with_content(
  cps: &CpsResult,
  ast_index: &PropGraph<AstId, Option<&AstNode<'_>>>,
  source_name: &str,
  source_content: &str,
) -> (String, SourceMap) {
  let mut w = MappedWriter::new();
  let ir_ctx = IrCtx::new(&cps.origin, ast_index);
  let module = collect::collect(&cps.root, &ir_ctx);
  let ctx = Ctx::new(ir_ctx.with_globals(module.globals.clone()));
  emit_module(&module, &ctx, &mut w);
  w.finish_with_content(source_name, source_content)
}

// ---------------------------------------------------------------------------
// WAT-specific context — wraps IrCtx with MappedWriter mark support
// ---------------------------------------------------------------------------

struct Ctx<'a, 'src> {
  ir: IrCtx<'a, 'src>,
}

impl<'a, 'src> Ctx<'a, 'src> {
  fn new(ir: IrCtx<'a, 'src>) -> Self {
    Self { ir }
  }

  fn is_global(&self, id: CpsId) -> bool { self.ir.is_global(id) }
  fn ast_node(&self, id: CpsId) -> Option<&'src AstNode<'src>> { self.ir.ast_node(id) }
  fn label(&self, id: CpsId) -> String { self.ir.label(id) }

  /// Mark the output position with the source location of a CPS node.
  fn mark(&self, id: CpsId, w: &mut MappedWriter) {
    if let Some(node) = self.ir.ast_node(id) {
      w.mark(node.loc);
    }
  }

  /// For a pipe-desugared App, return the `|` separator token loc for this call stage.
  fn pipe_sep_loc(&self, expr_id: CpsId, func_val: &Val<'_>) -> Option<Loc> {
    let node = self.ir.ast_node(expr_id)?;
    let NodeKind::Pipe(exprs) = &node.kind else { return None };
    let func_ast_id = self.ir.origin.try_get(func_val.id).and_then(|o| *o)?;
    let stage_idx = exprs.items.iter().position(|item| item.id == func_ast_id)?;
    if stage_idx == 0 { return None; }
    exprs.seps.get(stage_idx - 1).map(|sep| sep.loc)
  }
}

// ---------------------------------------------------------------------------
// WatExpr — source-mapped inline WAT s-expression tree
//
// Represents a WAT s-expression with optional source location marks.
// `write_expr` traverses the tree, calling `w.mark` before each marked node
// and `w.push_str` for the text — preserving inline style while tracking
// output positions for source maps.
//
// Usage:
//   WatExpr::list("struct.new $Num", vec![WatExpr::atom("(f64.const 42)")])
//   WatExpr::marked(loc, inner)  — mark this position in the source map
// ---------------------------------------------------------------------------

use crate::lexer::Loc;

enum WatExpr {
  /// A pre-formatted atom — emitted verbatim (no parens added).
  Atom(String),
  /// A compound s-expression: (head arg0 arg1 ...).
  List(String, Vec<WatExpr>),
  /// Attach a source location mark before emitting the inner expression.
  Marked(Loc, Box<WatExpr>),
}

impl WatExpr {
  fn atom(s: impl Into<String>) -> Self { WatExpr::Atom(s.into()) }
  fn list(head: impl Into<String>, args: Vec<WatExpr>) -> Self { WatExpr::List(head.into(), args) }
  fn marked(loc: Loc, inner: WatExpr) -> Self { WatExpr::Marked(loc, Box::new(inner)) }
}

/// Write a `WatExpr` to `w` inline (no trailing newline).
fn write_expr(expr: &WatExpr, w: &mut MappedWriter) {
  match expr {
    WatExpr::Atom(s) => w.push_str(s),
    WatExpr::List(head, args) => {
      w.push_str("(");
      w.push_str(head);
      for arg in args {
        w.push_str(" ");
        write_expr(arg, w);
      }
      w.push_str(")");
    }
    WatExpr::Marked(loc, inner) => {
      w.mark(*loc);
      write_expr(inner, w);
    }
  }
}


// ---------------------------------------------------------------------------
// Emit pass
// ---------------------------------------------------------------------------

fn emit_module(module: &Module<'_, '_>, ctx: &Ctx<'_, '_>, w: &mut MappedWriter) {
  w.push_str("(module\n");
  emit_types(module, w);
  w.push_str("\n");
  for func in &module.funcs {
    emit_func(func, ctx, w);
  }
  emit_exports(module, ctx, w);
  w.push_str(")\n");
}

/// Emit the type section: $Any root, $Num struct, then $FnN for each arity.
fn emit_types(module: &Module<'_, '_>, w: &mut MappedWriter) {
  w.push_str("  (type $Any (sub (struct)))\n");
  w.push_str("  (type $Num (sub $Any (struct (field f64))))\n");
  for arity in &module.arities {
    let params: String = (0..*arity).map(|_| "(ref $Any)").collect::<Vec<_>>().join(" ");
    if params.is_empty() {
      w.push_str(&format!("  (type $Fn{} (func))\n", arity));
    } else {
      w.push_str(&format!("  (type $Fn{} (func (param {})))\n", arity, params));
    }
  }
}

fn emit_exports(module: &Module<'_, '_>, ctx: &Ctx<'_, '_>, w: &mut MappedWriter) {
  for func in &module.funcs {
    if let Some(name) = &func.export_as {
      if let Some(bind_id) = func.export_bind_id {
        ctx.mark(bind_id, w);
      }
      w.push_str(&format!("  (export {:?} (func ${}))\n", name, func.label));
    }
  }
}

fn emit_func(func: &CollectedFn<'_, '_>, ctx: &Ctx<'_, '_>, w: &mut MappedWriter) {
  let arity = func.params.len();
  // Emit the alias global before the function it names.
  if let Some((alias_id, alias_label)) = &func.alias {
    ctx.mark(*alias_id, w);
    w.push_str(&format!("  (global ${} (ref $Fn{}) (ref.func ${}))\n", alias_label, arity, func.label));
  }
  ctx.mark(func.fn_id, w);
  w.push_str(&format!("  (func ${} (type $Fn{})", func.label, arity));
  for (id, label) in &func.params {
    w.push_str(" ");
    ctx.mark(*id, w);
    w.push_str(&format!("(param ${} (ref $Any))", label));
  }
  w.push_str("\n");

  // Pre-scan locals (LetVal bindings inside the body).
  let locals = collect_locals(func.body, &ctx.ir);
  for local in &locals {
    w.push_str(&format!("    (local ${} (ref $Any))\n", local));
  }

  // Body.
  emit_body(func.body, ctx, w, 2);

  w.push_str("  )\n");
}

// ---------------------------------------------------------------------------
// Body emission
// ---------------------------------------------------------------------------

fn emit_body(expr: &Expr<'_>, ctx: &Ctx<'_, '_>, w: &mut MappedWriter, indent: usize) {
  match &expr.kind {
    ExprKind::LetVal { name, val, cont } => {
      let local = ctx.label(name.id);
      // Mark local.set with the = operator, $name with the binding ident.
      let set_loc = ctx.ast_node(expr.id)
        .and_then(|n| match &n.kind {
          NodeKind::Bind { op, .. } => Some(op.loc),
          _ => None,
        });
      let name_loc = ctx.ast_node(name.id).map(|n| n.loc);
      let mut set_args = vec![];
      if let Some(loc) = name_loc {
        set_args.push(WatExpr::marked(loc, WatExpr::atom(format!("${local}"))));
      } else {
        set_args.push(WatExpr::atom(format!("${local}")));
      }
      set_args.push(val_expr(val, ctx));
      let set_inner = WatExpr::list("local.set", set_args);
      let set_expr = match set_loc {
        Some(loc) => WatExpr::marked(loc, set_inner),
        None => set_inner,
      };
      w.push_str(&ind(indent));
      write_expr(&set_expr, w);
      w.push_str("\n");
      match cont {
        Cont::Expr { body, .. } => emit_body(body, ctx, w, indent),
        Cont::Ref(id) => {
          ctx.mark(val.id, w);
          let call_expr = WatExpr::list(
            "return_call_ref $Fn1",
            vec![
              WatExpr::list("local.get", vec![WatExpr::atom(format!("${}", ctx.label(*id)))]),
              WatExpr::list("local.get", vec![WatExpr::atom(format!("${}", local))]),
            ],
          );
          w.push_str(&ind(indent));
          write_expr(&call_expr, w);
          w.push_str("\n");
        }
      }
    }
    ExprKind::App { func, args } => {
      // For a continuation call, the meaningful source location is the value
      // being passed (the "return expression"). Detect cont calls by checking
      // if the callee's origin has no user-visible source name (Bind::Cont /
      // synthetic callee). For user fn calls, the App expr id maps to the site.
      let is_cont_call = match func {
        Callable::Val(v) => match &v.kind {
          ValKind::ContRef(_) => true,
          // A Ref::Synth targeting a Bind::Cont has no user-visible source name —
          // its origin maps to the `fn:` node (NodeKind::Fn), not an Ident.
          // That is the signal that this is a CPS-inserted cont param call.
          ValKind::Ref(Ref::Synth(id)) => matches!(
            ctx.ast_node(*id).map(|n| &n.kind),
            None | Some(NodeKind::Fn { .. })
          ),
          _ => false,
        },
        _ => false,
      };
      // For builtin ops, mark at the operator token (op.loc inside InfixOp/UnaryOp),
      // not the whole expression node. For cont calls, mark at the value being passed.
      // For user fn calls, mark at the call-site expression.
      match func {
        Callable::BuiltIn(_) => {
          if let Some(node) = ctx.ast_node(expr.id) {
            let loc = match &node.kind {
              NodeKind::InfixOp { op, .. } | NodeKind::UnaryOp { op, .. } => op.loc,
              _ => node.loc,
            };
            w.mark(loc);
          }
        }
        Callable::Val(_) if is_cont_call => {
          let mark_id = args.iter()
            .find_map(|a| if let Arg::Val(v) = a { Some(v.id) } else { None })
            .unwrap_or(expr.id);
          ctx.mark(mark_id, w);
        }
        Callable::Val(func_val) => {
          if let Some(loc) = ctx.pipe_sep_loc(expr.id, func_val) {
            w.mark(loc);
          } else {
            ctx.mark(expr.id, w);
          }
        }
      }
      emit_app(func, args, ctx, w, indent);
    }
    ExprKind::If { cond, then, else_ } => {
      ctx.mark(expr.id, w);
      // Unbox cond to f64, compare != 0 for truthiness.
      let cond_wat = WatExpr::list("f64.ne", vec![
        WatExpr::atom("(f64.const 0)"),
        WatExpr::list("struct.get $Num 0", vec![
          WatExpr::list("ref.cast (ref $Num)", vec![val_expr(cond, ctx)]),
        ]),
      ]);
      w.push_str(&format!("{}(if ", ind(indent)));
      write_expr(&cond_wat, w);
      w.push_str("\n");
      w.push_str(&format!("{}(then\n", ind(indent + 1)));
      emit_body(then, ctx, w, indent + 2);
      w.push_str(&format!("{})\n", ind(indent + 1)));
      w.push_str(&format!("{}(else\n", ind(indent + 1)));
      emit_body(else_, ctx, w, indent + 2);
      w.push_str(&format!("{})\n", ind(indent + 1)));
      w.push_str(&format!("{})\n", ind(indent)));
    }
    ExprKind::LetFn { cont, .. } => {
      // LetFn inside a fn body shouldn't appear post-lifting.
      if let Cont::Expr { body, .. } = cont {
        emit_body(body, ctx, w, indent);
      }
    }
  }
}

fn emit_app(
  func: &Callable<'_>,
  args: &[Arg<'_>],
  ctx: &Ctx<'_, '_>,
  w: &mut MappedWriter,
  indent: usize,
) {
  match func {
    Callable::BuiltIn(op) => emit_builtin(*op, args, ctx, w, indent),
    Callable::Val(val) => emit_call(val, args, ctx, w, indent),
  }
}

/// Emit a call to a user function or continuation.
fn emit_call(
  func_val: &Val<'_>,
  args: &[Arg<'_>],
  ctx: &Ctx<'_, '_>,
  w: &mut MappedWriter,
  indent: usize,
) {
  let (val_args, cont_arg) = split_args(args);
  let total_arity = val_args.len() + if cont_arg.is_some() { 1 } else { 0 };

  let callee = val_expr(func_val, ctx);

  let mut call_args: Vec<WatExpr> = val_args.iter().map(|a| val_arg_expr(a, ctx)).collect();
  if let Some(cont) = cont_arg {
    call_args.push(cont_expr(cont, ctx));
  }

  let expr = WatExpr::list(
    format!("return_call_ref $Fn{}", total_arity),
    std::iter::once(callee).chain(call_args).collect(),
  );
  w.push_str(&ind(indent));
  write_expr(&expr, w);
  w.push_str("\n");
}

/// Emit a builtin operation call.
///
/// Most builtins are CPS: emit `return_call $name` with all args (values + cont) flat.
///
/// `FnClosure` is a value-returning constructor: emit `call $closure_N` where N is the
/// number of captured values (args excluding cont). The cont is handled inline:
/// - `Cont::Expr { bind, body }` → `local.set $bind (call $closure_N ...)` + inline body
/// - `Cont::Ref(id)` → `return_call_ref $Fn1 (local.get $id) (call $closure_N ...)`
fn emit_builtin(
  op: BuiltIn,
  args: &[Arg<'_>],
  ctx: &Ctx<'_, '_>,
  w: &mut MappedWriter,
  indent: usize,
) {
  if op == BuiltIn::FnClosure {
    let (val_args, cont) = split_args(args);
    let n = val_args.len();
    let val_exprs: Vec<WatExpr> = val_args.iter().map(|a| val_arg_expr(a, ctx)).collect();
    let call_expr = WatExpr::list(format!("call $closure_{}", n), val_exprs);
    match cont {
      Some(Cont::Expr { args: bind_args, body }) => {
        if let Some(bind) = bind_args.first() {
          let set_expr = WatExpr::list(
            format!("local.set ${}", ctx.label(bind.id)),
            vec![call_expr],
          );
          w.push_str(&ind(indent));
          write_expr(&set_expr, w);
          w.push_str("\n");
        }
        emit_body(body, ctx, w, indent);
      }
      Some(Cont::Ref(id)) => {
        let expr = WatExpr::list(
          "return_call_ref $Fn1",
          vec![
            WatExpr::list("local.get", vec![WatExpr::atom(format!("${}", ctx.label(*id)))]),
            call_expr,
          ],
        );
        w.push_str(&ind(indent));
        write_expr(&expr, w);
        w.push_str("\n");
      }
      None => {
        w.push_str(&ind(indent));
        write_expr(&call_expr, w);
        w.push_str("\n");
      }
    }
    return;
  }

  let fn_name = builtin_name(op);
  let all_args: Vec<WatExpr> = args.iter().map(|a| match a {
    Arg::Val(v) => val_expr(v, ctx),
    Arg::Spread(v) => val_expr(v, ctx),
    Arg::Cont(Cont::Ref(id)) =>
      WatExpr::list("local.get", vec![WatExpr::atom(format!("${}", ctx.label(*id)))]),
    Arg::Cont(Cont::Expr { .. }) => WatExpr::atom("(;; inline-cont ;)"),
    Arg::Expr(_) => WatExpr::atom("(;; expr-as-arg ;)"),
  }).collect();
  let expr = WatExpr::list(format!("return_call ${}", fn_name), all_args);
  w.push_str(&ind(indent));
  write_expr(&expr, w);
  w.push_str("\n");
}

// ---------------------------------------------------------------------------
// Value emission — statement form and WatExpr inline form
// ---------------------------------------------------------------------------

/// Build a source-mapped WatExpr for a value.
fn val_expr(val: &Val<'_>, ctx: &Ctx<'_, '_>) -> WatExpr {
  let inner = match &val.kind {
    ValKind::Lit(lit) => lit_expr(lit),
    ValKind::Ref(Ref::Synth(id)) => {
      let get = if ctx.is_global(*id) { "global.get" } else { "local.get" };
      WatExpr::list(get, vec![WatExpr::atom(format!("${}", ctx.label(*id)))])
    }
    ValKind::Ref(Ref::Unresolved(id)) => WatExpr::atom(format!("(;; unresolved: v_{} ;)", id.0)),
    ValKind::ContRef(id) => WatExpr::list("local.get", vec![WatExpr::atom(format!("${}", ctx.label(*id)))]),
    ValKind::Panic => WatExpr::atom("unreachable"),
    ValKind::BuiltIn(_) => WatExpr::atom("(;; builtin-as-val not supported ;)"),
  };
  match ctx.ast_node(val.id) {
    Some(node) => WatExpr::marked(node.loc, inner),
    None => inner,
  }
}

fn lit_expr(lit: &Lit<'_>) -> WatExpr {
  match lit {
    Lit::Int(n) => WatExpr::list("struct.new $Num", vec![
      WatExpr::atom(format!("(f64.const {})", *n as f64)),
    ]),
    Lit::Float(f) | Lit::Decimal(f) => WatExpr::list("struct.new $Num", vec![
      WatExpr::atom(format!("(f64.const {})", f)),
    ]),
    Lit::Bool(b) => WatExpr::list("struct.new $Num", vec![
      WatExpr::atom(format!("(f64.const {})", if *b { 1.0_f64 } else { 0.0_f64 })),
    ]),
    Lit::Str(_) | Lit::Seq | Lit::Rec => WatExpr::atom("(ref.null $Any) ;; TODO"),
  }
}

/// Build a WatExpr for a call argument.
fn val_arg_expr(arg: &Arg<'_>, ctx: &Ctx<'_, '_>) -> WatExpr {
  match arg {
    Arg::Val(v) => val_expr(v, ctx),
    Arg::Spread(v) => val_expr(v, ctx),
    Arg::Cont(_) => WatExpr::atom("(;; cont-as-arg ;)"),
    Arg::Expr(_) => WatExpr::atom("(;; expr-as-arg ;)"),
  }
}

/// Build a WatExpr for a continuation reference argument.
fn cont_expr(cont: &Cont<'_>, ctx: &Ctx<'_, '_>) -> WatExpr {
  match cont {
    Cont::Ref(id) => {
      let get = if ctx.is_global(*id) { "global.get" } else { "local.get" };
      WatExpr::list(get, vec![WatExpr::atom(format!("${}", ctx.label(*id)))])
    }
    Cont::Expr { .. } => WatExpr::atom("(;; inline-cont-as-arg ;)"),
  }
}

// ---------------------------------------------------------------------------
// Indent helper
// ---------------------------------------------------------------------------

fn ind(level: usize) -> String {
  "  ".repeat(level)
}
