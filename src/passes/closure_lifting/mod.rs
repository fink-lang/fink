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

  fn synth_bind(&mut self) -> BindNode {
    self.bind(Bind::Synth, None)
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
  cont:    BindNode,
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
  captures: &CaptureGraph,
  ast_index: &PropGraph<crate::ast::AstId, Option<&'src crate::ast::Node<'src>>>,
) -> CpsResult<'src> {
  let mut alloc = Alloc::new(result.origin);
  let mut synth_alias = result.synth_alias;
  let mut hoisted: Vec<HoistedFn<'src>> = Vec::new();
  let new_root = lift_expr(result.root, captures, resolve, ast_index, &mut alloc, &mut hoisted, &mut synth_alias);
  let new_root = wrap_hoisted(new_root, hoisted, &mut alloc);
  CpsResult { root: new_root, origin: alloc.origin, synth_alias }
}

/// Returns true if any Ref::Name in the IR resolves as `Captured`.
/// Only checks Name refs — Synth captures are structural (from cont_lift
/// hoisting) and handled by codegen, not by closure lifting.
fn has_captures(root: &Expr<'_>, resolve: &ResolveResult) -> bool {
  has_name_captures(root, resolve)
}

fn has_name_captures(expr: &Expr<'_>, resolve: &ResolveResult) -> bool {
  use ExprKind::*;
  match &expr.kind {
    LetVal { val, body, .. } => {
      if is_name_captured(val, resolve) { return true; }
      if let Cont::Expr { body: b, .. } = body { return has_name_captures(b, resolve); }
      false
    }
    LetFn { fn_body, body, .. } => {
      if has_name_captures(fn_body, resolve) { return true; }
      if let Cont::Expr { body: b, .. } = body { return has_name_captures(b, resolve); }
      false
    }
    App { func, args } => {
      if let Callable::Val(v) = func
        && is_name_captured(v, resolve)
      { return true; }
      for arg in args {
        match arg {
          Arg::Val(v) | Arg::Spread(v) => { if is_name_captured(v, resolve) { return true; } }
          Arg::Cont(Cont::Expr { body, .. }) | Arg::Expr(body) => {
            if has_name_captures(body, resolve) { return true; }
          }
          _ => {}
        }
      }
      false
    }
    If { cond, then, else_ } => {
      is_name_captured(cond, resolve) || has_name_captures(then, resolve) || has_name_captures(else_, resolve)
    }
    Yield { value, cont } => {
      if is_name_captured(value, resolve) { return true; }
      if let Cont::Expr { body, .. } = cont { return has_name_captures(body, resolve); }
      false
    }
  }
}

fn is_name_captured(val: &Val<'_>, resolve: &ResolveResult) -> bool {
  matches!(
    (&val.kind, resolve.resolution.try_get(val.id)),
    (ValKind::Ref(Ref::Name), Some(Some(Resolution::Captured { .. })))
  )
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
  use crate::passes::cont_lifting::lift as cont_lift;
  use crate::passes::name_res::resolve;

  // Iterate cont_lifting + closure_lifting until no captures remain.
  // cont_lift before closure_lifting — hoists inline conts so closure_lifting
  // can see all named functions. Then iterate closure_lifting until no captures.
  // FnClosure Cont::Expr bindings from closure_lift stay inline — they're
  // value-binding conts, not computation boundaries. Codegen handles them
  // directly (via closure_fn / cap_param_fn maps).
  const MAX_ROUNDS: usize = 20;
  let mut current = cont_lift(cps);
  for round in 0..MAX_ROUNDS {
    let node_count = current.origin.len();
    let resolve_result = resolve(&current.root, &current.origin, ast_index, node_count, &current.synth_alias);
    if !has_captures(&current.root, &resolve_result) {
      return (current, resolve_result);
    }
    if round == MAX_ROUNDS - 1 {
      panic!("lift_all: did not converge after {MAX_ROUNDS} rounds");
    }
    let cap_graph = analyse(&current, &resolve_result);
    current = lift_once(current, &resolve_result, &cap_graph, ast_index);
  }
  unreachable!()
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
    let result_bind = alloc.synth_bind();
    expr = Expr {
      id: wrapper_id,
      kind: ExprKind::LetFn {
        name: h.name,
        params: h.params,
        cont: h.cont,
        fn_body: Box::new(h.fn_body),
        body: Cont::Expr { args: vec![result_bind], body: Box::new(expr) },
      },
    };
  }
  expr
}


fn lift_cont<'src>(
  cont: Cont<'src>,
  captures: &CaptureGraph,
  resolve: &ResolveResult,
  ast_index: &PropGraph<crate::ast::AstId, Option<&'src crate::ast::Node<'src>>>,
  alloc: &mut Alloc,
  hoisted: &mut Vec<HoistedFn<'src>>,
  synth_alias: &mut PropGraph<CpsId, Option<CpsId>>,
) -> Cont<'src> {
  match cont {
    Cont::Ref(val) => Cont::Ref(val),
    Cont::Expr { args, body } => Cont::Expr { args, body: Box::new(lift_expr(*body, captures, resolve, ast_index, alloc, hoisted, synth_alias)) },
  }
}

fn lift_expr<'src>(
  expr: Expr<'src>,
  captures: &CaptureGraph,
  resolve: &ResolveResult,
  ast_index: &PropGraph<crate::ast::AstId, Option<&'src crate::ast::Node<'src>>>,
  alloc: &mut Alloc,
  hoisted: &mut Vec<HoistedFn<'src>>,
  synth_alias: &mut PropGraph<CpsId, Option<CpsId>>,
) -> Expr<'src> {
  use ExprKind::*;
  match expr.kind {
    LetFn { name, params, cont, fn_body, body } => {
      let cap_entries: Vec<(CpsId, Bind)> = captures.try_get(name.id)
        .cloned()
        .unwrap_or_default();
      let cap_bind_ids: Vec<CpsId> = cap_entries.iter().map(|(id, _)| *id).collect();

      // Recurse into fn_body and body; collect their hoisted fns.
      let mut inner_hoisted: Vec<HoistedFn<'src>> = Vec::new();
      let rewritten_fn_body = lift_expr(*fn_body, captures, resolve, ast_index, alloc, &mut inner_hoisted, synth_alias);
      let rewritten_body    = lift_cont(body, captures, resolve, ast_index, alloc, hoisted, synth_alias);

      if cap_bind_ids.is_empty() {
        // Pure function — bubble hoisted fns from fn_body upward to the outer scope.
        hoisted.extend(inner_hoisted);
        Expr {
          id: expr.id,
          kind: LetFn {
            name,
            params,
            cont,
            fn_body: Box::new(rewritten_fn_body),
            body: rewritten_body,
          },
        }
      } else {
        // Closure — hoist this fn to the outer scope.
        //
        // 1. Build bind nodes for capture params using the original bind kind.
        //    This ensures the WASM type matches (Cont → ref $Cont, others → anyref).
        let cap_params: Vec<Param> = cap_entries.iter().map(|(bind_id, bind_kind)| {
          let ast_origin = alloc.origin.try_get(*bind_id).and_then(|o| *o);
          let param = alloc.bind(*bind_kind, ast_origin);
          // For Synth/Cont captures, record the alias so name_res can resolve
          // Ref::Synth(old_bind_id) to this new param in the hoisted fn body.
          if *bind_kind != Bind::Name {
            let idx: usize = param.id.into();
            while synth_alias.len() <= idx { synth_alias.push(None); }
            synth_alias.set(param.id, Some(*bind_id));
          }
          Param::Name(param)
        }).collect();

        // 2. Build Val refs for the capture args at the call site (outer scope).
        //    Name captures use Ref::Name (resolved by name_res via source name).
        //    Synth/Cont captures use Ref::Synth(bind_id) (resolved via synths scope).
        let cap_ref_vals: Vec<Val<'src>> = cap_entries.iter().map(|(bind_id, bind_kind)| {
          let ast_origin = alloc.origin.try_get(*bind_id).and_then(|o| *o);
          if *bind_kind == Bind::Name {
            alloc.val(ValKind::Ref(Ref::Name), ast_origin)
          } else {
            alloc.val(ValKind::Ref(Ref::Synth(*bind_id)), None)
          }
        }).collect();

        // 4. Push the hoisted lifted fn; also bubble any fns hoisted from fn_body.
        //    All go to the outer scope (top level).
        hoisted.extend(inner_hoisted);
        let mut lifted_params = cap_params;
        lifted_params.extend(params);
        let lifted_fn_bind = alloc.synth_bind();
        let lifted_fn_id = lifted_fn_bind.id;

        let lifted_cont = alloc.bind(Bind::Cont, None);
        hoisted.push(HoistedFn {
          name:    lifted_fn_bind,
          params:  lifted_params,
          cont:    lifted_cont,
          fn_body: rewritten_fn_body,
        });

        // 5. At the original site: emit ·fn_closure call, bind result directly
        //    as the original closure name (no intermediate LetVal).
        let lifted_ref = alloc.val(ValKind::Ref(Ref::Synth(lifted_fn_id)), None);

        // The rewritten_body is a Cont (from the original LetFn body).
        // Prepend the closure name bind to its args so FnClosure binds the
        // closure value directly under the original name.
        let closure_cont = match rewritten_body {
          Cont::Expr { args: mut cont_args, body } => {
            cont_args.insert(0, name);
            Cont::Expr { args: cont_args, body }
          }
          Cont::Ref(id) => {
            // Body is just a Cont::Ref — wrap in Expr that forwards the closure value.
            let fwd_ref = alloc.val(ValKind::Ref(Ref::Synth(name.id)), None);
            let cont_val = alloc.val(ValKind::ContRef(id), None);
            let fwd_app = alloc.expr(ExprKind::App {
              func: Callable::Val(cont_val),
              args: vec![Arg::Val(fwd_ref)],
            }, None);
            Cont::Expr { args: vec![name], body: Box::new(fwd_app) }
          }
        };

        let mut fn_closure_args: Vec<Arg<'src>> = vec![Arg::Val(lifted_ref)];
        fn_closure_args.extend(cap_ref_vals.into_iter().map(Arg::Val));
        fn_closure_args.push(Arg::Cont(closure_cont));

        alloc.expr(ExprKind::App {
          func: Callable::BuiltIn(BuiltIn::FnClosure),
          args: fn_closure_args,
        }, None)
      }
    }

    // Structural recursion for all other node kinds.
    LetVal { name, val, body } => Expr {
      id: expr.id,
      kind: LetVal {
        name,
        val,
        body: lift_cont(body, captures, resolve, ast_index, alloc, hoisted, synth_alias),
      },
    },

    App { func, args } => {
      let args = args.into_iter().map(|a| match a {
        Arg::Cont(c) => Arg::Cont(lift_cont(c, captures, resolve, ast_index, alloc, hoisted, synth_alias)),
        other => other,
      }).collect();
      Expr { id: expr.id, kind: App { func, args } }
    }

    If { cond, then, else_ } => Expr {
      id: expr.id,
      kind: If {
        cond,
        then: Box::new(lift_expr(*then, captures, resolve, ast_index, alloc, hoisted, synth_alias)),
        else_: Box::new(lift_expr(*else_, captures, resolve, ast_index, alloc, hoisted, synth_alias)),
      },
    },

    Yield { value, cont } => Expr {
      id: expr.id,
      kind: Yield {
        value,
        cont: lift_cont(cont, captures, resolve, ast_index, alloc, hoisted, synth_alias),
      },
    },

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
        Some(Some(Resolution::Captured { bind, depth, .. })) => {
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
      LetVal { val, body, .. } => {
        emit_val(val, result, origin, ast_index, out);
        if let Cont::Expr { body: body_expr, .. } = body { collect_lines(body_expr, result, origin, ast_index, out); }
      }
      LetFn { fn_body, body, .. } => {
        collect_lines(fn_body, result, origin, ast_index, out);
        if let Cont::Expr { body: body_expr, .. } = body { collect_lines(body_expr, result, origin, ast_index, out); }
      }
      App { func, args } => {
        emit_callable(func, result, origin, ast_index, out);
        for arg in args {
          match arg {
            Arg::Val(v) | Arg::Spread(v) => emit_val(v, result, origin, ast_index, out),
            Arg::Cont(Cont::Expr { body, .. }) => collect_lines(body, result, origin, ast_index, out),
            Arg::Cont(_) | Arg::Expr(_) => {}
          }
        }
      }
      If { cond, then, else_ } => {
        emit_val(cond, result, origin, ast_index, out);
        collect_lines(then, result, origin, ast_index, out);
        collect_lines(else_, result, origin, ast_index, out);
      }
      Yield { value, cont } => {
        emit_val(value, result, origin, ast_index, out);
        if let Cont::Expr { body, .. } = cont { collect_lines(body, result, origin, ast_index, out); }
      }
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
