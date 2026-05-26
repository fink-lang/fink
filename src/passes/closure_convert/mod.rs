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
