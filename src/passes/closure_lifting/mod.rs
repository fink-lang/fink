// Closure lifting pass (lambda lifting).
//
// A CPS transform: rewrites the IR so that every closure LetFn has its
// captured values threaded as explicit leading params. After this pass,
// no LetFn closes over any outer binding — all values are passed explicitly.
//
// Input:  CpsResult + ResolveResult + CaptureGraph
// Output: CpsResult (rewritten IR + updated origin map)
//
// Rewrite for each closure LetFn (non-empty captures):
//   Before (inside fn a: ...):
//     ·fn
//       fn {cap: [a]}, b: <body using a>
//       fn ·v_N: <cont>
//
//   After (hoisted to outer ·fn scope):
//     ·fn
//       fn ·v_cap, b: <body using a>      — ·v_cap is the new leading param
//       fn ·v_hoisted:                    — bind the lifted fn at outer scope
//         ·fn
//           fn a:                         — original outer fn (rewritten)
//             ·apply ·fn_closure, ·v_hoisted, a, ·state, fn ·v_result, ·state:
//               <original cont using ·v_result as the closure val>
//           fn ·v_outer: ...
//
// Hoisting: lifted fns bubble up through the `lift_expr` return value.
// The outermost non-closure LetFn that contains a closure wraps all
// hoisted fns as new LetFn nodes above itself.
//
// CPS transform contract (see src/passes/cps-transform-contract.md):
//   1. Every new node gets a CpsId via the id allocator + origin entry.
//   2. Rewritten nodes carry forward the original AstId (same origin).
//      Synthesized nodes with no direct AST source carry None.
//   3. The output CpsResult.origin must be dense.
//   4. Produce a fresh tree — never mutate the input in place.

use crate::passes::name_res::{Resolution, ResolveResult};
use crate::passes::closure_capture::CaptureGraph;
use crate::passes::cps::ir::{
  Arg, Bind, BindNode, BuiltIn, Callable, CpsId, CpsResult, Expr, ExprKind,
  Param, Ref, Val, ValKind,
};
use crate::propgraph::PropGraph;
use crate::ast::AstId;

// ---------------------------------------------------------------------------
// Id allocator
// ---------------------------------------------------------------------------

struct Alloc {
  origin: PropGraph<CpsId, Option<AstId>>,
}

impl Alloc {
  fn new(existing: PropGraph<CpsId, Option<AstId>>) -> Self {
    Alloc { origin: existing }
  }

  fn next(&mut self, origin: Option<AstId>) -> CpsId {
    self.origin.push(origin)
  }

  fn bind(&mut self, kind: Bind, origin: Option<AstId>) -> BindNode {
    let id = self.next(origin);
    BindNode { id, kind }
  }

  fn gen_bind(&mut self) -> BindNode {
    self.bind(Bind::Gen, None)
  }

  fn expr<'src>(&mut self, kind: ExprKind<'src>, origin: Option<AstId>) -> Expr<'src> {
    let id = self.next(origin);
    Expr { id, kind }
  }

  fn val<'src>(&mut self, kind: ValKind<'src>, origin: Option<AstId>) -> Val<'src> {
    let id = self.next(origin);
    Val { id, kind }
  }
}

// ---------------------------------------------------------------------------
// Hoisted fn — a lifted fn to be inserted at the outer scope
// ---------------------------------------------------------------------------

struct HoistedFn<'src> {
  name:    BindNode,
  params:  Vec<Param>,
  fn_body: Expr<'src>,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Rewrite all closure LetFn nodes to take their captures as explicit params.
pub fn lift<'src>(
  result: CpsResult<'src>,
  resolve: &ResolveResult,
  captures: &CaptureGraph<'src>,
) -> CpsResult<'src> {
  let mut alloc = Alloc::new(result.origin);
  let mut hoisted: Vec<HoistedFn<'src>> = Vec::new();
  let new_root = lift_expr(result.root, captures, resolve, &mut alloc, &mut hoisted);
  // Any fns still pending after the root are wrapped at the top.
  let new_root = wrap_hoisted(new_root, hoisted, &mut alloc);
  CpsResult { root: new_root, origin: alloc.origin }
}

// ---------------------------------------------------------------------------
// Wrap hoisted fns around an expression (outermost scope injection).
//
// Produces:
//   LetFn(hoisted[0], body=
//     LetFn(hoisted[1], body=
//       ... expr))
//
// Fns are wrapped in order (first hoisted = outermost).
// ---------------------------------------------------------------------------

fn wrap_hoisted<'src>(
  mut expr: Expr<'src>,
  hoisted: Vec<HoistedFn<'src>>,
  alloc: &mut Alloc,
) -> Expr<'src> {
  for h in hoisted.into_iter().rev() {
    let wrapper_id = alloc.next(None);
    expr = Expr {
      id: wrapper_id,
      kind: ExprKind::LetFn {
        name: h.name,
        params: h.params,
        fn_body: Box::new(h.fn_body),
        body: Box::new(expr),
      },
    };
  }
  expr
}

// ---------------------------------------------------------------------------
// Scan inner fn body for captured bind CpsIds
// ---------------------------------------------------------------------------

fn collect_bind_ids(
  expr: &Expr<'_>,
  resolve: &ResolveResult,
  out: &mut std::collections::HashMap<CpsId, CpsId>,
) {
  use ExprKind::*;
  match &expr.kind {
    Ret(val) => scan_val(val, resolve, out),
    LetVal { val, body, .. } => {
      scan_val(val, resolve, out);
      collect_bind_ids(body, resolve, out);
    }
    LetFn { body, .. } => collect_bind_ids(body, resolve, out),
    App { func, args, body, .. } => {
      if let Callable::Val(v) = func { scan_val(v, resolve, out); }
      for arg in args {
        match arg { Arg::Val(v) | Arg::Spread(v) => scan_val(v, resolve, out) }
      }
      collect_bind_ids(body, resolve, out);
    }
    If { cond, then, else_ } => {
      scan_val(cond, resolve, out);
      collect_bind_ids(then, resolve, out);
      collect_bind_ids(else_, resolve, out);
    }
    Yield { value, body, .. } => {
      scan_val(value, resolve, out);
      collect_bind_ids(body, resolve, out);
    }
    MatchLetVal { val, fail, body, .. } => {
      scan_val(val, resolve, out);
      collect_bind_ids(fail, resolve, out);
      collect_bind_ids(body, resolve, out);
    }
    MatchApp { func, args, fail, body, .. } => {
      if let Callable::Val(v) = func { scan_val(v, resolve, out); }
      for v in args { scan_val(v, resolve, out); }
      collect_bind_ids(fail, resolve, out);
      collect_bind_ids(body, resolve, out);
    }
    MatchIf { func, args, fail, body, .. } => {
      if let Callable::Val(v) = func { scan_val(v, resolve, out); }
      for v in args { scan_val(v, resolve, out); }
      collect_bind_ids(fail, resolve, out);
      collect_bind_ids(body, resolve, out);
    }
    MatchValue { val, fail, body, .. } => {
      scan_val(val, resolve, out);
      collect_bind_ids(fail, resolve, out);
      collect_bind_ids(body, resolve, out);
    }
    MatchSeq { val, fail, body, .. } => {
      scan_val(val, resolve, out);
      collect_bind_ids(fail, resolve, out);
      collect_bind_ids(body, resolve, out);
    }
    MatchNext { fail, body, .. } |
    MatchDone { fail, body, .. } |
    MatchNotDone { fail, body, .. } |
    MatchRest { fail, body, .. } => {
      collect_bind_ids(fail, resolve, out);
      collect_bind_ids(body, resolve, out);
    }
    MatchRec { val, fail, body, .. } => {
      scan_val(val, resolve, out);
      collect_bind_ids(fail, resolve, out);
      collect_bind_ids(body, resolve, out);
    }
    MatchField { fail, body, .. } => {
      collect_bind_ids(fail, resolve, out);
      collect_bind_ids(body, resolve, out);
    }
    MatchBlock { params, fail, arms, body, .. } => {
      for v in params { scan_val(v, resolve, out); }
      collect_bind_ids(fail, resolve, out);
      for arm in arms { collect_bind_ids(arm, resolve, out); }
      collect_bind_ids(body, resolve, out);
    }
    LetRec { bindings, body } => {
      for b in bindings { collect_bind_ids(&b.fn_body, resolve, out); }
      collect_bind_ids(body, resolve, out);
    }
    Panic | FailCont => {}
  }
}

fn scan_val(
  val: &Val<'_>,
  resolve: &ResolveResult,
  out: &mut std::collections::HashMap<CpsId, CpsId>,
) {
  if let ValKind::Ref(Ref::Name) = &val.kind {
    if let Some(Some(Resolution::Captured { bind, .. })) = resolve.resolution.try_get(val.id) {
      out.entry(*bind).or_insert(*bind);
    }
  }
}

// ---------------------------------------------------------------------------
// Transform — walk and rewrite, bubble hoisted fns upward
// ---------------------------------------------------------------------------

fn lift_expr<'src>(
  expr: Expr<'src>,
  captures: &CaptureGraph<'src>,
  resolve: &ResolveResult,
  alloc: &mut Alloc,
  hoisted: &mut Vec<HoistedFn<'src>>,
) -> Expr<'src> {
  use ExprKind::*;
  match expr.kind {
    LetFn { name, params, fn_body, body } => {
      let caps: Vec<&'src str> = captures.try_get(name.id)
        .cloned()
        .unwrap_or_default();

      // Recurse into fn_body and body; collect their hoisted fns.
      let mut inner_hoisted: Vec<HoistedFn<'src>> = Vec::new();
      let rewritten_fn_body = lift_expr(*fn_body, captures, resolve, alloc, &mut inner_hoisted);
      let rewritten_body    = lift_expr(*body, captures, resolve, alloc, hoisted);

      if caps.is_empty() {
        // Pure function — bubble hoisted fns from fn_body upward to the outer scope.
        // They will be injected at the outermost LetFn (module root) by lift().
        hoisted.extend(inner_hoisted);
        Expr {
          id: expr.id,
          kind: LetFn {
            name,
            params,
            fn_body: Box::new(rewritten_fn_body),
            body: Box::new(rewritten_body),
          },
        }
      } else {
        // Closure — hoist this fn to the outer scope.
        //
        // 1. Scan the original fn_body to find bind CpsIds for each capture.
        let mut cap_bind_map: std::collections::HashMap<CpsId, CpsId> =
          std::collections::HashMap::new();
        collect_bind_ids(&rewritten_fn_body, resolve, &mut cap_bind_map);

        // 2. Build Gen bind nodes for capture params — rendered as ·v_N.
        let cap_binds: Vec<BindNode> = caps.iter().map(|_| alloc.gen_bind()).collect();
        let cap_params: Vec<Param> = cap_binds.iter().map(|b| Param::Name(b.clone())).collect();

        // 3. Build Val refs for the capture args at the call site.
        //    Use Ref::Name with AST origin copied from the original binding.
        let cap_ref_vals: Vec<Val<'src>> = cap_bind_map.values().map(|&bind_id| {
          let ast_origin = alloc.origin.try_get(bind_id).and_then(|o| *o);
          alloc.val(ValKind::Ref(Ref::Name), ast_origin)
        }).collect();

        // 4. Push the hoisted lifted fn; also bubble any fns hoisted from fn_body.
        //    All go to the outer scope (top level).
        hoisted.extend(inner_hoisted);
        let mut lifted_params = cap_params;
        lifted_params.extend(params);
        let lifted_fn_bind = alloc.gen_bind();
        let lifted_fn_id = lifted_fn_bind.id;

        hoisted.push(HoistedFn {
          name:    lifted_fn_bind,
          params:  lifted_params,
          fn_body: rewritten_fn_body,
        });

        // 5. At the original site: emit ·fn_closure call, bind result as original name.
        let lifted_ref  = alloc.val(ValKind::Ref(Ref::Gen(lifted_fn_id)), None);
        let result_bind = alloc.gen_bind();
        let result_ref  = alloc.val(ValKind::Ref(Ref::Gen(result_bind.id)), None);

        // Bind the closure val under the original closure name, then run body.
        let closure_bind = name;
        let subst_body = Expr {
          id: expr.id, // carry forward original LetFn's CpsId
          kind: LetVal {
            name: closure_bind,
            val: Box::new(result_ref),
            body: Box::new(rewritten_body),
          },
        };

        let mut fn_closure_args: Vec<Arg<'src>> = vec![Arg::Val(lifted_ref)];
        fn_closure_args.extend(cap_ref_vals.into_iter().map(Arg::Val));

        alloc.expr(ExprKind::App {
          func: Callable::BuiltIn(BuiltIn::FnClosure),
          args: fn_closure_args,
          result: result_bind,
          body: Box::new(subst_body),
        }, None)
      }
    }

    // Structural recursion for all other node kinds.
    LetVal { name, val, body } => Expr {
      id: expr.id,
      kind: LetVal {
        name,
        val,
        body: Box::new(lift_expr(*body, captures, resolve, alloc, hoisted)),
      },
    },

    App { func, args, result, body } => Expr {
      id: expr.id,
      kind: App {
        func,
        args,
        result,
        body: Box::new(lift_expr(*body, captures, resolve, alloc, hoisted)),
      },
    },

    If { cond, then, else_ } => Expr {
      id: expr.id,
      kind: If {
        cond,
        then: Box::new(lift_expr(*then, captures, resolve, alloc, hoisted)),
        else_: Box::new(lift_expr(*else_, captures, resolve, alloc, hoisted)),
      },
    },

    Yield { value, result, body } => Expr {
      id: expr.id,
      kind: Yield {
        value,
        result,
        body: Box::new(lift_expr(*body, captures, resolve, alloc, hoisted)),
      },
    },

    LetRec { bindings, body } => {
      let new_bindings = bindings.into_iter().map(|b| {
        crate::passes::cps::ir::Binding {
          fn_body: Box::new(lift_expr(*b.fn_body, captures, resolve, alloc, hoisted)),
          ..b
        }
      }).collect();
      Expr {
        id: expr.id,
        kind: LetRec {
          bindings: new_bindings,
          body: Box::new(lift_expr(*body, captures, resolve, alloc, hoisted)),
        },
      }
    }

    MatchLetVal { name, val, fail, body } => Expr {
      id: expr.id,
      kind: MatchLetVal {
        name, val,
        fail: Box::new(lift_expr(*fail, captures, resolve, alloc, hoisted)),
        body: Box::new(lift_expr(*body, captures, resolve, alloc, hoisted)),
      },
    },

    MatchApp { func, args, fail, result, body } => Expr {
      id: expr.id,
      kind: MatchApp {
        func, args, result,
        fail: Box::new(lift_expr(*fail, captures, resolve, alloc, hoisted)),
        body: Box::new(lift_expr(*body, captures, resolve, alloc, hoisted)),
      },
    },

    MatchIf { func, args, fail, body } => Expr {
      id: expr.id,
      kind: MatchIf {
        func, args,
        fail: Box::new(lift_expr(*fail, captures, resolve, alloc, hoisted)),
        body: Box::new(lift_expr(*body, captures, resolve, alloc, hoisted)),
      },
    },

    MatchValue { val, lit, fail, body } => Expr {
      id: expr.id,
      kind: MatchValue {
        val, lit,
        fail: Box::new(lift_expr(*fail, captures, resolve, alloc, hoisted)),
        body: Box::new(lift_expr(*body, captures, resolve, alloc, hoisted)),
      },
    },

    MatchSeq { val, cursor, fail, body } => Expr {
      id: expr.id,
      kind: MatchSeq {
        val, cursor,
        fail: Box::new(lift_expr(*fail, captures, resolve, alloc, hoisted)),
        body: Box::new(lift_expr(*body, captures, resolve, alloc, hoisted)),
      },
    },

    MatchNext { val, cursor, next_cursor, fail, elem, body } => Expr {
      id: expr.id,
      kind: MatchNext {
        val, cursor, next_cursor, elem,
        fail: Box::new(lift_expr(*fail, captures, resolve, alloc, hoisted)),
        body: Box::new(lift_expr(*body, captures, resolve, alloc, hoisted)),
      },
    },

    MatchDone { val, cursor, fail, result, body } => Expr {
      id: expr.id,
      kind: MatchDone {
        val, cursor, result,
        fail: Box::new(lift_expr(*fail, captures, resolve, alloc, hoisted)),
        body: Box::new(lift_expr(*body, captures, resolve, alloc, hoisted)),
      },
    },

    MatchNotDone { val, cursor, fail, body } => Expr {
      id: expr.id,
      kind: MatchNotDone {
        val, cursor,
        fail: Box::new(lift_expr(*fail, captures, resolve, alloc, hoisted)),
        body: Box::new(lift_expr(*body, captures, resolve, alloc, hoisted)),
      },
    },

    MatchRest { val, cursor, fail, result, body } => Expr {
      id: expr.id,
      kind: MatchRest {
        val, cursor, result,
        fail: Box::new(lift_expr(*fail, captures, resolve, alloc, hoisted)),
        body: Box::new(lift_expr(*body, captures, resolve, alloc, hoisted)),
      },
    },

    MatchRec { val, cursor, fail, body } => Expr {
      id: expr.id,
      kind: MatchRec {
        val, cursor,
        fail: Box::new(lift_expr(*fail, captures, resolve, alloc, hoisted)),
        body: Box::new(lift_expr(*body, captures, resolve, alloc, hoisted)),
      },
    },

    MatchField { val, cursor, next_cursor, field, fail, elem, body } => Expr {
      id: expr.id,
      kind: MatchField {
        val, cursor, next_cursor, field, elem,
        fail: Box::new(lift_expr(*fail, captures, resolve, alloc, hoisted)),
        body: Box::new(lift_expr(*body, captures, resolve, alloc, hoisted)),
      },
    },

    MatchBlock { params, fail, arm_params, arms, result, body } => {
      let new_arms = arms.into_iter()
        .map(|a| lift_expr(a, captures, resolve, alloc, hoisted))
        .collect();
      Expr {
        id: expr.id,
        kind: MatchBlock {
          params,
          arm_params,
          result,
          fail: Box::new(lift_expr(*fail, captures, resolve, alloc, hoisted)),
          arms: new_arms,
          body: Box::new(lift_expr(*body, captures, resolve, alloc, hoisted)),
        },
      }
    }

    Ret(_) | Panic | FailCont => expr,
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use test_macros::include_fink_tests;

  use crate::parser::parse;
  use crate::ast::build_index;
  use crate::passes::cps::transform::lower_expr;
  use crate::passes::cps::fmt::{fmt_with, Ctx};
  use crate::passes::name_res::resolve;
  use crate::passes::closure_capture::analyse;
  use super::lift;

  /// Run the full pipeline (parse → CPS → name_res → capture → lift) on `src`
  /// and return the rewritten CPS IR.
  fn closure_lift(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let cps = lower_expr(&r.root);
        let node_count = cps.origin.len();
        let resolve_result = resolve(&cps.root, &cps.origin, &ast_index, node_count);
        let cap_graph = analyse(&cps, &resolve_result, &ast_index);
        let lifted = lift(cps, &resolve_result, &cap_graph);
        let ctx = Ctx {
          origin: &lifted.origin,
          ast_index: &ast_index,
          captures: None,
        };
        fmt_with(&lifted.root, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  include_fink_tests!("src/passes/closure_lifting/test_closure_lifting.fnk");
}
