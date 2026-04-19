# `src/passes/scopes` — AST-level scope analysis

Computes binding origins and scope structure over the desugared AST,
producing the `BindOrigin` / `ScopeId` / `BindId` tables that downstream
passes (CPS lowering, lifting) consume to resolve names.

## Design

- [name-resolution.md](name-resolution.md) — scope graph design: typed
  IDs, scope kinds, the `ScopeResult` tables, the resolution algorithm,
  and how downstream passes (CPS, lifting) consume the output.
