# CPS Node Unification — Val gets CpsId

## Problem

`Val` (trivial values: refs, literals) and `Expr` (computation nodes)
were separate types. Only `Expr` carried a `CpsId`. This meant:

- Vals were invisible to property graphs — no way to attach resolution, types,
  or other per-node metadata to a ref or literal.
- Analysis passes that needed to annotate Vals (name resolution, type inference)
  had to either embed metadata in the tree (breaking the PropGraph design) or
  introduce a second ID space.

## Solution: `Node<K>` — generic shell with shared CpsId

Unify `Val` and `Expr` into a single `Node<K>` struct parameterised by its
kind type. Both share the same `CpsId` space.

```rust
struct Node<K> {
  id: CpsId,
  kind: K,
}

type Expr<'src> = Node<ExprKind<'src>>;
type Val<'src>  = Node<ValKind<'src>>;
type BindNode   = Node<Bind>;
```

### What changes

- `Val` gains an `id: CpsId` field (via the shared `Node` shell).
- The CPS transform assigns IDs to Vals from the same counter as Exprs.
- Node count is `CpsResult.origin.len()` (the origin prop graph covers all nodes).
- PropGraph<CpsId, T> covers all nodes uniformly — no second ID space.

### What stays the same

- `Val` and `Expr` remain distinct types — `func: Box<Val>` still enforces
  at compile time that only trivial values appear in value positions.
- `ValKind` and `ExprKind` remain separate enums.
- All existing field types (`Box<Val>`, `Box<Expr>`, `Vec<Val>`, etc.) unchanged.

### Why not merge Val into Expr?

Merging would lose the compile-time guarantee that fields like `App.func` or
`If.cond` can only hold trivial values. The generic `Node<K>` approach
preserves this guarantee while sharing the ID infrastructure.
