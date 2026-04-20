# Scopes

Name resolution over the AST. Builds the scope graph, resolves every identifier reference to its binding site, and records the ordering information lifting needs for forward references and mutual recursion.

## Key files

- [mod.rs](mod.rs) — `ScopeId`, `BindId`, `ScopeInfo`, `BindInfo`, `RefInfo`, `ScopeResult`. Entry point: `analyse`.

## Design

- [name-resolution-design.md](name-resolution-design.md) — scope kinds (`Module`, `Fn`, `Arm`), the `ScopeResult` shape, `RefKind::{Ref, FwdRef, SelfRef, Unresolved}` classification with `depth`, and how mutual recursion works via module-scope pre-registration + `bind_to_cps` handoff to CPS.

## Entry point

Read `ScopeResult` in [mod.rs](mod.rs) for the output shape, then `analyse` for the walk. The design doc covers the why; the code is the what.
