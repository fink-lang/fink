# CPS Transform Contract

Every pass that takes a `CpsResult` and produces a new `CpsResult` must uphold
this contract. Violations cause silent data corruption in downstream prop graph
lookups.

## Rules

### 1. Every node gets an origin entry
Every new CPS node — `Expr`, `Val`, `BindNode` — must be assigned a `CpsId`
via the id allocator, with a corresponding entry pushed to the output origin map.
No node may be constructed with a `CpsId` that has no origin entry.

### 2. Carry forward AST origins
Rewritten nodes that correspond directly to a source-level construct carry the
original `AstId` forward (same origin as the input node). Synthesized nodes
with no direct AST source use `None`.

This preserves source location information for error reporting and source maps
through arbitrarily many transform passes.

### 3. Dense origin map
The output `CpsResult.origin` must be dense: every index `0..node_count` has
an entry. Sparse maps (gaps) cause prop graph `try_get` to return `None` for
valid nodes, silently breaking name recovery and diagnostics.

### 4. Produce a fresh tree
Never mutate the input `CpsResult` in place. Produce a new tree so callers can
hold both before/after for debugging. The input `CpsResult` is consumed
(`lift(result, ...)`) to enforce this at the type level.

## Pattern

Use a `Gen` struct (see `cps/transform.rs`) to allocate ids and track the
origin map:

```rust
fn expr<'src>(&mut self, kind: ExprKind<'src>, origin: Option<AstId>) -> Expr<'src>
fn val<'src>(&mut self, kind: ValKind<'src>, origin: Option<AstId>) -> Val<'src>
fn bind(&mut self, kind: Bind, origin: Option<AstId>) -> BindNode
```

When rewriting an existing node, pass its origin through:

```rust
// Rewrite a LetFn — carry forward the original expr's origin
let new_expr = gen.expr(ExprKind::LetFn { .. }, input_origin);
```

When synthesizing a new node with no AST counterpart:

```rust
let synth = gen.bind(Bind::Gen, None);
```

## Passes

| Pass | Input | Output | Status |
|------|-------|--------|--------|
| `cps/transform` | AST | CpsResult | complete |
| `closure_lifting` | CpsResult + CaptureGraph | CpsResult | in progress |
