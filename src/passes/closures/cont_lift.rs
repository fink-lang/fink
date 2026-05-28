//! Lift inline `Cont::Expr` args of user-fn calls into named LetFns.
//!
//! After `thread_ctx`, calls to user fns are shaped as
//!
//! ```text
//!   App { Callable::Val(callee), args: [ctx, Cont::Expr { args, body }, ...] }
//! ```
//!
//! At the wasm apply boundary the cont must be a real `$Closure` value —
//! an inline `Cont::Expr` cannot cross `apply_3`. This pass replaces each
//! such `Cont::Expr` arg with a `Val::Ref(Ref::Synth(fresh_id))` and
//! wraps the surrounding expression in a `LetFn { name: fresh_id, params:
//! cont_args, fn_body: cont_body, cont: <wrapped> }`.
//!
//! After this pass, every `Arg::Cont(Cont::Expr { .. })` left in the IR
//! sits at a `Callable::BuiltIn` call in the small `builtin_keeps_inline_conts`
//! set (Pub, Panic, FnClosure, FinkModule — these are structural ops
//! whose cont body is emitted inline as straight-line wasm). Every
//! other builtin's cont takes its arg as a closure value (runtime
//! calls it via apply_3), so inline Cont::Expr there must be lifted.
//!
//! Subsequent passes:
//! - `convert` runs over the synthesised LetFns identically to user
//!   LetFns, computing free vars and emitting `Closure` + `LetCaps`.
//! - `hoist` flattens them to the top level.

use crate::ast::AstId;
use crate::passes::cps::ir::BuiltIn;

/// Builtins that keep their `Cont::Expr` args **inline** at codegen
/// time. Codegen lowers them as straight-line wasm without crossing
/// the runtime apply boundary — the cont's body is emitted directly
/// into the enclosing fn. All other builtins take their cont as a
/// closure value (the runtime tail-calls it via `apply_3`); their
/// inline Cont::Expr args must be lifted into named LetFns.
fn builtin_keeps_inline_conts(b: BuiltIn) -> bool {
  matches!(
    b,
    BuiltIn::Pub          // structural: descend into cont body
      | BuiltIn::Panic    // no cont (trap)
      | BuiltIn::FnClosure // closure construction — cont takes the new closure
      | BuiltIn::FinkModule, // module root — cont is the module body
  )
}
use crate::passes::cps::ir::{
  Arg, Bind, BindNode, Callable, Cont, CpsFnKind, CpsId, CpsResult, Expr, ExprKind, Param,
  Ref, Val, ValKind,
};
use crate::propgraph::PropGraph;

pub fn cont_lift(mut cps: CpsResult) -> CpsResult {
  let mut cx = Cx { origin: &mut cps.origin };
  cps.root = cx.lift_expr(cps.root);
  cps
}

struct Cx<'a> {
  origin: &'a mut PropGraph<CpsId, Option<AstId>>,
}

impl Cx<'_> {
  fn fresh_id(&mut self, origin: Option<AstId>) -> CpsId {
    self.origin.push(origin)
  }

  fn lift_expr(&mut self, expr: Expr) -> Expr {
    let Expr { id, kind } = expr;
    match kind {
      ExprKind::LetVal { name, val, cont } => {
        let cont = self.lift_cont(cont);
        Expr { id, kind: ExprKind::LetVal { name, val, cont } }
      }
      ExprKind::LetFn { name, params, fn_kind, fn_body, cont } => {
        let fn_body = Box::new(self.lift_expr(*fn_body));
        let cont = self.lift_cont(cont);
        Expr { id, kind: ExprKind::LetFn { name, params, fn_kind, fn_body, cont } }
      }
      ExprKind::LetRec { slots, body } => {
        let body = Box::new(self.lift_expr(*body));
        Expr { id, kind: ExprKind::LetRec { slots, body } }
      }
      ExprKind::Set { name, val, cont } => {
        let cont = self.lift_cont(cont);
        Expr { id, kind: ExprKind::Set { name, val, cont } }
      }
      ExprKind::Closure { funcref, captures, cont } => {
        let cont = self.lift_cont(cont);
        Expr { id, kind: ExprKind::Closure { funcref, captures, cont } }
      }
      ExprKind::LetCaps { caps, binds, cont } => {
        let cont = self.lift_cont(cont);
        Expr { id, kind: ExprKind::LetCaps { caps, binds, cont } }
      }
      ExprKind::If { cond, then, else_ } => {
        let then = Box::new(self.lift_expr(*then));
        let else_ = Box::new(self.lift_expr(*else_));
        Expr { id, kind: ExprKind::If { cond, then, else_ } }
      }
      ExprKind::App { func, args } => {
        // Recurse into args first — any nested Cont::Expr bodies lift
        // their own inner App-conts.
        let args: Vec<Arg> = args.into_iter().map(|a| self.lift_arg(a)).collect();
        let app = Expr { id, kind: ExprKind::App { func: func.clone(), args } };
        // User-fn calls (Callable::Val) always need their Cont::Expr
        // args lifted — the runtime apply path takes closure values.
        // Most BuiltIn calls keep their inline Cont::Expr (codegen
        // emits straight-line wasm), but a handful (IsSeqLike,
        // IsRecLike, SeqPop, SeqPopBack, RecPop) pass their conts to
        // a runtime function that calls them as closure values —
        // those Cont::Expr args must also be lifted.
        match &func {
          Callable::Val(_) => self.lift_user_app(app),
          Callable::BuiltIn(b) if builtin_keeps_inline_conts(*b) => app,
          Callable::BuiltIn(_) => self.lift_user_app(app),
        }
      }
    }
  }

  fn lift_cont(&mut self, cont: Cont) -> Cont {
    match cont {
      Cont::Ref(_) => cont,
      Cont::Expr { args, body } => {
        let body = Box::new(self.lift_expr(*body));
        Cont::Expr { args, body }
      }
    }
  }

  fn lift_arg(&mut self, arg: Arg) -> Arg {
    match arg {
      Arg::Cont(c) => Arg::Cont(self.lift_cont(c)),
      Arg::Expr(e) => Arg::Expr(Box::new(self.lift_expr(*e))),
      other => other,
    }
  }

  /// Walk the args of a user-fn App. For each `Arg::Cont(Cont::Expr {
  /// args, body })`, mint a fresh fn id, replace the arg with a
  /// `Ref::Synth` to it, and wrap the resulting App in a `LetFn`
  /// definition for the lifted cont fn.
  fn lift_user_app(&mut self, app: Expr) -> Expr {
    let Expr { id: app_id, kind } = app;
    let ExprKind::App { func, args } = kind else { unreachable!() };

    // Collect (replacement_arg, optional lifted_letfn_pieces).
    let mut lifted: Vec<(CpsId, Vec<BindNode>, Box<Expr>)> = Vec::new();
    let new_args: Vec<Arg> = args.into_iter().map(|a| match a {
      Arg::Cont(Cont::Expr { args: cont_args, body }) => {
        // Mint a fresh id for the lifted cont fn.
        let fn_id = self.fresh_id(None);
        lifted.push((fn_id, cont_args, body));
        Arg::Val(Val {
          id: self.fresh_id(None),
          kind: ValKind::Ref(Ref::Synth(fn_id)),
        })
      }
      other => other,
    }).collect();

    let inner_app = Expr {
      id: app_id,
      kind: ExprKind::App { func, args: new_args },
    };

    // Wrap the App in nested LetFns, one per lifted cont. Innermost
    // LetFn's cont body is the App; each outer LetFn's cont body is
    // the next inner LetFn.
    let mut acc = inner_app;
    for (fn_id, cont_args, body) in lifted.into_iter().rev() {
      let params: Vec<Param> = cont_args.into_iter().map(Param::Name).collect();
      let name_bind = BindNode { id: fn_id, kind: Bind::Synth };
      let wrap_id = self.fresh_id(None);
      acc = Expr {
        id: wrap_id,
        kind: ExprKind::LetFn {
          name: name_bind,
          params,
          fn_kind: CpsFnKind::CpsClosure,
          fn_body: body,
          cont: Cont::Expr { args: vec![], body: Box::new(acc) },
        },
      };
    }
    acc
  }
}
