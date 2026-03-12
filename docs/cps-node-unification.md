# CPS Node Unification — Val gets CpsId

## Problem

`Val` (trivial values: idents, keys, literals) and `Expr` (computation nodes)
are separate types. Only `Expr` carries a `CpsId`. This means:

- Vals are invisible to property graphs — no way to attach resolution, types,
  or other per-node metadata to a key reference or literal.
- Analysis passes that need to annotate Vals (name resolution, type inference)
  must either embed metadata in the tree (breaking the PropGraph design) or
  introduce a second ID space.

## Solution: `Node<K>` — generic shell with shared CpsId

Unify `Val` and `Expr` into a single `Node<K>` struct parameterised by its
kind type. Both share the same `CpsId` space.

```rust
struct Node<'src, K> {
  id: CpsId,
  kind: K,
  meta: Meta,
}

type Expr<'src> = Node<'src, ExprKind<'src>>;
type Val<'src>  = Node<'src, ValKind<'src>>;
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
- The free_vars pass (deprecated) continues to work unmodified.

### Why not merge Val into Expr?

Merging would lose the compile-time guarantee that fields like `App.func` or
`If.cond` can only hold trivial values. The generic `Node<K>` approach
preserves this guarantee while sharing the ID infrastructure.

## Dependency chain

This is a refactoring change affecting multiple layers:

1. **ir.rs** — introduce `Node<K>`, update `Val`/`Expr` to type aliases
2. **transform.rs** — assign CpsIds to Val nodes
3. **fmt.rs** — access `val.id` where needed (mostly transparent via type alias)
4. **free_vars.rs** — transparent (deprecated, but should still compile)

## Downstream: name resolution pass

With Vals carrying CpsIds, name resolution becomes:

```
PropGraph<CpsId, Option<Resolution>>
```

- Populated for every `Key` Val node (the CpsId of the Val containing the Key)
- `None` for non-Key nodes (literals, idents, all Expr nodes)
- Sparse — use `Option<Resolution>` as the PropGraph element type

This replaces the current `Key.resolution: Option<Resolution>` field, which
can then be removed from the IR. Resolution becomes a side table, consistent
with the property graph design.

## Future: `Key.resolution` removal

Once the resolution pass produces `PropGraph<CpsId, Option<Resolution>>`,
the `resolution` field on `Key` becomes redundant. It can be removed in a
follow-up, making the CPS tree fully immutable after construction.
