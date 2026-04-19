# `src/passes/partial` — partial-application desugaring (`?`)

Rewrites `?` placeholders in source into synthetic `Fn` nodes that bind
a single parameter `$`. Runs as part of the desugar phase, before scope
analysis.

```fink
add 2, ?      ⇢   fn $: add 2, $
2 | add ?, 3  ⇢   2 | (fn $: add $, 3)
[1, 2, 3] | map (multiply ?, 2)  ⇢  [1, 2, 3] | map (fn $: multiply $, 2)
```

## Scope rules

- `?` bubbles up to the **nearest enclosing scope boundary**.
- Scope boundaries: `Group (...)`, each segment of a `Pipe`, top of a
  statement.
- Everything else is transparent: `Apply`, `InfixOp`, `UnaryOp`,
  `Member`, `Range`, `Spread`, `LitSeq`, `LitRec`, `StrTempl`,
  `Bind` (RHS only), `BindRight` (LHS only).
- All `?`s in the same scope become the **same** single param `$`. So
  `add ?, ?` is `fn $: add $, $`, not `fn $a, $b: add $a, $b`.
- `?` in **pattern position** (Arm lhs, Bind lhs) is a compile error.

## Implementation

Append-only on the flat AST arena (per the
[arena contract](../ast/arena-contract.md)):

- `has_partial(ast, id)` is a pure read that reports whether a subtree
  contains any `Partial` node not crossing a `Group` boundary.
- `replace_partial` produces fresh node ids for any subtree that
  changes; unchanged subtrees return their own id (fast path).
- The pass takes ownership of the input `Ast`, reopens it as an
  `AstBuilder`, and returns a new `Ast` whose root is the transformed
  Module. Old ids remain reachable; rewrites appear as fresh appended
  nodes.

`test_partial.fnk` is the `.fnk` test fixture (see
[`crates/test-macros/`](../../../crates/test-macros/)).
