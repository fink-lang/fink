// Collect phase — walks lifted CPS IR and gathers module-level structure.
//
// Shared by the WASM binary emitter and the WAT text writer. This module
// contains only format-independent data structures and IR-walking logic;
// it has no dependency on text formatting or binary encoding.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::ast::{AstId, Node as AstNode, NodeKind};
use crate::passes::cps::ir::{
  Arg, BuiltIn, Callable, Cont, CpsFnKind, CpsId, Expr, ExprKind,
  Param, Ref, ValKind,
};
use crate::propgraph::PropGraph;

// ---------------------------------------------------------------------------
// Context — origin map + AST index for name/loc recovery
// ---------------------------------------------------------------------------

/// Shared IR context for name and location recovery from CPS nodes.
///
/// Both the WASM emitter and WAT writer build on this. WAT-specific
/// concerns (MappedWriter marks, pipe separator locs) live in the WAT
/// writer's own wrapper.
pub struct IrCtx<'a, 'src> {
  pub origin: &'a PropGraph<CpsId, Option<AstId>>,
  pub ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
  /// CpsIds that are module-level fn globals — rendered as global.get, not local.get.
  pub globals: HashSet<CpsId>,
}

impl<'a, 'src> IrCtx<'a, 'src> {
  pub fn new(
    origin: &'a PropGraph<CpsId, Option<AstId>>,
    ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
  ) -> Self {
    Self { origin, ast_index, globals: HashSet::new() }
  }

  pub fn with_globals(mut self, globals: HashSet<CpsId>) -> Self {
    self.globals = globals;
    self
  }

  pub fn is_global(&self, id: CpsId) -> bool {
    self.globals.contains(&id)
  }

  /// Recover the AST node for a CPS node via the origin map.
  pub fn ast_node(&self, id: CpsId) -> Option<&'src AstNode<'src>> {
    self.origin.try_get(id)
      .and_then(|opt| *opt)
      .and_then(|ast_id| self.ast_index.try_get(ast_id))
      .and_then(|opt| *opt)
  }

  /// Recover the source name for a CPS bind/ref, or fall back to a synthetic label.
  pub fn label(&self, id: CpsId) -> String {
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
// Collected data structures
// ---------------------------------------------------------------------------

/// A lifted function, ready to emit.
pub struct CollectedFn<'a> {
  /// WASM function label (e.g. "v_8").
  pub label: String,
  /// CpsId of the LetFn name — used to source-map the (func ...) header.
  pub fn_id: CpsId,
  /// Parameter (id, label, is_spread) triples in order (all anyref).
  /// After lifting: [cap0, cap1, ..., val0, val1, ..., cont?].
  pub params: Vec<(CpsId, String, bool)>,
  /// Number of leading params that are captures (from lifting).
  /// These are unpacked from the $Captures struct, not the args list.
  pub n_captures: usize,
  /// Whether the last param is a continuation (Bind::Cont).
  /// User functions have has_cont=true; continuations/match-arms have has_cont=false.
  pub has_cont: bool,
  /// The fn body expression.
  pub body: &'a Expr,
  /// Whether this fn is exported under a user name.
  pub export_as: Option<String>,
  /// CpsId of the LetVal alias that names this export — used to source-map (export ...).
  pub export_bind_id: Option<CpsId>,
  /// LetVal alias for this fn (e.g. "add_0"), emitted as a global before (func ...).
  /// Set for all top-level LetVal aliases, not just exports.
  pub alias: Option<(CpsId, String)>,
}

/// Module-level collected data.
pub struct Module<'a> {
  pub funcs: Vec<CollectedFn<'a>>,
  /// All function arities encountered (= param count). Used to emit type section.
  pub arities: BTreeSet<usize>,
  /// CpsIds of module-level bindings that are WASM globals, not locals.
  /// Includes fn aliases and value bindings visible to sibling functions.
  pub globals: HashSet<CpsId>,
  /// Labels of module-level value globals (non-fn-alias LetVals).
  /// These are `(mut (ref null any))` globals — plain values, no $Closure wrapping.
  pub value_globals: HashSet<String>,
  /// User exports: `(cps_id, source_name)` pairs from ·ƒpub.
  pub exports: Vec<(CpsId, String)>,
  /// Module-scope import declarations: url → [name, ...].
  /// Carried from CpsResult so the emitter can emit WASM global imports
  /// and reconstruct the imported rec without re-scanning lifted CPS.
  ///
  /// In multi-module builds, `compile_package` rewrites the keys from the
  /// raw consumer-relative URLs (as written in source) to canonical
  /// entry-relative URLs before handing the `Module` to the emitter. The
  /// emitter itself doesn't care which form the keys are in — as long as
  /// `url_rewrite` below maps any raw Lit::Str URL in the CPS back to the
  /// same form used as the key here.
  pub module_imports: std::collections::BTreeMap<String, Vec<String>>,
  /// Raw source URL → canonical URL mapping for module imports. Empty in
  /// single-module builds (the emitter falls back to identity lookup).
  /// Populated by `compile_package` from `canonicalise_url` so the
  /// `BuiltIn::Import` emit site can translate a CPS `Lit::Str` URL (raw,
  /// as written in source) to the canonical form used throughout the
  /// linked binary.
  pub url_rewrite: std::collections::BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// Collection logic
// ---------------------------------------------------------------------------

/// Walk the top-level chain and collect all lifted functions + the export list.
///
/// `module_locals` is the authoritative list of module-level binding leaves
/// from CPS lowering (includes destructure leaves). It's consulted when
/// deciding which CpsIds become WASM globals — the CPS spine walk alone
/// misses bindings that destructure lowering hoists into helper fn bodies.
pub fn collect<'a, 'src>(
  root: &'a Expr,
  ctx: &IrCtx<'_, 'src>,
  module_locals: &[(CpsId, String)],
  module_imports: std::collections::BTreeMap<String, Vec<String>>,
) -> Module<'a> {
  let mut funcs: Vec<CollectedFn<'a>> = Vec::new();
  let mut arities: BTreeSet<usize> = BTreeSet::new();

  let exports = collect_exports(root, ctx);

  // New root shape: App(FinkModule, [Cont::Expr { args: [ƒret], body }]).
  // The Cont::Expr IS the module body function. Its params (ƒret) and body
  // are the internal fink_module Fn2. Lifted fns inside the body become
  // sibling WASM functions at module scope.
  if let ExprKind::App { func: Callable::BuiltIn(BuiltIn::FinkModule), args } = &root.kind
    && let Some(Arg::Cont(Cont::Expr { args: cont_args, body })) = args.first() {
    let param_labels: Vec<(CpsId, String, bool)> = cont_args.iter().map(|b| {
      (b.id, ctx.label(b.id), false)
    }).collect();
    arities.insert(param_labels.len());
    funcs.push(CollectedFn {
      label: "fink_module".into(),
      fn_id: root.id,
      params: param_labels,
      n_captures: 0,
      has_cont: false,
      body,
      export_as: None,
      export_bind_id: None,
      alias: None,
    });
    // Walk the body for lifted LetFn/LetVal siblings.
    collect_chain(body, ctx, &exports, &mut funcs, &mut arities);
  }

  // Fill in n_captures for each function by scanning FnClosure call sites.
  let cap_counts = scan_fn_capture_counts(&funcs, ctx);
  for cf in &mut funcs {
    if let Some(&n) = cap_counts.get(&cf.label) {
      cf.n_captures = n;
    }
  }

  // has_cont is now set directly from CpsFnKind at collection time.
  // No call-site scanning needed — the CPS transform tags each LetFn.

  // Every module-level fn alias gets a global.
  let mut globals: HashSet<CpsId> = funcs.iter()
    .filter_map(|cf| cf.alias.as_ref().map(|(id, _)| *id))
    .collect();

  // Module-level value bindings (non-fn-alias LetVals and fink_module params)
  // become globals only if referenced by sibling functions. Collect all
  // candidates, then intersect with refs from sibling function bodies.
  let mut value_globals: HashSet<String> = HashSet::new();
  if let Some(fm) = funcs.first() {
    // Step 1: collect all module-chain binding CpsIds + fink_module params.
    let mut module_binds: HashMap<CpsId, String> = HashMap::new();
    // Include fink_module params (e.g. ·ƒret_N).
    for (id, label, _) in &fm.params {
      module_binds.insert(*id, label.clone());
    }
    scan_module_bindings(fm.body, ctx, &globals, &mut module_binds);

    // Step 2: collect all refs from sibling function bodies (skip fink_module itself).
    let mut sibling_refs: HashSet<CpsId> = HashSet::new();
    for f in funcs.iter().skip(1) {
      collect_all_refs(f.body, &mut sibling_refs);
    }

    // Step 3: intersect — only bindings referenced by siblings become globals.
    for (id, label) in &module_binds {
      if sibling_refs.contains(id) {
        value_globals.insert(label.clone());
        globals.insert(*id);
      }
    }

    // Step 4: module locals from CPS lowering — patches the destructure-case
    // gap. Destructure leaves (e.g. `x` from `{x} = ...`) live inside hoisted
    // matcher success-body fns that scan_module_bindings doesn't reach.
    //
    // Promotion criterion: the binding's LetVal lives in fn A, but is read
    // from a different fn B. Same-fn reads work fine as plain locals.
    //
    // We identify A by walking each sibling fn body for the binding's
    // defining LetVal, and check if any OTHER sibling fn refs the CpsId.
    let mut letval_owner: HashMap<CpsId, usize> = HashMap::new();
    for (fi, f) in funcs.iter().enumerate() {
      collect_letval_ids(f.body, &mut |id| { letval_owner.entry(id).or_insert(fi); });
    }
    for (id, _) in module_locals {
      if globals.contains(id) || module_binds.contains_key(id) { continue; }
      let Some(&owner_fi) = letval_owner.get(id) else { continue; };
      let cross_fn_ref = funcs.iter().enumerate()
        .filter(|(fi, _)| *fi != owner_fi)
        .any(|(_, f)| {
          let mut refs = HashSet::new();
          collect_fn_scoped_refs(f.body, &mut refs);
          refs.contains(id)
        });
      if cross_fn_ref {
        let label = ctx.label(*id);
        value_globals.insert(label.clone());
        globals.insert(*id);
      }
    }
  }

  Module {
    funcs,
    arities,
    globals,
    value_globals,
    exports,
    module_imports,
    url_rewrite: std::collections::BTreeMap::new(),
  }
}

/// Scan the top-level chain for the terminal App and extract export pairs.
///
/// The module-root spine is a mix of LetFn/LetVal (whose cont body holds the
/// rest of the chain) and Apps whose last Cont::Expr arg holds the rest of the
/// chain. This happens when a top-level statement's RHS lowers to runtime
/// operators (e.g. `[a,b] = [1,2]` lowers to `·seq_prepend 2, [], fn v_6: ...`).
/// We walk through all of these until we hit the terminal `·export` App.
fn collect_exports<'src>(root: &Expr, ctx: &IrCtx<'_, 'src>) -> Vec<(CpsId, String)> {
  // The `·export` App may live anywhere in the tree: in the top-level spine
  // for simple modules, or deep inside a lifted cont/closure body for modules
  // whose last statement bound a call result (causing the trailing cont chain
  // — including the terminal `·export` — to be lifted into a hoisted sibling
  // fn). We recursively search the whole IR tree for the first
  // `App(BuiltIn::Export, ...)` occurrence.
  //
  // After lifting, the Val args of `·export` reference capture-param CpsIds
  // (rewritten by the lifting pass) rather than the original LetVal names.
  // Those capture params have `origin` set to the original source ident node,
  // so `export_name` still returns the user-visible name (e.g. "add", "s").
  let mut out: Vec<(CpsId, String)> = Vec::new();
  find_export_app(root, ctx, &mut out);
  out
}

fn find_export_app<'src>(expr: &Expr, ctx: &IrCtx<'_, 'src>, out: &mut Vec<(CpsId, String)>) {
  match &expr.kind {
    // Legacy: terminal ·export with all names at once.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Export), args } => {
      for arg in args {
        if let Arg::Val(v) = arg
          && let ValKind::Ref(Ref::Synth(id)) = v.kind {
            let name = export_name(ctx, id);
            out.push((id, name));
          }
      }
    }
    // New: per-binding ·ƒpub with one val arg + cont.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Pub), args } => {
      for arg in args {
        if let Arg::Val(v) = arg
          && let ValKind::Ref(Ref::Synth(id)) = v.kind {
            let name = export_name(ctx, id);
            out.push((id, name));
          }
        // Recurse into the cont body (rest of module).
        if let Arg::Cont(Cont::Expr { body, .. }) = arg {
          find_export_app(body, ctx, out);
        }
      }
    }
    ExprKind::App { args, .. } => {
      for arg in args {
        match arg {
          Arg::Cont(Cont::Expr { body, .. }) => find_export_app(body, ctx, out),
          Arg::Expr(e) => find_export_app(e, ctx, out),
          _ => {}
        }
      }
    }
    ExprKind::LetFn { fn_body, cont, .. } => {
      find_export_app(fn_body, ctx, out);
      if let Cont::Expr { body, .. } = cont {
        find_export_app(body, ctx, out);
      }
    }
    ExprKind::LetVal { cont, .. } => {
      if let Cont::Expr { body, .. } = cont {
        find_export_app(body, ctx, out);
      }
    }
    ExprKind::If { then, else_, .. } => {
      find_export_app(then, ctx, out);
      find_export_app(else_, ctx, out);
    }
  }
}

/// Extract the bare export name for a CpsId: the source Ident string, or "v_N".
pub fn export_name(ctx: &IrCtx<'_, '_>, id: CpsId) -> String {
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
  expr: &'a Expr,
  ctx: &IrCtx<'_, 'src>,
  exports: &[(CpsId, String)],
  funcs: &mut Vec<CollectedFn<'a>>,
  arities: &mut BTreeSet<usize>,
) {
  match &expr.kind {
    ExprKind::LetFn { name, params, fn_kind, fn_body, cont } => {
      let label = ctx.label(name.id);
      let param_labels: Vec<(CpsId, String, bool)> = params.iter().map(|p| match p {
        Param::Name(b) => (b.id, ctx.label(b.id), false),
        Param::Spread(b) => (b.id, ctx.label(b.id), true),
      }).collect();
      // CpsFnKind tells us directly: CpsFunction is called with Arg::Cont.
      let has_cont = *fn_kind == CpsFnKind::CpsFunction;
      arities.insert(param_labels.len());

      let export_as = exports.iter()
        .find(|(id, _)| *id == name.id)
        .map(|(_, n)| n.clone());

      funcs.push(CollectedFn { label, fn_id: name.id, params: param_labels, n_captures: 0, has_cont, body: fn_body, export_as, export_bind_id: None, alias: None });

      // Descend into the cont spine (regular case).
      match cont {
        Cont::Expr { body, .. } => collect_chain(body, ctx, exports, funcs, arities),
        Cont::Ref(_) => {}
      }

    }
    ExprKind::LetVal { name, val, cont } => {
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
    // Module-root Apps (e.g. `·seq_prepend 2, [], fn v_6: <rest>`) hold the
    // rest of the module in their last Cont::Expr arg. Descend so we keep
    // finding LetFn/LetVal siblings that live inside the cont spine.
    ExprKind::App { args, .. } => {
      if let Some(Arg::Cont(Cont::Expr { body, .. })) = args.last() {
        collect_chain(body, ctx, exports, funcs, arities);
      }
    }
    ExprKind::If { .. } => {}
  }
}

/// Walk the module-level chain and collect all non-fn-alias binding CpsIds
/// with their labels. These are candidates for value globals — the caller
/// intersects with sibling function refs to determine which actually need
/// to be promoted.
fn scan_module_bindings<'src>(
  expr: &Expr,
  ctx: &IrCtx<'_, 'src>,
  fn_alias_globals: &HashSet<CpsId>,
  out: &mut HashMap<CpsId, String>,
) {
  match &expr.kind {
    ExprKind::LetFn { cont, .. } => {
      if let Cont::Expr { body, .. } = cont {
        scan_module_bindings(body, ctx, fn_alias_globals, out);
      }
    }
    ExprKind::LetVal { name, cont, .. } => {
      if !fn_alias_globals.contains(&name.id) {
        out.insert(name.id, ctx.label(name.id));
      }
      if let Cont::Expr { args, body } = cont {
        for a in args {
          if !fn_alias_globals.contains(&a.id) {
            out.insert(a.id, ctx.label(a.id));
          }
        }
        scan_module_bindings(body, ctx, fn_alias_globals, out);
      }
    }
    ExprKind::App { args, .. } => {
      for arg in args {
        if let Arg::Cont(Cont::Expr { args: bind_args, body }) = arg {
          for a in bind_args {
            if !fn_alias_globals.contains(&a.id) {
              out.insert(a.id, ctx.label(a.id));
            }
          }
          scan_module_bindings(body, ctx, fn_alias_globals, out);
        }
      }
    }
    ExprKind::If { .. } => {}
  }
}

/// Visit each LetVal binding CpsId in an expression tree. Recurses into
/// cont bodies but NOT into LetFn fn_bodies — each fn's locals are its own.
fn collect_letval_ids(expr: &Expr, visit: &mut impl FnMut(CpsId)) {
  match &expr.kind {
    ExprKind::LetVal { name, cont, .. } => {
      visit(name.id);
      if let Cont::Expr { body, .. } = cont {
        collect_letval_ids(body, visit);
      }
    }
    ExprKind::LetFn { cont, .. } => {
      if let Cont::Expr { body, .. } = cont {
        collect_letval_ids(body, visit);
      }
    }
    ExprKind::App { args, .. } => {
      for arg in args {
        match arg {
          Arg::Cont(Cont::Expr { args: bind_args, body }) => {
            for b in bind_args {
              visit(b.id);
            }
            collect_letval_ids(body, visit);
          }
          Arg::Expr(e) => collect_letval_ids(e, visit),
          _ => {}
        }
      }
    }
    ExprKind::If { then, else_, .. } => {
      collect_letval_ids(then, visit);
      collect_letval_ids(else_, visit);
    }
  }
}

/// Collect Ref::Synth CpsIds referenced in an expression tree, scoped to a
/// single fn: does NOT recurse into nested LetFn fn_bodies. Use this when
/// you want the refs that belong to *this* fn's body only, not transitive
/// refs from lifted siblings that haven't been physically detached yet.
fn collect_fn_scoped_refs(expr: &Expr, out: &mut HashSet<CpsId>) {
  match &expr.kind {
    ExprKind::LetVal { val, cont, .. } => {
      collect_val_refs(val, out);
      if let Cont::Expr { body, .. } = cont {
        collect_fn_scoped_refs(body, out);
      } else if let Cont::Ref(id) = cont {
        out.insert(*id);
      }
    }
    ExprKind::LetFn { cont, .. } => {
      // Skip fn_body — that's another fn's scope.
      if let Cont::Expr { body, .. } = cont {
        collect_fn_scoped_refs(body, out);
      } else if let Cont::Ref(id) = cont {
        out.insert(*id);
      }
    }
    ExprKind::App { func, args } => {
      if let Callable::Val(v) = func {
        collect_val_refs(v, out);
      }
      for arg in args {
        match arg {
          Arg::Val(v) | Arg::Spread(v) => collect_val_refs(v, out),
          Arg::Cont(Cont::Expr { body, .. }) => collect_fn_scoped_refs(body, out),
          Arg::Cont(Cont::Ref(id)) => { out.insert(*id); }
          Arg::Expr(e) => collect_fn_scoped_refs(e, out),
        }
      }
    }
    ExprKind::If { cond, then, else_ } => {
      collect_val_refs(cond, out);
      collect_fn_scoped_refs(then, out);
      collect_fn_scoped_refs(else_, out);
    }
  }
}

/// Collect all Ref::Synth CpsIds referenced in an expression tree.
fn collect_all_refs(expr: &Expr, out: &mut HashSet<CpsId>) {
  match &expr.kind {
    ExprKind::LetVal { val, cont, .. } => {
      collect_val_refs(val, out);
      collect_cont_refs(cont, out);
    }
    ExprKind::LetFn { fn_body, cont, .. } => {
      collect_all_refs(fn_body, out);
      collect_cont_refs(cont, out);
    }
    ExprKind::App { func, args } => {
      if let Callable::Val(v) = func {
        collect_val_refs(v, out);
      }
      for arg in args {
        match arg {
          Arg::Val(v) | Arg::Spread(v) => collect_val_refs(v, out),
          Arg::Cont(c) => collect_cont_refs(c, out),
          Arg::Expr(e) => collect_all_refs(e, out),
        }
      }
    }
    ExprKind::If { cond, then, else_ } => {
      collect_val_refs(cond, out);
      collect_all_refs(then, out);
      collect_all_refs(else_, out);
    }
  }
}

fn collect_val_refs(val: &crate::passes::cps::ir::Val, out: &mut HashSet<CpsId>) {
  if let ValKind::Ref(Ref::Synth(id)) = val.kind {
    out.insert(id);
  }
  if let ValKind::ContRef(id) = val.kind {
    out.insert(id);
  }
}

fn collect_cont_refs(cont: &Cont, out: &mut HashSet<CpsId>) {
  match cont {
    Cont::Ref(id) => { out.insert(*id); }
    Cont::Expr { body, .. } => collect_all_refs(body, out),
  }
}

// ---------------------------------------------------------------------------
// Local collection — pre-scan fn body for LetVal names
// ---------------------------------------------------------------------------

pub fn collect_locals<'src>(expr: &Expr, ctx: &IrCtx<'_, 'src>) -> Vec<String> {
  let mut locals = Vec::new();
  collect_locals_expr(expr, ctx, &mut locals);
  locals
}

fn collect_locals_expr<'src>(expr: &Expr, ctx: &IrCtx<'_, 'src>, out: &mut Vec<String>) {
  match &expr.kind {
    ExprKind::LetVal { name, cont, .. } => {
      out.push(ctx.label(name.id));
      match cont {
        Cont::Expr { args, body } => {
          for a in args {
            out.push(ctx.label(a.id));
          }
          collect_locals_expr(body, ctx, out);
        }
        Cont::Ref(_) => {}
      }
    }
    ExprKind::LetFn { cont, .. } => {
      match cont {
        Cont::Expr { args, body } => {
          for a in args {
            out.push(ctx.label(a.id));
          }
          collect_locals_expr(body, ctx, out);
        }
        Cont::Ref(_) => {}
      }
    }
    ExprKind::If { then, else_, .. } => {
      collect_locals_expr(then, ctx, out);
      collect_locals_expr(else_, ctx, out);
    }
    ExprKind::App { func, args } => {
      let walk_conts = matches!(func,
        Callable::BuiltIn(BuiltIn::FnClosure)
        | Callable::BuiltIn(BuiltIn::Pub)
        | Callable::BuiltIn(BuiltIn::FinkModule));
      if walk_conts {
        for arg in args {
          if let Arg::Cont(Cont::Expr { args: bind_args, body }) = arg {
            for bind in bind_args {
              out.push(ctx.label(bind.id));
            }
            collect_locals_expr(body, ctx, out);
          }
        }
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Capture count scanning
// ---------------------------------------------------------------------------

/// Scan all function bodies for FnClosure calls and record how many captures
/// each target function has. Returns a map: fn_label → n_captures.
fn scan_fn_capture_counts<'a, 'src>(
  funcs: &[CollectedFn<'a>],
  ctx: &IrCtx<'_, 'src>,
) -> HashMap<String, usize> {
  let mut counts: HashMap<String, usize> = HashMap::new();
  for func in funcs {
    scan_fn_closures_in_expr(func.body, ctx, &mut counts);
  }
  counts
}

fn scan_fn_closures_in_expr<'src>(
  expr: &Expr,
  ctx: &IrCtx<'_, 'src>,
  counts: &mut HashMap<String, usize>,
) {
  match &expr.kind {
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::FnClosure), args } => {
      // ·closure fn_ref, cap0, cap1, ...
      // First val arg is the funcref, rest are captures.
      let (val_args, _) = split_args(args);
      if let Some(Arg::Val(v)) = val_args.first()
        && let ValKind::Ref(Ref::Synth(id)) = v.kind
      {
        let label = ctx.label(id);
        let n_captures = val_args.len().saturating_sub(1);
        counts.insert(label, n_captures);
      }
      // Recurse into cont args.
      for arg in args {
        if let Arg::Cont(Cont::Expr { body, .. }) = arg {
          scan_fn_closures_in_expr(body, ctx, counts);
        }
      }
    }
    ExprKind::App { args, .. } => {
      for arg in args {
        if let Arg::Cont(Cont::Expr { body, .. }) = arg {
          scan_fn_closures_in_expr(body, ctx, counts);
        }
      }
    }
    ExprKind::LetVal { cont, .. } | ExprKind::LetFn { cont, .. } => {
      match cont {
        Cont::Expr { body, .. } => scan_fn_closures_in_expr(body, ctx, counts),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::If { then, else_, .. } => {
      scan_fn_closures_in_expr(then, ctx, counts);
      scan_fn_closures_in_expr(else_, ctx, counts);
    }
  }
  if let ExprKind::LetFn { fn_body, .. } = &expr.kind {
    scan_fn_closures_in_expr(fn_body, ctx, counts);
  }
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

/// Split args into (value_args, Option<trailing_cont>).
pub fn split_args(args: &[Arg]) -> (&[Arg], Option<&Cont>) {
  match args.last() {
    Some(Arg::Cont(c)) => (&args[..args.len() - 1], Some(c)),
    _ => (args, None),
  }
}

// ---------------------------------------------------------------------------
// BuiltIn name mapping
// ---------------------------------------------------------------------------

pub fn builtin_name(op: BuiltIn) -> &'static str {
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
    BuiltIn::Shl      => "op_shl",
    BuiltIn::Shr      => "op_shr",
    BuiltIn::RotL     => "op_rotl",
    BuiltIn::RotR     => "op_rotr",
    BuiltIn::Range    => "op_rngex",
    BuiltIn::RangeIncl => "op_rngin",
    BuiltIn::In       => "op_in",
    BuiltIn::NotIn    => "op_notin",
    BuiltIn::Get      => "op_dot",
    BuiltIn::SeqPrepend  => "seq_prepend",
    BuiltIn::SeqConcat   => "seq_concat",
    BuiltIn::RecPut     => "rec_set",
    BuiltIn::RecMerge   => "rec_merge",
    BuiltIn::StrFmt     => "str_fmt",
    BuiltIn::FnClosure  => "closure",
    BuiltIn::IsSeqLike     => "is_seq_like",
    BuiltIn::IsRecLike     => "is_rec_like",
    BuiltIn::SeqPop        => "seq_pop",
    BuiltIn::RecPop        => "rec_pop",
    BuiltIn::Empty         => "op_empty",
    BuiltIn::StrMatch      => "str_match",
    BuiltIn::Yield         => "yield",
    BuiltIn::Spawn         => "spawn",
    BuiltIn::Await         => "await",
    BuiltIn::Channel       => "channel",
    BuiltIn::Receive       => "receive",
    BuiltIn::Read          => "op_read",
    BuiltIn::Export        => "export",
    // BuiltIn::Import is a compile-time marker, not a runtime call. It must
    // be handled by the emitter (erased, rewritten to a cross-module WASM
    // import) before reaching builtin_name. Reaching here is a bug.
    BuiltIn::Import        => unreachable!(
      "BuiltIn::Import is a compile-time marker and must not reach the \
       runtime-builtin name table; see wasm-link multi-module pass"
    ),
    BuiltIn::FinkModule    => "fink_module",
    BuiltIn::Pub           => "pub",
    BuiltIn::Panic         => "panic",
  }
}
