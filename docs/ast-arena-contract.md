# AST Arena Contract

The AST is stored in a **flat, append-only arena**. This document spells out
the contract every pass that touches the AST must honour.

The current implementation is mid-migration (see
`src/passes/ast/mod.rs` — `Ast`, `AstBuilder`, `appended_only`). Today's
code still has the owning-tree `Node` shape; the flat arena types exist
alongside it. The contract described here is the target state and applies
to the new types as soon as they are used.

---

## Core types

```rust
pub struct Ast<'src> {
  pub nodes: PropGraph<AstId, Node<'src>>,
  pub root: AstId,
}

pub struct AstBuilder<'src> { /* owning, single lifetime */ }
```

`Ast` is the AST value type. Not a wrapper, not a pipeline stage marker —
the pair `(nodes, root)` is what it means to have "an AST". Neither half
is meaningful alone: `root` without `nodes` is a dangling id, `nodes`
without `root` is a bag of disconnected subtrees with no entry point.

`AstBuilder` is the only append-authoritative handle. Its API is
deliberately narrow — `new`, `from_ast`, `append`, `read`, `len`,
`is_empty`, `finish`. There is no `set`, no `get_mut`, no public field
access. A pass that wants to extend the arena must go through a builder.

---

## The append-only invariant

**A pass never overwrites an existing slot.** A pass takes `Ast` by value,
wraps it in `AstBuilder::from_ast`, walks from the input root, appends any
new or replacement nodes to the arena, and finishes with a new root id.
Nodes that don't need rewriting stay at their original slots, untouched.

Consequences:

- Any `AstId` stored in a side-table (`PropGraph<AstId, T>`) at any point
  in the pipeline **remains valid for every subsequent pass**. No remap,
  no invalidation, no versioning. The node the id pointed at is still
  there, byte-for-byte identical.

- The `nodes` vec is a **persistent data structure**. Each pass's root
  picks out its own reachable tree from the shared arena. Older roots
  remain walkable if a debug tool or differential pass keeps them around.

- Unreachable older slots are not a bug — they are history. A debug
  dump of `ast.nodes.iter()` will include nodes that the current root
  no longer reaches. Walks must always start from a named root.

- Compaction (dropping unreachable slots) is an optional, offline
  operation. It is never part of a regular pass.

---

## The "append a parent copy" pattern

When a pass rewrites a node, it doesn't mutate the node — it appends a
replacement. If the replacement must be referenced from the parent, the
parent is **also** appended (as a fresh copy with an updated child id).
The same applies up the chain to the root.

Example: a desugar pass wants to replace `Foo(old)` inside `Bar(Foo(old))`
with `Foo(new)`:

```
before:
  slot 0: Leaf("old")
  slot 1: Foo(inner = 0)
  slot 2: Bar(inner = 1)   ← old root

after (append-only):
  slot 0: Leaf("old")      ← untouched
  slot 1: Foo(inner = 0)   ← untouched
  slot 2: Bar(inner = 1)   ← untouched, unreachable from new root
  slot 3: Leaf("new")      ← appended
  slot 4: Foo(inner = 3)   ← appended
  slot 5: Bar(inner = 4)   ← appended, new root
```

Every old id (0, 1, 2) still resolves to its original node. Any side-table
built against the pre-pass Ast (e.g. a source-location propgraph indexed
by AstId) continues to work against the post-pass Ast unchanged.

If a rewrite touches only a leaf of a large subtree, the majority of the
old tree is not copied — only the ancestors from the rewrite site up to
the root are appended. In practice most passes append a handful of nodes
per invocation and leave 99% of the arena untouched.

---

## Fast-path: unchanged subtrees

A pass's default behaviour when walking a subtree that doesn't need
changes is to **return the same id it received**, with no append. This
is the single most important optimisation for append-only rewrites:

```rust
fn rewrite(builder: &mut AstBuilder, src: &Ast, id: AstId) -> AstId {
  if !needs_rewrite(src, id) {
    return id;               // untouched — no append, no allocation
  }
  // ... walk children, append replacements, append new parent ...
  builder.append(NodeKind::..., loc)
}
```

A pass that only touches one subtree out of a 10000-node Ast appends
maybe 20 nodes (the rewritten subtree plus the ancestor chain) and
returns quickly.

---

## Side-tables and id stability

Every pass that produces a `PropGraph<AstId, T>` side-table — e.g. CPS
origin tracking, source-location hints, scope bind tables — can key
against the AstIds of whatever arena state was current when the table was
built. **All those keys remain valid forever.** A table built before
desugar still works against the post-desugar Ast, the post-lifting Ast,
and so on.

This is the single strongest argument for the append-only rule: without
it, every downstream side-table has to worry about remapping when the
arena changes shape. With it, side-tables compose trivially.

---

## Runtime checks

Two mechanisms back the invariant:

1. **Compile-time:** `AstBuilder` has no mutation API. Passes that use it
   cannot rewrite old slots. Reaching into `ast.nodes` directly via
   `PropGraph::set` / `get_mut` is legal Rust but is a glaring signal in
   code review. Keep such reaches out of the normal pass path; they
   belong (if anywhere) in debug tooling or a compaction pass.

2. **Runtime:** `passes::ast::appended_only(before, after) -> Result<(),
   String>` verifies the invariant by walking the old arena length and
   checking every slot is byte-for-byte identical in the new arena, plus
   that the new arena is at least as long as the old. Use it in pass
   tests via `debug_assert!`:

   ```rust
   let before_snapshot = input.clone();
   let output = my_pass::apply(input);
   debug_assert!(appended_only(&before_snapshot, &output).is_ok());
   ```

   `AstBuilder::finish` also asserts that the builder's internal length
   never fell below its `start_len` — a cheap tripwire for accidental
   external mutation while the builder is live.

---

## When to violate the rule

Compaction and debug tooling are allowed to rewrite the arena. They do
so **outside** `AstBuilder` by reaching into `ast.nodes` directly.
Mark any such site with a comment explaining why:

```rust
// INVARIANT BREAK: this is a compaction pass; it rewrites the arena
// in place because all downstream consumers have been remapped.
ast.nodes.set(id, compacted_node);
```

If the comment isn't obvious, the code is wrong.

---

## Why not a tree?

The previous AST shape was an owning tree (`NodeKind` variants held
`Box<Node>` / `Vec<Node>`). The migration to the flat arena was driven
by three concrete problems:

- **`unsafe` in `DesugaredAst`:** the struct carried both an owning
  `Box<ParseResult>` and a shadow index `PropGraph<AstId, Option<&Node>>`
  that pointed back into the box. Constructing the two together required
  an `unsafe` reborrow at `src/passes/mod.rs:78`. The flat arena removes
  the need for the shadow index — `ast.nodes.get(id)` is the index —
  and deletes the unsafe.

- **Pass rewrites are expensive.** Rebuilding an owning tree means
  cloning untouched subtrees. Append-only rewrites clone only the
  changed nodes and their ancestors.

- **Side-tables break under mutation.** Any propgraph keyed by `AstId`
  loses its meaning if a pass rewrites a node in place. Append-only
  preserves id stability as an invariant of every pass, so side-tables
  compose without remap logic.

The second and third points apply equally to CPS. A follow-up phase of
this refactor is expected to migrate `CpsResult` to the same
`{ nodes: PropGraph<CpsId, _>, root: CpsId }` shape with the same
append-only invariant.

---

## Non-goals

- The arena is not a general-purpose graph. It is `Vec<Node>` with typed
  indexing. Every id maps to exactly one slot; there are no back-edges,
  no mutability, no reference counting.

- The arena is not thread-safe. A pass runs single-threaded, takes
  ownership of the input Ast, produces a new Ast, and hands it to the
  next pass. No shared access.

- The arena is not canonicalised. Two structurally identical subtrees
  at different slots are distinct. Hash-consing can be added as an
  optimisation later but is not required for correctness.

- `Node.id` may exist as a field for backwards-compatibility but should
  eventually be dropped — it's redundant with the slot index and is
  never consulted except by debug tooling.

---

## References

- `src/passes/ast/mod.rs` — `Ast`, `AstBuilder`, `appended_only`
- `src/propgraph.rs` — the underlying dense storage
- `docs/cps-ir-design.md` — sibling document for CPS; CPS will adopt the
  same pattern in a follow-up phase
