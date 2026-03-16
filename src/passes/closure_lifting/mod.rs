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
  Arg, Bind, BindNode, BuiltIn, Callable, Cont, CpsId, CpsResult, Expr, ExprKind,
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

/// Single lifting pass: rewrite all closure LetFn nodes found in `result`
/// to take their captures as explicit params.
fn lift_once<'src>(
  result: CpsResult<'src>,
  resolve: &ResolveResult,
  captures: &CaptureGraph<'src>,
  ast_index: &PropGraph<crate::ast::AstId, Option<&'src crate::ast::Node<'src>>>,
) -> CpsResult<'src> {
  let mut alloc = Alloc::new(result.origin);
  let mut hoisted: Vec<HoistedFn<'src>> = Vec::new();
  let new_root = lift_expr(result.root, captures, resolve, ast_index, &mut alloc, &mut hoisted);
  let new_root = wrap_hoisted(new_root, hoisted, &mut alloc);
  CpsResult { root: new_root, origin: alloc.origin }
}

/// Returns true if any ref in the IR still resolves as `Captured`.
/// Scans the resolution prop graph directly — O(n) in node count, no tree walk.
fn has_captures(resolve: &ResolveResult) -> bool {
  resolve.any_captured()
}

/// Run lifting until no `Captured` refs remain, then return the lifted IR
/// together with the final name resolution result.
///
/// After this call the caller has a capture-free CPS IR and an up-to-date
/// `ResolveResult` — no further name resolution pass is needed before codegen.
pub fn lift_all<'src>(
  cps: CpsResult<'src>,
  ast_index: &PropGraph<crate::ast::AstId, Option<&'src crate::ast::Node<'src>>>,
) -> (CpsResult<'src>, ResolveResult) {
  use crate::passes::closure_capture::analyse;
  use crate::passes::name_res::resolve;

  let mut current = cps;
  loop {
    let node_count = current.origin.len();
    let resolve_result = resolve(&current.root, &current.origin, ast_index, node_count);
    if !has_captures(&resolve_result) {
      return (current, resolve_result);
    }
    let cap_graph = analyse(&current, &resolve_result, ast_index);
    current = lift_once(current, &resolve_result, &cap_graph, ast_index);
  }
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
    App { func, args, cont } => {
      if let Callable::Val(v) = func { scan_val(v, resolve, out); }
      for arg in args {
        match arg { Arg::Val(v) | Arg::Spread(v) => scan_val(v, resolve, out) }
      }
      if let Cont::Expr(_, body) = cont { collect_bind_ids(body, resolve, out); }
    }
    If { cond, then, else_ } => {
      scan_val(cond, resolve, out);
      collect_bind_ids(then, resolve, out);
      collect_bind_ids(else_, resolve, out);
    }
    Yield { value, cont } => {
      scan_val(value, resolve, out);
      if let Cont::Expr(_, body) = cont { collect_bind_ids(body, resolve, out); }
    }
    MatchLetVal { val, fail, body, .. } => {
      scan_val(val, resolve, out);
      collect_bind_ids(fail, resolve, out);
      collect_bind_ids(body, resolve, out);
    }
    MatchApp { func, args, fail, cont } => {
      if let Callable::Val(v) = func { scan_val(v, resolve, out); }
      for v in args { scan_val(v, resolve, out); }
      collect_bind_ids(fail, resolve, out);
      if let Cont::Expr(_, body) = cont { collect_bind_ids(body, resolve, out); }
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
    MatchNext { fail, cont, .. } => {
      collect_bind_ids(fail, resolve, out);
      if let Cont::Expr(_, body) = cont { collect_bind_ids(body, resolve, out); }
    }
    MatchDone { fail, cont, .. } => {
      collect_bind_ids(fail, resolve, out);
      if let Cont::Expr(_, body) = cont { collect_bind_ids(body, resolve, out); }
    }
    MatchNotDone { fail, body, .. } => {
      collect_bind_ids(fail, resolve, out);
      collect_bind_ids(body, resolve, out);
    }
    MatchRest { fail, cont, .. } => {
      collect_bind_ids(fail, resolve, out);
      if let Cont::Expr(_, body) = cont { collect_bind_ids(body, resolve, out); }
    }
    MatchRec { val, fail, body, .. } => {
      scan_val(val, resolve, out);
      collect_bind_ids(fail, resolve, out);
      collect_bind_ids(body, resolve, out);
    }
    MatchField { fail, cont, .. } => {
      collect_bind_ids(fail, resolve, out);
      if let Cont::Expr(_, body) = cont { collect_bind_ids(body, resolve, out); }
    }
    MatchBlock { params, fail, arms, cont, .. } => {
      for v in params { scan_val(v, resolve, out); }
      collect_bind_ids(fail, resolve, out);
      for arm in arms { collect_bind_ids(arm, resolve, out); }
      if let Cont::Expr(_, body) = cont { collect_bind_ids(body, resolve, out); }
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
  if let ValKind::Ref(Ref::Name) = &val.kind
    && let Some(Some(Resolution::Captured { bind, .. })) = resolve.resolution.try_get(val.id)
  {
    // bind → bind: we just want the set of captured bind CpsIds
    out.entry(*bind).or_insert(*bind);
  }
}

/// Look up the bind CpsId for a captured name by scanning the fn body.
/// Returns the bind CpsId whose source name (via origin map + AST) matches `name`.
fn find_bind_for_capture<'src>(
  fn_body: &Expr<'src>,
  cap_name: &str,
  resolve: &ResolveResult,
  origin: &PropGraph<CpsId, Option<crate::ast::AstId>>,
  ast_index: &PropGraph<crate::ast::AstId, Option<&'src crate::ast::Node<'src>>>,
) -> Option<CpsId> {
  let mut found: Option<CpsId> = None;
  let mut map = std::collections::HashMap::new();
  collect_bind_ids(fn_body, resolve, &mut map);
  for &bind_id in map.keys() {
    if let Some(Some(ast_id)) = origin.try_get(bind_id)
      && let Some(Some(node)) = ast_index.try_get(*ast_id)
      && let crate::ast::NodeKind::Ident(s) = &node.kind
      && *s == cap_name
    {
      found = Some(bind_id);
      break;
    }
  }
  found
}

// ---------------------------------------------------------------------------
// Transform — walk and rewrite, bubble hoisted fns upward
// ---------------------------------------------------------------------------

fn lift_cont<'src>(
  cont: Cont<'src>,
  captures: &CaptureGraph<'src>,
  resolve: &ResolveResult,
  ast_index: &PropGraph<crate::ast::AstId, Option<&'src crate::ast::Node<'src>>>,
  alloc: &mut Alloc,
  hoisted: &mut Vec<HoistedFn<'src>>,
) -> Cont<'src> {
  match cont {
    Cont::Ref(val) => Cont::Ref(val),
    Cont::Expr(bind, body) => Cont::Expr(bind, Box::new(lift_expr(*body, captures, resolve, ast_index, alloc, hoisted))),
  }
}

fn lift_expr<'src>(
  expr: Expr<'src>,
  captures: &CaptureGraph<'src>,
  resolve: &ResolveResult,
  ast_index: &PropGraph<crate::ast::AstId, Option<&'src crate::ast::Node<'src>>>,
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
      let rewritten_fn_body = lift_expr(*fn_body, captures, resolve, ast_index, alloc, &mut inner_hoisted);
      let rewritten_body    = lift_expr(*body, captures, resolve, ast_index, alloc, hoisted);

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
        // 1. For each capture name (in order), find its bind CpsId so we can
        //    copy the AST origin. This keeps cap params and cap args in the
        //    same order as caps and gives them correct source names.
        let cap_bind_ids: Vec<Option<CpsId>> = caps.iter().map(|cap_name| {
          find_bind_for_capture(&rewritten_fn_body, cap_name, resolve, &alloc.origin, ast_index)
        }).collect();

        // 2. Build User bind nodes for capture params — carry forward the
        //    original binding's AST origin so the formatter renders the source name.
        let cap_params: Vec<Param> = cap_bind_ids.iter().map(|bind_id| {
          let ast_origin = bind_id
            .and_then(|id| alloc.origin.try_get(id))
            .and_then(|o| *o);
          Param::Name(alloc.bind(Bind::User, ast_origin))
        }).collect();

        // 3. Build Val refs for the capture args at the call site (outer scope).
        //    Same AST origin as the binding — the formatter renders the source name.
        let cap_ref_vals: Vec<Val<'src>> = cap_bind_ids.iter().map(|bind_id| {
          let ast_origin = bind_id
            .and_then(|id| alloc.origin.try_get(id))
            .and_then(|o| *o);
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
          cont: Cont::Expr(result_bind, Box::new(subst_body)),
        }, None)
      }
    }

    // Structural recursion for all other node kinds.
    LetVal { name, val, body } => Expr {
      id: expr.id,
      kind: LetVal {
        name,
        val,
        body: Box::new(lift_expr(*body, captures, resolve, ast_index, alloc, hoisted)),
      },
    },

    App { func, args, cont } => Expr {
      id: expr.id,
      kind: App {
        func,
        args,
        cont: lift_cont(cont, captures, resolve, ast_index, alloc, hoisted),
      },
    },

    If { cond, then, else_ } => Expr {
      id: expr.id,
      kind: If {
        cond,
        then: Box::new(lift_expr(*then, captures, resolve, ast_index, alloc, hoisted)),
        else_: Box::new(lift_expr(*else_, captures, resolve, ast_index, alloc, hoisted)),
      },
    },

    Yield { value, cont } => Expr {
      id: expr.id,
      kind: Yield {
        value,
        cont: lift_cont(cont, captures, resolve, ast_index, alloc, hoisted),
      },
    },

    LetRec { bindings, body } => {
      let new_bindings = bindings.into_iter().map(|b| {
        crate::passes::cps::ir::Binding {
          fn_body: Box::new(lift_expr(*b.fn_body, captures, resolve, ast_index, alloc, hoisted)),
          ..b
        }
      }).collect();
      Expr {
        id: expr.id,
        kind: LetRec {
          bindings: new_bindings,
          body: Box::new(lift_expr(*body, captures, resolve, ast_index, alloc, hoisted)),
        },
      }
    }

    MatchLetVal { name, val, fail, body } => Expr {
      id: expr.id,
      kind: MatchLetVal {
        name, val,
        fail: Box::new(lift_expr(*fail, captures, resolve, ast_index, alloc, hoisted)),
        body: Box::new(lift_expr(*body, captures, resolve, ast_index, alloc, hoisted)),
      },
    },

    MatchApp { func, args, fail, cont } => Expr {
      id: expr.id,
      kind: MatchApp {
        func, args,
        fail: Box::new(lift_expr(*fail, captures, resolve, ast_index, alloc, hoisted)),
        cont: lift_cont(cont, captures, resolve, ast_index, alloc, hoisted),
      },
    },

    MatchIf { func, args, fail, body } => Expr {
      id: expr.id,
      kind: MatchIf {
        func, args,
        fail: Box::new(lift_expr(*fail, captures, resolve, ast_index, alloc, hoisted)),
        body: Box::new(lift_expr(*body, captures, resolve, ast_index, alloc, hoisted)),
      },
    },

    MatchValue { val, lit, fail, body } => Expr {
      id: expr.id,
      kind: MatchValue {
        val, lit,
        fail: Box::new(lift_expr(*fail, captures, resolve, ast_index, alloc, hoisted)),
        body: Box::new(lift_expr(*body, captures, resolve, ast_index, alloc, hoisted)),
      },
    },

    MatchSeq { val, cursor, fail, body } => Expr {
      id: expr.id,
      kind: MatchSeq {
        val, cursor,
        fail: Box::new(lift_expr(*fail, captures, resolve, ast_index, alloc, hoisted)),
        body: Box::new(lift_expr(*body, captures, resolve, ast_index, alloc, hoisted)),
      },
    },

    MatchNext { val, cursor, next_cursor, fail, cont } => Expr {
      id: expr.id,
      kind: MatchNext {
        val, cursor, next_cursor,
        fail: Box::new(lift_expr(*fail, captures, resolve, ast_index, alloc, hoisted)),
        cont: lift_cont(cont, captures, resolve, ast_index, alloc, hoisted),
      },
    },

    MatchDone { val, cursor, fail, cont } => Expr {
      id: expr.id,
      kind: MatchDone {
        val, cursor,
        fail: Box::new(lift_expr(*fail, captures, resolve, ast_index, alloc, hoisted)),
        cont: lift_cont(cont, captures, resolve, ast_index, alloc, hoisted),
      },
    },

    MatchNotDone { val, cursor, fail, body } => Expr {
      id: expr.id,
      kind: MatchNotDone {
        val, cursor,
        fail: Box::new(lift_expr(*fail, captures, resolve, ast_index, alloc, hoisted)),
        body: Box::new(lift_expr(*body, captures, resolve, ast_index, alloc, hoisted)),
      },
    },

    MatchRest { val, cursor, fail, cont } => Expr {
      id: expr.id,
      kind: MatchRest {
        val, cursor,
        fail: Box::new(lift_expr(*fail, captures, resolve, ast_index, alloc, hoisted)),
        cont: lift_cont(cont, captures, resolve, ast_index, alloc, hoisted),
      },
    },

    MatchRec { val, cursor, fail, body } => Expr {
      id: expr.id,
      kind: MatchRec {
        val, cursor,
        fail: Box::new(lift_expr(*fail, captures, resolve, ast_index, alloc, hoisted)),
        body: Box::new(lift_expr(*body, captures, resolve, ast_index, alloc, hoisted)),
      },
    },

    MatchField { val, cursor, next_cursor, field, fail, cont } => Expr {
      id: expr.id,
      kind: MatchField {
        val, cursor, next_cursor, field,
        fail: Box::new(lift_expr(*fail, captures, resolve, ast_index, alloc, hoisted)),
        cont: lift_cont(cont, captures, resolve, ast_index, alloc, hoisted),
      },
    },

    MatchBlock { params, fail, arm_params, arms, cont } => {
      let new_arms = arms.into_iter()
        .map(|a| lift_expr(a, captures, resolve, ast_index, alloc, hoisted))
        .collect();
      Expr {
        id: expr.id,
        kind: MatchBlock {
          params,
          arm_params,
          fail: Box::new(lift_expr(*fail, captures, resolve, ast_index, alloc, hoisted)),
          arms: new_arms,
          cont: lift_cont(cont, captures, resolve, ast_index, alloc, hoisted),
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

  use crate::ast::{build_index, NodeKind};
  use crate::parser::parse;
  use crate::passes::cps::fmt::{fmt_with, Ctx};
  use crate::passes::cps::ir::{Arg, Callable, Cont, Expr, ExprKind, Val, ValKind, Ref};
  use crate::passes::name_res::{Resolution, ResolveResult};
  use crate::passes::cps::transform::lower_expr;
  use crate::propgraph::PropGraph;
  use crate::ast::{AstId, Node as AstNode};
  use crate::passes::cps::ir::CpsId;
  use super::lift_all;

  /// Run the full pipeline (parse → CPS → lift_all) on `src` and return the
  /// rewritten CPS IR formatted as Fink source.
  fn closure_lift(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let cps = lower_expr(&r.root);
        let (lifted, _) = lift_all(cps, &ast_index);
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

  /// Run the full pipeline then return the final name resolution result
  /// as classified resolution lines (same format as `cps_resolve` tests).
  fn closure_lift_resolve(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let cps = lower_expr(&r.root);
        let (lifted, lifted_resolve) = lift_all(cps, &ast_index);
        fmt_classified(&lifted.root, &lifted_resolve, &lifted.origin, &ast_index)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  // ---------------------------------------------------------------------------
  // Resolution formatter — duplicated from name_res tests (private there)
  // ---------------------------------------------------------------------------

  fn source_name<'src>(
    cps_id: CpsId,
    origin: &PropGraph<CpsId, Option<AstId>>,
    ast_index: &PropGraph<AstId, Option<&'src AstNode<'src>>>,
  ) -> Option<&'src str> {
    let ast_id = (*origin.try_get(cps_id)?)?;
    let node = (*ast_index.try_get(ast_id)?)?;
    match &node.kind {
      NodeKind::Ident(s) => Some(s),
      _ => None,
    }
  }

  fn emit_val<'src>(
    val: &Val<'src>,
    result: &ResolveResult,
    origin: &PropGraph<CpsId, Option<AstId>>,
    ast_index: &PropGraph<AstId, Option<&'src AstNode<'src>>>,
    out: &mut Vec<String>,
  ) {
    if let ValKind::Ref(Ref::Name) = &val.kind {
      let ref_name = source_name(val.id, origin, ast_index).unwrap_or("?");
      match result.resolution.try_get(val.id) {
        Some(Some(Resolution::Local(bind_id))) => {
          let bind_name = source_name(*bind_id, origin, ast_index).unwrap_or("?");
          let scope = result.bind_scope.try_get(*bind_id)
            .and_then(|s| *s).map(|s| s.0).unwrap_or(0);
          out.push(format!(
            "(ref {}, {}) == (local (bind {}, {})) in scope {}",
            val.id.0, ref_name, bind_id.0, bind_name, scope
          ));
        }
        Some(Some(Resolution::Captured { bind, depth })) => {
          let bind_name = source_name(*bind, origin, ast_index).unwrap_or("?");
          let scope = result.bind_scope.try_get(*bind)
            .and_then(|s| *s).map(|s| s.0).unwrap_or(0);
          out.push(format!(
            "(ref {}, {}) == (captured {}, (bind {}, {})) in scope {}",
            val.id.0, ref_name, depth, bind.0, bind_name, scope
          ));
        }
        Some(Some(Resolution::Recursive(bind_id))) => {
          let bind_name = source_name(*bind_id, origin, ast_index).unwrap_or("?");
          let scope = result.bind_scope.try_get(*bind_id)
            .and_then(|s| *s).map(|s| s.0).unwrap_or(0);
          out.push(format!(
            "(ref {}, {}) == (recursive (bind {}, {})) in scope {}",
            val.id.0, ref_name, bind_id.0, bind_name, scope
          ));
        }
        Some(Some(Resolution::Unresolved)) | Some(None) | None => {
          out.push(format!("(ref {}, {}) == unresolved", val.id.0, ref_name));
        }
      }
    }
  }

  fn emit_callable<'src>(
    callable: &Callable<'src>,
    result: &ResolveResult,
    origin: &PropGraph<CpsId, Option<AstId>>,
    ast_index: &PropGraph<AstId, Option<&'src AstNode<'src>>>,
    out: &mut Vec<String>,
  ) {
    if let Callable::Val(val) = callable {
      emit_val(val, result, origin, ast_index, out);
    }
  }

  fn collect_lines<'src>(
    expr: &Expr<'src>,
    result: &ResolveResult,
    origin: &PropGraph<CpsId, Option<AstId>>,
    ast_index: &PropGraph<AstId, Option<&'src AstNode<'src>>>,
    out: &mut Vec<String>,
  ) {
    use ExprKind::*;
    match &expr.kind {
      Ret(val) => { emit_val(val, result, origin, ast_index, out); }
      LetVal { val, body, .. } => {
        emit_val(val, result, origin, ast_index, out);
        collect_lines(body, result, origin, ast_index, out);
      }
      LetFn { fn_body, body, .. } => {
        collect_lines(fn_body, result, origin, ast_index, out);
        collect_lines(body, result, origin, ast_index, out);
      }
      LetRec { bindings, body } => {
        for b in bindings { collect_lines(&b.fn_body, result, origin, ast_index, out); }
        collect_lines(body, result, origin, ast_index, out);
      }
      App { func, args, cont } => {
        emit_callable(func, result, origin, ast_index, out);
        for arg in args {
          match arg { Arg::Val(v) | Arg::Spread(v) => emit_val(v, result, origin, ast_index, out) }
        }
        if let Cont::Expr(_, body) = cont { collect_lines(body, result, origin, ast_index, out); }
      }
      If { cond, then, else_ } => {
        emit_val(cond, result, origin, ast_index, out);
        collect_lines(then, result, origin, ast_index, out);
        collect_lines(else_, result, origin, ast_index, out);
      }
      Yield { value, cont } => {
        emit_val(value, result, origin, ast_index, out);
        if let Cont::Expr(_, body) = cont { collect_lines(body, result, origin, ast_index, out); }
      }
      MatchLetVal { val, fail, body, .. } => {
        emit_val(val, result, origin, ast_index, out);
        collect_lines(fail, result, origin, ast_index, out);
        collect_lines(body, result, origin, ast_index, out);
      }
      MatchApp { func, args, fail, cont } => {
        emit_callable(func, result, origin, ast_index, out);
        for v in args { emit_val(v, result, origin, ast_index, out); }
        collect_lines(fail, result, origin, ast_index, out);
        if let Cont::Expr(_, body) = cont { collect_lines(body, result, origin, ast_index, out); }
      }
      MatchIf { func, args, fail, body } => {
        emit_callable(func, result, origin, ast_index, out);
        for v in args { emit_val(v, result, origin, ast_index, out); }
        collect_lines(fail, result, origin, ast_index, out);
        collect_lines(body, result, origin, ast_index, out);
      }
      MatchValue { val, fail, body, .. } => {
        emit_val(val, result, origin, ast_index, out);
        collect_lines(fail, result, origin, ast_index, out);
        collect_lines(body, result, origin, ast_index, out);
      }
      MatchSeq { val, fail, body, .. } => {
        emit_val(val, result, origin, ast_index, out);
        collect_lines(fail, result, origin, ast_index, out);
        collect_lines(body, result, origin, ast_index, out);
      }
      MatchNext { fail, cont, .. } => {
        collect_lines(fail, result, origin, ast_index, out);
        if let Cont::Expr(_, body) = cont { collect_lines(body, result, origin, ast_index, out); }
      }
      MatchDone { fail, cont, .. } => {
        collect_lines(fail, result, origin, ast_index, out);
        if let Cont::Expr(_, body) = cont { collect_lines(body, result, origin, ast_index, out); }
      }
      MatchNotDone { fail, body, .. } => {
        collect_lines(fail, result, origin, ast_index, out);
        collect_lines(body, result, origin, ast_index, out);
      }
      MatchRest { fail, cont, .. } => {
        collect_lines(fail, result, origin, ast_index, out);
        if let Cont::Expr(_, body) = cont { collect_lines(body, result, origin, ast_index, out); }
      }
      MatchRec { val, fail, body, .. } => {
        emit_val(val, result, origin, ast_index, out);
        collect_lines(fail, result, origin, ast_index, out);
        collect_lines(body, result, origin, ast_index, out);
      }
      MatchField { fail, cont, .. } => {
        collect_lines(fail, result, origin, ast_index, out);
        if let Cont::Expr(_, body) = cont { collect_lines(body, result, origin, ast_index, out); }
      }
      MatchBlock { params, fail, arms, cont, .. } => {
        for v in params { emit_val(v, result, origin, ast_index, out); }
        collect_lines(fail, result, origin, ast_index, out);
        for arm in arms { collect_lines(arm, result, origin, ast_index, out); }
        if let Cont::Expr(_, body) = cont { collect_lines(body, result, origin, ast_index, out); }
      }
      Panic | FailCont => {}
    }
  }

  fn fmt_classified<'src>(
    expr: &Expr<'src>,
    result: &ResolveResult,
    origin: &PropGraph<CpsId, Option<AstId>>,
    ast_index: &PropGraph<AstId, Option<&'src AstNode<'src>>>,
  ) -> String {
    let mut lines = Vec::new();
    collect_lines(expr, result, origin, ast_index, &mut lines);
    lines.join("\n")
  }

  include_fink_tests!("src/passes/closure_lifting/test_closure_lifting.fnk");
  include_fink_tests!("src/passes/closure_lifting/test_closure_lift_resolve.fnk");
}
