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
let synth = gen.bind(Bind::Synth, None);
```

## Passes

| Pass | Input | Output |
|---|---|---|
| [cps/transform.rs](transform.rs) — `lower_module` | AST + ScopeResult | CpsResult |
| [../lifting/mod.rs](../lifting/mod.rs) — `lift` | CpsResult + AST | CpsResult |

Downstream consumers that depend on these invariants: [../wasm/collect.rs](../wasm/collect.rs), [../wasm/emit.rs](../wasm/emit.rs), [fmt.rs](fmt.rs).

## Verifying a CPS pass

There is no runtime equivalent of the AST arena's `appended_only` check for CPS — no tripwire asserts denseness of `origin` or that every node has an entry. When writing a new CPS → CPS pass, add per-pass `debug_assert!`s that:

- `result.origin.len() == node_count` the pass believes it produced.
- every id in the output tree (`Ref::Synth(id)`, `Param` bindings, `App` callees, etc.) is `< result.origin.len()`.
- every source node in the input has a carried-forward origin in the output.

A generalised checker is a follow-up.
