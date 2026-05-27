//! Hoist nested fn definitions out of the module body to top level.
//!
//! After [`super::convert`], every `LetFn` body is closed (captures are
//! materialised via `LetCaps` from the `ƒcaps` param). Once that's true,
//! the fn body no longer depends on its surrounding lexical scope — the
//! definition can live anywhere. This pass flattens all `LetFn`
//! occurrences in the IR so they appear as siblings wrapping the
//! module root, leaving the module body free of nested `·fn` definitions.
//!
//! Concretely:
//!
//! - Walk the IR; collect every `LetFn { name, params, fn_kind, fn_body, .. }`
//!   into a flat list, preserving source order.
//! - Replace each `LetFn` site with just its `cont`'s body (skip the
//!   definition — the surrounding Closure node already binds the
//!   funcref by name).
//! - Wrap the rewritten root in a chain of `LetFn`s — one per collected
//!   defn — using `Cont::Expr { args: [], body: ... }` so each lifted
//!   fn's name is in scope for the rest.
//!
//! After hoist, the IR shape is:
//!
//! ```text
//!   LetFn ƒname_1 = fn ...: ..., cont:
//!     LetFn ƒname_2 = fn ...: ..., cont:
//!       ...
//!         <original module root, with all inner LetFns elided>
//! ```
//!
//! The Closure construction sites inside the module body reference
//! the hoisted ƒname_i CpsIds — those are in scope thanks to the
//! enclosing LetFn chain.

use crate::passes::cps::ir::{
  Arg, BindNode, Callable, Cont, CpsFnKind, CpsResult, Expr, ExprKind, Param,
};

pub fn hoist(mut cps: CpsResult) -> CpsResult {
  let mut hoisted: Vec<HoistedFn> = Vec::new();
  let new_root = walk_expr(cps.root, &mut hoisted);
  cps.root = wrap_with_hoisted(new_root, hoisted);
  cps
}

/// Collected LetFn definition awaiting placement at the top level.
struct HoistedFn {
  name: BindNode,
  params: Vec<Param>,
  fn_kind: CpsFnKind,
  fn_body: Box<Expr>,
  expr_id: crate::passes::cps::ir::CpsId,
}

/// Walk an expression. Every encountered `LetFn` is moved into `hoisted`
/// and the LetFn site is replaced by the cont's body (the original
/// `cont` carried the rest of the enclosing chain).
fn walk_expr(expr: Expr, hoisted: &mut Vec<HoistedFn>) -> Expr {
  let Expr { id, kind } = expr;
  match kind {
    ExprKind::LetFn { name, params, fn_kind, fn_body, cont } => {
      // Recursively walk the fn body — inner LetFns are hoisted too.
      let fn_body = Box::new(walk_expr(*fn_body, hoisted));
      hoisted.push(HoistedFn {
        name,
        params,
        fn_kind,
        fn_body,
        expr_id: id,
      });
      // Replace the LetFn site with the cont's body.
      match cont {
        Cont::Expr { args: _, body } => walk_expr(*body, hoisted),
        Cont::Ref(cont_id) => {
          // Direct ref cont — synthesise a forwarding tail call.
          // Cont::Ref at a LetFn-cont site is rare (LetFn conts are
          // typically Cont::Expr from the lowering). Forward to the
          // cont with no args; if codegen needs a value it'll error.
          let app_id = id;
          let cont_val = crate::passes::cps::ir::Val {
            id,
            kind: crate::passes::cps::ir::ValKind::ContRef(cont_id),
          };
          Expr {
            id: app_id,
            kind: ExprKind::App {
              func: Callable::Val(cont_val),
              args: vec![],
            },
          }
        }
      }
    }
    ExprKind::LetVal { name, val, cont } => {
      let cont = walk_cont(cont, hoisted);
      Expr { id, kind: ExprKind::LetVal { name, val, cont } }
    }
    ExprKind::App { func, args } => {
      let args = args.into_iter().map(|a| match a {
        Arg::Cont(c) => Arg::Cont(walk_cont(c, hoisted)),
        Arg::Expr(e) => Arg::Expr(Box::new(walk_expr(*e, hoisted))),
        other => other,
      }).collect();
      Expr { id, kind: ExprKind::App { func, args } }
    }
    ExprKind::If { cond, then, else_ } => {
      let then = Box::new(walk_expr(*then, hoisted));
      let else_ = Box::new(walk_expr(*else_, hoisted));
      Expr { id, kind: ExprKind::If { cond, then, else_ } }
    }
    ExprKind::LetRec { slots, body } => {
      let body = Box::new(walk_expr(*body, hoisted));
      Expr { id, kind: ExprKind::LetRec { slots, body } }
    }
    ExprKind::Set { name, val, cont } => {
      let cont = walk_cont(cont, hoisted);
      Expr { id, kind: ExprKind::Set { name, val, cont } }
    }
    ExprKind::Closure { funcref, captures, cont } => {
      let cont = walk_cont(cont, hoisted);
      Expr { id, kind: ExprKind::Closure { funcref, captures, cont } }
    }
    ExprKind::LetCaps { caps, binds, cont } => {
      let cont = walk_cont(cont, hoisted);
      Expr { id, kind: ExprKind::LetCaps { caps, binds, cont } }
    }
  }
}

fn walk_cont(cont: Cont, hoisted: &mut Vec<HoistedFn>) -> Cont {
  match cont {
    Cont::Ref(_) => cont,
    Cont::Expr { args, body } => {
      let body = Box::new(walk_expr(*body, hoisted));
      Cont::Expr { args, body }
    }
  }
}

/// Wrap `root` in a chain of `LetFn`s — one per hoisted defn — in the
/// order they were collected (source order). Each LetFn's cont passes
/// through to the next.
fn wrap_with_hoisted(root: Expr, hoisted: Vec<HoistedFn>) -> Expr {
  let mut acc = root;
  // Build inside-out: the innermost cont wraps the original root, then
  // each outer LetFn wraps that.
  for h in hoisted.into_iter().rev() {
    let cont = Cont::Expr {
      args: vec![],
      body: Box::new(acc),
    };
    acc = Expr {
      id: h.expr_id,
      kind: ExprKind::LetFn {
        name: h.name,
        params: h.params,
        fn_kind: h.fn_kind,
        fn_body: h.fn_body,
        cont,
      },
    };
  }
  acc
}
