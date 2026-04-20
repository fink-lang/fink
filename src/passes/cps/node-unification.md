# Node Unification — one CpsId space for Val, Expr, and BindNode

`Val` (trivial values — refs, literals) and `Expr` (computation nodes) are both type aliases for a generic `Node<K>` shell that carries a `CpsId`. `BindNode` is the same shape. All three live in a single `CpsId` address space.

```rust
struct Node<K> {
  id: CpsId,
  kind: K,
}

type Val      = Node<ValKind>;
type Expr     = Node<ExprKind>;
type BindNode = Node<Bind>;
```

## Consequences

- Every CPS node — whether a value, expression, or binding — is a key in a `PropGraph<CpsId, T>`. Metadata (origin map, param info, synth aliases) attaches to any node uniformly, with no second id space.
- `Val` and `Expr` remain distinct types at compile time, so fields like `App.func: Box<Val>` and `If.cond: Box<Val>` still enforce that only trivial values appear in value positions.
- Node count is `CpsResult.origin.len()` — the origin PropGraph covers every node exactly once.

## Why not merge `ValKind` into `ExprKind`?

Merging would lose the compile-time guarantee that only trivial values appear in value positions. The generic `Node<K>` shell preserves that guarantee while sharing the id and metadata infrastructure.

## See also

- [ir.rs](ir.rs) — `Node`, `Val`, `Expr`, `BindNode`.
- [ir-design.md](ir-design.md) — the broader IR design that this invariant enables.
