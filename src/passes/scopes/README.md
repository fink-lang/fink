# Scopes

Name resolution over the AST. Builds the scope graph, resolves every identifier reference to its binding site, and records the ordering information lifting needs for forward references and mutual recursion.

## Key files

- [mod.rs](mod.rs) — `ScopeId`, `BindId`, `ScopeInfo`, `BindInfo`, `RefInfo`, `ScopeResult`. Entry point: `analyse`.

## Design

- [name-resolution-design.md](name-resolution-design.md) — three property graphs (`resolution`, `bind_scope`, `parent_scope`), the `Resolution::Local` / `Captured` / `Recursive` / `Unresolved` classification, and how scope kinds (fn body, match arm body, record field body) differ for visibility vs capture.

## Entry point

Read `ScopeResult` in [mod.rs](mod.rs) for the output shape, then `analyse` for the walk. The design doc covers the why; the code is the what.
