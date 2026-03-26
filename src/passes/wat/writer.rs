// WAT text generator from lifted CPS IR.
//
// Produces a WAT module (s-expression text) from fully-lifted CPS IR.
// The output is a fragment — builtin functions are referenced by name but not
// defined. The module is suitable for snapshot testing and playground use.
//
// ## Calling convention
//
// Every Fink function: (param anyref * N) where the last param is the cont.
// The cont is always anyref; callers cast to the appropriate func type at the
// call site. All tail calls use return_call_ref with the matching $FnN type.
//
// ## Type layout
//
// $Num   — (struct (field f64))        — boxed number (i32/f64 unified as f64)
// $FnN   — (func (param anyref * N))   — function type of arity N (N >= 1)
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
// Collect pass
// ---------------------------------------------------------------------------

/// A lifted function, ready to emit.
struct CollectedFn<'a, 'src> {
  /// WASM function label (e.g. "main_1", "add_0").
  label: String,
  /// Parameter labels in order (all anyref). Last is the cont.
  params: Vec<String>,
  /// The fn body expression.
  body: &'a Expr<'src>,
  /// Whether this fn is exported under a user name.
  export_as: Option<String>,
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
      ExprKind::App { func: Callable::Val(val), args } => {
        // Terminal: ·ƒ_0 ·foo_0, ·bar_1
        // Each arg is a Ref to a named fn. The export name is the source name
        // without the _N suffix (i.e. the Ident string from the origin map).
        if !matches!(val.kind, ValKind::ContRef(_)) { return vec![]; }
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

      funcs.push(CollectedFn { label, params: param_labels, body: fn_body, export_as });

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
  for func in &module.funcs {
    emit_func(func, ctx, w);
  }
  emit_exports(module, w);
  w.push_str(")\n");
}

/// Emit the type section: $Num struct, then $FnN for each arity.
fn emit_types(module: &Module<'_, '_>, w: &mut MappedWriter) {
  w.push_str("  (type $Num (struct (field f64)))\n");
  for arity in &module.arities {
    let params: String = (0..*arity).map(|_| "anyref").collect::<Vec<_>>().join(" ");
    if params.is_empty() {
      w.push_str(&format!("  (type $Fn{} (func))\n", arity));
    } else {
      w.push_str(&format!("  (type $Fn{} (func (param {})))\n", arity, params));
    }
  }
}

fn emit_exports(module: &Module<'_, '_>, w: &mut MappedWriter) {
  for func in &module.funcs {
    if let Some(name) = &func.export_as {
      w.push_str(&format!("  (export {:?} (func ${}))\n", name, func.label));
    }
  }
}

fn emit_func(func: &CollectedFn<'_, '_>, ctx: &Ctx<'_, '_>, w: &mut MappedWriter) {
  // Function signature.
  let params: String = func.params.iter()
    .map(|p| format!("(param ${} anyref)", p))
    .collect::<Vec<_>>()
    .join(" ");
  w.push_str(&format!("  (func ${}", func.label));
  if !params.is_empty() {
    w.push_str(" ");
    w.push_str(&params);
  }
  w.push_str("\n");

  // Pre-scan locals (LetVal bindings inside the body).
  let locals = collect_locals(func.body);
  for local in &locals {
    w.push_str(&format!("    (local ${} anyref)\n", local));
  }

  // Body.
  emit_body(func.body, ctx, w, 2);

  w.push_str("  )\n");
}

// ---------------------------------------------------------------------------
// Local collection — pre-scan fn body for LetVal names
// ---------------------------------------------------------------------------

fn collect_locals(expr: &Expr<'_>) -> Vec<String> {
  let mut locals = Vec::new();
  collect_locals_expr(expr, &mut locals);
  locals
}

fn collect_locals_expr(expr: &Expr<'_>, out: &mut Vec<String>) {
  match &expr.kind {
    ExprKind::LetVal { name, cont, .. } => {
      out.push(format!("v_{}", name.id.0));
      match cont {
        Cont::Expr { body, .. } => collect_locals_expr(body, out),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::LetFn { cont, .. } => {
      // LetFn inside a fn body shouldn't appear after lifting, but handle gracefully.
      match cont {
        Cont::Expr { body, .. } => collect_locals_expr(body, out),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::If { then, else_, .. } => {
      collect_locals_expr(then, out);
      collect_locals_expr(else_, out);
    }
    ExprKind::App { .. } => {}
  }
}

// ---------------------------------------------------------------------------
// Body emission
// ---------------------------------------------------------------------------

fn emit_body(expr: &Expr<'_>, ctx: &Ctx<'_, '_>, w: &mut MappedWriter, indent: usize) {
  ctx.mark(expr.id, w);
  match &expr.kind {
    ExprKind::LetVal { name, val, cont } => {
      // Evaluate val onto the stack, store in local, then continue.
      emit_val(val, ctx, w, indent);
      w.push_str(&format!("{}(local.set $v_{})\n", ind(indent), name.id.0));
      match cont {
        Cont::Expr { body, .. } => emit_body(body, ctx, w, indent),
        Cont::Ref(id) => {
          // Tail: pass the local to the cont ref.
          w.push_str(&format!("{}(local.get $v_{})\n", ind(indent), name.id.0));
          emit_cont_ref_call(*id, 1, ctx, w, indent);
        }
      }
    }
    ExprKind::App { func, args } => {
      emit_app(func, args, ctx, w, indent);
    }
    ExprKind::If { cond, then, else_ } => {
      emit_val(cond, ctx, w, indent);
      // Condition is anyref (a boxed number). Unbox to f64, convert to i32 for if.
      w.push_str(&format!("{}(struct.get $Num 0 (ref.cast (ref $Num)))\n", ind(indent)));
      w.push_str(&format!("{}(f64.ne (f64.const 0))\n", ind(indent)));
      w.push_str(&format!("{}(if\n", ind(indent)));
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
  // Separate value args from the trailing cont arg.
  let (val_args, cont_arg) = split_args(args);
  let total_arity = val_args.len() + if cont_arg.is_some() { 1 } else { 0 };

  match &func_val.kind {
    ValKind::ContRef(id) => {
      // Direct tail call to a cont param — terminal App at module level.
      // Push all args then call via return_call_ref.
      for arg in val_args {
        emit_arg(arg, ctx, w, indent);
      }
      if let Some(cont) = cont_arg {
        emit_cont_as_arg(cont, ctx, w, indent);
      }
      emit_cont_ref_call(*id, total_arity, ctx, w, indent);
    }
    ValKind::Ref(Ref::Synth(id)) => {
      // Call through a local/param — push callee ref, push args, return_call_ref.
      w.push_str(&format!("{}(local.get ${})\n", ind(indent), ctx.label(*id)));
      for arg in val_args {
        emit_arg(arg, ctx, w, indent);
      }
      if let Some(cont) = cont_arg {
        emit_cont_as_arg(cont, ctx, w, indent);
      }
      w.push_str(&format!("{}(return_call_ref $Fn{})\n", ind(indent), total_arity));
    }
    _ => {
      // Fallback — emit as a comment with the raw debug repr.
      w.push_str(&format!("{}(;; unhandled call: {:?} ;)\n", ind(indent), func_val.kind));
    }
  }
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
  // Push value args, call builtin, tail-call cont with result.
  for arg in val_args {
    emit_arg(arg, ctx, w, indent);
  }
  w.push_str(&format!("{}(call ${})\n", ind(indent), fn_name));
  // The builtin returns one anyref result. Pass it to the cont.
  if let Some(cont) = cont_arg {
    emit_cont_call_with_result(cont, ctx, w, indent);
  }
}

// ---------------------------------------------------------------------------
// Value emission
// ---------------------------------------------------------------------------

fn emit_val(val: &Val<'_>, ctx: &Ctx<'_, '_>, w: &mut MappedWriter, indent: usize) {
  ctx.mark(val.id, w);
  match &val.kind {
    ValKind::Lit(lit) => emit_lit(lit, w, indent),
    ValKind::Ref(Ref::Synth(id)) => {
      w.push_str(&format!("{}(local.get ${})\n", ind(indent), ctx.label(*id)));
    }
    ValKind::Ref(Ref::Unresolved(id)) => {
      w.push_str(&format!("{}(;; unresolved ref: v_{} ;)\n", ind(indent), id.0));
      w.push_str(&format!("{}(ref.null none)\n", ind(indent)));
    }
    ValKind::ContRef(id) => {
      // A cont used as a value — load from the cont param local.
      w.push_str(&format!("{}(local.get ${})\n", ind(indent), ctx.label(*id)));
    }
    ValKind::Panic => {
      w.push_str(&format!("{}(unreachable)\n", ind(indent)));
    }
    ValKind::BuiltIn(_) => {
      // BuiltIn used as a value (e.g. MatchIf func arg) — not representable as anyref yet.
      w.push_str(&format!("{}(;; builtin-as-val not supported yet ;)\n", ind(indent)));
      w.push_str(&format!("{}(ref.null none)\n", ind(indent)));
    }
  }
}

fn emit_lit(lit: &Lit<'_>, w: &mut MappedWriter, indent: usize) {
  match lit {
    Lit::Int(n) => {
      // Box as $Num(f64).
      w.push_str(&format!("{}(struct.new $Num (f64.const {}))\n", ind(indent), *n as f64));
    }
    Lit::Float(f) | Lit::Decimal(f) => {
      w.push_str(&format!("{}(struct.new $Num (f64.const {}))\n", ind(indent), f));
    }
    Lit::Bool(b) => {
      let v = if *b { 1.0_f64 } else { 0.0_f64 };
      w.push_str(&format!("{}(struct.new $Num (f64.const {}))\n", ind(indent), v));
    }
    Lit::Str(_) | Lit::Seq | Lit::Rec => {
      // Not yet implemented — emit null placeholder.
      w.push_str(&format!("{}(ref.null none) ;; TODO: {:?}\n", ind(indent), lit));
    }
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

fn emit_arg(arg: &Arg<'_>, ctx: &Ctx<'_, '_>, w: &mut MappedWriter, indent: usize) {
  match arg {
    Arg::Val(v) => emit_val(v, ctx, w, indent),
    Arg::Spread(v) => {
      // Spread not yet representable — emit the value.
      emit_val(v, ctx, w, indent);
    }
    Arg::Cont(_) => {} // handled separately as trailing cont
    Arg::Expr(e) => emit_body(e, ctx, w, indent),
  }
}

/// Emit a continuation as a value argument (push the local onto the stack).
fn emit_cont_as_arg(cont: &Cont<'_>, ctx: &Ctx<'_, '_>, w: &mut MappedWriter, indent: usize) {
  match cont {
    Cont::Ref(id) => {
      w.push_str(&format!("{}(local.get ${})\n", ind(indent), ctx.label(*id)));
    }
    Cont::Expr { .. } => {
      // Inline conts can't be passed as a first-class value — shouldn't appear here.
      w.push_str(&format!("{}(;; inline cont as arg — not supported ;)\n", ind(indent)));
      w.push_str(&format!("{}(ref.null none)\n", ind(indent)));
    }
  }
}

/// Emit a tail call to a cont ref, assuming the result value is already on the stack.
fn emit_cont_call_with_result(
  cont: &Cont<'_>,
  ctx: &Ctx<'_, '_>,
  w: &mut MappedWriter,
  indent: usize,
) {
  match cont {
    Cont::Ref(id) => {
      w.push_str(&format!("{}(local.get ${})\n", ind(indent), ctx.label(*id)));
      w.push_str(&format!("{}(return_call_ref $Fn1)\n", ind(indent)));
    }
    Cont::Expr { args, body } => {
      // Bind result to the cont arg local(s), then fall through to body.
      for bind in args {
        w.push_str(&format!("{}(local.set $v_{})\n", ind(indent), bind.id.0));
      }
      emit_body(body, ctx, w, indent);
    }
  }
}

/// Emit a tail call to a ContRef — callee is loaded from its param label.
fn emit_cont_ref_call(id: CpsId, arity: usize, ctx: &Ctx<'_, '_>, w: &mut MappedWriter, indent: usize) {
  w.push_str(&format!("{}(local.get ${})\n", ind(indent), ctx.label(id)));
  w.push_str(&format!("{}(return_call_ref $Fn{})\n", ind(indent), arity));
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
  }
}

// ---------------------------------------------------------------------------
// Indent helper
// ---------------------------------------------------------------------------

fn ind(level: usize) -> String {
  "  ".repeat(level)
}
