//! Closure conversion — every user fn becomes a top-level closure-converted
//! form. Each lifted fn takes its captures as a single `ƒcaps` record arg
//! (first user arg), followed by `ƒctx, ƒret, args...`. At each fn-definition
//! site, the original `LetFn` is followed by an `ExprKind::Closure` node that
//! records the captured values; codegen materialises the caps struct.
//!
//! Conventions:
//! - Lifted fn signature: `fn ƒcaps, ƒctx, ƒret, args...`. Pure fns get
//!   `ƒcaps = {}` (still passed; codegen sees an empty captures list).
//! - The lifted fn body is NOT rewritten here. It still references free
//!   variables by their original CpsIds. The `Closure` node carries the
//!   captures list as metadata; codegen reads them from the caps struct
//!   at fn entry (the fn's body sees its capture-bind CpsIds as already
//!   in-scope at the start of the body — the contract between this pass
//!   and codegen).
//!
//! Free variables: a `Ref::Synth(id)` or `ContRef(id)` whose `id` was not
//! bound inside the fn's body (params, LetVal, LetFn, LetRec slots, Cont
//! args, Set names). LetRec slots from an enclosing scope ARE captured —
//! they are not bound inside the fn.
//!
//! Runs after `cps::transform::lower_module` + `cps::thread_ctx`.

use std::collections::HashSet;

use crate::ast::AstId;
use crate::passes::cps::ir::{
  Arg, Bind, BindNode, Callable, Cont, CpsId, CpsResult, Expr, ExprKind, Param,
  Ref, Val, ValKind,
};
use crate::propgraph::PropGraph;

pub fn convert(mut cps: CpsResult) -> CpsResult {
  // Snapshot bind kinds of the input tree so we can propagate them when
  // minting local CpsIds for captures. A local that captures a Cont
  // stays a Cont; a local that captures a Ctx stays a Ctx. Without this
  // the local would default to `Bind::SynthName` and the renderer would
  // mis-name it as a value binding.
  let pre_bind_kinds = crate::passes::cps::ir::collect_bind_kinds(&cps.root);
  // Walk the IR collecting every LetRec slot id. Captures of a slot
  // need Bind::Slot on the local (the local holds a cell ref, not a
  // value) so codegen can route reads/writes through the cell.
  let mut slot_ids: HashSet<CpsId> = HashSet::new();
  collect_slot_ids(&cps.root, &mut slot_ids);
  let mut cx = Cx {
    origin: &mut cps.origin,
    pre_bind_kinds: &pre_bind_kinds,
    slot_ids: &slot_ids,
    ctx_stack: Vec::new(),
  };
  cps.root = cx.convert_expr(cps.root);
  drop(cx);
  cps
}

fn collect_slot_ids(expr: &Expr, out: &mut HashSet<CpsId>) {
  match &expr.kind {
    ExprKind::LetRec { slots, body } => {
      for s in slots { out.insert(s.id); }
      collect_slot_ids(body, out);
    }
    ExprKind::LetVal { cont, .. } => { walk_cont(cont, out); }
    ExprKind::LetFn { fn_body, cont, .. } => {
      collect_slot_ids(fn_body, out);
      walk_cont(cont, out);
    }
    ExprKind::App { args, .. } => {
      for a in args {
        match a {
          Arg::Cont(Cont::Expr { body, .. }) => collect_slot_ids(body, out),
          Arg::Expr(e) => collect_slot_ids(e, out),
          _ => {}
        }
      }
    }
    ExprKind::If { then, else_, .. } => {
      collect_slot_ids(then, out);
      collect_slot_ids(else_, out);
    }
    ExprKind::Set { cont, .. } => walk_cont(cont, out),
    ExprKind::Closure { cont, .. } => walk_cont(cont, out),
    ExprKind::LetCaps { cont, .. } => walk_cont(cont, out),
  }
}

fn walk_cont(cont: &Cont, out: &mut HashSet<CpsId>) {
  if let Cont::Expr { body, .. } = cont {
    collect_slot_ids(body, out);
  }
}

struct Cx<'a> {
  origin: &'a mut PropGraph<CpsId, Option<AstId>>,
  pre_bind_kinds: &'a PropGraph<CpsId, Option<Bind>>,
  /// Every LetRec slot id in the input tree. Captures of slots get
  /// `Bind::Slot` on the local — codegen treats the local as a cell
  /// ref, not an unwrapped value.
  slot_ids: &'a HashSet<CpsId>,
  /// Stack of enclosing fns' ctx-param CpsIds. Top = innermost.
  /// Used when synthesising a forwarding App for a Closure's
  /// `Cont::Ref` cont: the synthesised call needs the enclosing fn's
  /// ctx as the 0th arg.
  ctx_stack: Vec<CpsId>,
}

impl Cx<'_> {
  fn fresh_id(&mut self, origin: Option<AstId>) -> CpsId {
    self.origin.push(origin)
  }

  /// Look up the Bind kind of an outer CpsId. Slot ids → `Bind::Slot`
  /// (the local holds a cell ref, not the unwrapped value). Otherwise
  /// the outer's bind kind from the pre-snapshot, defaulting to
  /// `SynthName` if not recorded.
  fn outer_bind_kind(&self, outer: CpsId) -> Bind {
    if self.slot_ids.contains(&outer) {
      return Bind::Slot;
    }
    self.pre_bind_kinds
      .try_get(outer)
      .and_then(|o| *o)
      .unwrap_or(Bind::SynthName)
  }

  /// Recursively convert an expression, lifting every LetFn encountered.
  fn convert_expr(&mut self, expr: Expr) -> Expr {
    let Expr { id, kind } = expr;
    let new_kind = match kind {
      ExprKind::LetVal { name, val, cont } => {
        let cont = self.convert_cont(cont);
        ExprKind::LetVal { name, val, cont }
      }
      ExprKind::LetFn { name, params, fn_kind, fn_body, cont } => {
        // Track this fn's ctx param while we recurse into its body so
        // nested closure-construction sites can reference the enclosing
        // ctx when synthesising forwarding calls. thread_ctx inserts a
        // `Bind::Ctx` as the leading param of every fn.
        let ctx_param_id = params.iter().find_map(|p| {
          let bn = match p { Param::Name(b) | Param::Spread(b) => b };
          matches!(bn.kind, Bind::Ctx).then_some(bn.id)
        });
        if let Some(id) = ctx_param_id { self.ctx_stack.push(id); }
        // Recurse first — inner fns get converted before this one is
        // analysed.
        let fn_body = self.convert_expr(*fn_body);
        if ctx_param_id.is_some() { self.ctx_stack.pop(); }
        let cont = self.convert_cont(cont);

        // Compute free variables in the converted body.
        let mut bound: HashSet<CpsId> = HashSet::new();
        for p in &params {
          let bn = match p { Param::Name(b) | Param::Spread(b) => b };
          bound.insert(bn.id);
        }
        let mut frees = Frees::new();
        collect_free_in_expr(&fn_body, &bound, &mut frees);

        // For each free var, mint a fresh local CpsId. The lifted body
        // will reference these local ids; LetCaps at fn entry binds them
        // from the caps record. Origin propgraph maps the local back to
        // the same AST node as the outer capture so the rendered name
        // matches.
        let outer_to_local: Vec<(CpsId, CpsId)> = frees.order.iter().map(|&outer| {
          let outer_origin = self.origin.try_get(outer).and_then(|o| *o);
          let local = self.fresh_id(outer_origin);
          (outer, local)
        }).collect();
        let rename: std::collections::HashMap<CpsId, CpsId> =
          outer_to_local.iter().copied().collect();

        // Rewrite all refs in the body: outer_id → local_id.
        let fn_body = rename_refs_in_expr(fn_body, &rename);

        // Inject a fresh ƒcaps param at the head of the params list.
        // caps_id is a synth param with no source origin — give it
        // `None` so it can't accidentally render under the LetFn's
        // name. Bind::Caps drives the `·ƒcaps_N` rendering.
        let name_origin = self.origin.try_get(name.id).and_then(|o| *o);
        let caps_id = self.fresh_id(None);
        let caps_bind = BindNode { id: caps_id, kind: Bind::Caps };
        let mut new_params = vec![Param::Name(caps_bind)];
        new_params.extend(params);

        // Wrap the rewritten body in LetCaps that binds each local id.
        // Local inherits the outer's Bind kind: a captured Cont stays a
        // Cont, a captured Ctx stays a Ctx — so the renderer names them
        // by semantic role rather than as a generic value binding.
        let letcaps_binds: Vec<BindNode> = outer_to_local.iter().map(|&(outer, local)| {
          let kind = self.outer_bind_kind(outer);
          BindNode { id: local, kind }
        }).collect();
        let caps_ref_id = self.fresh_id(name_origin);
        let caps_ref = Val {
          id: caps_ref_id,
          kind: ValKind::Ref(Ref::Synth(caps_id)),
        };
        let letcaps_id = self.fresh_id(name_origin);
        let new_fn_body = if letcaps_binds.is_empty() {
          // No captures — skip the LetCaps wrapper.
          fn_body
        } else {
          Expr {
            id: letcaps_id,
            kind: ExprKind::LetCaps {
              caps: caps_ref,
              binds: letcaps_binds,
              cont: Cont::Expr {
                args: vec![],
                body: Box::new(fn_body),
              },
            },
          }
        };

        // Build captures for the Closure node: each (local_bind, outer_ref).
        // The local_bind documents the name as it appears in the lifted
        // body (its CpsId is the local id LetCaps will bind); the outer
        // Val is the construction-site read.
        let captures: Vec<(BindNode, Val)> = outer_to_local.iter().map(|&(outer, local)| {
          let outer_origin = self.origin.try_get(outer).and_then(|o| *o);
          let kind = self.outer_bind_kind(outer);
          let bn = BindNode { id: local, kind };
          let val_id = self.fresh_id(outer_origin);
          let val = Val { id: val_id, kind: ValKind::Ref(Ref::Synth(outer)) };
          (bn, val)
        }).collect();

        // Rename LetFn name to a fresh lifted-fn id; use the original
        // `name` as the Closure's bound result so surrounding-scope
        // refs resolve to the closure value.
        let lifted_id = self.fresh_id(name_origin);
        let lifted_name = BindNode { id: lifted_id, kind: name.kind };

        let funcref_id = self.fresh_id(name_origin);
        let funcref = Val {
          id: funcref_id,
          kind: ValKind::Ref(Ref::Synth(lifted_id)),
        };
        // Build the Closure's cont. For an inline `Cont::Expr`, use
        // its body verbatim (bind the closure result to `name`).
        // For `Cont::Ref(cont_id)` synthesise an explicit forwarding
        // call `App(cont_id, [enclosing_ctx, name])`. Closure
        // construction is value-producing, not a tail-call, so the
        // named cont must be invoked with ctx + result. The enclosing
        // ctx is the `Bind::Ctx` param of the fn that contains this
        // closure-construction site — tracked via `ctx_stack`.
        let closure_cont = match cont {
          Cont::Expr { args: _, body } => Cont::Expr {
            args: vec![name.clone()],
            body,
          },
          Cont::Ref(cont_id) => {
            let enclosing_ctx_id = *self.ctx_stack.last()
              .expect("convert: LetFn with Cont::Ref at top level — no enclosing ctx to thread");
            let cont_val_id = self.fresh_id(None);
            let cont_val = Val { id: cont_val_id, kind: ValKind::ContRef(cont_id) };
            let ctx_ref_id = self.fresh_id(None);
            let ctx_ref = Val { id: ctx_ref_id, kind: ValKind::Ref(Ref::Synth(enclosing_ctx_id)) };
            let name_ref_id = self.fresh_id(name_origin);
            let name_ref = Val { id: name_ref_id, kind: ValKind::Ref(Ref::Synth(name.id)) };
            let app_id = self.fresh_id(None);
            let body = Box::new(Expr {
              id: app_id,
              kind: ExprKind::App {
                func: Callable::Val(cont_val),
                args: vec![Arg::Val(ctx_ref), Arg::Val(name_ref)],
              },
            });
            Cont::Expr { args: vec![name.clone()], body }
          }
        };
        let closure_id = self.fresh_id(name_origin);
        let closure_node = Expr {
          id: closure_id,
          kind: ExprKind::Closure {
            funcref,
            captures,
            cont: closure_cont,
          },
        };

        let new_cont = Cont::Expr {
          args: vec![],
          body: Box::new(closure_node),
        };

        ExprKind::LetFn {
          name: lifted_name,
          params: new_params,
          fn_kind,
          fn_body: Box::new(new_fn_body),
          cont: new_cont,
        }
      }
      ExprKind::App { func, args } => {
        let args = args.into_iter().map(|a| match a {
          Arg::Cont(c) => Arg::Cont(self.convert_cont(c)),
          Arg::Expr(e) => Arg::Expr(Box::new(self.convert_expr(*e))),
          other => other,
        }).collect();
        ExprKind::App { func, args }
      }
      ExprKind::If { cond, then, else_ } => {
        let then = Box::new(self.convert_expr(*then));
        let else_ = Box::new(self.convert_expr(*else_));
        ExprKind::If { cond, then, else_ }
      }
      ExprKind::LetRec { slots, body } => {
        let body = Box::new(self.convert_expr(*body));
        ExprKind::LetRec { slots, body }
      }
      ExprKind::Set { name, val, cont } => {
        let cont = self.convert_cont(cont);
        ExprKind::Set { name, val, cont }
      }
      ExprKind::Closure { funcref, captures, cont } => {
        let cont = self.convert_cont(cont);
        ExprKind::Closure { funcref, captures, cont }
      }
      ExprKind::LetCaps { caps, binds, cont } => {
        let cont = self.convert_cont(cont);
        ExprKind::LetCaps { caps, binds, cont }
      }
    };
    Expr { id, kind: new_kind }
  }

  fn convert_cont(&mut self, cont: Cont) -> Cont {
    match cont {
      Cont::Ref(_) => cont,
      Cont::Expr { args, body } => {
        let body = Box::new(self.convert_expr(*body));
        Cont::Expr { args, body }
      }
    }
  }
}


// ---------------------------------------------------------------------------
// Free-variable collection
// ---------------------------------------------------------------------------

/// Insertion-ordered set of CpsIds. Order matters so capture layouts are
/// deterministic; dedupe via the hash set.
struct Frees {
  seen: HashSet<CpsId>,
  order: Vec<CpsId>,
}

impl Frees {
  fn new() -> Self {
    Self { seen: HashSet::new(), order: Vec::new() }
  }
  fn insert(&mut self, id: CpsId) {
    if self.seen.insert(id) {
      self.order.push(id);
    }
  }
}

/// Walk an expression and record every `Ref::Synth(id)` / `ContRef(id)` whose
/// `id` is not in `bound`. Adds new binds encountered along the way to
/// `bound` for the duration of the subexpression where they're in scope.
fn collect_free_in_expr(
  expr: &Expr,
  bound: &HashSet<CpsId>,
  frees: &mut Frees,
) {
  match &expr.kind {
    ExprKind::LetVal { name, val, cont } => {
      collect_free_in_val(val, bound, frees);
      let mut inner = bound.clone();
      inner.insert(name.id);
      collect_free_in_cont(cont, &inner, frees);
    }
    ExprKind::LetFn { name, params: _, fn_body: _, cont, .. } => {
      // The lifted fn's body is closed — its captures have already been
      // collected at its own conversion site and propagated into the
      // surrounding Closure node. Refs inside the body are the inner fn's
      // concern, NOT this scope's. We do NOT recurse into `fn_body`.
      //
      // Names from THIS scope that the inner fn captures appear as
      // `Ref::Synth` Vals in the Closure node's `captures` list (sibling
      // to this LetFn in the parent's body); those reads are handled
      // when we walk the Closure arm separately.
      //
      // The LetFn name is in scope for the cont; track it.
      let mut inner_cont = bound.clone();
      inner_cont.insert(name.id);
      collect_free_in_cont(cont, &inner_cont, frees);
    }
    ExprKind::App { func, args } => {
      if let Callable::Val(v) = func { collect_free_in_val(v, bound, frees); }
      for a in args {
        match a {
          Arg::Val(v) | Arg::Spread(v) => collect_free_in_val(v, bound, frees),
          Arg::Cont(c) => collect_free_in_cont(c, bound, frees),
          Arg::Expr(e) => collect_free_in_expr(e, bound, frees),
        }
      }
    }
    ExprKind::If { cond, then, else_ } => {
      collect_free_in_val(cond, bound, frees);
      collect_free_in_expr(then, bound, frees);
      collect_free_in_expr(else_, bound, frees);
    }
    ExprKind::LetRec { slots, body } => {
      let mut inner = bound.clone();
      for s in slots { inner.insert(s.id); }
      collect_free_in_expr(body, &inner, frees);
    }
    ExprKind::Set { name: _, val, cont } => {
      // Set's `name` IS a ref to a slot in an enclosing LetRec, not a
      // new binding — count it as a use if it's free in this scope.
      // The `name` BindNode field is the slot binding (defined by the
      // LetRec); we don't record it as a fresh bind here.
      collect_free_in_val(val, bound, frees);
      collect_free_in_cont(cont, bound, frees);
    }
    ExprKind::Closure { funcref, captures, cont } => {
      collect_free_in_val(funcref, bound, frees);
      for (_name, val) in captures {
        collect_free_in_val(val, bound, frees);
      }
      collect_free_in_cont(cont, bound, frees);
    }
    ExprKind::LetCaps { caps, binds, cont } => {
      collect_free_in_val(caps, bound, frees);
      // The cont's body sees `binds` as locals.
      let mut inner = bound.clone();
      for b in binds { inner.insert(b.id); }
      collect_free_in_cont(cont, &inner, frees);
    }
  }
}

fn collect_free_in_val(val: &Val, bound: &HashSet<CpsId>, frees: &mut Frees) {
  match &val.kind {
    ValKind::Ref(Ref::Synth(id)) if !bound.contains(id) => { frees.insert(*id); }
    ValKind::ContRef(id) if !bound.contains(id) => { frees.insert(*id); }
    _ => {}
  }
}

fn collect_free_in_cont(cont: &Cont, bound: &HashSet<CpsId>, frees: &mut Frees) {
  match cont {
    Cont::Ref(id) => {
      if !bound.contains(id) { frees.insert(*id); }
    }
    Cont::Expr { args, body } => {
      let mut inner = bound.clone();
      for a in args { inner.insert(a.id); }
      collect_free_in_expr(body, &inner, frees);
    }
  }
}

// ---------------------------------------------------------------------------
// Ref-renaming: rewrite captured outer CpsIds to local CpsIds inside a
// lifted fn body. Only refs (Ref::Synth, ContRef, Cont::Ref) get rewritten;
// binding sites are untouched (they're already locally-fresh, or are
// LetVal/LetFn/LetRec slots that THIS lifted body introduces locally).
// ---------------------------------------------------------------------------

fn rename_refs_in_expr(
  expr: Expr,
  rename: &std::collections::HashMap<CpsId, CpsId>,
) -> Expr {
  if rename.is_empty() {
    return expr;
  }
  let Expr { id, kind } = expr;
  let new_kind = match kind {
    ExprKind::LetVal { name, val, cont } => {
      let val = Box::new(rename_in_val(*val, rename));
      let cont = rename_refs_in_cont(cont, rename);
      ExprKind::LetVal { name, val, cont }
    }
    ExprKind::LetFn { name, params, fn_kind, fn_body, cont } => {
      // The fn_body is its OWN scope. After conversion (which has
      // already happened by the time rename runs), inner LetFn bodies
      // reference their own local CpsIds (post-rename). We should NOT
      // recurse into a nested LetFn's body — its refs are not in
      // THIS scope. However, the nested LetFn's cont IS in this scope.
      let cont = rename_refs_in_cont(cont, rename);
      ExprKind::LetFn { name, params, fn_kind, fn_body, cont }
    }
    ExprKind::App { func, args } => {
      // Pub's `val` arg (args[1] after thread_ctx) names the slot
      // being exported. Pub'd globals are allocated keyed on the
      // outer slot id; if we rename `x_1 → x_48` inside a lifted
      // body, `x_48` has no global and lower will not find it. Keep
      // the val arg pointing at the outer slot id; reads at codegen
      // route through pub_globals.
      let is_pub = matches!(func, Callable::BuiltIn(crate::passes::cps::ir::BuiltIn::Pub));
      let func = match func {
        Callable::Val(v) => Callable::Val(rename_in_val(v, rename)),
        Callable::BuiltIn(_) => func,
      };
      let args = args.into_iter().enumerate().map(|(i, a)| match a {
        Arg::Val(v) if is_pub && i == 1 => Arg::Val(v),
        Arg::Val(v) => Arg::Val(rename_in_val(v, rename)),
        Arg::Spread(v) => Arg::Spread(rename_in_val(v, rename)),
        Arg::Cont(c) => Arg::Cont(rename_refs_in_cont(c, rename)),
        Arg::Expr(e) => Arg::Expr(Box::new(rename_refs_in_expr(*e, rename))),
      }).collect();
      ExprKind::App { func, args }
    }
    ExprKind::If { cond, then, else_ } => {
      let cond = Box::new(rename_in_val(*cond, rename));
      let then = Box::new(rename_refs_in_expr(*then, rename));
      let else_ = Box::new(rename_refs_in_expr(*else_, rename));
      ExprKind::If { cond, then, else_ }
    }
    ExprKind::LetRec { slots, body } => {
      let body = Box::new(rename_refs_in_expr(*body, rename));
      ExprKind::LetRec { slots, body }
    }
    ExprKind::Set { name, val, cont } => {
      // Set's `name` is a slot binding *defined* by an enclosing LetRec;
      // its CpsId is a USE site (refers to the slot). Rename it if it's
      // in the rename map.
      let new_name_id = rename.get(&name.id).copied().unwrap_or(name.id);
      let name = BindNode { id: new_name_id, kind: name.kind };
      let val = rename_in_val(val, rename);
      let cont = rename_refs_in_cont(cont, rename);
      ExprKind::Set { name, val, cont }
    }
    ExprKind::Closure { funcref, captures, cont } => {
      // funcref refers to a lifted fn id — that's a SIBLING binding
      // in the enclosing scope's LetFn chain. Inside the current
      // lifted body, the SIBLING funcref id isn't captured (the inner
      // fn is its own thing). The captures Val refs ARE in this body's
      // scope and could be renamed.
      let funcref = rename_in_val(funcref, rename);
      let captures = captures.into_iter().map(|(name, val)| {
        (name, rename_in_val(val, rename))
      }).collect();
      let cont = rename_refs_in_cont(cont, rename);
      ExprKind::Closure { funcref, captures, cont }
    }
    ExprKind::LetCaps { caps, binds, cont } => {
      let caps = rename_in_val(caps, rename);
      let cont = rename_refs_in_cont(cont, rename);
      ExprKind::LetCaps { caps, binds, cont }
    }
  };
  Expr { id, kind: new_kind }
}

fn rename_in_val(
  val: Val,
  rename: &std::collections::HashMap<CpsId, CpsId>,
) -> Val {
  let Val { id, kind } = val;
  let new_kind = match kind {
    ValKind::Ref(Ref::Synth(target)) => {
      if let Some(&new_target) = rename.get(&target) {
        ValKind::Ref(Ref::Synth(new_target))
      } else {
        ValKind::Ref(Ref::Synth(target))
      }
    }
    ValKind::ContRef(target) => {
      if let Some(&new_target) = rename.get(&target) {
        ValKind::ContRef(new_target)
      } else {
        ValKind::ContRef(target)
      }
    }
    other => other,
  };
  Val { id, kind: new_kind }
}

fn rename_refs_in_cont(
  cont: Cont,
  rename: &std::collections::HashMap<CpsId, CpsId>,
) -> Cont {
  match cont {
    Cont::Ref(id) => {
      let new_id = rename.get(&id).copied().unwrap_or(id);
      Cont::Ref(new_id)
    }
    Cont::Expr { args, body } => {
      let body = Box::new(rename_refs_in_expr(*body, rename));
      Cont::Expr { args, body }
    }
  }
}
