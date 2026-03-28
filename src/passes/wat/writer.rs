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

use std::collections::{BTreeSet, HashSet};

use crate::ast::{AstId, Node as AstNode, NodeKind};
use crate::passes::cps::ir::{
  Arg, BuiltIn, Callable, Cont, CpsId, CpsResult, Expr, ExprKind,
  Lit, Param, Ref, Val, ValKind,
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
  let module = collect(&cps.root, &Ctx::new(&cps.origin, ast_index));
  let ctx = Ctx::new(&cps.origin, ast_index).with_globals(module.globals.clone());
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
  let module = collect(&cps.root, &Ctx::new(&cps.origin, ast_index));
  let ctx = Ctx::new(&cps.origin, ast_index).with_globals(module.globals.clone());
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
  let module = collect(&cps.root, &Ctx::new(&cps.origin, ast_index));
  let ctx = Ctx::new(&cps.origin, ast_index).with_globals(module.globals.clone());
  emit_module(&module, &ctx, &mut w);
  w.finish_with_content(source_name, source_content)
}

// ---------------------------------------------------------------------------
// Context — origin map + AST index for name/loc recovery
// ---------------------------------------------------------------------------

struct Ctx<'a, 'src> {
  origin: &'a PropGraph<CpsId, Option<AstId>>,
  ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
  /// CpsIds that are module-level fn globals — rendered as global.get, not local.get.
  // TODO: eliminate this hashmap by checking at emit time whether an id is a param
  // or let-local of the current fn (same approach the flat formatter uses).
  globals: HashSet<CpsId>,
}

impl<'a, 'src> Ctx<'a, 'src> {
  fn new(
    origin: &'a PropGraph<CpsId, Option<AstId>>,
    ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
  ) -> Self {
    Self { origin, ast_index, globals: HashSet::new() }
  }

  fn with_globals(mut self, globals: HashSet<CpsId>) -> Self {
    self.globals = globals;
    self
  }

  fn is_global(&self, id: CpsId) -> bool {
    self.globals.contains(&id)
  }

  fn ast_node(&self, id: CpsId) -> Option<&'src AstNode<'src>> {
    self.origin.try_get(id)
      .and_then(|opt| *opt)
      .and_then(|ast_id| self.ast_index.try_get(ast_id))
      .and_then(|opt| *opt)
  }

  /// Mark the output position with the source location of a CPS node.
  fn mark(&self, id: CpsId, w: &mut MappedWriter) {
    if let Some(node) = self.ast_node(id) {
      w.mark(node.loc);
    }
  }

  /// Recover the source name for a CPS bind/ref, or fall back to a synthetic label.
  fn label(&self, id: CpsId) -> String {
    match self.ast_node(id) {
      Some(node) => match &node.kind {
        NodeKind::Ident(s) => format!("{}_{}", s, id.0),
        NodeKind::SynthIdent(n) => format!("$_{}_{}", n, id.0),
        _ => format!("v_{}", id.0),
      },
      None => format!("v_{}", id.0),
    }
  }

  /// For a pipe-desugared App, return the `|` separator token loc for this call stage.
  /// The App's origin points to the Pipe node; the func val's origin identifies which stage.
  fn pipe_sep_loc(&self, expr_id: CpsId, func_val: &Val<'_>) -> Option<Loc> {
    let node = self.ast_node(expr_id)?;
    let NodeKind::Pipe(exprs) = &node.kind else { return None };
    let func_ast_id = self.origin.try_get(func_val.id).and_then(|o| *o)?;
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
// Collect pass
// ---------------------------------------------------------------------------

/// A lifted function, ready to emit.
struct CollectedFn<'a, 'src> {
  /// WASM function label (e.g. "v_8").
  label: String,
  /// CpsId of the LetFn name — used to source-map the (func ...) header.
  fn_id: CpsId,
  /// Parameter (id, label) pairs in order (all anyref). Last is the cont.
  params: Vec<(CpsId, String)>,
  /// The fn body expression.
  body: &'a Expr<'src>,
  /// Whether this fn is exported under a user name.
  export_as: Option<String>,
  /// CpsId of the LetVal alias that names this export — used to source-map (export ...).
  export_bind_id: Option<CpsId>,
  /// LetVal alias for this fn (e.g. "add_0"), emitted as a global before (func ...).
  /// Set for all top-level LetVal aliases, not just exports.
  alias: Option<(CpsId, String)>,
}

/// Module-level collected data.
struct Module<'a, 'src> {
  funcs: Vec<CollectedFn<'a, 'src>>,
  /// All function arities encountered (= param count). Used to emit type section.
  arities: BTreeSet<usize>,
  /// CpsIds of LetVal aliases for module-level fns — these are globals, not locals.
  globals: HashSet<CpsId>,
}

/// Walk the top-level chain and collect all lifted functions + the export list.
fn collect<'a, 'src>(root: &'a Expr<'src>, ctx: &Ctx<'_, 'src>) -> Module<'a, 'src> {
  let mut funcs: Vec<CollectedFn<'a, 'src>> = Vec::new();
  let mut arities: BTreeSet<usize> = BTreeSet::new();

  // The exports map: CpsId of a named fn → export name.
  // We collect this from the terminal App first (two-sub-pass), but since we
  // walk top-to-bottom we build it lazily: the terminal App fills a map, then
  // after the walk we annotate funcs. Simpler: collect exports from the terminal
  // in a first scan, then do the main collect pass.
  let exports = collect_exports(root, ctx);

  collect_chain(root, ctx, &exports, &mut funcs, &mut arities);

  // Every module-level fn alias gets a global — consistent with the flat formatter's
  // `·add_0 = ·v_8 = fn ...` layout, and allows global.get at all call sites.
  let globals: HashSet<CpsId> = funcs.iter()
    .filter_map(|cf| cf.alias.as_ref().map(|(id, _)| *id))
    .collect();

  Module { funcs, arities, globals }
}

/// Scan the top-level chain for the terminal App and extract export pairs:
/// (CpsId-of-named-fn, export-name).
fn collect_exports<'src>(root: &Expr<'src>, ctx: &Ctx<'_, 'src>) -> Vec<(CpsId, String)> {
  let mut expr = root;
  loop {
    match &expr.kind {
      ExprKind::LetFn { cont, .. } | ExprKind::LetVal { cont, .. } => {
        match cont {
          Cont::Expr { body, .. } => { expr = body; }
          Cont::Ref(_) => return vec![],
        }
      }
      ExprKind::App { func: Callable::BuiltIn(BuiltIn::Export), args } => {
        // Terminal: ·export ·foo_0, ·bar_1
        // Each arg is a Ref to a named fn. The export name is the source name
        // without the _N suffix (i.e. the Ident string from the origin map).
        return args.iter().filter_map(|arg| {
          if let Arg::Val(v) = arg
            && let ValKind::Ref(Ref::Synth(id)) = v.kind {
              let name = export_name(ctx, id);
              return Some((id, name));
            }
          None
        }).collect();
      }
      _ => return vec![],
    }
  }
}

/// Extract the bare export name for a CpsId: the source Ident string, or "v_N".
fn export_name(ctx: &Ctx<'_, '_>, id: CpsId) -> String {
  match ctx.ast_node(id) {
    Some(node) => match &node.kind {
      NodeKind::Ident(s) => s.to_string(),
      _ => format!("v_{}", id.0),
    },
    None => format!("v_{}", id.0),
  }
}

/// Recursively walk the top-level chain and populate `funcs`.
fn collect_chain<'a, 'src>(
  expr: &'a Expr<'src>,
  ctx: &Ctx<'_, 'src>,
  exports: &[(CpsId, String)],
  funcs: &mut Vec<CollectedFn<'a, 'src>>,
  arities: &mut BTreeSet<usize>,
) {
  match &expr.kind {
    ExprKind::LetFn { name, params, fn_body, cont } => {
      let label = ctx.label(name.id);
      let param_labels: Vec<(CpsId, String)> = params.iter().map(|p| match p {
        Param::Name(b) => (b.id, ctx.label(b.id)),
        Param::Spread(b) => (b.id, ctx.label(b.id)),
      }).collect();
      arities.insert(param_labels.len());

      // Check if this fn is in the exports list.
      // The terminal App references the LetVal alias CpsId, but the LetFn name
      // is a different CpsId. We match on the label suffix. Instead, we track
      // the LetVal alias → LetFn via the cont chain below (see LetVal arm).
      let export_as = exports.iter()
        .find(|(id, _)| *id == name.id)
        .map(|(_, n)| n.clone());

      funcs.push(CollectedFn { label, fn_id: name.id, params: param_labels, body: fn_body, export_as, export_bind_id: None, alias: None });

      // Continue down the cont chain.
      match cont {
        Cont::Expr { body, .. } => collect_chain(body, ctx, exports, funcs, arities),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::LetVal { name, val, cont } => {
      // A LetVal at the top level is typically a name alias for a preceding LetFn:
      //   ·add_0 = ·v_8 (Ref::Synth pointing at the LetFn name)
      // Record the alias on the fn so we can emit a global before (func ...).
      if let ValKind::Ref(Ref::Synth(fn_id)) = val.kind
        && let Some(cf) = funcs.iter_mut().find(|cf| cf.label == ctx.label(fn_id))
      {
        cf.alias = Some((name.id, ctx.label(name.id)));
        if let Some((_, export_name)) = exports.iter().find(|(id, _)| *id == name.id) {
          cf.export_as = Some(export_name.clone());
          cf.export_bind_id = Some(name.id);
        }
      }
      match cont {
        Cont::Expr { body, .. } => collect_chain(body, ctx, exports, funcs, arities),
        Cont::Ref(_) => {}
      }
    }
    // Terminal App — nothing more to collect.
    ExprKind::App { .. } | ExprKind::If { .. } => {}
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
  let locals = collect_locals(func.body, ctx);
  for local in &locals {
    w.push_str(&format!("    (local ${} (ref $Any))\n", local));
  }

  // Body.
  emit_body(func.body, ctx, w, 2);

  w.push_str("  )\n");
}

// ---------------------------------------------------------------------------
// Local collection — pre-scan fn body for LetVal names
// ---------------------------------------------------------------------------

fn collect_locals<'src>(expr: &Expr<'_>, ctx: &Ctx<'_, 'src>) -> Vec<String> {
  let mut locals = Vec::new();
  collect_locals_expr(expr, ctx, &mut locals);
  locals
}

fn collect_locals_expr<'src>(expr: &Expr<'_>, ctx: &Ctx<'_, 'src>, out: &mut Vec<String>) {
  match &expr.kind {
    ExprKind::LetVal { name, cont, .. } => {
      out.push(ctx.label(name.id));
      match cont {
        Cont::Expr { body, .. } => collect_locals_expr(body, ctx, out),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::LetFn { cont, .. } => {
      match cont {
        Cont::Expr { body, .. } => collect_locals_expr(body, ctx, out),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::If { then, else_, .. } => {
      collect_locals_expr(then, ctx, out);
      collect_locals_expr(else_, ctx, out);
    }
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::FnClosure), args } => {
      // FnClosure's Cont::Expr bind produces a local variable.
      for arg in args {
        if let Arg::Cont(Cont::Expr { args: bind_args, body }) = arg {
          for bind in bind_args {
            out.push(ctx.label(bind.id));
          }
          collect_locals_expr(body, ctx, out);
        }
      }
    }
    ExprKind::App { .. } => {}
  }
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
// Argument helpers
// ---------------------------------------------------------------------------

/// Split args into (value_args, Option<trailing_cont>).
fn split_args<'a>(args: &'a [Arg<'a>]) -> (&'a [Arg<'a>], Option<&'a Cont<'a>>) {
  match args.last() {
    Some(Arg::Cont(c)) => (&args[..args.len() - 1], Some(c)),
    _ => (args, None),
  }
}

// ---------------------------------------------------------------------------
// BuiltIn name mapping
// ---------------------------------------------------------------------------

fn builtin_name(op: BuiltIn) -> &'static str {
  match op {
    BuiltIn::Add      => "op_plus",
    BuiltIn::Sub      => "op_minus",
    BuiltIn::Mul      => "op_mul",
    BuiltIn::Div      => "op_div",
    BuiltIn::IntDiv   => "op_intdiv",
    BuiltIn::Mod      => "op_rem",
    BuiltIn::IntMod   => "op_intmod",
    BuiltIn::DivMod   => "op_divmod",
    BuiltIn::Pow      => "op_pow",
    BuiltIn::Eq       => "op_eq",
    BuiltIn::Neq      => "op_neq",
    BuiltIn::Lt       => "op_lt",
    BuiltIn::Lte      => "op_lte",
    BuiltIn::Gt       => "op_gt",
    BuiltIn::Gte      => "op_gte",
    BuiltIn::Cmp      => "op_cmp",
    BuiltIn::And      => "op_and",
    BuiltIn::Or       => "op_or",
    BuiltIn::Xor      => "op_xor",
    BuiltIn::Not      => "op_not",
    BuiltIn::BitAnd   => "op_bitand",
    BuiltIn::BitXor   => "op_bitxor",
    BuiltIn::Shl      => "op_shl",
    BuiltIn::Shr      => "op_shr",
    BuiltIn::RotL     => "op_rotl",
    BuiltIn::RotR     => "op_rotr",
    BuiltIn::BitNot   => "op_bitnot",
    BuiltIn::Range    => "op_rngex",
    BuiltIn::RangeIncl => "op_rngin",
    BuiltIn::In       => "op_in",
    BuiltIn::NotIn    => "op_notin",
    BuiltIn::Get      => "op_dot",
    BuiltIn::SeqAppend  => "seq_append",
    BuiltIn::SeqConcat  => "seq_concat",
    BuiltIn::RecPut     => "rec_put",
    BuiltIn::RecMerge   => "rec_merge",
    BuiltIn::StrFmt     => "str_fmt",
    BuiltIn::FnClosure  => "closure",
    BuiltIn::MatchValue    => "match_value",
    BuiltIn::MatchSeq      => "match_seq",
    BuiltIn::MatchNext     => "match_next",
    BuiltIn::MatchDone     => "match_done",
    BuiltIn::MatchNotDone  => "match_not_done",
    BuiltIn::MatchRest     => "match_rest",
    BuiltIn::MatchRec      => "match_rec",
    BuiltIn::MatchField    => "match_field",
    BuiltIn::MatchIf       => "match_if",
    BuiltIn::MatchApp      => "match_app",
    BuiltIn::MatchBlock    => "match_block",
    BuiltIn::MatchArm      => "match_arm",
    BuiltIn::Yield         => "yield",
    BuiltIn::Export        => "export",
    BuiltIn::Import        => "import",
  }
}

// ---------------------------------------------------------------------------
// Indent helper
// ---------------------------------------------------------------------------

fn ind(level: usize) -> String {
  "  ".repeat(level)
}
