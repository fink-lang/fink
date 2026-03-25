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

    if !needs_lifting(&current.root) {
      return current;
    }

    if round == MAX_ROUNDS - 1 {
      panic!("lifting::lift: did not converge after {MAX_ROUNDS} rounds");
    }

    // Extract LetFn from fn_bodies and hoist inline Cont::Expr (one level).
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
// Check: does the tree need more lifting?
// True if any fn_body contains a nested LetFn, or any App/Yield has an
// inline Cont::Expr with non-trivial body.
// ---------------------------------------------------------------------------

fn needs_lifting(expr: &Expr<'_>) -> bool {
  match &expr.kind {
    ExprKind::LetFn { fn_body, cont, name, .. } => {
      contains_letfn_or_inline_cont(fn_body) || cont_needs_lifting(cont)
    }
    ExprKind::LetVal { cont, .. } => cont_needs_lifting(cont),
    ExprKind::App { args, .. } => {
      args.iter().any(|a| match a {
        Arg::Cont(c) => app_cont_needs_lifting(c),
        Arg::Expr(e) => needs_lifting(e),
        _ => false,
      })
    }
    ExprKind::If { then, else_, .. } => needs_lifting(then) || needs_lifting(else_),
    ExprKind::Yield { cont, .. } => app_cont_needs_lifting(cont),
  }
}

fn cont_needs_lifting(cont: &Cont<'_>) -> bool {
  match cont {
    Cont::Ref(_) => false,
    Cont::Expr { body, .. } => needs_lifting(body),
  }
}

fn app_cont_needs_lifting(cont: &Cont<'_>) -> bool {
  match cont {
    Cont::Ref(_) => false,
    Cont::Expr { body, .. } if is_simple_forward(body) => false,
    Cont::Expr { body, .. } => needs_lifting(body),
  }
}

/// Does this expression contain a LetFn or an inline Cont::Expr in an App?
fn contains_letfn_or_inline_cont(expr: &Expr<'_>) -> bool {
  match &expr.kind {
    ExprKind::LetFn { .. } => true,
    ExprKind::LetVal { cont, .. } => match cont {
      Cont::Ref(_) => false,
      Cont::Expr { body, .. } => contains_letfn_or_inline_cont(body),
    },
    ExprKind::App { args, .. } => {
      args.iter().any(|a| match a {
        Arg::Cont(Cont::Expr { body, .. }) => !is_simple_forward(body),
        Arg::Expr(body) => contains_letfn_or_inline_cont(body),
        _ => false,
      })
    }
    ExprKind::If { then, else_, .. } => contains_letfn_or_inline_cont(then) || contains_letfn_or_inline_cont(else_),
    ExprKind::Yield { cont, .. } => matches!(cont, Cont::Expr { body, .. } if !is_simple_forward(body)),
  }
}

/// A "simple forward" is a single App with no nested LetFn or Cont::Expr.
/// These are trivial continuations like `fn v, k: k v` that don't need hoisting.
fn is_simple_forward(expr: &Expr<'_>) -> bool {
  match &expr.kind {
    ExprKind::App { args, .. } => {
      args.iter().all(|a| match a {
        Arg::Val(_) | Arg::Spread(_) => true,
        Arg::Cont(Cont::Ref(_)) => true,
        _ => false,
      })
    }
    _ => false,
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
// Phase 1: Name inline Cont::Expr bodies → convert to LetFn
//
// For each App { func, args } where an arg is Arg::Cont(Cont::Expr { args, body }),
// convert to: LetFn { name: fresh, params: args, fn_body: body,
//             cont: Cont::Expr { [synth], App { func, args with Cont::Ref(fresh) } } }
// ---------------------------------------------------------------------------

fn name_inline_conts<'src>(expr: Expr<'src>, alloc: &mut Alloc) -> Expr<'src> {
  match expr.kind {
    ExprKind::App { func, args } => {
      // Find the last Arg::Cont(Cont::Expr { .. }) — that's the inline cont to name.
      match args.iter().rposition(|a| matches!(a, Arg::Cont(Cont::Expr { .. }))) {
        Some(idx) => {
          let mut args = args;
          let cont = match args.remove(idx) {
            Arg::Cont(c) => c,
            _ => unreachable!(),
          };
          if let Cont::Expr { args: cont_args, body } = cont {
            let body = name_inline_conts(*body, alloc);
            let cont_name = alloc.bind(Bind::Cont, None);
            // Rebuild the App with Cont::Ref instead of Cont::Expr.
            args.insert(idx, Arg::Cont(Cont::Ref(cont_name.id)));
            let inner_app = name_inline_conts(alloc.expr(ExprKind::App { func, args }, None), alloc);
            Expr {
              id: expr.id,
              kind: ExprKind::LetFn {
                name: cont_name,
                params: cont_args.into_iter().map(Param::Name).collect(),
                fn_body: Box::new(body),
                cont: Cont::Expr {
                  args: vec![alloc.synth_bind()],
                  body: Box::new(inner_app),
                },
              },
            }
          } else {
            unreachable!()
          }
        }
        None => {
          // No inline conts — recurse into Arg::Expr.
          let args = args.into_iter().map(|a| match a {
            Arg::Expr(e) => Arg::Expr(Box::new(name_inline_conts(*e, alloc))),
            other => other,
          }).collect();
          Expr { id: expr.id, kind: ExprKind::App { func, args } }
        }
      }
    }

    ExprKind::Yield { value, cont } => {
      if let Cont::Expr { args: cont_args, body } = cont {
        let body = name_inline_conts(*body, alloc);
        let cont_name = alloc.bind(Bind::Cont, None);
        let inner_yield = alloc.expr(ExprKind::Yield {
          value,
          cont: Cont::Ref(cont_name.id),
        }, None);
        Expr {
          id: expr.id,
          kind: ExprKind::LetFn {
            name: cont_name,
            params: cont_args.into_iter().map(Param::Name).collect(),
            fn_body: Box::new(body),
            cont: Cont::Expr {
              args: vec![alloc.synth_bind()],
              body: Box::new(inner_yield),
            },
          },
        }
      } else {
        Expr { id: expr.id, kind: ExprKind::Yield { value, cont } }
      }
    }

    ExprKind::LetFn { name, params, fn_body, cont } => {
      let fn_body = name_inline_conts(*fn_body, alloc);
      let cont = name_inline_conts_cont(cont, alloc);
      Expr { id: expr.id, kind: ExprKind::LetFn { name, params, fn_body: Box::new(fn_body), cont } }
    }

    ExprKind::LetVal { name, val, cont } => {
      let cont = name_inline_conts_cont(cont, alloc);
      Expr { id: expr.id, kind: ExprKind::LetVal { name, val, cont } }
    }

    ExprKind::If { cond, then, else_ } => {
      let then = name_inline_conts(*then, alloc);
      let else_ = name_inline_conts(*else_, alloc);
      Expr { id: expr.id, kind: ExprKind::If { cond, then: Box::new(then), else_: Box::new(else_) } }
    }
  }
}

fn name_inline_conts_cont<'src>(cont: Cont<'src>, alloc: &mut Alloc) -> Cont<'src> {
  match cont {
    Cont::Ref(_) => cont,
    Cont::Expr { args, body } => {
      let body = name_inline_conts(*body, alloc);
      Cont::Expr { args, body: Box::new(body) }
    }
  }
}

// ---------------------------------------------------------------------------
// Phase 2: Lift one level — extract LetFn from fn_bodies
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

      // If fn_body contains nested LetFn or inline Cont::Expr, extract them.
      if contains_letfn_or_inline_cont(&fn_body) {
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
      // This LetFn is inside a fn_body — extract it one level.
      // Don't recurse into the extracted fn's own fn_body — the next
      // iteration handles deeper nesting (one level at a time).
      let inner_fn_body = *fn_body;

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
        let mut rewrite_map: std::collections::HashMap<CpsId, CpsId> = std::collections::HashMap::new();

        // Build the lifted fn ref.
        let lifted_fn_bind = alloc.synth_bind();
        let lifted_fn_id = lifted_fn_bind.id;

        // Build capture params and args.
        for (cap_id, cap_kind) in &cap_entries {
          let ast_origin = alloc.origin.try_get(*cap_id).and_then(|o| *o);
          let param_bind = alloc.bind(*cap_kind, ast_origin);
          rewrite_map.insert(*cap_id, param_bind.id);
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

        // Rewrite refs in the fn_body to use new capture param ids.
        let inner_fn_body = rewrite_refs(inner_fn_body, &rewrite_map);

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

    ExprKind::App { func, mut args } => {
      // Check for inline Cont::Expr args that need hoisting (skip simple forwards).
      let cont_idx = args.iter().rposition(|a| matches!(a, Arg::Cont(Cont::Expr { body, .. }) if !is_simple_forward(body)));
      if let Some(idx) = cont_idx {
        let cont = match args.remove(idx) {
          Arg::Cont(c) => c,
          _ => unreachable!(),
        };
        if let Cont::Expr { args: cont_args, body } = cont {
          let body = extract_from_body(*body, parent_params, captures, resolve, ast_index, alloc, hoisted);
          let cont_params: Vec<Param> = cont_args.into_iter().map(Param::Name).collect();
          let cap_entries = compute_captures_for_lift(&body, &cont_params, parent_params, resolve, alloc);

          if cap_entries.is_empty() {
            // Pure — hoist directly, replace with Cont::Ref.
            let cont_name = alloc.bind(Bind::Cont, None);
            let cont_name_id = cont_name.id;
            hoisted.push(HoistedFn {
              name: cont_name,
              params: cont_params,
              fn_body: body,
              cont_args: vec![alloc.synth_bind()],
            });
            args.insert(idx, Arg::Cont(Cont::Ref(cont_name_id)));
            // Recurse on remaining args.
            let args = args.into_iter().map(|a| match a {
              Arg::Expr(e) => Arg::Expr(Box::new(extract_from_body(*e, parent_params, captures, resolve, ast_index, alloc, hoisted))),
              other => other,
            }).collect();
            Expr { id: expr.id, kind: ExprKind::App { func, args } }
          } else {
            // Has captures — hoist the cont, wrap the App in ·fn_closure.
            let mut rewrite_map: std::collections::HashMap<CpsId, CpsId> = std::collections::HashMap::new();
            let lifted_fn_bind = alloc.synth_bind();
            let lifted_fn_id = lifted_fn_bind.id;
            let mut lifted_params: Vec<Param> = Vec::new();
            let mut closure_args: Vec<Arg<'src>> = Vec::new();
            for (cap_id, cap_kind) in &cap_entries {
              let ast_origin = alloc.origin.try_get(*cap_id).and_then(|o| *o);
              let param_bind = alloc.bind(*cap_kind, ast_origin);
              rewrite_map.insert(*cap_id, param_bind.id);
              if *cap_kind != Bind::Name {
                let idx: usize = param_bind.id.into();
                while alloc.synth_alias.len() <= idx { alloc.synth_alias.push(None); }
                alloc.synth_alias.set(param_bind.id, Some(*cap_id));
              }
              lifted_params.push(Param::Name(param_bind));
              let arg_val = if *cap_kind == Bind::Name {
                alloc.val(ValKind::Ref(Ref::Name), ast_origin)
              } else {
                alloc.val(ValKind::Ref(Ref::Synth(*cap_id)), None)
              };
              closure_args.push(Arg::Val(arg_val));
            }
            let body = rewrite_refs(body, &rewrite_map);
            lifted_params.extend(cont_params);
            hoisted.push(HoistedFn {
              name: lifted_fn_bind,
              params: lifted_params,
              fn_body: body,
              cont_args: vec![alloc.synth_bind()],
            });
            // Wrap: ·fn_closure(lifted_fn, caps..., fn closure_val: original_app(..., closure_val))
            let lifted_ref = alloc.val(ValKind::Ref(Ref::Synth(lifted_fn_id)), None);
            closure_args.insert(0, Arg::Val(lifted_ref));
            let closure_result = alloc.bind(Bind::Synth, None);
            let closure_result_id = closure_result.id;
            // Rebuild the original App with Cont::Ref(closure_result_id) as the cont.
            args.insert(idx, Arg::Cont(Cont::Ref(closure_result_id)));
            let inner_app = alloc.expr(ExprKind::App { func, args }, None);
            closure_args.push(Arg::Cont(Cont::Expr {
              args: vec![closure_result],
              body: Box::new(inner_app),
            }));
            alloc.expr(ExprKind::App {
              func: Callable::BuiltIn(BuiltIn::FnClosure),
              args: closure_args,
            }, None)
          }
        } else {
          unreachable!()
        }
      } else {
        // No inline conts — just recurse into Arg::Expr.
        let args = args.into_iter().map(|a| match a {
          Arg::Expr(e) => Arg::Expr(Box::new(extract_from_body(*e, parent_params, captures, resolve, ast_index, alloc, hoisted))),
          other => other,
        }).collect();
        Expr { id: expr.id, kind: ExprKind::App { func, args } }
      }
    }

    ExprKind::Yield { value, cont } => {
      if let Cont::Expr { args: cont_args, body } = cont {
        let body = extract_from_body(*body, parent_params, captures, resolve, ast_index, alloc, hoisted);
        let cont_params: Vec<Param> = cont_args.into_iter().map(Param::Name).collect();
        let cap_entries = compute_captures_for_lift(&body, &cont_params, parent_params, resolve, alloc);
        if cap_entries.is_empty() {
          let cont_name = alloc.bind(Bind::Cont, None);
          let cont_name_id = cont_name.id;
          hoisted.push(HoistedFn {
            name: cont_name,
            params: cont_params,
            fn_body: body,
            cont_args: vec![alloc.synth_bind()],
          });
          Expr { id: expr.id, kind: ExprKind::Yield { value, cont: Cont::Ref(cont_name_id) } }
        } else {
          // Captures — hoist with captures, wrap yield in ·fn_closure.
          let mut rewrite_map: std::collections::HashMap<CpsId, CpsId> = std::collections::HashMap::new();
          let lifted_fn_bind = alloc.synth_bind();
          let lifted_fn_id = lifted_fn_bind.id;
          let mut lifted_params: Vec<Param> = Vec::new();
          let mut closure_args: Vec<Arg<'src>> = Vec::new();
          for (cap_id, cap_kind) in &cap_entries {
            let ast_origin = alloc.origin.try_get(*cap_id).and_then(|o| *o);
            let param_bind = alloc.bind(*cap_kind, ast_origin);
            rewrite_map.insert(*cap_id, param_bind.id);
            if *cap_kind != Bind::Name {
              let idx: usize = param_bind.id.into();
              while alloc.synth_alias.len() <= idx { alloc.synth_alias.push(None); }
              alloc.synth_alias.set(param_bind.id, Some(*cap_id));
            }
            lifted_params.push(Param::Name(param_bind));
            let arg_val = if *cap_kind == Bind::Name {
              alloc.val(ValKind::Ref(Ref::Name), ast_origin)
            } else {
              alloc.val(ValKind::Ref(Ref::Synth(*cap_id)), None)
            };
            closure_args.push(Arg::Val(arg_val));
          }
          let body = rewrite_refs(body, &rewrite_map);
          lifted_params.extend(cont_params);
          hoisted.push(HoistedFn {
            name: lifted_fn_bind,
            params: lifted_params,
            fn_body: body,
            cont_args: vec![alloc.synth_bind()],
          });
          let lifted_ref = alloc.val(ValKind::Ref(Ref::Synth(lifted_fn_id)), None);
          closure_args.insert(0, Arg::Val(lifted_ref));
          let closure_result = alloc.bind(Bind::Synth, None);
          let closure_result_id = closure_result.id;
          let inner_yield = alloc.expr(ExprKind::Yield { value, cont: Cont::Ref(closure_result_id) }, None);
          closure_args.push(Arg::Cont(Cont::Expr {
            args: vec![closure_result],
            body: Box::new(inner_yield),
          }));
          alloc.expr(ExprKind::App {
            func: Callable::BuiltIn(BuiltIn::FnClosure),
            args: closure_args,
          }, None)
        }
      } else {
        Expr { id: expr.id, kind: ExprKind::Yield { value, cont } }
      }
    }

    ExprKind::If { cond, then, else_ } => {
      let then = extract_from_body(*then, parent_params, captures, resolve, ast_index, alloc, hoisted);
      let else_ = extract_from_body(*else_, parent_params, captures, resolve, ast_index, alloc, hoisted);
      Expr { id: expr.id, kind: ExprKind::If { cond, then: Box::new(then), else_: Box::new(else_) } }
    }

    other => Expr { id: expr.id, kind: other },
  }
}

// ---------------------------------------------------------------------------
// Rewrite refs in an expression tree using a CpsId → CpsId map
// ---------------------------------------------------------------------------

fn rewrite_refs<'src>(expr: Expr<'src>, map: &std::collections::HashMap<CpsId, CpsId>) -> Expr<'src> {
  match expr.kind {
    ExprKind::LetFn { name, params, fn_body, cont } => {
      let fn_body = rewrite_refs(*fn_body, map);
      let cont = rewrite_refs_cont(cont, map);
      Expr { id: expr.id, kind: ExprKind::LetFn { name, params, fn_body: Box::new(fn_body), cont } }
    }
    ExprKind::LetVal { name, val, cont } => {
      let val = rewrite_refs_val(*val, map);
      let cont = rewrite_refs_cont(cont, map);
      Expr { id: expr.id, kind: ExprKind::LetVal { name, val: Box::new(val), cont } }
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
      Expr { id: expr.id, kind: ExprKind::App { func, args } }
    }
    ExprKind::If { cond, then, else_ } => {
      let cond = rewrite_refs_val(*cond, map);
      let then = rewrite_refs(*then, map);
      let else_ = rewrite_refs(*else_, map);
      Expr { id: expr.id, kind: ExprKind::If { cond: Box::new(cond), then: Box::new(then), else_: Box::new(else_) } }
    }
    ExprKind::Yield { value, cont } => {
      let value = rewrite_refs_val(*value, map);
      let cont = rewrite_refs_cont(cont, map);
      Expr { id: expr.id, kind: ExprKind::Yield { value: Box::new(value), cont } }
    }
  }
}

fn rewrite_refs_val<'src>(val: Val<'src>, map: &std::collections::HashMap<CpsId, CpsId>) -> Val<'src> {
  match &val.kind {
    ValKind::Ref(Ref::Synth(id)) => {
      if let Some(&new_id) = map.get(id) {
        Val { id: val.id, kind: ValKind::Ref(Ref::Synth(new_id)) }
      } else {
        val
      }
    }
    ValKind::ContRef(id) => {
      if let Some(&new_id) = map.get(id) {
        Val { id: val.id, kind: ValKind::ContRef(new_id) }
      } else {
        val
      }
    }
    _ => val,
  }
}

fn rewrite_refs_cont<'src>(cont: Cont<'src>, map: &std::collections::HashMap<CpsId, CpsId>) -> Cont<'src> {
  match cont {
    Cont::Ref(id) => {
      if let Some(&new_id) = map.get(&id) {
        Cont::Ref(new_id)
      } else {
        Cont::Ref(id)
      }
    }
    Cont::Expr { args, body } => {
      let body = rewrite_refs(*body, map);
      Cont::Expr { args, body: Box::new(body) }
    }
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
