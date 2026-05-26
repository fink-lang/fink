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
  // Step 1: scaffolding only. Real conversion lands in next steps.
  cps
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;

  // Basic sanity: convert is a no-op for now.
  #[test]
  fn t_convert_is_noop_for_empty() {
    // The default no-op convert just returns input. A fuller test suite
    // arrives once steps 2-4 land.
    let _ = convert;  // silence unused warning during scaffold phase
  }
}
