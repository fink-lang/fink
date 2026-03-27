// WAT text generator from lifted CPS IR.
//
// Produces a WAT module (s-expression text) from fully-lifted CPS IR.
// The output is a fragment — builtin functions are referenced by name but not
// defined. The module is suitable for snapshot testing and playground use.
//
// ## Calling convention
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

use std::collections::BTreeSet;

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
  let ctx = Ctx::new(&cps.origin, ast_index);
  let module = collect(&cps.root, &ctx);
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
  let ctx = Ctx::new(&cps.origin, ast_index);
  let module = collect(&cps.root, &ctx);
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
  let ctx = Ctx::new(&cps.origin, ast_index);
  let module = collect(&cps.root, &ctx);
  emit_module(&module, &ctx, &mut w);
  w.finish_with_content(source_name, source_content)
}

// ---------------------------------------------------------------------------
// Context — origin map + AST index for name/loc recovery
// ---------------------------------------------------------------------------

struct Ctx<'a, 'src> {
  origin: &'a PropGraph<CpsId, Option<AstId>>,
  ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
}

impl<'a, 'src> Ctx<'a, 'src> {
  fn new(
    origin: &'a PropGraph<CpsId, Option<AstId>>,
    ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
  ) -> Self {
    Self { origin, ast_index }
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
  /// WASM function label (e.g. "main_1", "add_0").
  label: String,
  /// CpsId of the LetFn name — used to source-map the (func ...) header.
  fn_id: CpsId,
  /// Parameter labels in order (all anyref). Last is the cont.
  params: Vec<String>,
  /// The fn body expression.
  body: &'a Expr<'src>,
  /// Whether this fn is exported under a user name.
  export_as: Option<String>,
  /// CpsId of the LetVal alias that names this export — used to source-map (export ...).
  export_bind_id: Option<CpsId>,
}

/// Module-level collected data.
struct Module<'a, 'src> {
  funcs: Vec<CollectedFn<'a, 'src>>,
  /// All function arities encountered (= param count). Used to emit type section.
  arities: BTreeSet<usize>,
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

  Module { funcs, arities }
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
          if let Arg::Val(v) = arg {
            if let ValKind::Ref(Ref::Synth(id)) = v.kind {
              let name = export_name(ctx, id);
              return Some((id, name));
            }
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
      let param_labels: Vec<String> = params.iter().map(|p| match p {
        Param::Name(b) => ctx.label(b.id),
        Param::Spread(b) => ctx.label(b.id),
      }).collect();
      arities.insert(param_labels.len());

      // Check if this fn is in the exports list.
      // The terminal App references the LetVal alias CpsId, but the LetFn name
      // is a different CpsId. We match on the label suffix. Instead, we track
      // the LetVal alias → LetFn via the cont chain below (see LetVal arm).
      let export_as = exports.iter()
        .find(|(id, _)| *id == name.id)
        .map(|(_, n)| n.clone());

      funcs.push(CollectedFn { label, fn_id: name.id, params: param_labels, body: fn_body, export_as, export_bind_id: None });

      // Continue down the cont chain.
      match cont {
        Cont::Expr { body, .. } => collect_chain(body, ctx, exports, funcs, arities),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::LetVal { name, val, cont } => {
      // A LetVal at the top level is typically a name alias for a preceding LetFn:
      //   ·main_1 = ·v_53 (Ref::Synth pointing at the LetFn name)
      // We use this to associate the export name with the right function.
      if let ValKind::Ref(Ref::Synth(fn_id)) = val.kind {
        // If name.id is in exports, annotate the fn whose name_id == fn_id.
        if let Some((_, export_name)) = exports.iter().find(|(id, _)| *id == name.id) {
          if let Some(cf) = funcs.iter_mut().find(|cf| cf.label == ctx.label(fn_id)) {
            cf.export_as = Some(export_name.clone());
            cf.export_bind_id = Some(name.id);
          }
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
  let params: String = func.params.iter()
    .map(|p| format!("(param ${} (ref $Any))", p))
    .collect::<Vec<_>>()
    .join(" ");
  ctx.mark(func.fn_id, w);
  w.push_str(&format!("  (func ${} (type $Fn{})", func.label, arity));
  if !params.is_empty() {
    w.push_str(" ");
    w.push_str(&params);
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
      // Mark the local.set at the LetVal source location.
      ctx.mark(expr.id, w);
      let set_expr = WatExpr::list(format!("local.set ${}", local), vec![val_expr(val, ctx)]);
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
      let mark_id = if is_cont_call {
        args.iter().find_map(|a| if let Arg::Val(v) = a { Some(v.id) } else { None })
          .unwrap_or(expr.id)
      } else {
        expr.id
      };
      ctx.mark(mark_id, w);
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

  let callee = match &func_val.kind {
    ValKind::ContRef(id) | ValKind::Ref(Ref::Synth(id)) =>
      WatExpr::list("local.get", vec![WatExpr::atom(format!("${}", ctx.label(*id)))]),
    _ => WatExpr::atom(format!("(;; unhandled callee: {:?} ;)", func_val.kind)),
  };

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
fn emit_builtin(
  op: BuiltIn,
  args: &[Arg<'_>],
  ctx: &Ctx<'_, '_>,
  w: &mut MappedWriter,
  indent: usize,
) {
  let (val_args, cont_arg) = split_args(args);
  let fn_name = builtin_name(op);
  let builtin_args: Vec<WatExpr> = val_args.iter().map(|a| val_arg_expr(a, ctx)).collect();
  let builtin_call = WatExpr::list(format!("call ${}", fn_name), builtin_args);

  match cont_arg {
    Some(Cont::Ref(id)) => {
      let expr = WatExpr::list(
        "return_call_ref $Fn1",
        vec![
          WatExpr::list("local.get", vec![WatExpr::atom(format!("${}", ctx.label(*id)))]),
          builtin_call,
        ],
      );
      w.push_str(&ind(indent));
      write_expr(&expr, w);
      w.push_str("\n");
    }
    Some(Cont::Expr { args: bind_args, body }) => {
      for bind in bind_args {
        let expr = WatExpr::list(
          format!("local.set ${}", ctx.label(bind.id)),
          vec![builtin_call],  // NOTE: moves builtin_call — only first bind gets it
        );
        w.push_str(&ind(indent));
        write_expr(&expr, w);
        w.push_str("\n");
        break; // single-result builtins only
      }
      emit_body(body, ctx, w, indent);
    }
    None => {
      w.push_str(&ind(indent));
      write_expr(&builtin_call, w);
      w.push_str("\n");
    }
  }
}

// ---------------------------------------------------------------------------
// Value emission — statement form and WatExpr inline form
// ---------------------------------------------------------------------------

fn emit_val(val: &Val<'_>, ctx: &Ctx<'_, '_>, w: &mut MappedWriter, indent: usize) {
  w.push_str(&ind(indent));
  write_expr(&val_expr(val, ctx), w);
  w.push_str("\n");
}

/// Build a source-mapped WatExpr for a value.
fn val_expr(val: &Val<'_>, ctx: &Ctx<'_, '_>) -> WatExpr {
  let inner = match &val.kind {
    ValKind::Lit(lit) => lit_expr(lit),
    ValKind::Ref(Ref::Synth(id)) => WatExpr::list("local.get", vec![WatExpr::atom(format!("${}", ctx.label(*id)))]),
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
    Cont::Ref(id) => WatExpr::list("local.get", vec![WatExpr::atom(format!("${}", ctx.label(*id)))]),
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
    BuiltIn::Add      => "builtin_add",
    BuiltIn::Sub      => "builtin_sub",
    BuiltIn::Mul      => "builtin_mul",
    BuiltIn::Div      => "builtin_div",
    BuiltIn::IntDiv   => "builtin_int_div",
    BuiltIn::Mod      => "builtin_mod",
    BuiltIn::IntMod   => "builtin_int_mod",
    BuiltIn::DivMod   => "builtin_div_mod",
    BuiltIn::Pow      => "builtin_pow",
    BuiltIn::Eq       => "builtin_eq",
    BuiltIn::Neq      => "builtin_neq",
    BuiltIn::Lt       => "builtin_lt",
    BuiltIn::Lte      => "builtin_lte",
    BuiltIn::Gt       => "builtin_gt",
    BuiltIn::Gte      => "builtin_gte",
    BuiltIn::Cmp      => "builtin_cmp",
    BuiltIn::And      => "builtin_and",
    BuiltIn::Or       => "builtin_or",
    BuiltIn::Xor      => "builtin_xor",
    BuiltIn::Not      => "builtin_not",
    BuiltIn::BitAnd   => "builtin_bit_and",
    BuiltIn::BitXor   => "builtin_bit_xor",
    BuiltIn::Shl      => "builtin_shl",
    BuiltIn::Shr      => "builtin_shr",
    BuiltIn::RotL     => "builtin_rot_l",
    BuiltIn::RotR     => "builtin_rot_r",
    BuiltIn::BitNot   => "builtin_bit_not",
    BuiltIn::Range    => "builtin_range",
    BuiltIn::RangeIncl => "builtin_range_incl",
    BuiltIn::In       => "builtin_in",
    BuiltIn::NotIn    => "builtin_not_in",
    BuiltIn::Get      => "builtin_get",
    BuiltIn::SeqAppend  => "builtin_seq_append",
    BuiltIn::SeqConcat  => "builtin_seq_concat",
    BuiltIn::RecPut     => "builtin_rec_put",
    BuiltIn::RecMerge   => "builtin_rec_merge",
    BuiltIn::StrFmt     => "builtin_str_fmt",
    BuiltIn::FnClosure  => "builtin_fn_closure",
    BuiltIn::MatchValue    => "builtin_match_value",
    BuiltIn::MatchSeq      => "builtin_match_seq",
    BuiltIn::MatchNext     => "builtin_match_next",
    BuiltIn::MatchDone     => "builtin_match_done",
    BuiltIn::MatchNotDone  => "builtin_match_not_done",
    BuiltIn::MatchRest     => "builtin_match_rest",
    BuiltIn::MatchRec      => "builtin_match_rec",
    BuiltIn::MatchField    => "builtin_match_field",
    BuiltIn::MatchIf       => "builtin_match_if",
    BuiltIn::MatchApp      => "builtin_match_app",
    BuiltIn::MatchBlock    => "builtin_match_block",
    BuiltIn::MatchArm      => "builtin_match_arm",
    BuiltIn::Yield         => "builtin_yield",
    BuiltIn::Export        => "export",
  }
}

// ---------------------------------------------------------------------------
// Indent helper
// ---------------------------------------------------------------------------

fn ind(level: usize) -> String {
  "  ".repeat(level)
}
