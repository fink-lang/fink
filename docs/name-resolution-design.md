# Name Resolution — Scope Graph Design

## Overview

Name resolution produces three property graphs, all keyed by `CpsId`:

1. **`resolution`** — classifies how each `Ref::Name` resolves
2. **`bind_scope`** — maps each bind to its owning scope
3. **`parent_scope`** — maps each scope to its parent scope

Scopes are not a separate ID space — a scope is identified by the `CpsId`
of the node that introduces it (a `LetFn`, match arm body, record field body,
etc.). The module root uses a sentinel CpsId.

## Resolution enum

```rust
enum Resolution {
    Local(CpsId),                         // bind is in the same scope
    Captured { bind: CpsId, depth: u32 }, // bind is across fn boundaries
    Recursive(CpsId),                     // fn references its own name
    Unresolved,                           // no binding found
}
```

- **`depth`** counts the number of `LetFn` boundaries crossed.
  Only `LetFn` boundaries count — match arms, record field bodies, and other
  body-introducing nodes are visibility boundaries but not capture boundaries.

## Scope tree

Every indented body creates a new scope:

- `fn` body → new scope (crossing = capture)
- match arm body → new scope (crossing ≠ capture)
- record field body → new scope (bindings don't leak)
- any continuation body → inherits parent scope unless it introduces bindings

```cypher
// Example: nested fns
(s0:Scope {kind: "module"})
(s1:Scope {kind: "fn"}) -[:PARENT_SCOPE]-> (s0)
(s2:Scope {kind: "fn"}) -[:PARENT_SCOPE]-> (s1)
(s3:Scope {kind: "fn"}) -[:PARENT_SCOPE]-> (s2)
```

The scope kind is not stored separately — it's derivable from the CPS node
at that CpsId (is it a `LetFn` or not?).

## Property graphs

```rust
struct ResolveResult {
    resolution:   PropGraph<CpsId, Option<Resolution>>,
    bind_scope:   PropGraph<CpsId, Option<CpsId>>,
    parent_scope: PropGraph<CpsId, Option<CpsId>>,
}
```

All sized to the full CpsId space. Most entries `None`.

### `resolution`

Populated for every `Ref::Name` node. The variant tells downstream passes
everything they need — no re-walking required.

### `bind_scope`

Populated for every `Bind` node. Maps the bind's CpsId to the CpsId of
the scope-introducing node that owns it.

### `parent_scope`

Populated only for scope-introducing nodes. Maps each scope's CpsId to
the CpsId of its parent scope (`None` for the module root).

## Classification algorithm

During resolution, the pass already walks scopes. When a ref is resolved:

1. Find the ref's current scope (`ref_scope`).
2. Find the bind's scope via `bind_scope`.
3. If `ref_scope == bind_scope` → `Local(bind_id)`.
4. If bind is the fn whose body we're inside → `Recursive(bind_id)`.
5. Otherwise walk from `ref_scope` to `bind_scope` via `parent_scope`,
   count `LetFn` boundaries crossed → `Captured { bind: bind_id, depth }`.
6. If no bind found → `Unresolved`.

## Example — nested capture

```fink
outer = fn a:
  middle = fn b:
    inner = fn c:
      a + b + c
```

```cypher
// Scopes
(s_mod) -[:PARENT_SCOPE]-> ()
(s_outer:LetFn) -[:PARENT_SCOPE]-> (s_mod)
(s_middle:LetFn) -[:PARENT_SCOPE]-> (s_outer)
(s_inner:LetFn) -[:PARENT_SCOPE]-> (s_middle)

// Bind scopes
(bind_a) -[:BIND_SCOPE]-> (s_outer)
(bind_b) -[:BIND_SCOPE]-> (s_middle)
(bind_c) -[:BIND_SCOPE]-> (s_inner)

// Resolutions
(ref_a) -> Captured { bind: bind_a, depth: 2 }  // crosses inner, middle
(ref_b) -> Captured { bind: bind_b, depth: 1 }  // crosses inner
(ref_c) -> Local(bind_c)                         // same scope
```

## Example — record field body scope

```fink
foo = {
   spam:
     ni = 3       # ni bound in field body scope, does not leak
     ni * 2       # spam field receives this value
}
# ni is not visible here
```

```cypher
(s_mod)
(s_field) -[:PARENT_SCOPE]-> (s_mod)

(bind_ni) -[:BIND_SCOPE]-> (s_field)
(ref_ni)  -> Local(bind_ni)          // same scope, no fn boundary
```

## Test output format

Tests output one line per resolved `Ref::Name`. Each line shows the
resolution classification and the bind's scope:

```
(ref ID, name) == (local (bind ID, name)) in scope ID
(ref ID, name) == (captured DEPTH, (bind ID, name)) in scope ID
(ref ID, name) == (recursive (bind ID, name)) in scope ID
(ref ID, name) == unresolved
```

The `in scope ID` is the CpsId of the scope-introducing node that owns
the bind (the bind's scope, not the ref's).

Example (nested capture):
```
(ref 30, a) == (captured 2, (bind 10, a)) in scope 5
(ref 31, b) == (captured 1, (bind 20, b)) in scope 15
(ref 32, c) == (local (bind 28, c)) in scope 25
```

## Downstream use

- **Closure hoisting / lambda lifting**: read `resolution` to find all
  `Captured` refs, thread captured values through as extra params.
  `depth` tells you how many intermediate scopes need threading.
- **Diagnostics**: `Unresolved` refs are errors.
- **Scope tree** (`bind_scope` + `parent_scope`): available for any pass
  that needs to reason about scope ownership.
