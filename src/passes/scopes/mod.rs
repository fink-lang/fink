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

#[derive(Clone, Debug)]
pub struct BindInfo {
  pub scope: ScopeId,
  pub name: String,
  pub ast_id: AstId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefKind {
  /// Normal reference — binding already in scope.
  Ref,
  /// Forward reference — binding later in same module scope (mutual recursion).
  FwdRef,
  /// Self-reference — fn references its own binding.
  SelfRef,
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

pub struct ScopeResult {
  pub scopes: PropGraph<ScopeId, ScopeInfo>,
  pub binds: PropGraph<BindId, BindInfo>,
  /// For each AST node that is a reference, which BindId does it resolve to.
  pub resolution: PropGraph<AstId, Option<BindId>>,
  /// Ordered list of bindings per scope (for output formatting).
  pub scope_binds: PropGraph<ScopeId, Vec<BindId>>,
  /// Ordered list of child scopes per scope (for nested output).
  pub scope_children: PropGraph<ScopeId, Vec<ScopeId>>,
  /// Ordered list of references per scope (for output formatting).
  pub scope_refs: PropGraph<ScopeId, Vec<RefInfo>>,
}

// ---------------------------------------------------------------------------
// Analysis context (mutable during walk)
// ---------------------------------------------------------------------------

struct Ctx<'src> {
  scopes: PropGraph<ScopeId, ScopeInfo>,
  binds: PropGraph<BindId, BindInfo>,
  resolution: PropGraph<AstId, Option<BindId>>,
  scope_binds: PropGraph<ScopeId, Vec<BindId>>,
  scope_children: PropGraph<ScopeId, Vec<ScopeId>>,
  scope_refs: PropGraph<ScopeId, Vec<RefInfo>>,
  /// Name → BindId lookup stack. Each scope pushes its bindings; pop on exit.
  /// For module scopes, all bindings are pre-registered before walking bodies.
  name_stack: Vec<(String, BindId, ScopeId)>,
  /// Track which AstId is being defined (for self_ref detection).
  current_bind_ast_id: Option<AstId>,
  /// Node count for sizing the resolution graph.
  _node_count: usize,
  _src: std::marker::PhantomData<&'src ()>,
}

impl<'src> Ctx<'src> {
  fn new(node_count: usize) -> Self {
    Self {
      scopes: PropGraph::new(),
      binds: PropGraph::new(),
      resolution: PropGraph::with_size(node_count, None),
      scope_binds: PropGraph::new(),
      scope_children: PropGraph::new(),
      scope_refs: PropGraph::new(),
      name_stack: Vec::new(),
      current_bind_ast_id: None,
      _node_count: node_count,
      _src: std::marker::PhantomData,
    }
  }

  fn push_scope(&mut self, kind: ScopeKind, parent: Option<ScopeId>, ast_id: AstId) -> ScopeId {
    let id = self.scopes.push(ScopeInfo { kind, parent, ast_id });
    self.scope_binds.push(Vec::new());
    self.scope_children.push(Vec::new());
    self.scope_refs.push(Vec::new());
    if let Some(pid) = parent {
      self.scope_children.get_mut(pid).push(id);
    }
    id
  }

  fn push_bind(&mut self, scope: ScopeId, name: &str, ast_id: AstId) -> BindId {
    let id = self.binds.push(BindInfo {
      scope,
      name: name.to_string(),
      ast_id,
    });
    self.scope_binds.get_mut(scope).push(id);
    self.name_stack.push((name.to_string(), id, scope));
    id
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
        let kind = if self.current_bind_ast_id == Some(self.binds.get(bind_id).ast_id) {
          RefKind::SelfRef
        } else if self.scopes.get(bind_scope).kind == ScopeKind::Module
          && self.is_forward_ref(bind_id, ref_ast_id, bind_scope)
        {
          RefKind::FwdRef
        } else {
          RefKind::Ref
        };

        self.resolution.set(ref_ast_id, Some(bind_id));
        self.scope_refs.get_mut(current_scope).push(RefInfo {
          kind,
          name: name.to_string(),
          bind_id,
          depth,
          ast_id: ref_ast_id,
        });
        return;
      }
    }
    // Unresolved — leave as None in resolution.
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
    let bind_ast_id = self.binds.get(bind_id).ast_id;
    // Forward ref: the reference's AstId is before the binding's AstId.
    ref_ast_id.0 < bind_ast_id.0
  }

  /// Remove bindings from name_stack that belong to the given scope.
  fn pop_scope_binds(&mut self, scope: ScopeId) {
    self.name_stack.retain(|(_, _, s)| *s != scope);
  }

}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn analyse<'src>(root: &'src Node<'src>, node_count: usize) -> ScopeResult {
  let mut ctx = Ctx::new(node_count);
  let module_scope = ctx.push_scope(ScopeKind::Module, None, root.id);

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
    scope_binds: ctx.scope_binds,
    scope_children: ctx.scope_children,
    scope_refs: ctx.scope_refs,
  }
}

// ---------------------------------------------------------------------------
// Phase 1: pre-register module-level bindings
// ---------------------------------------------------------------------------

fn pre_register_binds(stmts: &[Node<'_>], scope: ScopeId, ctx: &mut Ctx<'_>) {
  for stmt in stmts {
    if let NodeKind::Bind { lhs, .. } = &stmt.kind {
      register_pattern_binds(lhs, scope, ctx);
    }
  }
}

/// Register bindings from the LHS of a bind pattern.
fn register_pattern_binds(node: &Node<'_>, scope: ScopeId, ctx: &mut Ctx<'_>) {
  match &node.kind {
    NodeKind::Ident(name) => {
      ctx.push_bind(scope, name, node.id);
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
      register_pattern_binds(lhs, scope, ctx);
    }
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
      // Walk RHS first (the value expression).
      walk_node(rhs, scope, ctx);
      ctx.current_bind_ast_id = prev_bind;
      // For non-module scopes, register the binding now (sequential).
      if ctx.scopes.get(scope).kind != ScopeKind::Module {
        register_pattern_binds(lhs, scope, ctx);
      }
    }

    NodeKind::Ident(name) => {
      // This is a reference — resolve it.
      ctx.resolve(name, node.id, scope);
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
    NodeKind::Try(inner) | NodeKind::Yield(inner) => walk_node(inner, scope, ctx),
    NodeKind::Member { lhs, .. } => walk_node(lhs, scope, ctx),
    NodeKind::Spread { inner: Some(inner), .. } => walk_node(inner, scope, ctx),
    NodeKind::Spread { inner: None, .. } => {}
    NodeKind::ChainedCmp(parts) => {
      for part in parts {
        if let crate::ast::CmpPart::Operand(n) = part { walk_node(n, scope, ctx); }
      }
    }

    NodeKind::LitSeq { items, .. } | NodeKind::LitRec { items, .. } => {
      for item in &items.items {
        walk_node(item, scope, ctx);
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
    | NodeKind::Partial | NodeKind::Wildcard | NodeKind::Patterns(_) => {}

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
  if result.scopes.len() > 0 {
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
  out.push_str(&format!("scope {}, '{}':\n", info.ast_id.0, kind_str));

  // Bindings: bind <ast_id>, '<name>'
  let binds = result.scope_binds.get(scope_id);
  for bind_id in binds {
    let bind = result.binds.get(*bind_id);
    write_indent(out, indent + 1);
    out.push_str(&format!("bind {}, '{}'\n", bind.ast_id.0, bind.name));
  }

  // Child scopes.
  let children = result.scope_children.get(scope_id);
  for child_id in children {
    out.push('\n');
    format_scope(*child_id, result, out, indent + 1);
  }

  // References: ref '<name>', <bind_ast_id>[, depth N]
  let refs = result.scope_refs.get(scope_id);
  for r in refs {
    write_indent(out, indent + 1);
    let bind_ast_id = result.binds.get(r.bind_id).ast_id;
    let kind_prefix = match r.kind {
      RefKind::Ref => "ref",
      RefKind::FwdRef => "fwd_ref",
      RefKind::SelfRef => "self_ref",
    };
    if r.depth > 0 {
      out.push_str(&format!("{} '{}', {}, depth {}\n", kind_prefix, r.name, bind_ast_id.0, r.depth));
    } else {
      out.push_str(&format!("{} '{}', {}\n", kind_prefix, r.name, bind_ast_id.0));
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
    match crate::parser::parse(src) {
      Ok(r) => {
        let result = analyse(&r.root, r.node_count as usize);
        format_result(&result)
      }
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  test_macros::include_fink_tests!("src/passes/scopes/test_scope.fnk");
}
