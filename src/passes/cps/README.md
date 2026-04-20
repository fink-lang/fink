# CPS

Lowers the desugared AST into a continuation-passing-style IR. Every intermediate result gets an explicit name, every control-flow edge is an explicit continuation. Downstream passes (lifting, WASM codegen) operate on this IR.

## Key files

- [ir.rs](ir.rs) — CPS node types: `Val`, `Expr`, `Bind`, `Ref`, `Cont`, `CpsId`, `CpsResult`. All nodes live in a `PropGraph<CpsId, _>` arena.
- [transform.rs](transform.rs) — AST → CPS lowering. Entry point: `lower_module`.
- [fmt.rs](fmt.rs) — pretty-printer for the IR (debug output, snapshot tests).

## Contracts and design

- [transform-contract.md](transform-contract.md) — invariants every AST → CPS or CPS → CPS pass must uphold.
- [ir-design.md](ir-design.md) — IR shape, metadata strategy (property graphs keyed by `CpsId`), name/ref model, pattern-match lowering.
- [node-unification.md](node-unification.md) — why `Val` and `Expr` share a single `CpsId` space via a generic `Node<K>` shell.

## Entry point

Start with [ir.rs](ir.rs) to understand `CpsId` and the `Val` / `Expr` split, then [transform.rs](transform.rs) for the lowering. Pass authors read [transform-contract.md](transform-contract.md) before writing a CPS → CPS pass.
