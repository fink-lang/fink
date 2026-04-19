# `src/passes/scopes` — AST-level scope analysis

Computes binding origins and scope structure over the desugared AST,
producing the `BindOrigin` / `ScopeId` / `BindId` tables that downstream
passes (CPS lowering, lifting) consume to resolve names.

## Design

- [name-resolution.md](name-resolution.md) — scope graph design. **Status:
  this file currently describes a superseded CPS-keyed design that lived
  in the deleted `src/passes/name_res/` module.** Phase 1c rewrites the body
  against the live `BindOrigin` / `ScopeId` / `BindId` shape implemented in
  `mod.rs`. Until then, treat the doc as historical and read `mod.rs` for
  current behaviour.
