// Scope analysis pass — AST-level name resolution.
//
// Walks the AST and builds a scope graph:
// - Each scope has a parent link and ordered bindings
// - Every identifier reference resolves to the CpsId of its binding
// - Mutual recursion: all bindings in a scope are visible to each other
//
// The scope graph survives as PropGraphs for use by CPS transform, lifting,
// and codegen. The CPS transform uses it to emit Ref::Synth(target_cps_id)
// instead of Ref::Name, so that lifting can rearrange the tree without
// breaking scope resolution.
//
// ## Design
//
// 1. Walk the AST, building scopes:
//    - Module level: one scope for all top-level bindings
//    - Fn body: new scope with params as bindings
//    - Let binding: adds to current scope
//    - Match arms: new scope per arm
//
// 2. For each scope, record bindings in order (name → ScopeEntry).
//    A binding is visible to subsequent siblings and nested scopes.
//    Module-level bindings are mutually recursive (all visible to each other).
//
// 3. Resolve every identifier reference to its binding's ScopeEntry.
//    Walk up parent scopes until found.
//
// ## PropGraphs produced
//
// TODO: design the output graphs

#[cfg(test)]
mod tests {
  test_macros::include_fink_tests!("src/passes/scopes/test_scope.fnk");
}
