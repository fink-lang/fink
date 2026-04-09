// Scope analysis pass — AST-level name resolution.
//
// Walks the AST and builds a scope graph:
// - Each scope has a parent link, kind, and ordered bindings
// - Every identifier reference resolves to the AstId of its binding
// - Module-level bindings are mutually recursive (forward refs allowed)
// - Fn body bindings are sequential (visible to subsequent siblings only)
//
// The scope graph survives as PropGraphs for use by CPS transform, lifting,
// and codegen. The CPS transform uses it to emit Ref::Synth(target_cps_id)
// instead of Ref::Name, so that lifting can rearrange the tree without
// breaking scope resolution.

use crate::ast::{AstId, Node, NodeKind};
use crate::propgraph::PropGraph;

// ---------------------------------------------------------------------------
// Typed IDs
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScopeId(pub u32);

impl std::fmt::Debug for ScopeId {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "S{}", self.0)
  }
}

impl From<ScopeId> for usize { fn from(id: ScopeId) -> usize { id.0 as usize } }
impl From<usize> for ScopeId { fn from(n: usize) -> ScopeId { ScopeId(n as u32) } }

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BindId(pub u32);

impl std::fmt::Debug for BindId {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "B{}", self.0)
  }
}

impl From<BindId> for usize { fn from(id: BindId) -> usize { id.0 as usize } }
impl From<usize> for BindId { fn from(n: usize) -> BindId { BindId(n as u32) } }

// ---------------------------------------------------------------------------
// Scope kinds
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
  /// Module level — all bindings mutually recursive.
  Module,
  /// Function body — sequential bindings.
  Fn,
  /// Match arm — pattern bindings visible in arm body.
  Arm,
}

// ---------------------------------------------------------------------------
// Scope graph data
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ScopeInfo {
  pub kind: ScopeKind,
  pub parent: Option<ScopeId>,
  /// AstId of the node that created this scope (Fn node, Module node, Arm node).
  pub ast_id: AstId,
}

/// Where a binding comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindOrigin {
  Ast(AstId),
  Builtin(u32),
}

#[derive(Clone, Debug)]
pub struct BindInfo {
  pub scope: ScopeId,
  pub name: String,
  pub origin: BindOrigin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefKind {
  /// Normal reference — binding already in scope.
  Ref,
  /// Forward reference — binding later in same module scope (mutual recursion).
  FwdRef,
  /// Self-reference — fn references its own binding.
  SelfRef,
  /// Unresolved — no binding found in any scope.
  Unresolved,
}

#[derive(Clone, Debug)]
pub struct RefInfo {
  pub kind: RefKind,
  pub name: String,
  pub bind_id: BindId,
  /// How many scope levels up the binding is (0 = same scope).
  pub depth: u32,
  pub ast_id: AstId,
}

// ---------------------------------------------------------------------------
// Result of scope analysis
// ---------------------------------------------------------------------------

/// An event in a scope — binding, reference, or child scope, in source order.
#[derive(Clone, Debug)]
pub enum ScopeEvent {
  Bind(BindId),
  Ref(RefInfo),
  ChildScope(ScopeId),
}

pub struct ScopeResult {
  pub scopes: PropGraph<ScopeId, ScopeInfo>,
  pub binds: PropGraph<BindId, BindInfo>,
  /// For each AST node that is a reference, which BindId does it resolve to.
  pub resolution: PropGraph<AstId, Option<BindId>>,
  /// Events per scope in source order — bindings, refs, and child scopes interleaved.
  pub scope_events: PropGraph<ScopeId, Vec<ScopeEvent>>,
}

// ---------------------------------------------------------------------------
// Analysis context (mutable during walk)
// ---------------------------------------------------------------------------

struct Ctx<'src> {
  scopes: PropGraph<ScopeId, ScopeInfo>,
  binds: PropGraph<BindId, BindInfo>,
  resolution: PropGraph<AstId, Option<BindId>>,
  scope_events: PropGraph<ScopeId, Vec<ScopeEvent>>,
  /// Name → BindId lookup stack. Each scope pushes its bindings; pop on exit.
  /// For module scopes, all bindings are pre-registered before walking bodies.
  name_stack: Vec<(String, BindId, ScopeId)>,
  /// Track which AstId is being defined (for self_ref detection).
  current_bind_ast_id: Option<AstId>,
  /// Prelude names — always available, checked as fallback in resolve.
  builtins: Vec<String>,
  _src: std::marker::PhantomData<&'src ()>,
}

impl<'src> Ctx<'src> {
  fn new(node_count: usize) -> Self {
    Self {
      scopes: PropGraph::new(),
      binds: PropGraph::new(),
      resolution: PropGraph::with_size(node_count, None),
      scope_events: PropGraph::new(),
      name_stack: Vec::new(),
      current_bind_ast_id: None,
      builtins: Vec::new(),
      _src: std::marker::PhantomData,
    }
  }

  fn push_scope(&mut self, kind: ScopeKind, parent: Option<ScopeId>, ast_id: AstId) -> ScopeId {
    let id = self.scopes.push(ScopeInfo { kind, parent, ast_id });
    self.scope_events.push(Vec::new());
    if let Some(pid) = parent {
      self.scope_events.get_mut(pid).push(ScopeEvent::ChildScope(id));
    }
    id
  }

  fn push_bind(&mut self, scope: ScopeId, name: &str, origin: BindOrigin) -> BindId {
    let id = self.binds.push(BindInfo {
      scope,
      name: name.to_string(),
      origin,
    });
    self.scope_events.get_mut(scope).push(ScopeEvent::Bind(id));
    self.name_stack.push((name.to_string(), id, scope));
    id
  }

  /// Pre-register a binding for mutual recursion (module scope).
  /// Adds to name_stack for resolution but does NOT emit a bind event —
  /// the event is emitted later when the binding is encountered during walk.
  fn pre_register_bind(&mut self, scope: ScopeId, name: &str, origin: BindOrigin) -> BindId {
    let id = self.binds.push(BindInfo {
      scope,
      name: name.to_string(),
      origin,
    });
    self.name_stack.push((name.to_string(), id, scope));
    id
  }

  /// Emit a bind event for a pre-registered binding (found by AstId).
  /// Returns true if the binding was found (pre-registered), false otherwise.
  fn emit_bind_event(&mut self, scope: ScopeId, ast_id: AstId) -> bool {
    for i in 0..self.binds.len() {
      let bid = BindId(i as u32);
      if self.binds.get(bid).origin == BindOrigin::Ast(ast_id) {
        self.scope_events.get_mut(scope).push(ScopeEvent::Bind(bid));
        return true;
      }
    }
    false
  }

  /// Resolve a name reference. Walk the name stack from top (innermost) to bottom.
  fn resolve(&mut self, name: &str, ref_ast_id: AstId, current_scope: ScopeId) {
    // Search from most recent binding backward.
    for i in (0..self.name_stack.len()).rev() {
      let (bind_name, bind_id, bind_scope) = &self.name_stack[i];
      if bind_name == name {
        let bind_id = *bind_id;
        let bind_scope = *bind_scope;

        // Compute depth (how many scope levels up).
        let depth = self.scope_depth(current_scope, bind_scope);

        // Determine ref kind.
        let is_module_fwd = self.scopes.get(bind_scope).kind == ScopeKind::Module
          && self.is_forward_ref(bind_id, ref_ast_id, bind_scope);

        let kind = if is_module_fwd && depth == 0 {
          // Forward ref at module level (depth 0) — value not yet available.
          // Only fn bodies (depth > 0) can forward-ref module bindings.
          break; // fall through to unresolved
        } else if self.current_bind_ast_id.is_some_and(|id| self.binds.get(bind_id).origin == BindOrigin::Ast(id)) {
          RefKind::SelfRef
        } else if is_module_fwd {
          RefKind::FwdRef
        } else {
          RefKind::Ref
        };

        self.resolution.set(ref_ast_id, Some(bind_id));
        self.scope_events.get_mut(current_scope).push(ScopeEvent::Ref(RefInfo {
          kind,
          name: name.to_string(),
          bind_id,
          depth,
          ast_id: ref_ast_id,
        }));
        return;
      }
    }
    // Check builtins.
    if self.builtins.iter().any(|n| n == name) {
      // Find or create the BindId for this builtin.
      let bind_id = self.find_or_create_builtin_bind(name, current_scope);
      self.resolution.set(ref_ast_id, Some(bind_id));
      self.scope_events.get_mut(current_scope).push(ScopeEvent::Ref(RefInfo {
        kind: RefKind::Ref,
        name: name.to_string(),
        bind_id,
        depth: 0,
        ast_id: ref_ast_id,
      }));
      return;
    }
    // Unresolved.
    self.scope_events.get_mut(current_scope).push(ScopeEvent::Ref(RefInfo {
      kind: RefKind::Unresolved,
      name: name.to_string(),
      bind_id: BindId(u32::MAX),
      depth: 0,
      ast_id: ref_ast_id,
    }));
  }

  /// Count how many scope levels from `from` up to `to`.
  fn scope_depth(&self, from: ScopeId, to: ScopeId) -> u32 {
    let mut depth = 0;
    let mut cur = from;
    while cur != to {
      if let Some(parent) = self.scopes.get(cur).parent {
        cur = parent;
        depth += 1;
      } else {
        break;
      }
    }
    depth
  }

  /// Check if a reference is a forward reference (ref appears before bind in source).
  /// For module scopes, we pre-register all bindings, so we check source position.
  fn is_forward_ref(&self, bind_id: BindId, ref_ast_id: AstId, _scope: ScopeId) -> bool {
    match self.binds.get(bind_id).origin {
      BindOrigin::Ast(bind_ast_id) => ref_ast_id.0 < bind_ast_id.0,
      BindOrigin::Builtin(_) => false,
    }
  }

  /// Remove bindings from name_stack that belong to the given scope.
  fn find_or_create_builtin_bind(&mut self, name: &str, scope: ScopeId) -> BindId {
    // Check if already created.
    for i in 0..self.binds.len() {
      let bid = BindId(i as u32);
      let info = self.binds.get(bid);
      if info.name == name && matches!(info.origin, BindOrigin::Builtin(_)) {
        return bid;
      }
    }
    // Create new.
    let idx = self.builtins.iter().position(|n| n == name).unwrap_or(0) as u32;
    self.binds.push(BindInfo {
      scope,
      name: name.to_string(),
      origin: BindOrigin::Builtin(idx),
    })
  }

  fn pop_scope_binds(&mut self, scope: ScopeId) {
    self.name_stack.retain(|(_, _, s)| *s != scope);
  }

}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn analyse<'src>(root: &'src Node<'src>, node_count: usize, builtins: &[&str]) -> ScopeResult {
  let mut ctx = Ctx::new(node_count);
  let module_scope = ctx.push_scope(ScopeKind::Module, None, root.id);

  // Language builtins (always in scope) + caller-provided extras.
  ctx.builtins = ["import", "yield", "spawn", "await", "channel", "receive", "read"].iter().chain(builtins.iter()).map(|s| s.to_string()).collect();

  // Phase 1: pre-register all module-level bindings (for mutual recursion).
  if let NodeKind::Module(items) = &root.kind {
    pre_register_binds(&items.items, module_scope, &mut ctx);
  }

  // Phase 2: walk the tree and resolve references.
  if let NodeKind::Module(items) = &root.kind {
    walk_stmts(&items.items, module_scope, &mut ctx);
  }

  ScopeResult {
    scopes: ctx.scopes,
    binds: ctx.binds,
    resolution: ctx.resolution,
    scope_events: ctx.scope_events,
  }
}

// ---------------------------------------------------------------------------
// Phase 1: pre-register module-level bindings
// ---------------------------------------------------------------------------

fn pre_register_binds(stmts: &[Node<'_>], scope: ScopeId, ctx: &mut Ctx<'_>) {
  for stmt in stmts {
    if let NodeKind::Bind { lhs, .. } = &stmt.kind {
      pre_register_pattern_binds(lhs, scope, ctx);
    }
  }
}

/// Find the binding Ident node from a simple bind LHS.
/// For `Ident` → returns the node itself.
/// For `InfixOp { lhs, .. }` (guard pattern like `a > 0`) → recurses into lhs.
/// For complex patterns (LitSeq, LitRec, etc.) → returns None (handled separately).
fn binding_ident<'a>(node: &'a Node<'a>) -> Option<&'a Node<'a>> {
  match &node.kind {
    NodeKind::Ident(_) => Some(node),
    NodeKind::InfixOp { lhs, .. } => binding_ident(lhs),
    _ => None,
  }
}

/// Register bindings from the LHS of a bind pattern.
fn register_pattern_binds(node: &Node<'_>, scope: ScopeId, ctx: &mut Ctx<'_>) {
  register_pattern_binds_inner(node, scope, ctx, false);
}

fn pre_register_pattern_binds(node: &Node<'_>, scope: ScopeId, ctx: &mut Ctx<'_>) {
  register_pattern_binds_inner(node, scope, ctx, true);
}

fn register_pattern_binds_inner(node: &Node<'_>, scope: ScopeId, ctx: &mut Ctx<'_>, pre_register: bool) {
  match &node.kind {
    NodeKind::Ident(name) => {
      let origin = BindOrigin::Ast(node.id);
      if pre_register {
        ctx.pre_register_bind(scope, name, origin);
      } else {
        ctx.push_bind(scope, name, origin);
      }
    }
    NodeKind::SynthIdent(n) => {
      let name = format!("·$_{n}");
      let origin = BindOrigin::Ast(node.id);
      if pre_register {
        ctx.pre_register_bind(scope, &name, origin);
      } else {
        ctx.push_bind(scope, &name, origin);
      }
    }
    // Destructuring patterns: [a, b] = ..., {x, y} = ...
    NodeKind::LitSeq { items, .. } | NodeKind::LitRec { items, .. }
    | NodeKind::Patterns(items) => {
      for item in &items.items {
        register_pattern_binds(item, scope, ctx);
      }
    }
    NodeKind::Spread { inner: Some(inner), .. } => {
      register_pattern_binds(inner, scope, ctx);
    }
    NodeKind::Bind { lhs, .. } => {
      // Nested bind in pattern: `{x: y}` — lhs is the key, rhs is the bind target.
      // The lhs ident is the binding in rec destructure.
      register_pattern_binds_inner(lhs, scope, ctx, pre_register);
    }
    NodeKind::BindRight { lhs, rhs, .. } => {
      // `foo |= [bar, spam]` — lhs binds the whole value, rhs destructures it.
      register_pattern_binds_inner(lhs, scope, ctx, pre_register);
      register_pattern_binds_inner(rhs, scope, ctx, pre_register);
    }
    NodeKind::InfixOp { lhs, .. } => {
      // Guard pattern: `a > 0` — lhs holds the binding Ident; rhs is the guard value.
      // Only the lhs side introduces a binding.
      register_pattern_binds_inner(lhs, scope, ctx, pre_register);
    }
    NodeKind::Apply { args, .. } => {
      // Predicate/constructor pattern: `is_even y`, `Ok b` — func is a reference, args are bindings.
      for arg in &args.items {
        register_pattern_binds_inner(arg, scope, ctx, pre_register);
      }
    }
    NodeKind::Arm { body, .. } => {
      // Record field rename pattern in a LitRec: `y: z` — lhs is the field key (a ref),
      // body items are the binding targets.
      for item in &body.items {
        register_pattern_binds_inner(item, scope, ctx, pre_register);
      }
    }
    _ => {}
  }
}

/// Walk guard expressions inside patterns for reference resolution.
/// Called after `register_pattern_binds` so that bindings are visible to guards.
///
/// Pattern nodes contain both binding sites (Ident) and guard expressions
/// (InfixOp rhs, Apply func). This function walks only the guard/expression
/// parts, skipping binding idents which are already registered.
fn walk_pattern_refs(node: &Node<'_>, scope: ScopeId, ctx: &mut Ctx<'_>) {
  match &node.kind {
    NodeKind::InfixOp { lhs, rhs, .. } => {
      // Guard pattern: `a > 0` or `a > 0 or a < 9`.
      // Both sides are expressions — walk them for ref resolution.
      // The binding ident (lhs leaf) resolves to itself (already registered).
      walk_node(lhs, scope, ctx);
      walk_node(rhs, scope, ctx);
    }
    NodeKind::Apply { func, args, .. } => {
      // Predicate guard: `is_even y` — func is a reference.
      walk_node(func, scope, ctx);
      // args contain bindings — recurse for nested guards.
      for arg in &args.items {
        walk_pattern_refs(arg, scope, ctx);
      }
    }
    NodeKind::LitSeq { items, .. } | NodeKind::LitRec { items, .. }
    | NodeKind::Patterns(items) => {
      for item in &items.items {
        walk_pattern_refs(item, scope, ctx);
      }
    }
    NodeKind::Bind { lhs, .. } => {
      walk_pattern_refs(lhs, scope, ctx);
    }
    NodeKind::Arm { lhs, body, .. } => {
      walk_pattern_refs(lhs, scope, ctx);
      for item in &body.items {
        walk_pattern_refs(item, scope, ctx);
      }
    }
    NodeKind::Spread { inner: Some(inner), .. } => {
      walk_pattern_refs(inner, scope, ctx);
    }
    // Ident, Wildcard, literals — these are binding sites or values, not guard refs.
    _ => {}
  }
}

// ---------------------------------------------------------------------------
// Phase 2: walk and resolve
// ---------------------------------------------------------------------------

fn walk_stmts(stmts: &[Node<'_>], scope: ScopeId, ctx: &mut Ctx<'_>) {
  for stmt in stmts {
    walk_node(stmt, scope, ctx);
  }
}

fn walk_node(node: &Node<'_>, scope: ScopeId, ctx: &mut Ctx<'_>) {
  match &node.kind {
    NodeKind::Bind { lhs, rhs, .. } => {
      // Track which binding we're defining (for self_ref detection).
      let prev_bind = ctx.current_bind_ast_id;
      if let NodeKind::Ident(_) = &lhs.kind {
        ctx.current_bind_ast_id = Some(lhs.id);
      }
      // Walk RHS first (evaluate the value expression).
      walk_node(rhs, scope, ctx);
      ctx.current_bind_ast_id = prev_bind;
      if ctx.scopes.get(scope).kind == ScopeKind::Module {
        // Module scope: binding was pre-registered. Emit the event now (after RHS).
        // For Ident LHS: if not pre-registered (e.g. a nested Bind inside a RHS expression),
        // fall back to sequential registration.
        // For InfixOp LHS (guard pattern): emit event for the Ident inside.
        let needs_register = match binding_ident(lhs) {
          Some(ident) => !ctx.emit_bind_event(scope, ident.id),
          None => false,
        };
        if needs_register {
          register_pattern_binds(lhs, scope, ctx);
        }
      } else {
        // Non-module scopes: register the binding now (sequential).
        register_pattern_binds(lhs, scope, ctx);
      }
      // Walk guard expressions in patterns for reference resolution.
      walk_pattern_refs(lhs, scope, ctx);
    }

    NodeKind::Ident(name) => {
      // This is a reference — resolve it.
      ctx.resolve(name, node.id, scope);
    }

    NodeKind::SynthIdent(n) => {
      // Synthetic identifier reference — resolve by its synthetic name.
      let name = format!("·$_{n}");
      ctx.resolve(&name, node.id, scope);
    }

    NodeKind::Fn { params, body, .. } => {
      // Create a new fn scope.
      // Try to get fn name from context (the LHS of a Bind).
      let fn_scope = ctx.push_scope(ScopeKind::Fn, Some(scope), node.id);

      // Register params as bindings in the fn scope.
      if let NodeKind::Patterns(pat_items) = &params.kind {
        for param in &pat_items.items {
          register_pattern_binds(param, fn_scope, ctx);
        }
      }

      // Walk body statements.
      walk_stmts(&body.items, fn_scope, ctx);

      // Pop fn scope bindings.
      ctx.pop_scope_binds(fn_scope);
    }

    NodeKind::Apply { func, args } => {
      walk_node(func, scope, ctx);
      for arg in &args.items {
        walk_node(arg, scope, ctx);
      }
    }

    NodeKind::Module(items) => {
      walk_stmts(&items.items, scope, ctx);
    }

    NodeKind::InfixOp { lhs, rhs, .. } => {
      walk_node(lhs, scope, ctx);
      walk_node(rhs, scope, ctx);
    }

    NodeKind::UnaryOp { operand, .. } => {
      walk_node(operand, scope, ctx);
    }

    NodeKind::Match { subjects, arms, .. } => {
      for subj in &subjects.items {
        walk_node(subj, scope, ctx);
      }
      for arm in &arms.items {
        walk_node(arm, scope, ctx);
      }
    }

    NodeKind::Arm { lhs, body, .. } => {
      let arm_scope = ctx.push_scope(ScopeKind::Arm, Some(scope), node.id);
      // Register pattern bindings from the arm LHS.
      register_pattern_binds(lhs, arm_scope, ctx);
      // Walk guard expressions in patterns for reference resolution.
      walk_pattern_refs(lhs, arm_scope, ctx);
      // Walk body in arm scope.
      walk_stmts(&body.items, arm_scope, ctx);
      ctx.pop_scope_binds(arm_scope);
    }

    NodeKind::Pipe(items) => {
      for item in &items.items {
        walk_node(item, scope, ctx);
      }
    }

    NodeKind::Group { inner, .. } => walk_node(inner, scope, ctx),
    NodeKind::Try(inner) => walk_node(inner, scope, ctx),
    NodeKind::Member { lhs, .. } => walk_node(lhs, scope, ctx),
    NodeKind::Spread { inner: Some(inner), .. } => walk_node(inner, scope, ctx),
    NodeKind::Spread { inner: None, .. } => {}
    NodeKind::ChainedCmp(parts) => {
      for part in parts {
        if let crate::ast::CmpPart::Operand(n) = part { walk_node(n, scope, ctx); }
      }
    }

    NodeKind::LitSeq { items, .. } => {
      for item in &items.items {
        walk_node(item, scope, ctx);
      }
    }

    NodeKind::LitRec { items, .. } => {
      // Record literals use Arm nodes for fields ({x: 1} → Arm(Ident("x"), LitInt("1"))).
      // Don't create arm scopes — these are value expressions, not pattern matches.
      for item in &items.items {
        match &item.kind {
          NodeKind::Arm { lhs: _, body, .. } => {
            // Walk the field value only (not the key — it's a literal key, not a binding).
            for stmt in &body.items { walk_node(stmt, scope, ctx); }
          }
          _ => walk_node(item, scope, ctx),
        }
      }
    }

    NodeKind::Block { name, params, body, .. } => {
      walk_node(name, scope, ctx);
      walk_node(params, scope, ctx);
      walk_stmts(&body.items, scope, ctx);
    }

    NodeKind::StrTempl { children, .. } | NodeKind::StrRawTempl { children, .. } => {
      for child in children { walk_node(child, scope, ctx); }
    }

    // Leaves — no children to walk.
    NodeKind::LitBool(_) | NodeKind::LitInt(_) | NodeKind::LitFloat(_)
    | NodeKind::LitDecimal(_) | NodeKind::LitStr { .. }
    | NodeKind::Partial | NodeKind::Wildcard | NodeKind::Token(_) | NodeKind::Patterns(_) => {}

    NodeKind::BindRight { lhs, rhs, .. } => {
      walk_node(lhs, scope, ctx);
      walk_node(rhs, scope, ctx);
    }
  }
}

// ---------------------------------------------------------------------------
// Formatter — produces nested scope output for tests
// ---------------------------------------------------------------------------

pub fn format_result(result: &ScopeResult) -> String {
  let mut out = String::new();
  if !result.scopes.is_empty() {
    format_scope(ScopeId(0), result, &mut out, 0);
  }
  // Trim trailing newline.
  while out.ends_with('\n') { out.pop(); }
  out
}

fn format_scope(scope_id: ScopeId, result: &ScopeResult, out: &mut String, indent: usize) {
  let info = result.scopes.get(scope_id);
  let kind_str = match info.kind {
    ScopeKind::Module => "module".to_string(),
    ScopeKind::Fn => "fn".to_string(),
    ScopeKind::Arm => "arm".to_string(),
  };

  write_indent(out, indent);
  out.push_str(&format!("scope {}, '{}',\n", info.ast_id.0, kind_str));

  // Events in source order — bindings, refs, and child scopes interleaved.
  let events = result.scope_events.get(scope_id);
  for event in events {
    match event {
      ScopeEvent::Bind(bind_id) => {
        let bind = result.binds.get(*bind_id);
        write_indent(out, indent + 1);
        match bind.origin {
          BindOrigin::Ast(ast_id) => out.push_str(&format!("bind {}, '{}'\n", ast_id.0, bind.name)),
          BindOrigin::Builtin(idx) => out.push_str(&format!("builtin {}, '{}'\n", idx, bind.name)),
        }
      }
      ScopeEvent::Ref(r) => {
        write_indent(out, indent + 1);
        if r.kind == RefKind::Unresolved {
          out.push_str(&format!("unresolved '{}'\n", r.name));
          continue;
        }
        let bind_origin = &result.binds.get(r.bind_id).origin;
        let kind_prefix = match r.kind {
          RefKind::Ref => "ref",
          RefKind::FwdRef => "fwd_ref",
          RefKind::SelfRef => "self_ref",
          RefKind::Unresolved => unreachable!(),
        };
        let origin_str = match bind_origin {
          BindOrigin::Ast(ast_id) => format!("{}", ast_id.0),
          BindOrigin::Builtin(idx) => format!("builtin {}", idx),
        };
        if r.depth > 0 {
          out.push_str(&format!("{} '{}', {}, depth {}\n", kind_prefix, r.name, origin_str, r.depth));
        } else {
          out.push_str(&format!("{} '{}', {}\n", kind_prefix, r.name, origin_str));
        }
      }
      ScopeEvent::ChildScope(child_id) => {
        out.push('\n');
        format_scope(*child_id, result, out, indent + 1);
      }
    }
  }
}

fn write_indent(out: &mut String, level: usize) {
  for _ in 0..level {
    out.push_str("  ");
  }
}

// ---------------------------------------------------------------------------
// Test function — called by test macro for `expect scope`
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;

  fn scope(src: &str) -> String {
    match crate::to_desugared(src) {
      Ok(desugared) => format_result(&desugared.scope),
      Err(e) => format!("ERROR: {e}"),
    }
  }

  test_macros::include_fink_tests!("src/passes/scopes/test_scope.fnk");
}
