//! Thread the universe context (`·ƒctx`) as a uniform 0th value throughout
//! the CPS IR. Runs after `lower_module`. **Strangler-pipeline pass** —
//! not yet on the default lowering path. Selected tests opt in via the
//! `cps_module_ctx` helper to validate the IR shape; downstream passes
//! (lifting / wasm) and the runtime will be ported in subsequent slices.
//!
//! What this pass does:
//! - Every `LetFn` gets a fresh `Bind::Ctx` prepended to its `params`.
//!   While walking the fn body, the new ctx CpsId is the "current" one.
//! - Every `Cont::Expr { args, body }` likewise gets a fresh `Bind::Ctx`
//!   prepended to its `args`. Continuations are invoked with ctx as 0th value.
//! - Every `App` whose `func` is `Callable::Val(_)` gets `Arg::Val(Ref::Synth(ctx))`
//!   prepended — the user fn / cont being called expects ctx as 0th arg.
//! - `App` with `Callable::BuiltIn(_)` is unchanged — host fns don't take ctx.
//!
//! Module-root setup (already done by `lower_module`):
//! - `App(FinkModule, [Cont::Expr { args: [ƒctx_root, ƒret], body }])`
//! - `ƒctx_root` is the initial in-scope ctx for the module body. The pass
//!   recognises this and uses it as the seed for the ctx stack while
//!   walking `body`, instead of allocating a fresh ctx on top.

use crate::ast::AstId;
use crate::propgraph::PropGraph;

use super::ir::{
  Arg, Bind, BindNode, Callable, Cont, CpsId, CpsResult, Expr, ExprKind, Param, Ref, Val, ValKind,
};

pub fn thread_ctx(mut cps: CpsResult) -> CpsResult {
  let mut threader = Threader { origin: &mut cps.origin, ctx_stack: Vec::new() };

  // Recognise the module-root shape:
  //   App(FinkModule, [Cont::Expr { args: [ƒctx, ƒret], body }])
  // Use ƒctx as the initial in-scope ctx and walk the body. We do not
  // allocate a fresh ctx for the module body — it's already there from
  // the slice-1 module-init plumbing.
  let new_root = match cps.root.kind {
    ExprKind::App { func: Callable::BuiltIn(super::ir::BuiltIn::FinkModule), mut args }
        if args.len() == 1 => {
      let cont_arg = args.remove(0);
      let new_cont_arg = match cont_arg {
        Arg::Cont(Cont::Expr { args: cont_args, body })
            if cont_args.len() == 2
            && matches!(cont_args[0].kind, Bind::Ctx) => {
          let ctx_id = cont_args[0].id;
          threader.ctx_stack.push(ctx_id);
          let new_body = threader.thread_expr(*body);
          threader.ctx_stack.pop();
          Arg::Cont(Cont::Expr { args: cont_args, body: Box::new(new_body) })
        }
        // Unexpected shape — fall back to identity walk.
        other => other,
      };
      Expr {
        id: cps.root.id,
        kind: ExprKind::App {
          func: Callable::BuiltIn(super::ir::BuiltIn::FinkModule),
          args: vec![new_cont_arg],
        },
      }
    }
    // Non-module root — defensive identity.
    other => Expr { id: cps.root.id, kind: other },
  };

  cps.root = new_root;
  cps
}

struct Threader<'a> {
  origin: &'a mut PropGraph<CpsId, Option<AstId>>,
  ctx_stack: Vec<CpsId>,
}

impl Threader<'_> {
  /// Allocate a fresh CpsId for a new ctx bind. We deliberately leave the
  /// origin unset — Bind::Ctx has no AST counterpart, and the formatter
  /// falls back to bind-kind rendering when origin is None, so refs
  /// render as `·ƒctx_<id>` rather than borrowing an unrelated source name.
  fn fresh_ctx_bind(&mut self, _origin_hint: Option<AstId>) -> BindNode {
    let id = self.origin.push(None);
    BindNode { id, kind: Bind::Ctx }
  }

  fn current_ctx(&self) -> Option<CpsId> {
    self.ctx_stack.last().copied()
  }

  fn thread_expr(&mut self, expr: Expr) -> Expr {
    let Expr { id, kind } = expr;
    let new_kind = match kind {
      ExprKind::LetVal { name, val, cont } => {
        let new_cont = self.thread_cont(cont);
        ExprKind::LetVal { name, val, cont: new_cont }
      }
      ExprKind::LetFn { name, mut params, fn_kind, fn_body, cont } => {
        let body_origin = self.origin.try_get(fn_body.id).and_then(|o| *o);
        let ctx_bind = self.fresh_ctx_bind(body_origin);
        params.insert(0, Param::Name(ctx_bind.clone()));
        self.ctx_stack.push(ctx_bind.id);
        let new_body = self.thread_expr(*fn_body);
        self.ctx_stack.pop();
        // The continuation that consumes the LetFn's name lives in the
        // outer scope — walk it with the outer ctx still on top.
        let new_cont = self.thread_cont(cont);
        ExprKind::LetFn {
          name,
          params,
          fn_kind,
          fn_body: Box::new(new_body),
          cont: new_cont,
        }
      }
      ExprKind::App { func, args } => {
        let new_args = self.thread_args(args, &func);
        ExprKind::App { func, args: new_args }
      }
      ExprKind::If { cond, then, else_ } => {
        let new_then = self.thread_expr(*then);
        let new_else = self.thread_expr(*else_);
        ExprKind::If {
          cond,
          then: Box::new(new_then),
          else_: Box::new(new_else),
        }
      }
    };
    Expr { id, kind: new_kind }
  }

  fn thread_cont(&mut self, cont: Cont) -> Cont {
    match cont {
      Cont::Ref(_) => cont,
      Cont::Expr { mut args, body } => {
        let body_origin = self.origin.try_get(body.id).and_then(|o| *o);
        let ctx_bind = self.fresh_ctx_bind(body_origin);
        args.insert(0, ctx_bind.clone());
        self.ctx_stack.push(ctx_bind.id);
        let new_body = self.thread_expr(*body);
        self.ctx_stack.pop();
        Cont::Expr { args, body: Box::new(new_body) }
      }
    }
  }

  fn thread_args(&mut self, args: Vec<Arg>, _func: &Callable) -> Vec<Arg> {
    let mut new_args: Vec<Arg> = Vec::with_capacity(args.len() + 1);
    // Every Apply gets ctx as its 0th arg — `Callable::Val` (user fn /
    // continuation) and `Callable::BuiltIn` alike. Builtins like `op_plus`
    // are protocol-keyed operations that may be overridden inside a `with`
    // block (e.g. `with mx: 3 * m`), so they must see the active ctx to
    // dispatch through the right impl. The runtime decides what to do with
    // ctx; truly external host imports just ignore it.
    if let Some(ctx_id) = self.current_ctx() {
      let ctx_val = self.make_ctx_ref(ctx_id);
      new_args.push(Arg::Val(ctx_val));
    }
    for arg in args {
      let new_arg = match arg {
        Arg::Val(_) | Arg::Spread(_) => arg,
        Arg::Cont(c) => Arg::Cont(self.thread_cont(c)),
        Arg::Expr(e) => Arg::Expr(Box::new(self.thread_expr(*e))),
      };
      new_args.push(new_arg);
    }
    new_args
  }

  fn make_ctx_ref(&mut self, ctx_id: CpsId) -> Val {
    let id = self.origin.push(None);
    Val { id, kind: ValKind::Ref(Ref::Synth(ctx_id)) }
  }
}
