# `src/passes/cps` — CPS IR and Transform

Lowers desugared AST into Continuation-Passing-Style IR. Every intermediate
result has an explicit name; control flow is explicit via continuations.

## Contracts and design

- [transform-contract.md](transform-contract.md) — every pass that takes a
  `CpsResult` and produces a new one must uphold this contract (origin
  density, AST-id forwarding, no in-place mutation).
- [ir-design.md](ir-design.md) — conceptual overview of the IR
  (`Bind`/`Ref`/`Lit`/`BuiltIn`/`Cont`/etc.).
- [node-unification.md](node-unification.md) — design rationale for the
  `Node<K>` shell that gives `Val` and `Expr` a shared `CpsId` space.
