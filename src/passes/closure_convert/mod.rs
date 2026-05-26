//! Closure-conversion pass.
//!
//! Walks the CPS IR once and computes captures for every fn (LetFn and
//! LetRecDefn::Fn), producing an IR where:
//! - Every fn has its captures as Cap-typed params at the front of its
//!   params list (matching the existing `ParamInfo::Cap` machinery that
//!   lower.rs already understands).
//! - Every closure construction site is explicit, with the right captures
//!   in source-order.
//! - LetRec defns: siblings appear as captures (so cross-references resolve
//!   uniformly via the existing $Captures array unpacking in lower_fn).
//!
//! Design intent per `.brain/.scratch/closure-convert-design.md`. Currently
//! replaces lifting for LetRec-containing programs; non-rec programs go
//! through the existing lifting pass unchanged for now (until step 5 of the
//! migration switches the default pipeline).
//!
//! ## How this differs from lifting
//!
//! Lifting was designed for nested LetFns that close over their immediate
//! parent fn's params: "extract the nested LetFn, give it Cap params for
//! the parent's params it uses". This model doesn't fit LetRec groups,
//! where defns are mutually scoped — a sibling ref isn't from the parent.
//! Bolt-on patches in lifting (sibling stacks, top-of-body extraction)
//! eventually produce inconsistent closure shapes.
//!
//! Closure-convert tracks the FULL enclosing-scope chain (every binding
//! visible at the fn's definition site) rather than just the immediate
//! parent's params. Captures = refs in body that resolve to outer-scope
//! binds. For LetRec defns, siblings are part of the enclosing scope —
//! handled uniformly.

use super::cps::ir::CpsResult;
use crate::ast::Ast;

/// Run the closure-conversion pass. Currently a no-op pending the migration
/// steps in the design doc. Each step below adds incrementally:
///
/// 1. [DONE] Scaffold the pass
/// 2. [TODO] Free-vars query with full enclosing-scope context
/// 3. [TODO] Rewrite LetFn bodies with Cap params + ref rewriting
/// 4. [TODO] Handle LetRec groups (siblings as captures, cross-patch info)
/// 5. [TODO] Switch default pipeline (replace lift's call to lifting::lift)
/// 6. [TODO] Delete lifting/
pub fn convert<'src>(cps: CpsResult, _ast: &Ast<'src>) -> CpsResult {
  // Drive the IR walk from the root with an initial scope of module-level
  // CpsIds (module-local binds + pre-allocated scope binds).
  //
  // NOTE: this rewrites fn DEFINITIONS to have Cap params + rewritten body
  // refs. It does NOT yet emit FnClosure construction at use sites. Without
  // those, codegen would build no-capture closures and the cap-param reads
  // would be undefined at runtime. End-to-end pipeline switch requires
  // closure-construction emission, which is the next step.
  let CpsResult {
    root, origin, bind_to_cps, synth_alias, param_info, module_locals, module_imports,
  } = cps;

  // Pre-collect LetFn names so we can exclude them from capture analysis.
  // LetFn names resolve statically via fn_syms in codegen; threading them
  // through closure captures creates spurious caps.
  let mut letfn_names: std::collections::HashSet<super::cps::ir::CpsId>
    = std::collections::HashSet::new();
  collect_letfn_names(&root, &mut letfn_names);

  // Bootstrap state from current PropGraphs.
  let mut state = ConvertState {
    origin,
    synth_alias,
    param_info,
    letfn_names,
  };

  // Initial scope: all module-local CpsIds.
  let initial_scope: std::collections::HashSet<super::cps::ir::CpsId> =
    module_locals.iter().map(|(id, _)| *id).collect();
  let mut scope = initial_scope;

  let new_root = convert_expr(&mut state, &mut scope, root);

  CpsResult {
    root: new_root,
    origin: state.origin,
    bind_to_cps,
    synth_alias: state.synth_alias,
    param_info: state.param_info,
    module_locals,
    module_imports,
  }
}

// ---------------------------------------------------------------------------
// Free-vars query
// ---------------------------------------------------------------------------

/// Walk an Expr and collect every CpsId that appears as a Ref / ContRef.
/// Caller filters against bound-in-body to compute the free-var set.
///
/// This is the building block for capture analysis: a fn's captures are
/// the intersection of its body's free vars with the set of names visible
/// at the fn's definition site (its enclosing-scope chain).
pub fn collect_refs(expr: &super::cps::ir::Expr, out: &mut std::collections::HashSet<super::cps::ir::CpsId>) {
  use super::cps::ir::{Arg, Callable, ExprKind, LetRecDefn};
  match &expr.kind {
    ExprKind::LetVal { val, cont, .. } => {
      collect_refs_val(val, out);
      collect_refs_cont(cont, out);
    }
    ExprKind::LetFn { fn_body, cont, .. } => {
      collect_refs(fn_body, out);
      collect_refs_cont(cont, out);
    }
    ExprKind::App { func, args } => {
      if let Callable::Val(v) = func { collect_refs_val(v, out); }
      for a in args {
        match a {
          Arg::Val(v) | Arg::Spread(v) => collect_refs_val(v, out),
          Arg::Cont(c) => collect_refs_cont(c, out),
          Arg::Expr(e) => collect_refs(e, out),
        }
      }
    }
    ExprKind::If { cond, then, else_ } => {
      collect_refs_val(cond, out);
      collect_refs(then, out);
      collect_refs(else_, out);
    }
    ExprKind::LetRec { group, cont, .. } => {
      for d in group {
        match d {
          LetRecDefn::Fn { body, .. } => collect_refs(body, out),
          LetRecDefn::Val { val, .. } => collect_refs_val(val, out),
        }
      }
      collect_refs_cont(cont, out);
    }
  }
}

fn collect_refs_val(val: &super::cps::ir::Val, out: &mut std::collections::HashSet<super::cps::ir::CpsId>) {
  use super::cps::ir::{Ref, ValKind};
  match &val.kind {
    ValKind::Ref(Ref::Synth(id)) | ValKind::ContRef(id) => { out.insert(*id); }
    _ => {}
  }
}

fn collect_refs_cont(cont: &super::cps::ir::Cont, out: &mut std::collections::HashSet<super::cps::ir::CpsId>) {
  use super::cps::ir::Cont;
  match cont {
    Cont::Ref(id) => { out.insert(*id); }
    Cont::Expr { body, .. } => collect_refs(body, out),
  }
}

/// Walk an Expr and collect every CpsId that appears as a binding site
/// (LetVal/LetFn names, params, Cont::Expr args, LetRec defn names).
/// Used to subtract from collect_refs to compute the free-var set local
/// to the given Expr.
pub fn collect_binds(expr: &super::cps::ir::Expr, out: &mut std::collections::HashSet<super::cps::ir::CpsId>) {
  use super::cps::ir::{Arg, ExprKind, LetRecDefn, Param};
  match &expr.kind {
    ExprKind::LetVal { name, cont, .. } => {
      out.insert(name.id);
      collect_binds_cont(cont, out);
    }
    ExprKind::LetFn { name, params, fn_body, cont, .. } => {
      out.insert(name.id);
      for p in params {
        let b = match p { Param::Name(b) | Param::Spread(b) => b };
        out.insert(b.id);
      }
      collect_binds(fn_body, out);
      collect_binds_cont(cont, out);
    }
    ExprKind::App { args, .. } => {
      for a in args {
        match a {
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
    ExprKind::LetRec { group, cont, .. } => {
      for d in group {
        match d {
          LetRecDefn::Fn { name, params, body, .. } => {
            out.insert(name.id);
            for p in params {
              let b = match p { Param::Name(b) | Param::Spread(b) => b };
              out.insert(b.id);
            }
            collect_binds(body, out);
          }
          LetRecDefn::Val { name, .. } => { out.insert(name.id); }
        }
      }
      collect_binds_cont(cont, out);
    }
  }
}

fn collect_binds_cont(cont: &super::cps::ir::Cont, out: &mut std::collections::HashSet<super::cps::ir::CpsId>) {
  use super::cps::ir::Cont;
  if let Cont::Expr { args, body } = cont {
    for a in args { out.insert(a.id); }
    collect_binds(body, out);
  }
}

/// Compute the free-var set of an Expr: refs that are not defined
/// (bound) inside the Expr itself. Caller intersects with "in-scope at
/// this site" to get the actual capture set.
pub fn free_vars(expr: &super::cps::ir::Expr) -> std::collections::HashSet<super::cps::ir::CpsId> {
  let mut refs = std::collections::HashSet::new();
  collect_refs(expr, &mut refs);
  let mut binds = std::collections::HashSet::new();
  collect_binds(expr, &mut binds);
  refs.difference(&binds).copied().collect()
}

// ---------------------------------------------------------------------------
// Fn discovery + capture computation
// ---------------------------------------------------------------------------

/// One discovered fn definition during a closure-convert walk.
///
/// Records the fn's identity, what it captures from outer scope, and the
/// set of "in-scope at definition site" CpsIds for diagnostics. The
/// transform step uses these to add Cap params and rewrite body refs.
#[derive(Debug, Clone)]
pub struct FnDiscovery {
  /// The fn's name BindId.
  pub name_id: super::cps::ir::CpsId,
  /// CpsIds the fn body refs but doesn't bind itself, that ARE visible at
  /// the fn's definition site (the closure capture set).
  pub captures: Vec<super::cps::ir::CpsId>,
  /// Whether the fn is a LetRecDefn (vs. a plain LetFn).
  pub in_letrec: bool,
}

/// Walk an Expr top-down and discover every fn definition (LetFn and
/// LetRecDefn::Fn), computing its capture set against the enclosing-scope
/// chain. Outermost-first order.
///
/// `initial_scope` is the set of CpsIds visible at the root of the Expr
/// (module-level binds, top-level params, etc.).
pub fn discover_fns(
  expr: &super::cps::ir::Expr,
  initial_scope: &std::collections::HashSet<super::cps::ir::CpsId>,
) -> Vec<FnDiscovery> {
  let mut out = Vec::new();
  let mut scope = initial_scope.clone();
  walk_for_discovery(expr, &mut scope, &mut out);
  out
}

fn walk_for_discovery(
  expr: &super::cps::ir::Expr,
  scope: &mut std::collections::HashSet<super::cps::ir::CpsId>,
  out: &mut Vec<FnDiscovery>,
) {
  use super::cps::ir::{Arg, Callable, Cont, ExprKind, LetRecDefn, Param};
  match &expr.kind {
    ExprKind::LetVal { name, cont, .. } => {
      // val itself is in-scope for the cont body.
      let added = scope.insert(name.id);
      walk_cont_for_discovery(cont, scope, out);
      if added { scope.remove(&name.id); }
    }
    ExprKind::LetFn { name, params, fn_body, cont, .. } => {
      // Compute the fn's captures: free vars in body that are in scope at
      // this site. The fn's own name + params are NOT in scope-from-above;
      // they're bound by the fn itself.
      let body_free = free_vars(fn_body);
      let captures: Vec<super::cps::ir::CpsId> = body_free.iter()
        .filter(|id| scope.contains(id))
        .copied()
        .collect();
      out.push(FnDiscovery {
        name_id: name.id,
        captures,
        in_letrec: false,
      });
      // Descend into the body with the body's own scope (params).
      let mut body_scope = scope.clone();
      for p in params {
        let b = match p { Param::Name(b) | Param::Spread(b) => b };
        body_scope.insert(b.id);
      }
      walk_for_discovery(fn_body, &mut body_scope, out);
      // The fn's name is in scope for the cont.
      let added = scope.insert(name.id);
      walk_cont_for_discovery(cont, scope, out);
      if added { scope.remove(&name.id); }
    }
    ExprKind::App { func, args } => {
      let _ = func;  // App callee doesn't bind anything
      for a in args {
        match a {
          Arg::Cont(c) => walk_cont_for_discovery(c, scope, out),
          Arg::Expr(e) => walk_for_discovery(e, scope, out),
          _ => {}
        }
      }
    }
    ExprKind::If { then, else_, .. } => {
      walk_for_discovery(then, scope, out);
      walk_for_discovery(else_, scope, out);
    }
    ExprKind::LetRec { group, cont, .. } => {
      // All sibling names are mutually in scope of all defn bodies + the cont.
      let mut sibling_ids: Vec<super::cps::ir::CpsId> = Vec::new();
      for d in group {
        match d {
          LetRecDefn::Fn { name, .. } => sibling_ids.push(name.id),
          LetRecDefn::Val { name, .. } => sibling_ids.push(name.id),
        }
      }
      for id in &sibling_ids { scope.insert(*id); }
      for d in group {
        match d {
          LetRecDefn::Fn { name, params, body, .. } => {
            let body_free = free_vars(body);
            // Captures: free vars in scope at LetRec site (the parent scope
            // BEFORE we added siblings — siblings are siblings, not "outer").
            // But siblings count too: a defn body's ref to a sibling resolves
            // to the sibling's value, which lives outside the defn but inside
            // the LetRec.
            let captures: Vec<super::cps::ir::CpsId> = body_free.iter()
              .filter(|id| scope.contains(id))
              .copied()
              .collect();
            out.push(FnDiscovery {
              name_id: name.id,
              captures,
              in_letrec: true,
            });
            // Descend into body with body's params in scope.
            let mut body_scope = scope.clone();
            for p in params {
              let b = match p { Param::Name(b) | Param::Spread(b) => b };
              body_scope.insert(b.id);
            }
            walk_for_discovery(body, &mut body_scope, out);
          }
          LetRecDefn::Val { .. } => { /* nothing to discover */ }
        }
      }
      walk_cont_for_discovery(cont, scope, out);
      for id in &sibling_ids { scope.remove(id); }
    }
  }
  let _ = Callable::BuiltIn;  // silence unused-variant warning for the type import
  let _ = Cont::Ref;
}

fn walk_cont_for_discovery(
  cont: &super::cps::ir::Cont,
  scope: &mut std::collections::HashSet<super::cps::ir::CpsId>,
  out: &mut Vec<FnDiscovery>,
) {
  use super::cps::ir::Cont;
  if let Cont::Expr { args, body } = cont {
    let mut added: Vec<super::cps::ir::CpsId> = Vec::with_capacity(args.len());
    for a in args {
      if scope.insert(a.id) { added.push(a.id); }
    }
    walk_for_discovery(body, scope, out);
    for id in added { scope.remove(&id); }
  }
}

// ---------------------------------------------------------------------------
// LetFn name collection
// ---------------------------------------------------------------------------

/// Walk an Expr tree and collect every LetFn / LetRecDefn::Fn name CpsId.
/// These names are RESOLVED STATICALLY by lower.rs's fn_syms machinery —
/// refs to them must not become closure captures.
pub fn collect_letfn_names(
  expr: &super::cps::ir::Expr,
  out: &mut std::collections::HashSet<super::cps::ir::CpsId>,
) {
  use super::cps::ir::{Arg, Cont, ExprKind, LetRecDefn};
  match &expr.kind {
    ExprKind::LetVal { cont, .. } => {
      if let Cont::Expr { body, .. } = cont { collect_letfn_names(body, out); }
    }
    ExprKind::LetFn { name, fn_body, cont, .. } => {
      out.insert(name.id);
      collect_letfn_names(fn_body, out);
      if let Cont::Expr { body, .. } = cont { collect_letfn_names(body, out); }
    }
    ExprKind::App { args, .. } => {
      for a in args {
        match a {
          Arg::Cont(Cont::Expr { body, .. }) => collect_letfn_names(body, out),
          Arg::Expr(e) => collect_letfn_names(e, out),
          _ => {}
        }
      }
    }
    ExprKind::If { then, else_, .. } => {
      collect_letfn_names(then, out);
      collect_letfn_names(else_, out);
    }
    ExprKind::LetRec { group, cont, .. } => {
      for d in group {
        if let LetRecDefn::Fn { name, body, .. } = d {
          out.insert(name.id);
          collect_letfn_names(body, out);
        }
      }
      if let Cont::Expr { body, .. } = cont { collect_letfn_names(body, out); }
    }
  }
}

// ---------------------------------------------------------------------------
// Mutable state for the conversion pass
// ---------------------------------------------------------------------------

/// State the conversion pass mutates: PropGraph indexed by CpsId, plus
/// fresh-id allocation. Mirrors lifting's `Alloc` but only carries what
/// closure_convert needs.
pub struct ConvertState {
  /// Origin map — CpsId -> Option<AstId>. New CpsIds (for fresh cap params)
  /// get pushed here with the captured bind's origin.
  pub origin: crate::propgraph::PropGraph<super::cps::ir::CpsId, Option<crate::ast::AstId>>,
  /// Synth alias map — CpsId -> Option<CpsId>. For each fresh cap param,
  /// records the captured-bind id so name_res can resolve refs.
  pub synth_alias: crate::propgraph::PropGraph<super::cps::ir::CpsId, Option<super::cps::ir::CpsId>>,
  /// Param info map — CpsId -> Option<ParamInfo>. New cap params get
  /// ParamInfo::Cap(orig_id).
  pub param_info: crate::propgraph::PropGraph<super::cps::ir::CpsId, Option<super::cps::ir::ParamInfo>>,
  /// LetFn names reachable anywhere in the input tree. Refs to these
  /// must NOT become closure captures — the lower pass resolves LetFn
  /// names statically via fn_syms / fn refs. Without this exclusion,
  /// nested fns ref'ing a sibling LetFn name (lifted/hoisted) would
  /// get spurious cap params.
  pub letfn_names: std::collections::HashSet<super::cps::ir::CpsId>,
}

impl ConvertState {
  pub fn from_result(result: &super::cps::ir::CpsResult) -> Self {
    let mut letfn_names: std::collections::HashSet<super::cps::ir::CpsId>
      = std::collections::HashSet::new();
    collect_letfn_names(&result.root, &mut letfn_names);
    Self {
      origin: result.origin.clone(),
      synth_alias: result.synth_alias.clone(),
      param_info: result.param_info.clone(),
      letfn_names,
    }
  }

  /// Allocate a fresh CpsId with the given AstId origin.
  fn next(&mut self, ast_origin: Option<crate::ast::AstId>) -> super::cps::ir::CpsId {
    self.origin.push(ast_origin)
  }

  /// Allocate a fresh BindNode with the given Bind kind and origin.
  fn bind(&mut self, kind: super::cps::ir::Bind, ast_origin: Option<crate::ast::AstId>) -> super::cps::ir::BindNode {
    let id = self.next(ast_origin);
    super::cps::ir::BindNode { id, kind }
  }

  /// Record param info for a cap-tagged param.
  fn tag_cap(&mut self, param_id: super::cps::ir::CpsId, orig_cap_id: super::cps::ir::CpsId) {
    use super::cps::ir::ParamInfo;
    self.param_info.set(param_id, Some(ParamInfo::Cap(orig_cap_id)));
  }

  /// Record synth_alias for a cap param so name_res can resolve refs to
  /// the original CpsId via the new param.
  fn record_alias(&mut self, new_param_id: super::cps::ir::CpsId, orig_cap_id: super::cps::ir::CpsId) {
    let idx: usize = new_param_id.into();
    while self.synth_alias.len() <= idx { self.synth_alias.push(None); }
    self.synth_alias.set(new_param_id, Some(orig_cap_id));
  }
}

// ---------------------------------------------------------------------------
// Ref rewriting
// ---------------------------------------------------------------------------

/// Rewrite every `Ref::Synth(old_id)` and `ContRef(old_id)` in `expr`
/// using `map: old_id -> new_id`. Unmapped ids are left unchanged.
/// Recurses through the full Expr tree.
///
/// Used after closure conversion to redirect body refs from the original
/// outer-scope CpsId to the new fresh Cap-param CpsId.
pub fn rewrite_refs(
  expr: super::cps::ir::Expr,
  map: &std::collections::HashMap<super::cps::ir::CpsId, super::cps::ir::CpsId>,
) -> super::cps::ir::Expr {
  use super::cps::ir::{Arg, Callable, Expr, ExprKind, LetRecDefn};
  let Expr { id, kind } = expr;
  let new_kind = match kind {
    ExprKind::LetVal { name, val, cont } => {
      let val = rewrite_refs_val(*val, map);
      let cont = rewrite_refs_cont(cont, map);
      ExprKind::LetVal { name, val: Box::new(val), cont }
    }
    ExprKind::LetFn { name, params, fn_kind, fn_body, cont } => {
      let fn_body = rewrite_refs(*fn_body, map);
      let cont = rewrite_refs_cont(cont, map);
      ExprKind::LetFn { name, params, fn_kind, fn_body: Box::new(fn_body), cont }
    }
    ExprKind::App { func, args } => {
      let func = match func {
        Callable::Val(v) => Callable::Val(rewrite_refs_val(v, map)),
        other => other,
      };
      let args = args.into_iter().map(|a| match a {
        Arg::Val(v) => Arg::Val(rewrite_refs_val(v, map)),
        Arg::Spread(v) => Arg::Spread(rewrite_refs_val(v, map)),
        Arg::Cont(c) => Arg::Cont(rewrite_refs_cont(c, map)),
        Arg::Expr(e) => Arg::Expr(Box::new(rewrite_refs(*e, map))),
      }).collect();
      ExprKind::App { func, args }
    }
    ExprKind::If { cond, then, else_ } => {
      let cond = rewrite_refs_val(*cond, map);
      let then = rewrite_refs(*then, map);
      let else_ = rewrite_refs(*else_, map);
      ExprKind::If {
        cond: Box::new(cond),
        then: Box::new(then),
        else_: Box::new(else_),
      }
    }
    ExprKind::LetRec { group, no_self_edge, cont } => {
      let cont = rewrite_refs_cont(cont, map);
      let group = group.into_iter().map(|d| match d {
        LetRecDefn::Fn { name, params, fn_kind, body } => {
          let body = rewrite_refs(*body, map);
          LetRecDefn::Fn { name, params, fn_kind, body: Box::new(body) }
        }
        LetRecDefn::Val { name, val } => {
          let val = rewrite_refs_val(*val, map);
          LetRecDefn::Val { name, val: Box::new(val) }
        }
      }).collect();
      ExprKind::LetRec { group, no_self_edge, cont }
    }
  };
  Expr { id, kind: new_kind }
}

fn rewrite_refs_val(
  val: super::cps::ir::Val,
  map: &std::collections::HashMap<super::cps::ir::CpsId, super::cps::ir::CpsId>,
) -> super::cps::ir::Val {
  use super::cps::ir::{Ref, Val, ValKind};
  let Val { id, kind } = val;
  let new_kind = match kind {
    ValKind::Ref(Ref::Synth(old)) => {
      let new_id = map.get(&old).copied().unwrap_or(old);
      ValKind::Ref(Ref::Synth(new_id))
    }
    ValKind::ContRef(old) => {
      let new_id = map.get(&old).copied().unwrap_or(old);
      ValKind::ContRef(new_id)
    }
    other => other,
  };
  Val { id, kind: new_kind }
}

fn rewrite_refs_cont(
  cont: super::cps::ir::Cont,
  map: &std::collections::HashMap<super::cps::ir::CpsId, super::cps::ir::CpsId>,
) -> super::cps::ir::Cont {
  use super::cps::ir::Cont;
  match cont {
    Cont::Ref(old) => {
      let new_id = map.get(&old).copied().unwrap_or(old);
      Cont::Ref(new_id)
    }
    Cont::Expr { args, body } => {
      let body = rewrite_refs(*body, map);
      Cont::Expr { args, body: Box::new(body) }
    }
  }
}

// ---------------------------------------------------------------------------
// Fn rewriting — add Cap params, rewrite body refs
// ---------------------------------------------------------------------------

/// Add Cap-tagged params for the given capture CpsIds to a fn, rewriting
/// body refs to use the new params. Returns (new_params, new_body, cap_param_ids).
///
/// Side effects on `state`:
/// - Allocates fresh CpsIds for each cap param
/// - Records `synth_alias[new_param_id] = orig_cap_id` so name_res can
///   resolve refs in the body that escaped the rewrite
/// - Tags `param_info[new_param_id] = ParamInfo::Cap(orig_cap_id)`
pub fn rewrite_fn_for_captures(
  state: &mut ConvertState,
  captures: &[super::cps::ir::CpsId],
  original_params: Vec<super::cps::ir::Param>,
  body: super::cps::ir::Expr,
) -> (Vec<super::cps::ir::Param>, super::cps::ir::Expr, Vec<super::cps::ir::CpsId>) {
  use super::cps::ir::{Bind, Param};
  let mut cap_param_ids: Vec<super::cps::ir::CpsId> = Vec::with_capacity(captures.len());
  let mut new_params: Vec<Param> = Vec::with_capacity(captures.len() + original_params.len());
  let mut rewrite_map: std::collections::HashMap<super::cps::ir::CpsId, super::cps::ir::CpsId>
    = std::collections::HashMap::new();
  for &cap_id in captures {
    // Determine the captured bind's AstId origin (best-effort).
    let ast_origin = state.origin.try_get(cap_id).and_then(|o| *o);
    let new_bind = state.bind(Bind::Synth, ast_origin);
    cap_param_ids.push(new_bind.id);
    state.tag_cap(new_bind.id, cap_id);
    state.record_alias(new_bind.id, cap_id);
    rewrite_map.insert(cap_id, new_bind.id);
    new_params.push(Param::Name(new_bind));
  }
  // Append original params after the cap params.
  new_params.extend(original_params);
  // Rewrite the body using the map.
  let new_body = rewrite_refs(body, &rewrite_map);
  (new_params, new_body, cap_param_ids)
}

// ---------------------------------------------------------------------------
// Top-level driver
// ---------------------------------------------------------------------------

/// Walk the CPS tree top-down, computing captures and rewriting every fn
/// (LetFn and LetRecDefn::Fn) that has any captures from outer scope.
///
/// `initial_scope` is the set of CpsIds visible at the root of `expr`
/// (typically empty at module root, or the module's exports for fragments).
///
/// Output: an Expr with:
/// - Every capturing fn rewritten: Cap-tagged params prepended, body refs
///   rewritten to use the new cap-param ids.
/// - Non-capturing fns unchanged.
/// - LetRec wrappers preserved.
/// - state.synth_alias / state.param_info updated accordingly.
///
/// NOT YET DONE: FnClosure construction sites at the LetFn cont chain.
/// Today's lifting produces those via extract_from_body. closure_convert
/// will emit them in a follow-up step. Without that emission, the resulting
/// IR has cap params on fn defs but no closure construction at use sites
/// — codegen would build no-capture closures and the body's cap-param
/// reads would be undefined. End-to-end pipeline switch is a separate
/// step from this driver.
pub fn convert_expr(
  state: &mut ConvertState,
  scope: &mut std::collections::HashSet<super::cps::ir::CpsId>,
  expr: super::cps::ir::Expr,
) -> super::cps::ir::Expr {
  use super::cps::ir::{Arg, Callable, Cont, Expr, ExprKind, LetRecDefn, Param};
  let Expr { id, kind } = expr;
  let new_kind = match kind {
    ExprKind::LetVal { name, val, cont } => {
      let added = scope.insert(name.id);
      let cont = convert_cont(state, scope, cont);
      if added { scope.remove(&name.id); }
      ExprKind::LetVal { name, val, cont }
    }
    ExprKind::LetFn { name, params, fn_kind, fn_body, cont } => {
      // Compute captures for this fn. Exclude LetFn names — those resolve
      // statically via lower.rs's fn_syms; threading them as captures
      // produces spurious cap params.
      let body_free = free_vars(&fn_body);
      let captures: Vec<super::cps::ir::CpsId> = body_free.iter()
        .filter(|id| scope.contains(id))
        .filter(|id| !state.letfn_names.contains(id))
        .copied()
        .collect();
      // Rewrite the fn with cap params + body ref rewriting if captures non-empty.
      let (new_params, new_body, _cap_ids) = if captures.is_empty() {
        (params, *fn_body, Vec::new())
      } else {
        rewrite_fn_for_captures(state, &captures, params, *fn_body)
      };
      // Descend into the body with its own scope.
      let mut body_scope = scope.clone();
      for p in &new_params {
        let b = match p { Param::Name(b) | Param::Spread(b) => b };
        body_scope.insert(b.id);
      }
      let new_body = convert_expr(state, &mut body_scope, new_body);
      // The fn's name is in scope for the cont.
      let added = scope.insert(name.id);
      let cont = convert_cont(state, scope, cont);
      if added { scope.remove(&name.id); }
      // If this fn has captures, emit FnClosure construction so the closure
      // VALUE (not just the funcref) is materialised at the LetFn site. The
      // construction site is inserted in the cont chain — after the LetFn
      // defines the funcref under `name`, we wrap the cont in an FnClosure
      // App that builds the closure with cap values and binds it under a
      // fresh name. References to the LetFn name in the cont body get
      // rewritten to the new closure-bind name.
      //
      // Skipped for capture-free fns: those resolve via resolve_id_as_operand
      // which builds a no-capture closure on demand.
      if captures.is_empty() {
        ExprKind::LetFn {
          name,
          params: new_params,
          fn_kind,
          fn_body: Box::new(new_body),
          cont,
        }
      } else {
        emit_letfn_with_closure_construction(
          state, name, new_params, fn_kind,
          new_body, cont, &captures,
        )
      }
    }
    ExprKind::App { func, args } => {
      let new_args: Vec<Arg> = args.into_iter().map(|a| match a {
        Arg::Cont(c) => Arg::Cont(convert_cont(state, scope, c)),
        Arg::Expr(e) => Arg::Expr(Box::new(convert_expr(state, scope, *e))),
        other => other,
      }).collect();
      let _ = Callable::BuiltIn;  // silence unused-variant warning
      ExprKind::App { func, args: new_args }
    }
    ExprKind::If { cond, then, else_ } => {
      let then = convert_expr(state, scope, *then);
      let else_ = convert_expr(state, scope, *else_);
      ExprKind::If {
        cond,
        then: Box::new(then),
        else_: Box::new(else_),
      }
    }
    ExprKind::LetRec { group, no_self_edge, cont } => {
      // All sibling names are mutually in scope of all defn bodies + cont.
      let sibling_ids: Vec<super::cps::ir::CpsId> = group.iter().map(|d| match d {
        LetRecDefn::Fn { name, .. } => name.id,
        LetRecDefn::Val { name, .. } => name.id,
      }).collect();
      for id in &sibling_ids { scope.insert(*id); }
      // Process each defn.
      let new_group: Vec<LetRecDefn> = group.into_iter().map(|d| match d {
        LetRecDefn::Fn { name, params, fn_kind, body } => {
          let body_free = free_vars(&body);
          // For LetRec defns, siblings are part of the scope at this site.
          // We DO want sibling refs as captures (that's the whole point of
          // LetRec). Don't filter out via letfn_names — sibling defn names
          // are NOT in letfn_names (only LetFn names are).
          let captures: Vec<super::cps::ir::CpsId> = body_free.iter()
            .filter(|id| scope.contains(id))
            .filter(|id| !state.letfn_names.contains(id))
            .copied()
            .collect();
          let (new_params, new_body, _cap_ids) = if captures.is_empty() {
            (params, *body, Vec::new())
          } else {
            rewrite_fn_for_captures(state, &captures, params, *body)
          };
          // Descend into the rewritten body with its scope.
          let mut body_scope = scope.clone();
          for p in &new_params {
            let b = match p { Param::Name(b) | Param::Spread(b) => b };
            body_scope.insert(b.id);
          }
          let new_body = convert_expr(state, &mut body_scope, new_body);
          LetRecDefn::Fn { name, params: new_params, fn_kind, body: Box::new(new_body) }
        }
        LetRecDefn::Val { name, val } => {
          LetRecDefn::Val { name, val }
        }
      }).collect();
      let cont = convert_cont(state, scope, cont);
      for id in &sibling_ids { scope.remove(id); }
      let _ = Cont::Ref;
      ExprKind::LetRec { group: new_group, no_self_edge, cont }
    }
  };
  Expr { id, kind: new_kind }
}

/// For a LetFn with captures, restructure to emit an FnClosure construction
/// in the cont chain. The LetFn's funcref (under `name`) is consumed by the
/// FnClosure App, which builds a closure VALUE bound to a fresh user-bind.
/// All refs to the LetFn name in the cont body are rewritten to the new bind.
fn emit_letfn_with_closure_construction(
  state: &mut ConvertState,
  name: super::cps::ir::BindNode,
  new_params: Vec<super::cps::ir::Param>,
  fn_kind: super::cps::ir::CpsFnKind,
  new_body: super::cps::ir::Expr,
  cont: super::cps::ir::Cont,
  captures: &[super::cps::ir::CpsId],
) -> super::cps::ir::ExprKind {
  use super::cps::ir::{Arg, Bind, BuiltIn, Callable, Cont, Expr, ExprKind, Ref, Val, ValKind};

  // Allocate a fresh bind for the closure value (it inherits the name's
  // origin so source-map debugging shows the original binding site).
  let ast_origin = state.origin.try_get(name.id).and_then(|o| *o);
  let closure_bind = state.bind(Bind::Synth, ast_origin);

  // Build rewrite map: refs to the LetFn's name -> the new closure-bind id.
  let mut rewrite_map: std::collections::HashMap<super::cps::ir::CpsId, super::cps::ir::CpsId>
    = std::collections::HashMap::new();
  rewrite_map.insert(name.id, closure_bind.id);

  // Rewrite the cont chain's body refs.
  let cont = rewrite_refs_cont(cont, &rewrite_map);

  // Build the FnClosure App args:
  //   [Ref(letfn_name), cap_val_refs..., Cont::Expr { args: [closure_bind], body: cont's body }]
  let fn_name_val = Val {
    id: state.next(ast_origin),
    kind: ValKind::Ref(Ref::Synth(name.id)),
  };
  let mut app_args: Vec<Arg> = vec![Arg::Val(fn_name_val)];
  for &cap_id in captures {
    let cap_origin = state.origin.try_get(cap_id).and_then(|o| *o);
    let cap_val = Val {
      id: state.next(cap_origin),
      kind: ValKind::Ref(Ref::Synth(cap_id)),
    };
    app_args.push(Arg::Val(cap_val));
  }

  // The FnClosure cont: binds closure_bind, body is the original cont body.
  // For Cont::Ref tail, wrap it as the FnClosure's cont arg directly.
  let app_cont = match cont {
    Cont::Ref(_) => cont,
    Cont::Expr { args: orig_args, body } => {
      // The original cont's args are dropped — they were either empty or
      // a single bind for the LetFn's value, and we replace it with
      // closure_bind. If orig_args had a different shape, we drop them
      // (this is the standard FnClosure shape).
      let _ = orig_args;
      Cont::Expr {
        args: vec![closure_bind],
        body,
      }
    }
  };
  app_args.push(Arg::Cont(app_cont));

  let app = Expr {
    id: state.next(ast_origin),
    kind: ExprKind::App {
      func: Callable::BuiltIn(BuiltIn::FnClosure),
      args: app_args,
    },
  };

  // The LetFn's cont is now: a single Cont::Expr with no args, body = the FnClosure App.
  // This way the LetFn defines the funcref (under `name`), then immediately the
  // FnClosure App runs in the cont, consumes the funcref + caps, and binds the
  // resulting closure value under closure_bind for the rest of the program.
  let outer_cont = Cont::Expr {
    args: vec![],
    body: Box::new(app),
  };

  ExprKind::LetFn {
    name,
    params: new_params,
    fn_kind,
    fn_body: Box::new(new_body),
    cont: outer_cont,
  }
}

fn convert_cont(
  state: &mut ConvertState,
  scope: &mut std::collections::HashSet<super::cps::ir::CpsId>,
  cont: super::cps::ir::Cont,
) -> super::cps::ir::Cont {
  use super::cps::ir::Cont;
  match cont {
    Cont::Ref(id) => Cont::Ref(id),
    Cont::Expr { args, body } => {
      let mut added: Vec<super::cps::ir::CpsId> = Vec::with_capacity(args.len());
      for a in &args {
        if scope.insert(a.id) { added.push(a.id); }
      }
      let new_body = convert_expr(state, scope, *body);
      for id in &added { scope.remove(id); }
      Cont::Expr { args, body: Box::new(new_body) }
    }
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;

  use super::super::cps::ir::*;
  use std::collections::HashSet;

  // Helper: minimal Expr builders for tests.

  fn synth_bind(id: u32) -> BindNode {
    BindNode { id: CpsId(id), kind: Bind::Synth }
  }

  fn cps_id(n: u32) -> CpsId { CpsId(n) }

  fn ref_val(id: u32) -> Val {
    Val { id: CpsId(1000 + id), kind: ValKind::Ref(Ref::Synth(CpsId(id))) }
  }

  // Build a tiny LetFn:  inner = fn p: ref_to_outer_x
  // (no captures from caller perspective — depends on what's in scope).
  fn letfn_using(name_id: u32, param_id: u32, body_ref_id: u32) -> Expr {
    let body_val = ref_val(body_ref_id);
    let body = Expr {
      id: cps_id(2000 + name_id),
      kind: ExprKind::App {
        func: Callable::Val(body_val),
        args: vec![],
      },
    };
    Expr {
      id: cps_id(3000 + name_id),
      kind: ExprKind::LetFn {
        name: synth_bind(name_id),
        params: vec![Param::Name(synth_bind(param_id))],
        fn_kind: CpsFnKind::CpsFunction,
        fn_body: Box::new(body),
        cont: Cont::Expr {
          args: vec![],
          body: Box::new(Expr {
            id: cps_id(4000 + name_id),
            kind: ExprKind::App {
              func: Callable::Val(ref_val(name_id)),
              args: vec![],
            },
          }),
        },
      },
    }
  }

  #[test]
  fn t_collect_refs_basic() {
    // body: f(x, y)  -- 3 refs
    let body = Expr {
      id: cps_id(100),
      kind: ExprKind::App {
        func: Callable::Val(ref_val(1)),
        args: vec![Arg::Val(ref_val(2)), Arg::Val(ref_val(3))],
      },
    };
    let mut refs = HashSet::new();
    collect_refs(&body, &mut refs);
    let expected: HashSet<CpsId> = [cps_id(1), cps_id(2), cps_id(3)].into_iter().collect();
    assert_eq!(refs, expected);
  }

  #[test]
  fn t_free_vars_subtracts_local_binds() {
    // LetFn local = fn p: ref(p)  -- p is bound, refs locally
    // Body refs cps#10 (local).
    let inner_body = Expr {
      id: cps_id(200),
      kind: ExprKind::App {
        func: Callable::Val(ref_val(10)),  // p
        args: vec![],
      },
    };
    let expr = Expr {
      id: cps_id(300),
      kind: ExprKind::LetFn {
        name: synth_bind(11),
        params: vec![Param::Name(BindNode { id: cps_id(10), kind: Bind::Synth })],
        fn_kind: CpsFnKind::CpsFunction,
        fn_body: Box::new(inner_body),
        cont: Cont::Expr {
          args: vec![],
          body: Box::new(Expr {
            id: cps_id(400),
            kind: ExprKind::App {
              func: Callable::Val(ref_val(11)),
              args: vec![],
            },
          }),
        },
      },
    };
    let free = free_vars(&expr);
    // p (cps#10) and 11 (the fn name) are bound inside the expr. No free vars.
    assert_eq!(free.len(), 0, "free vars were: {:?}", free);
  }

  #[test]
  fn t_discover_fns_captures_outer_scope() {
    // outer = fn x: inner = fn y: x + y; inner 5
    // Model just: inner = fn y: x   (body refs x, outer's param)
    // discover_fns should report inner with captures=[x].
    let inner = letfn_using(/*name*/ 20, /*param*/ 21, /*body refs*/ 100);
    let mut scope: HashSet<CpsId> = HashSet::new();
    // Pretend cps#100 (x) is visible at this site (outer's param).
    scope.insert(cps_id(100));
    let fns = discover_fns(&inner, &scope);
    assert_eq!(fns.len(), 1, "fns: {:?}", fns);
    assert_eq!(fns[0].name_id, cps_id(20));
    assert_eq!(fns[0].captures, vec![cps_id(100)]);
    assert!(!fns[0].in_letrec);
  }

  #[test]
  fn t_discover_fns_letrec_sibling_capture() {
    // LetRec { is_even = fn k: is_odd, is_odd = fn k: is_even } in cont
    // discover_fns should report is_even captures [is_odd], is_odd captures [is_even].
    // (Siblings are in scope of each other.)
    let is_even_body = Expr {
      id: cps_id(500),
      kind: ExprKind::App {
        func: Callable::Val(ref_val(31)),  // is_odd
        args: vec![],
      },
    };
    let is_odd_body = Expr {
      id: cps_id(600),
      kind: ExprKind::App {
        func: Callable::Val(ref_val(30)),  // is_even
        args: vec![],
      },
    };
    let expr = Expr {
      id: cps_id(700),
      kind: ExprKind::LetRec {
        group: vec![
          LetRecDefn::Fn {
            name: synth_bind(30),  // is_even
            params: vec![Param::Name(synth_bind(40))],  // k
            fn_kind: CpsFnKind::CpsFunction,
            body: Box::new(is_even_body),
          },
          LetRecDefn::Fn {
            name: synth_bind(31),  // is_odd
            params: vec![Param::Name(synth_bind(41))],  // k
            fn_kind: CpsFnKind::CpsFunction,
            body: Box::new(is_odd_body),
          },
        ],
        no_self_edge: false,
        cont: Cont::Expr {
          args: vec![],
          body: Box::new(Expr {
            id: cps_id(800),
            kind: ExprKind::App {
              func: Callable::Val(ref_val(30)),
              args: vec![],
            },
          }),
        },
      },
    };
    let fns = discover_fns(&expr, &HashSet::new());
    assert_eq!(fns.len(), 2, "fns: {:?}", fns);
    let is_even = fns.iter().find(|f| f.name_id == cps_id(30)).unwrap();
    let is_odd  = fns.iter().find(|f| f.name_id == cps_id(31)).unwrap();
    assert!(is_even.in_letrec);
    assert!(is_odd.in_letrec);
    assert_eq!(is_even.captures, vec![cps_id(31)], "is_even should capture is_odd");
    assert_eq!(is_odd.captures, vec![cps_id(30)], "is_odd should capture is_even");
  }

  // Basic sanity: convert is a no-op for now.
  #[test]
  fn t_convert_is_noop_for_empty() {
    // The default no-op convert just returns input. A fuller test suite
    // arrives once steps 2-4 land.
    let _ = convert;  // silence unused warning during scaffold phase
  }

  #[test]
  fn t_rewrite_fn_adds_caps_and_rewrites_refs() {
    // fn p: ref(outer_x)  -- p is local, outer_x is captured.
    // Capture set = [outer_x].
    // After rewrite: fn cap0, p: ref(cap0)
    // ParamInfo[cap0] = Cap(outer_x).
    let outer_x = cps_id(50);
    let p = cps_id(60);

    let body = Expr {
      id: cps_id(100),
      kind: ExprKind::App {
        func: Callable::Val(ref_val(50)),  // ref outer_x
        args: vec![],
      },
    };
    let original_params = vec![Param::Name(BindNode { id: p, kind: Bind::Synth })];
    let captures = vec![outer_x];

    let mut state = ConvertState {
      origin: crate::propgraph::PropGraph::with_size(200, None),
      synth_alias: crate::propgraph::PropGraph::with_size(200, None),
      param_info: crate::propgraph::PropGraph::with_size(200, None),
      letfn_names: HashSet::new(),
    };

    let (new_params, new_body, cap_ids) = rewrite_fn_for_captures(
      &mut state, &captures, original_params, body,
    );

    // First param should be the cap, second should be the original p.
    assert_eq!(new_params.len(), 2);
    let cap_id = match &new_params[0] {
      Param::Name(b) => b.id,
      _ => panic!("expected Name param"),
    };
    assert_eq!(cap_ids, vec![cap_id]);
    let p_again = match &new_params[1] {
      Param::Name(b) => b.id,
      _ => panic!("expected Name param"),
    };
    assert_eq!(p_again, p);

    // ParamInfo for the new cap should be Cap(outer_x).
    let info = state.param_info.try_get(cap_id).cloned().flatten();
    assert_eq!(info, Some(ParamInfo::Cap(outer_x)));

    // synth_alias[cap_id] should be Some(outer_x).
    let alias = state.synth_alias.try_get(cap_id).cloned().flatten();
    assert_eq!(alias, Some(outer_x));

    // The body's ref to outer_x should now ref cap_id.
    let refs = {
      let mut s = HashSet::new();
      collect_refs(&new_body, &mut s);
      s
    };
    assert!(refs.contains(&cap_id), "body should ref cap_id={:?}, refs={:?}", cap_id, refs);
    assert!(!refs.contains(&outer_x), "body should no longer ref outer_x={:?}", outer_x);
  }

  #[test]
  fn t_convert_expr_emits_fnclosure_for_capturing_letfn() {
    // outer = fn outer_x:
    //   inner = fn p: outer_x   <- captures outer_x
    //   inner ()
    //
    // After conversion, inner should have Cap params AND an FnClosure App
    // should appear in inner's cont to materialize the closure value.
    let outer_x = cps_id(50);
    let inner_name = cps_id(51);
    let inner_param = cps_id(52);

    let inner_body = Expr {
      id: cps_id(100),
      kind: ExprKind::App {
        func: Callable::Val(ref_val(50)),  // refs outer_x
        args: vec![],
      },
    };

    let final_use = Expr {
      id: cps_id(200),
      kind: ExprKind::App {
        func: Callable::Val(ref_val(51)),  // call inner
        args: vec![],
      },
    };

    let outer_body_expr = Expr {
      id: cps_id(300),
      kind: ExprKind::LetFn {
        name: BindNode { id: inner_name, kind: Bind::Synth },
        params: vec![Param::Name(BindNode { id: inner_param, kind: Bind::Synth })],
        fn_kind: CpsFnKind::CpsFunction,
        fn_body: Box::new(inner_body),
        cont: Cont::Expr {
          args: vec![],
          body: Box::new(final_use),
        },
      },
    };

    let mut state = ConvertState {
      origin: crate::propgraph::PropGraph::with_size(500, None),
      synth_alias: crate::propgraph::PropGraph::with_size(500, None),
      param_info: crate::propgraph::PropGraph::with_size(500, None),
      letfn_names: HashSet::new(),
    };
    let mut scope: HashSet<CpsId> = [outer_x].into_iter().collect();

    let result = convert_expr(&mut state, &mut scope, outer_body_expr);

    // After conversion, the outer ExprKind should still be LetFn (inner),
    // but its cont should contain an FnClosure App.
    let (name, params, cont) = match &result.kind {
      ExprKind::LetFn { name, params, cont, .. } => (name, params, cont),
      _ => panic!("expected LetFn, got {:?}", result.kind),
    };
    assert_eq!(name.id, inner_name, "LetFn name should be preserved");
    // Should have cap params: 1 cap + 1 user = 2 params
    assert_eq!(params.len(), 2, "inner should have 1 cap + 1 user param");

    // Cont body should be an FnClosure App.
    let body = match cont {
      Cont::Expr { body, .. } => body,
      _ => panic!("expected Cont::Expr"),
    };
    let (func, args) = match &body.kind {
      ExprKind::App { func, args } => (func, args),
      _ => panic!("expected FnClosure App, got {:?}", body.kind),
    };
    assert!(matches!(func, Callable::BuiltIn(BuiltIn::FnClosure)),
      "expected FnClosure App, got func={:?}", func);
    // Args: [fn_ref, cap_val (outer_x), cont]
    assert_eq!(args.len(), 3, "expected [fn_ref, cap_val, cont], got {} args", args.len());
  }

  #[test]
  fn t_convert_expr_rewrites_letrec_siblings() {
    // outer = fn x: letrec { is_even = fn k: is_odd, is_odd = fn k: is_even } in is_even x
    // Each defn should get a cap param for the sibling after conversion.
    let is_even_body = Expr {
      id: cps_id(500),
      kind: ExprKind::App {
        func: Callable::Val(ref_val(31)),
        args: vec![],
      },
    };
    let is_odd_body = Expr {
      id: cps_id(600),
      kind: ExprKind::App {
        func: Callable::Val(ref_val(30)),
        args: vec![],
      },
    };
    let letrec = Expr {
      id: cps_id(700),
      kind: ExprKind::LetRec {
        group: vec![
          LetRecDefn::Fn {
            name: synth_bind(30),
            params: vec![Param::Name(synth_bind(40))],
            fn_kind: CpsFnKind::CpsFunction,
            body: Box::new(is_even_body),
          },
          LetRecDefn::Fn {
            name: synth_bind(31),
            params: vec![Param::Name(synth_bind(41))],
            fn_kind: CpsFnKind::CpsFunction,
            body: Box::new(is_odd_body),
          },
        ],
        no_self_edge: false,
        cont: Cont::Expr {
          args: vec![],
          body: Box::new(Expr {
            id: cps_id(800),
            kind: ExprKind::App {
              func: Callable::Val(ref_val(30)),
              args: vec![],
            },
          }),
        },
      },
    };

    let mut state = ConvertState {
      origin: crate::propgraph::PropGraph::with_size(900, None),
      synth_alias: crate::propgraph::PropGraph::with_size(900, None),
      param_info: crate::propgraph::PropGraph::with_size(900, None),
      letfn_names: HashSet::new(),
    };
    let mut scope: HashSet<CpsId> = HashSet::new();

    let result = convert_expr(&mut state, &mut scope, letrec);

    // Inspect the LetRec defns — each should have 2 params now (1 cap + 1 user).
    let group = match &result.kind {
      ExprKind::LetRec { group, .. } => group,
      _ => panic!("expected LetRec"),
    };
    assert_eq!(group.len(), 2);
    for d in group {
      match d {
        LetRecDefn::Fn { params, .. } => {
          assert_eq!(params.len(), 2,
            "each defn should have 1 cap + 1 user param, got {}", params.len());
          // First param should have ParamInfo::Cap set.
          let cap_param_id = match &params[0] {
            Param::Name(b) => b.id,
            _ => panic!("expected Name"),
          };
          let info = state.param_info.try_get(cap_param_id).cloned().flatten();
          assert!(matches!(info, Some(ParamInfo::Cap(_))),
            "first param should be Cap-tagged, got {:?}", info);
        }
        _ => panic!("expected Fn defn"),
      }
    }
  }

  #[test]
  fn t_rewrite_fn_handles_multiple_captures() {
    // fn p: f(a, b)   captures [a, b]
    // After rewrite: fn cap_a, cap_b, p: f(cap_a, cap_b)
    let a = cps_id(70);
    let b = cps_id(71);
    let p = cps_id(72);
    let body = Expr {
      id: cps_id(100),
      kind: ExprKind::App {
        func: Callable::Val(ref_val(0)),  // some other fn
        args: vec![Arg::Val(ref_val(70)), Arg::Val(ref_val(71))],
      },
    };
    let original_params = vec![Param::Name(BindNode { id: p, kind: Bind::Synth })];
    let captures = vec![a, b];

    let mut state = ConvertState {
      origin: crate::propgraph::PropGraph::with_size(200, None),
      synth_alias: crate::propgraph::PropGraph::with_size(200, None),
      param_info: crate::propgraph::PropGraph::with_size(200, None),
      letfn_names: HashSet::new(),
    };

    let (new_params, new_body, cap_ids) = rewrite_fn_for_captures(
      &mut state, &captures, original_params, body,
    );

    assert_eq!(new_params.len(), 3);
    assert_eq!(cap_ids.len(), 2);

    // ParamInfo set for both caps.
    let info_0 = state.param_info.try_get(cap_ids[0]).cloned().flatten();
    let info_1 = state.param_info.try_get(cap_ids[1]).cloned().flatten();
    assert_eq!(info_0, Some(ParamInfo::Cap(a)));
    assert_eq!(info_1, Some(ParamInfo::Cap(b)));

    // Body refs both cap ids and not the originals.
    let refs = {
      let mut s = HashSet::new();
      collect_refs(&new_body, &mut s);
      s
    };
    assert!(refs.contains(&cap_ids[0]));
    assert!(refs.contains(&cap_ids[1]));
    assert!(!refs.contains(&a));
    assert!(!refs.contains(&b));
  }
}
