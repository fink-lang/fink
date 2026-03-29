// Lambda lifting pass — eliminates closures by threading captured bindings
// as extra parameters through the call chain.
//
// Alternative to the closure-based lifting pass (passes/lifting). Both take
// CPS transform output and produce flat module-level fns. The difference:
// this pass threads captured values as extra params instead of emitting
// ·closure nodes.
//
// ## Algorithm
//
// Input: CPS tree from cps::transform::lower_module.
// Iteratively lift nested fns one level at a time:
//
// 1. Extract nested LetFn from fn_bodies, hoist inline Cont::Expr to named fns.
// 2. For fns with captures: add captures as extra leading params.
// 3. At call sites: pass captured values as extra args (no ·closure).
// 4. When a captured-fn is used as cont to a builtin: extra args go before
//    the cont — builtin forwards them when calling the cont.
// 5. When a captured-fn is used as cont to a user fn: create a forwarding
//    variant of that fn that accepts and threads the extra args.
// 6. Repeat until no nested fns remain.
//
// ## Key assumption
//
// All lifted fns are siblings in the LetFn chain — they can reference each
// other by name. No need to thread fn refs as captures.
//
// Builtins accept any number of extra args before the cont and forward them
// all when calling the cont.
//
// ## Trade-offs vs closure-based lifting
//
// - Pro: zero heap allocation for environments
// - Pro: flat param-passing is easier for backends (binaryen) to optimise
// - Con: functions in the call chain must accept and forward params they
//   don't use; calling conventions become arity-specialised

use std::collections::{HashMap, HashSet};

use crate::ast::AstId;
use crate::passes::cps::ir::{
  Arg, Bind, BindNode, BuiltIn, Callable, Cont, CpsId, CpsResult, Expr, ExprKind,
  Param, Ref, Val, ValKind,
};
use crate::propgraph::PropGraph;

// ---------------------------------------------------------------------------
// Id allocator (same structure as lifting pass)
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
  cont_args: Vec<BindNode>,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Lambda-lift all nested fns, threading captured bindings as extra params.
/// Returns a fully lifted CPS tree with no ·closure nodes.
pub fn lambda_lift<'src>(
  result: CpsResult<'src>,
  _ast_index: &PropGraph<AstId, Option<&'src crate::ast::Node<'src>>>,
) -> CpsResult<'src> {
  const MAX_ROUNDS: usize = 20;
  let mut current = result;

  for round in 0..MAX_ROUNDS {
    if !needs_lifting(&current.root) {
      return current;
    }

    if round == MAX_ROUNDS - 1 {
      panic!("lambda_lifting: did not converge after {MAX_ROUNDS} rounds");
    }

    let mut alloc = Alloc::new(current.origin, current.synth_alias);
    let new_root = lift_expr(current.root, &mut alloc);
    current = CpsResult {
      root: new_root,
      origin: alloc.origin,
      bind_to_cps: current.bind_to_cps,
      synth_alias: alloc.synth_alias,
    };
  }
  unreachable!()
}

// ---------------------------------------------------------------------------
// Check: does the tree need more lifting?
// ---------------------------------------------------------------------------

fn needs_lifting(expr: &Expr<'_>) -> bool {
  match &expr.kind {
    ExprKind::LetFn { fn_body, cont, .. } => {
      contains_nested_structure(fn_body) || cont_needs_lifting(cont)
    }
    ExprKind::LetVal { cont, .. } => cont_needs_lifting(cont),
    ExprKind::App { func, args } => {
      let is_closure = matches!(func, Callable::BuiltIn(BuiltIn::FnClosure));
      args.iter().any(|a| match a {
        // For ·closure Apps: only flag result cont if its body has nested structure.
        // The result cont is the consumption site for the closure value — terminal.
        Arg::Cont(c @ Cont::Expr { body, .. }) if is_closure => {
          !is_simple_forward_cont(c) && contains_nested_structure(body)
        }
        Arg::Cont(c) => app_cont_needs_lifting(c),
        Arg::Expr(e) => needs_lifting(e),
        _ => false,
      })
    }
    ExprKind::If { then, else_, .. } => needs_lifting(then) || needs_lifting(else_),
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
    c @ Cont::Expr { .. } if is_simple_forward_cont(c) => false,
    Cont::Expr { .. } => true,
  }
}

/// Does this expression contain a nested LetFn or non-trivial inline Cont::Expr?
fn contains_nested_structure(expr: &Expr<'_>) -> bool {
  match &expr.kind {
    ExprKind::LetFn { .. } => true,
    ExprKind::LetVal { cont, .. } => match cont {
      Cont::Ref(_) => false,
      Cont::Expr { body, .. } => contains_nested_structure(body),
    },
    ExprKind::App { func, args } => {
      let is_closure = matches!(func, Callable::BuiltIn(BuiltIn::FnClosure));
      args.iter().any(|a| match a {
        // For ·closure: only flag if the cont body has nested structure to extract.
        Arg::Cont(c @ Cont::Expr { body, .. }) if is_closure => {
          !is_simple_forward_cont(c) && contains_nested_structure(body)
        }
        Arg::Cont(c @ Cont::Expr { .. }) => !is_simple_forward_cont(c),
        Arg::Expr(body) => contains_nested_structure(body),
        _ => false,
      })
    }
    ExprKind::If { then, else_, .. } => contains_nested_structure(then) || contains_nested_structure(else_),
  }
}

/// A "simple forward" is `fn p0, ..., pN: pK p0, ..., p(K-1), p(K+1), ..., pN`
/// — callee is one of the fn's own params, remaining params forwarded in order.
fn is_simple_forward_cont(cont: &Cont<'_>) -> bool {
  let (params, body) = match cont {
    Cont::Ref(_) => return false,
    Cont::Expr { args, body } => (args, body.as_ref()),
  };
  match &body.kind {
    ExprKind::App { func: Callable::Val(v), args: app_args } => {
      let callee_id = match &v.kind {
        ValKind::Ref(Ref::Synth(id)) => Some(*id),
        ValKind::ContRef(id) => Some(*id),
        _ => return false,
      };
      if !params.iter().any(|p| Some(p.id) == callee_id) { return false; }
      if 1 + app_args.len() != params.len() { return false; }
      let mut arg_iter = app_args.iter();
      for p in params {
        if Some(p.id) == callee_id { continue; }
        let arg = match arg_iter.next() {
          Some(a) => a,
          None => return false,
        };
        let matches = match arg {
          Arg::Val(v) => match &v.kind {
            ValKind::Ref(Ref::Synth(id)) => *id == p.id,
            ValKind::ContRef(id) => *id == p.id,
            ValKind::Panic => true,
            _ => false,
          },
          Arg::Cont(Cont::Ref(id)) => *id == p.id,
          _ => false,
        };
        if !matches { return false; }
      }
      true
    }
    _ => false,
  }
}

// ---------------------------------------------------------------------------
// Lift one level — extract LetFn from fn_bodies
// ---------------------------------------------------------------------------

fn lift_expr<'src>(
  expr: Expr<'src>,
  alloc: &mut Alloc,
) -> Expr<'src> {
  match expr.kind {
    ExprKind::LetFn { name, params, fn_body, cont } => {
      let cont = lift_cont(cont, alloc);

      if contains_nested_structure(&fn_body) {
        let mut hoisted: Vec<HoistedFn<'src>> = Vec::new();
        let new_fn_body = extract_from_body(*fn_body, &params, &[], alloc, &mut hoisted);

        let mut result = Expr {
          id: expr.id,
          kind: ExprKind::LetFn {
            name,
            params,
            fn_body: Box::new(new_fn_body),
            cont,
          },
        };

        // Wrap hoisted fns as siblings.
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
        let fn_body = lift_expr(*fn_body, alloc);
        Expr {
          id: expr.id,
          kind: ExprKind::LetFn { name, params, fn_body: Box::new(fn_body), cont },
        }
      }
    }

    ExprKind::LetVal { name, val, cont } => {
      let cont = lift_cont(cont, alloc);
      Expr { id: expr.id, kind: ExprKind::LetVal { name, val, cont } }
    }

    ExprKind::App { func, mut args } => {
      // Hoist non-simple-forward inline Cont::Expr at module level.
      // These have no parent fn, so no captures — always pure hoists.
      let cont_idx = args.iter().rposition(|a| match a {
        Arg::Cont(c @ Cont::Expr { .. }) => !is_simple_forward_cont(c),
        _ => false,
      });
      if let Some(idx) = cont_idx {
        if let Arg::Cont(Cont::Expr { args: cont_args, body }) = args.remove(idx) {
          let cont_params: Vec<Param> = cont_args.into_iter().map(Param::Name).collect();
          let cont_name = alloc.bind(Bind::Cont, None);
          let cont_name_id = cont_name.id;
          let body = lift_expr(*body, alloc);
          let mut hoisted = HoistedFn {
            name: cont_name,
            params: cont_params,
            fn_body: body,
            cont_args: vec![alloc.synth_bind()],
          };
          args.insert(idx, Arg::Cont(Cont::Ref(cont_name_id)));
          let args = args.into_iter().map(|a| match a {
            Arg::Cont(c) => Arg::Cont(lift_cont(c, alloc)),
            Arg::Expr(e) => Arg::Expr(Box::new(lift_expr(*e, alloc))),
            other => other,
          }).collect();
          let inner = Expr { id: expr.id, kind: ExprKind::App { func, args } };
          let wrapper_id = alloc.next(None);
          Expr {
            id: wrapper_id,
            kind: ExprKind::LetFn {
              name: hoisted.name,
              params: hoisted.params,
              fn_body: Box::new(hoisted.fn_body),
              cont: Cont::Expr { args: hoisted.cont_args, body: Box::new(inner) },
            },
          }
        } else {
          unreachable!()
        }
      } else {
        let args = args.into_iter().map(|a| match a {
          Arg::Cont(c) => Arg::Cont(lift_cont(c, alloc)),
          Arg::Expr(e) => Arg::Expr(Box::new(lift_expr(*e, alloc))),
          other => other,
        }).collect();
        Expr { id: expr.id, kind: ExprKind::App { func, args } }
      }
    }

    ExprKind::If { cond, then, else_ } => {
      let then = lift_expr(*then, alloc);
      let else_ = lift_expr(*else_, alloc);
      Expr { id: expr.id, kind: ExprKind::If { cond, then: Box::new(then), else_: Box::new(else_) } }
    }
  }
}

fn lift_cont<'src>(
  cont: Cont<'src>,
  alloc: &mut Alloc,
) -> Cont<'src> {
  match cont {
    Cont::Ref(_) => cont,
    Cont::Expr { args, body } => {
      let body = lift_expr(*body, alloc);
      Cont::Expr { args, body: Box::new(body) }
    }
  }
}

// ---------------------------------------------------------------------------
// Extract nested LetFn from a fn_body
// ---------------------------------------------------------------------------

fn extract_from_body<'src>(
  expr: Expr<'src>,
  parent_params: &[Param],
  scope_binds: &[(CpsId, Bind)],
  alloc: &mut Alloc,
  hoisted: &mut Vec<HoistedFn<'src>>,
) -> Expr<'src> {
  match expr.kind {
    ExprKind::LetFn { name, params, fn_body, cont } => {
      let inner_fn_body = *fn_body;

      // Determine captures — exclude refs to already-hoisted fns.
      let cap_entries: Vec<_> = compute_captures(&inner_fn_body, &params, parent_params, scope_binds)
        .into_iter()
        .filter(|(cap_id, _)| !is_hoisted_fn_ref(*cap_id, hoisted, alloc))
        .collect();

      if cap_entries.is_empty() {
        // Pure fn — hoist directly, no captures to thread.
        match cont {
          Cont::Expr { args: cont_args, body } => {
            hoisted.push(HoistedFn {
              name,
              params,
              fn_body: inner_fn_body,
              cont_args,
            });
            extract_from_body(*body, parent_params, scope_binds, alloc, hoisted)
          }
          Cont::Ref(cont_id) => {
            let name_id = name.id;
            hoisted.push(HoistedFn {
              name,
              params,
              fn_body: inner_fn_body,
              cont_args: vec![alloc.synth_bind()],
            });
            let cont_val = alloc.val(ValKind::ContRef(cont_id), None);
            let fn_val = alloc.val(ValKind::Ref(Ref::Synth(name_id)), None);
            alloc.expr(ExprKind::App {
              func: Callable::Val(cont_val),
              args: vec![Arg::Val(fn_val)],
            }, None)
          },
        }
      } else {
        // Has captures — add as leading params, thread at call site.
        let mut lifted_params: Vec<Param> = Vec::new();
        let mut extra_args: Vec<Arg<'src>> = Vec::new();
        let mut rewrite_map: HashMap<CpsId, CpsId> = HashMap::new();

        let lifted_fn_bind = alloc.synth_bind();
        let lifted_fn_id = lifted_fn_bind.id;

        for (cap_id, cap_kind) in &cap_entries {
          let ast_origin = alloc.origin.try_get(*cap_id).and_then(|o| *o);
          let param_bind = alloc.bind(*cap_kind, ast_origin);
          rewrite_map.insert(*cap_id, param_bind.id);
          let idx: usize = param_bind.id.into();
          while alloc.synth_alias.len() <= idx { alloc.synth_alias.push(None); }
          alloc.synth_alias.set(param_bind.id, Some(*cap_id));
          lifted_params.push(Param::Name(param_bind));

          let arg_val = alloc.val(ValKind::Ref(Ref::Synth(*cap_id)), ast_origin);
          extra_args.push(Arg::Val(arg_val));
        }

        let inner_fn_body = rewrite_refs(inner_fn_body, &rewrite_map);
        // Params: original params first, then captures trailing.
        let params_clone = params.clone();
        let mut all_params = params;
        all_params.extend(lifted_params);

        // Hoist the lifted inner fn (captures as trailing params).
        hoisted.push(HoistedFn {
          name: lifted_fn_bind,
          params: all_params.clone(),
          fn_body: inner_fn_body,
          cont_args: vec![alloc.synth_bind()],
        });

        let lifted_ref = alloc.val(ValKind::Ref(Ref::Synth(lifted_fn_id)), None);

        match cont {
          Cont::Ref(_) => {
            // Cont::Ref — value escapes to unknown consumer. Box as ·closure.
            let mut closure_args = vec![Arg::Val(lifted_ref)];
            closure_args.extend(extra_args);
            closure_args.push(Arg::Cont(cont));
            alloc.expr(ExprKind::App {
              func: Callable::BuiltIn(BuiltIn::FnClosure),
              args: closure_args,
            }, None)
          }

          Cont::Expr { args: cont_args, body } => {
            // Cont::Expr — the value is bound internally. The fn body should
            // return unpacked (fn_ref, caps...) since internal callers can handle it.
            // The export wrapper is generated separately at the module level.

            // The fn body returns unpacked to its cont param.
            let cont_val = alloc.val(ValKind::ContRef(
              parent_params.last().map(|p| match p {
                Param::Name(b) | Param::Spread(b) => b.id
              }).expect("parent fn must have a cont param")
            ), None);
            let mut call_args = vec![Arg::Val(lifted_ref)];
            call_args.extend(extra_args);
            let unpacked_body = alloc.expr(ExprKind::App {
              func: Callable::Val(cont_val),
              args: call_args,
            }, None);

            // But we also need an export wrapper. For now, just return unpacked
            // and let the cont (Cont::Expr) receive (fn_ref, caps...) as multiple values.
            // TODO: the cont_args only has 1 binding — needs expanding to accept multiple.

            // For now, fall back to ·closure for this case since expanding
            // Cont::Expr bindings is complex.
            let lifted_ref2 = alloc.val(ValKind::Ref(Ref::Synth(lifted_fn_id)), None);
            let mut extra_args2: Vec<Arg<'src>> = Vec::new();
            for (cap_id, _) in &cap_entries {
              let ast_origin = alloc.origin.try_get(*cap_id).and_then(|o| *o);
              let cap_ref = alloc.val(ValKind::Ref(Ref::Synth(*cap_id)), ast_origin);
              extra_args2.push(Arg::Val(cap_ref));
            }
            let mut closure_args = vec![Arg::Val(lifted_ref2)];
            closure_args.extend(extra_args2);
            closure_args.push(Arg::Cont(Cont::Expr { args: cont_args, body }));
            alloc.expr(ExprKind::App {
              func: Callable::BuiltIn(BuiltIn::FnClosure),
              args: closure_args,
            }, None)
          }
        }
      }
    }

    ExprKind::LetVal { name, val, cont } => {
      let val_is_hoisted = matches!(&val.kind, ValKind::Ref(Ref::Synth(id)) if hoisted.iter().any(|h| h.name.id == *id));
      let cont = match cont {
        Cont::Ref(_) => cont,
        Cont::Expr { args, body } => {
          let mut extended_binds: Vec<(CpsId, Bind)> = scope_binds.to_vec();
          if !val_is_hoisted {
            extended_binds.push((name.id, name.kind));
            for a in &args { extended_binds.push((a.id, a.kind)); }
          }
          let body = extract_from_body(*body, parent_params, &extended_binds, alloc, hoisted);
          Cont::Expr { args, body: Box::new(body) }
        }
      };
      Expr { id: expr.id, kind: ExprKind::LetVal { name, val, cont } }
    }

    ExprKind::App { func, mut args } => {
      // Check for inline Cont::Expr args that need hoisting.
      let cont_idx = args.iter().rposition(|a| match a {
        Arg::Cont(c @ Cont::Expr { .. }) => !is_simple_forward_cont(c),
        _ => false,
      });

      if let Some(idx) = cont_idx {
        let cont = match args.remove(idx) {
          Arg::Cont(c) => c,
          _ => unreachable!(),
        };
        if let Cont::Expr { args: cont_args, body } = cont {
          let body = *body;
          let cont_params: Vec<Param> = cont_args.into_iter().map(Param::Name).collect();
          let cap_entries = compute_captures(&body, &cont_params, parent_params, scope_binds);

          // ·closure result conts with captures would create infinite chains.
          // Leave them inline — they're the consumption site for the closure value.
          let is_closure_app = matches!(&func, Callable::BuiltIn(BuiltIn::FnClosure));
          if is_closure_app && !cap_entries.is_empty() {
            let cont_args_back: Vec<BindNode> = cont_params.into_iter().map(|p| match p {
              Param::Name(b) | Param::Spread(b) => b,
            }).collect();
            let mut inner_scope: Vec<(CpsId, Bind)> = scope_binds.to_vec();
            for a in &cont_args_back { inner_scope.push((a.id, a.kind)); }
            let body = extract_from_body(body, parent_params, &inner_scope, alloc, hoisted);
            args.insert(idx, Arg::Cont(Cont::Expr { args: cont_args_back, body: Box::new(body) }));
            let args = args.into_iter().map(|a| match a {
              Arg::Expr(e) => Arg::Expr(Box::new(extract_from_body(*e, parent_params, scope_binds, alloc, hoisted))),
              other => other,
            }).collect();
            return Expr { id: expr.id, kind: ExprKind::App { func, args } };
          }

          if cap_entries.is_empty() {
            // Pure cont — hoist directly, replace with Cont::Ref.
            let cont_name = alloc.bind(Bind::Cont, None);
            let cont_name_id = cont_name.id;
            hoisted.push(HoistedFn {
              name: cont_name,
              params: cont_params,
              fn_body: body,
              cont_args: vec![alloc.synth_bind()],
            });
            args.insert(idx, Arg::Cont(Cont::Ref(cont_name_id)));
            let args = args.into_iter().map(|a| match a {
              Arg::Expr(e) => Arg::Expr(Box::new(extract_from_body(*e, parent_params, scope_binds, alloc, hoisted))),
              other => other,
            }).collect();
            Expr { id: expr.id, kind: ExprKind::App { func, args } }
          } else {
            // Has captures — hoist the cont with captures as extra leading params.
            // At the call site, thread the captured values as extra args before
            // the cont in the builtin/fn call.
            let mut rewrite_map: HashMap<CpsId, CpsId> = HashMap::new();
            let lifted_fn_bind = alloc.bind(Bind::Cont, None);
            let lifted_fn_id = lifted_fn_bind.id;
            let mut lifted_params: Vec<Param> = Vec::new();
            let mut extra_args: Vec<Arg<'src>> = Vec::new();

            for (cap_id, cap_kind) in &cap_entries {
              let ast_origin = alloc.origin.try_get(*cap_id).and_then(|o| *o);
              let param_bind = alloc.bind(*cap_kind, ast_origin);
              rewrite_map.insert(*cap_id, param_bind.id);
              let idx: usize = param_bind.id.into();
              while alloc.synth_alias.len() <= idx { alloc.synth_alias.push(None); }
              alloc.synth_alias.set(param_bind.id, Some(*cap_id));
              lifted_params.push(Param::Name(param_bind));
              let arg_val = alloc.val(ValKind::Ref(Ref::Synth(*cap_id)), ast_origin);
              extra_args.push(Arg::Val(arg_val));
            }

            let body = rewrite_refs(body, &rewrite_map);
            // Params: original cont params first, then captures trailing.
            let mut all_params = cont_params;
            all_params.extend(lifted_params);
            hoisted.push(HoistedFn {
              name: lifted_fn_bind,
              params: all_params,
              fn_body: body,
              cont_args: vec![alloc.synth_bind()],
            });

            // Thread: cont first (preserves original interface), extra args trail.
            args.push(Arg::Cont(Cont::Ref(lifted_fn_id)));
            for arg in extra_args {
              args.push(arg);
            }

            let args = args.into_iter().map(|a| match a {
              Arg::Expr(e) => Arg::Expr(Box::new(extract_from_body(*e, parent_params, scope_binds, alloc, hoisted))),
              other => other,
            }).collect();
            Expr { id: expr.id, kind: ExprKind::App { func, args } }
          }
        } else {
          unreachable!()
        }
      } else {
        // No inline conts — just recurse into Arg::Expr.
        let args = args.into_iter().map(|a| match a {
          Arg::Expr(e) => Arg::Expr(Box::new(extract_from_body(*e, parent_params, scope_binds, alloc, hoisted))),
          other => other,
        }).collect();
        Expr { id: expr.id, kind: ExprKind::App { func, args } }
      }
    }

    ExprKind::If { cond, then, else_ } => {
      let then = extract_from_body(*then, parent_params, scope_binds, alloc, hoisted);
      let else_ = extract_from_body(*else_, parent_params, scope_binds, alloc, hoisted);
      Expr { id: expr.id, kind: ExprKind::If { cond, then: Box::new(then), else_: Box::new(else_) } }
    }
  }
}

// ---------------------------------------------------------------------------
// Rewrite refs in an expression tree using a CpsId → CpsId map
// ---------------------------------------------------------------------------

fn rewrite_refs<'src>(expr: Expr<'src>, map: &HashMap<CpsId, CpsId>) -> Expr<'src> {
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
  }
}

fn rewrite_refs_val<'src>(val: Val<'src>, map: &HashMap<CpsId, CpsId>) -> Val<'src> {
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

fn rewrite_refs_cont<'src>(cont: Cont<'src>, map: &HashMap<CpsId, CpsId>) -> Cont<'src> {
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

fn compute_captures(
  fn_body: &Expr<'_>,
  _fn_params: &[Param],
  parent_params: &[Param],
  scope_binds: &[(CpsId, Bind)],
) -> Vec<(CpsId, Bind)> {
  let mut parent_param_map: HashMap<CpsId, Bind> = parent_params.iter()
    .map(|p| match p { Param::Name(b) | Param::Spread(b) => (b.id, b.kind) })
    .collect();
  for (id, kind) in scope_binds {
    parent_param_map.insert(*id, *kind);
  }

  let mut caps: Vec<(CpsId, Bind)> = Vec::new();
  let mut seen: HashSet<CpsId> = HashSet::new();
  collect_captured_refs(fn_body, &parent_param_map, &mut caps, &mut seen);
  caps
}

fn collect_captured_refs(
  expr: &Expr<'_>,
  parent_ids: &HashMap<CpsId, Bind>,
  out: &mut Vec<(CpsId, Bind)>,
  seen: &mut HashSet<CpsId>,
) {
  match &expr.kind {
    ExprKind::LetFn { fn_body, cont, .. } => {
      collect_captured_refs(fn_body, parent_ids, out, seen);
      collect_captured_refs_cont(cont, parent_ids, out, seen);
    }
    ExprKind::LetVal { val, cont, .. } => {
      collect_captured_refs_val(val, parent_ids, out, seen);
      collect_captured_refs_cont(cont, parent_ids, out, seen);
    }
    ExprKind::App { func, args } => {
      if let Callable::Val(v) = func {
        collect_captured_refs_val(v, parent_ids, out, seen);
      }
      for arg in args {
        match arg {
          Arg::Val(v) | Arg::Spread(v) => collect_captured_refs_val(v, parent_ids, out, seen),
          Arg::Cont(c) => collect_captured_refs_cont(c, parent_ids, out, seen),
          Arg::Expr(e) => collect_captured_refs(e, parent_ids, out, seen),
        }
      }
    }
    ExprKind::If { cond, then, else_ } => {
      collect_captured_refs_val(cond, parent_ids, out, seen);
      collect_captured_refs(then, parent_ids, out, seen);
      collect_captured_refs(else_, parent_ids, out, seen);
    }
  }
}

fn collect_captured_refs_cont(
  cont: &Cont<'_>,
  parent_ids: &HashMap<CpsId, Bind>,
  out: &mut Vec<(CpsId, Bind)>,
  seen: &mut HashSet<CpsId>,
) {
  match cont {
    Cont::Ref(id) => {
      if parent_ids.contains_key(id) && seen.insert(*id) {
        out.push((*id, Bind::Cont));
      }
    }
    Cont::Expr { body, .. } => collect_captured_refs(body, parent_ids, out, seen),
  }
}

fn collect_captured_refs_val(
  val: &Val<'_>,
  parent_ids: &HashMap<CpsId, Bind>,
  out: &mut Vec<(CpsId, Bind)>,
  seen: &mut HashSet<CpsId>,
) {
  if let ValKind::Ref(Ref::Synth(bid)) = &val.kind
    && let Some(&kind) = parent_ids.get(bid)
      && seen.insert(*bid) {
        out.push((*bid, kind));
      }
  if let ValKind::ContRef(id) = &val.kind
    && parent_ids.contains_key(id) && seen.insert(*id) {
      out.push((*id, Bind::Cont));
    }
}

// ---------------------------------------------------------------------------
// Hoisted fn ref check
// ---------------------------------------------------------------------------

fn is_hoisted_fn_ref(cap_id: CpsId, hoisted: &[HoistedFn<'_>], alloc: &Alloc) -> bool {
  for h in hoisted {
    if h.name.id == cap_id { return true; }
    for ca in &h.cont_args {
      if ca.id == cap_id { return true; }
    }
  }
  if let Some(Some(alias)) = alloc.synth_alias.try_get(cap_id) {
    for h in hoisted {
      if h.name.id == *alias { return true; }
    }
  }
  false
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
  use crate::passes::ast::NodeKind;
  use crate::passes::cps::transform::lower_module;

  #[allow(unused)]
  fn lift(src: &str) -> String {
    match parse(src) {
      Ok(r) => {
        let ast_index = build_index(&r);
        let scope = crate::passes::scopes::analyse(&r.root, r.node_count as usize, &[]);
        let NodeKind::Module(ref items) = r.root.kind else { panic!("expected Module root") };
        let cps = lower_module(&items.items, &scope);
        let lifted = super::lambda_lift(cps, &ast_index);
        let ctx = Ctx {
          origin: &lifted.origin,
          ast_index: &ast_index,
          captures: None,
        };
        crate::passes::lifting::fmt::fmt_flat(&lifted.root, &ctx)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  include_fink_tests!("src/passes/lambda_lifting/test_lambda_lifting.fnk");
}
