// Unified closure/continuation lifting pass.
//
// Replaces the separate cont_lifting and closure_lifting passes with a single
// iterative pass that lifts nested fns one level at a time, threading captured
// bindings as explicit params.
//
// ## Core invariant
//
// Before lifting a fn, answer: "if I move this fn one level up, which of its
// free variables would become out of scope?"
//
// Only variables bound by the immediate enclosing scope (siblings in the same
// LetFn/LetVal continuation chain) need to be threaded as params. Variables
// from parent scopes remain visible after a one-level lift.
//
// ## Algorithm
//
// 1. Run name resolution + capture analysis on the current tree.
// 2. Walk every LetFn fn_body: if it contains a nested LetFn, extract it
//    and place it as a sibling in the parent's cont chain.
//    - If the extracted fn has captures (refs to the enclosing fn's params),
//      add those as leading params and emit ·fn_closure at the call site.
//    - If pure, just move it.
// 3. Also hoist inline Cont::Expr bodies into named LetFn.
// 4. Repeat until no nested LetFn remain inside any fn_body.

use crate::ast::AstId;
use crate::passes::cps::ir::{
  Arg, Bind, BindNode, BuiltIn, Callable, Cont, CpsId, CpsResult, Expr, ExprKind,
  Param, Ref, Val, ValKind,
};
use crate::propgraph::PropGraph;
use crate::passes::name_res::{self, ResolveResult, Resolution};
use crate::passes::closure_capture::{self, CaptureGraph};

// ---------------------------------------------------------------------------
// Id allocator
// ---------------------------------------------------------------------------

struct Alloc {
  origin: PropGraph<CpsId, Option<AstId>>,
  synth_alias: PropGraph<CpsId, Option<CpsId>>,
}

impl Alloc {
  fn new(origin: PropGraph<CpsId, Option<AstId>>, synth_alias: PropGraph<CpsId, Option<CpsId>>) -> Self {
    Alloc { origin, synth_alias }
  }

  fn next(&mut self, ast_origin: Option<AstId>) -> CpsId {
    self.origin.push(ast_origin)
  }

  fn bind(&mut self, kind: Bind, ast_origin: Option<AstId>) -> BindNode {
    let id = self.next(ast_origin);
    BindNode { id, kind }
  }

  fn synth_bind(&mut self) -> BindNode {
    self.bind(Bind::Synth, None)
  }

  fn val<'src>(&mut self, kind: ValKind<'src>, ast_origin: Option<AstId>) -> Val<'src> {
    let id = self.next(ast_origin);
    Val { id, kind }
  }

  fn expr<'src>(&mut self, kind: ExprKind<'src>, ast_origin: Option<AstId>) -> Expr<'src> {
    let id = self.next(ast_origin);
    Expr { id, kind }
  }
}

// ---------------------------------------------------------------------------
// Hoisted fn — extracted from a fn_body, to be placed as a sibling
// ---------------------------------------------------------------------------

struct HoistedFn<'src> {
  name:    BindNode,
  params:  Vec<Param>,
  fn_body: Expr<'src>,
  /// The original cont args that bound the fn's result value.
  /// Used by wrap_hoisted to create the correct Cont::Expr binding.
  cont_args: Vec<BindNode>,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Lift all nested fns, one level at a time, until no nested LetFn remain
/// inside any fn_body. Returns the fully lifted CPS tree.
pub fn lift<'src>(
  result: CpsResult<'src>,
  ast_index: &PropGraph<AstId, Option<&'src crate::ast::Node<'src>>>,
) -> CpsResult<'src> {
  const MAX_ROUNDS: usize = 20;
  let mut current = result;

  for round in 0..MAX_ROUNDS {
    let node_count = current.origin.len();
    let resolve_result = name_res::resolve(
      &current.root, &current.origin, ast_index, node_count, &current.synth_alias,
    );
    let cap_graph = closure_capture::analyse(&current, &resolve_result);

    if !has_nested_letfn(&current.root) {
      return current;
    }

    if round == MAX_ROUNDS - 1 {
      panic!("lifting::lift: did not converge after {MAX_ROUNDS} rounds");
    }

    let mut alloc = Alloc::new(current.origin, current.synth_alias);
    let new_root = lift_expr(current.root, &cap_graph, &resolve_result, ast_index, &mut alloc);
    current = CpsResult {
      root: new_root,
      origin: alloc.origin,
      synth_alias: alloc.synth_alias,
    };
  }
  unreachable!()
}

// ---------------------------------------------------------------------------
// Check: does any fn_body contain a nested LetFn?
// ---------------------------------------------------------------------------

fn has_nested_letfn(expr: &Expr<'_>) -> bool {
  match &expr.kind {
    ExprKind::LetFn { fn_body, cont, .. } => {
      contains_letfn(fn_body) || cont_has_nested_letfn(cont)
    }
    ExprKind::LetVal { cont, .. } => cont_has_nested_letfn(cont),
    ExprKind::App { args, .. } => {
      args.iter().any(|a| match a {
        Arg::Cont(c) => cont_has_nested_letfn(c),
        Arg::Expr(e) => has_nested_letfn(e),
        _ => false,
      })
    }
    ExprKind::If { then, else_, .. } => has_nested_letfn(then) || has_nested_letfn(else_),
    ExprKind::Yield { cont, .. } => cont_has_nested_letfn(cont),
  }
}

fn cont_has_nested_letfn(cont: &Cont<'_>) -> bool {
  match cont {
    Cont::Ref(_) => false,
    Cont::Expr { body, .. } => has_nested_letfn(body),
  }
}

/// Does this expression (or its nested structure) contain a LetFn?
fn contains_letfn(expr: &Expr<'_>) -> bool {
  match &expr.kind {
    ExprKind::LetFn { .. } => true,
    ExprKind::LetVal { cont, .. } => match cont {
      Cont::Ref(_) => false,
      Cont::Expr { body, .. } => contains_letfn(body),
    },
    ExprKind::App { args, .. } => {
      args.iter().any(|a| match a {
        Arg::Cont(Cont::Expr { body, .. }) | Arg::Expr(body) => contains_letfn(body),
        _ => false,
      })
    }
    ExprKind::If { then, else_, .. } => contains_letfn(then) || contains_letfn(else_),
    ExprKind::Yield { cont, .. } => match cont {
      Cont::Ref(_) => false,
      Cont::Expr { body, .. } => contains_letfn(body),
    },
  }
}

// ---------------------------------------------------------------------------
// Lift one level: extract LetFn from fn_bodies
// ---------------------------------------------------------------------------

fn lift_expr<'src>(
  expr: Expr<'src>,
  captures: &CaptureGraph,
  resolve: &ResolveResult,
  ast_index: &PropGraph<AstId, Option<&'src crate::ast::Node<'src>>>,
  alloc: &mut Alloc,
) -> Expr<'src> {
  match expr.kind {
    ExprKind::LetFn { name, params, fn_body, cont } => {
      // Recurse into the cont first.
      let cont = lift_cont(cont, captures, resolve, ast_index, alloc);

      // If fn_body contains nested LetFn, extract them.
      if contains_letfn(&fn_body) {
        let mut hoisted: Vec<HoistedFn<'src>> = Vec::new();
        let new_fn_body = extract_from_body(*fn_body, &params, captures, resolve, ast_index, alloc, &mut hoisted);

        // Build the LetFn with the cleaned fn_body.
        let mut result = Expr {
          id: expr.id,
          kind: ExprKind::LetFn {
            name,
            params,
            fn_body: Box::new(new_fn_body),
            cont,
          },
        };

        // Wrap hoisted fns around the result (they become siblings).
        for h in hoisted.into_iter().rev() {
          let wrapper_id = alloc.next(None);
          result = Expr {
            id: wrapper_id,
            kind: ExprKind::LetFn {
              name: h.name,
              params: h.params,
              fn_body: Box::new(h.fn_body),
              cont: Cont::Expr { args: h.cont_args, body: Box::new(result) },
            },
          };
        }

        result
      } else {
        // No nested LetFn — just recurse into fn_body.
        let fn_body = lift_expr(*fn_body, captures, resolve, ast_index, alloc);
        Expr {
          id: expr.id,
          kind: ExprKind::LetFn { name, params, fn_body: Box::new(fn_body), cont },
        }
      }
    }

    ExprKind::LetVal { name, val, cont } => {
      let cont = lift_cont(cont, captures, resolve, ast_index, alloc);
      Expr { id: expr.id, kind: ExprKind::LetVal { name, val, cont } }
    }

    ExprKind::App { func, args } => {
      // Recurse into Arg::Cont and Arg::Expr.
      let args = args.into_iter().map(|a| match a {
        Arg::Cont(c) => Arg::Cont(lift_cont(c, captures, resolve, ast_index, alloc)),
        Arg::Expr(e) => Arg::Expr(Box::new(lift_expr(*e, captures, resolve, ast_index, alloc))),
        other => other,
      }).collect();
      Expr { id: expr.id, kind: ExprKind::App { func, args } }
    }

    ExprKind::If { cond, then, else_ } => {
      let then = lift_expr(*then, captures, resolve, ast_index, alloc);
      let else_ = lift_expr(*else_, captures, resolve, ast_index, alloc);
      Expr { id: expr.id, kind: ExprKind::If { cond, then: Box::new(then), else_: Box::new(else_) } }
    }

    ExprKind::Yield { value, cont } => {
      let cont = lift_cont(cont, captures, resolve, ast_index, alloc);
      Expr { id: expr.id, kind: ExprKind::Yield { value, cont } }
    }
  }
}

fn lift_cont<'src>(
  cont: Cont<'src>,
  captures: &CaptureGraph,
  resolve: &ResolveResult,
  ast_index: &PropGraph<AstId, Option<&'src crate::ast::Node<'src>>>,
  alloc: &mut Alloc,
) -> Cont<'src> {
  match cont {
    Cont::Ref(_) => cont,
    Cont::Expr { args, body } => {
      let body = lift_expr(*body, captures, resolve, ast_index, alloc);
      Cont::Expr { args, body: Box::new(body) }
    }
  }
}

// ---------------------------------------------------------------------------
// Extract nested LetFn from a fn_body
// ---------------------------------------------------------------------------

/// Walk a fn_body expression. When we encounter a LetFn, extract it as a
/// hoisted fn and replace it with its cont body (so the original site just
/// uses the result value).
fn extract_from_body<'src>(
  expr: Expr<'src>,
  parent_params: &[Param],
  captures: &CaptureGraph,
  resolve: &ResolveResult,
  ast_index: &PropGraph<AstId, Option<&'src crate::ast::Node<'src>>>,
  alloc: &mut Alloc,
  hoisted: &mut Vec<HoistedFn<'src>>,
) -> Expr<'src> {
  match expr.kind {
    ExprKind::LetFn { name, params, fn_body, cont } => {
      // This LetFn is inside a fn_body — extract it.
      // First, recursively extract from its own fn_body.
      let inner_fn_body = if contains_letfn(&fn_body) {
        extract_from_body(*fn_body, &params, captures, resolve, ast_index, alloc, hoisted)
      } else {
        *fn_body
      };

      // Determine captures: refs in inner_fn_body that resolve to parent's params.
      let cap_entries = compute_captures_for_lift(&inner_fn_body, &params, parent_params, resolve, alloc);

      if cap_entries.is_empty() {
        // Pure fn — hoist directly, no ·fn_closure needed.
        match cont {
          Cont::Expr { args: cont_args, body } => {
            hoisted.push(HoistedFn {
              name,
              params,
              fn_body: inner_fn_body,
              cont_args,
            });
            // Continue with the cont body (·v_N is bound by wrap_hoisted's cont).
            extract_from_body(*body, parent_params, captures, resolve, ast_index, alloc, hoisted)
          }
          Cont::Ref(cont_id) => {
            let name_id = name.id;
            hoisted.push(HoistedFn {
              name,
              params,
              fn_body: inner_fn_body,
              cont_args: vec![alloc.synth_bind()],
            });
            // The original cont was Cont::Ref — a tail pass of the fn value to the cont.
            // Synthesize: cont_id fn_value  (pass the fn value to the continuation)
            let cont_val = alloc.val(ValKind::ContRef(cont_id), None);
            let fn_val = alloc.val(ValKind::Ref(Ref::Synth(name_id)), None);
            alloc.expr(ExprKind::App {
              func: Callable::Val(cont_val),
              args: vec![Arg::Val(fn_val)],
            }, None)
          },
        }
      } else {
        // Closure — add captures as leading params, emit ·fn_closure at call site.
        let mut lifted_params: Vec<Param> = Vec::new();
        let mut closure_args: Vec<Arg<'src>> = Vec::new();

        // Build the lifted fn ref.
        let lifted_fn_bind = alloc.synth_bind();
        let lifted_fn_id = lifted_fn_bind.id;

        // Build capture params and args.
        for (cap_id, cap_kind) in &cap_entries {
          let ast_origin = alloc.origin.try_get(*cap_id).and_then(|o| *o);
          let param_bind = alloc.bind(*cap_kind, ast_origin);
          // Record alias so name_res can resolve refs to the old id via the new param.
          if *cap_kind != Bind::Name {
            let idx: usize = param_bind.id.into();
            while alloc.synth_alias.len() <= idx { alloc.synth_alias.push(None); }
            alloc.synth_alias.set(param_bind.id, Some(*cap_id));
          }
          lifted_params.push(Param::Name(param_bind));

          // At call site, pass the captured value.
          let arg_val = if *cap_kind == Bind::Name {
            alloc.val(ValKind::Ref(Ref::Name), ast_origin)
          } else {
            alloc.val(ValKind::Ref(Ref::Synth(*cap_id)), None)
          };
          closure_args.push(Arg::Val(arg_val));
        }

        // Lifted fn has captures as leading params, then original params.
        lifted_params.extend(params);

        // Extract cont args for wrap_hoisted binding.
        let (cont_args_for_hoist, fn_closure_cont) = match cont {
          Cont::Expr { args: ca, body } => (ca, Cont::Expr { args: vec![alloc.synth_bind()], body }),
          Cont::Ref(_) => (vec![alloc.synth_bind()], cont),
        };

        hoisted.push(HoistedFn {
          name: lifted_fn_bind,
          params: lifted_params,
          fn_body: inner_fn_body,
          cont_args: cont_args_for_hoist,
        });

        // At the original site: ·fn_closure lifted_fn, cap_0, cap_1, ..., cont
        let lifted_ref = alloc.val(ValKind::Ref(Ref::Synth(lifted_fn_id)), None);
        closure_args.insert(0, Arg::Val(lifted_ref));
        closure_args.push(Arg::Cont(fn_closure_cont));

        alloc.expr(ExprKind::App {
          func: Callable::BuiltIn(BuiltIn::FnClosure),
          args: closure_args,
        }, None)
      }
    }

    // For non-LetFn nodes inside fn_body, just recurse.
    ExprKind::LetVal { name, val, cont } => {
      let cont = match cont {
        Cont::Ref(_) => cont,
        Cont::Expr { args, body } => {
          let body = extract_from_body(*body, parent_params, captures, resolve, ast_index, alloc, hoisted);
          Cont::Expr { args, body: Box::new(body) }
        }
      };
      Expr { id: expr.id, kind: ExprKind::LetVal { name, val, cont } }
    }

    ExprKind::App { func, args } => {
      let args = args.into_iter().map(|a| match a {
        Arg::Cont(Cont::Expr { args: cargs, body }) => {
          let body = extract_from_body(*body, parent_params, captures, resolve, ast_index, alloc, hoisted);
          Arg::Cont(Cont::Expr { args: cargs, body: Box::new(body) })
        }
        Arg::Expr(e) => {
          let e = extract_from_body(*e, parent_params, captures, resolve, ast_index, alloc, hoisted);
          Arg::Expr(Box::new(e))
        }
        other => other,
      }).collect();
      Expr { id: expr.id, kind: ExprKind::App { func, args } }
    }

    other => Expr { id: expr.id, kind: other },
  }
}

// ---------------------------------------------------------------------------
// Compute captures for a one-level lift
// ---------------------------------------------------------------------------

/// For a fn being lifted out of a parent fn_body, determine which of its
/// free variables are bound by the parent's params (and thus would become
/// out of scope after lifting).
fn compute_captures_for_lift(
  fn_body: &Expr<'_>,
  _fn_params: &[Param],
  parent_params: &[Param],
  resolve: &ResolveResult,
  _alloc: &mut Alloc,
) -> Vec<(CpsId, Bind)> {
  let parent_param_map: std::collections::HashMap<CpsId, Bind> = parent_params.iter()
    .map(|p| match p { Param::Name(b) | Param::Spread(b) => (b.id, b.kind) })
    .collect();

  let mut caps: Vec<(CpsId, Bind)> = Vec::new();
  let mut seen: std::collections::HashSet<CpsId> = std::collections::HashSet::new();
  collect_captured_refs(fn_body, &parent_param_map, resolve, &mut caps, &mut seen);
  caps
}

/// Walk an expression and collect refs that resolve to one of the parent's param ids.
fn collect_captured_refs(
  expr: &Expr<'_>,
  parent_ids: &std::collections::HashMap<CpsId, Bind>,
  resolve: &ResolveResult,
  out: &mut Vec<(CpsId, Bind)>,
  seen: &mut std::collections::HashSet<CpsId>,
) {
  match &expr.kind {
    ExprKind::LetFn { fn_body, cont, .. } => {
      collect_captured_refs(fn_body, parent_ids, resolve, out, seen);
      collect_captured_refs_cont(cont, parent_ids, resolve, out, seen);
    }
    ExprKind::LetVal { val, cont, .. } => {
      collect_captured_refs_val(val, parent_ids, resolve, out, seen);
      collect_captured_refs_cont(cont, parent_ids, resolve, out, seen);
    }
    ExprKind::App { func, args } => {
      if let Callable::Val(v) = func {
        collect_captured_refs_val(v, parent_ids, resolve, out, seen);
      }
      for arg in args {
        match arg {
          Arg::Val(v) | Arg::Spread(v) => collect_captured_refs_val(v, parent_ids, resolve, out, seen),
          Arg::Cont(c) => collect_captured_refs_cont(c, parent_ids, resolve, out, seen),
          Arg::Expr(e) => collect_captured_refs(e, parent_ids, resolve, out, seen),
        }
      }
    }
    ExprKind::If { cond, then, else_ } => {
      collect_captured_refs_val(cond, parent_ids, resolve, out, seen);
      collect_captured_refs(then, parent_ids, resolve, out, seen);
      collect_captured_refs(else_, parent_ids, resolve, out, seen);
    }
    ExprKind::Yield { value, cont } => {
      collect_captured_refs_val(value, parent_ids, resolve, out, seen);
      collect_captured_refs_cont(cont, parent_ids, resolve, out, seen);
    }
  }
}

fn collect_captured_refs_cont(
  cont: &Cont<'_>,
  parent_ids: &std::collections::HashMap<CpsId, Bind>,
  resolve: &ResolveResult,
  out: &mut Vec<(CpsId, Bind)>,
  seen: &mut std::collections::HashSet<CpsId>,
) {
  match cont {
    Cont::Ref(id) => {
      if parent_ids.contains_key(id) && seen.insert(*id) {
        out.push((*id, Bind::Cont));
      }
    }
    Cont::Expr { body, .. } => collect_captured_refs(body, parent_ids, resolve, out, seen),
  }
}

fn collect_captured_refs_val(
  val: &Val<'_>,
  parent_ids: &std::collections::HashMap<CpsId, Bind>,
  resolve: &ResolveResult,
  out: &mut Vec<(CpsId, Bind)>,
  seen: &mut std::collections::HashSet<CpsId>,
) {
  // Check if this val resolves to a parent param.
  if let Some(Some(resolution)) = resolve.resolution.try_get(val.id) {
    let bind_id = match resolution {
      Resolution::Local(bind) | Resolution::Captured { bind, .. } => Some(*bind),
      Resolution::Recursive(bind) => Some(*bind),
      Resolution::Unresolved => None,
    };
    if let Some(bid) = bind_id {
      if let Some(&kind) = parent_ids.get(&bid) {
        if seen.insert(bid) {
          out.push((bid, kind));
        }
      }
    }
  }
  // Also check ContRef.
  if let ValKind::ContRef(id) = &val.kind {
    if parent_ids.contains_key(id) && seen.insert(*id) {
      out.push((*id, Bind::Cont));
    }
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use test_macros::include_fink_tests;

  use crate::ast::build_index;
  use crate::parser::parse;
  use crate::passes::cps::fmt::Ctx;
  use crate::passes::cps::transform::lower_expr;
  use crate::passes::cps_flat::fmt_flat;

  #[allow(unused)]
  fn lift(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let cps = lower_expr(&r.root);
        let lifted = super::lift(cps, &ast_index);
        let ctx = Ctx {
          origin: &lifted.origin,
          ast_index: &ast_index,
          captures: None,
        };
        fmt_flat(&lifted.root, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  include_fink_tests!("src/passes/lifting/test_lifting.fnk");
}
