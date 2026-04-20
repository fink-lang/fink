# Name Resolution — Scope Graph Design

The scopes pass runs over the **AST**, before CPS lowering. It answers "for every identifier reference in the source, which binding does it refer to?" and records the ordering information CPS lowering and lifting need.

Inputs: `&Ast`, plus a slice of builtin names (the prelude).
Output: `ScopeResult`.

## Identifier spaces

- `ScopeId` — dense index over scopes.
- `BindId` — dense index over source-level bindings.
- `AstId` — pre-existing; used as the key for ref-resolution lookups.

All three live in `PropGraph`s.

## Scope kinds

```rust
enum ScopeKind {
    Module,  // all bindings mutually recursive
    Fn,      // sequential bindings; fn boundary (captures cross it)
    Arm,     // match arm or other body that introduces bindings
}
```

Every indented body that introduces bindings gets its own scope. Record field bodies and similar constructs reuse `Arm`; they are visibility boundaries but not capture boundaries.

## Core data

```rust
struct ScopeResult {
    scopes:        PropGraph<ScopeId, ScopeInfo>,
    binds:         PropGraph<BindId, BindInfo>,
    resolution:    PropGraph<AstId, Option<BindId>>,
    scope_events:  PropGraph<ScopeId, Vec<ScopeEvent>>,
}

struct ScopeInfo {
    kind:   ScopeKind,
    parent: Option<ScopeId>,
    ast_id: AstId,         // node that created the scope (Module, Fn, Arm, …)
}

struct BindInfo {
    scope:  ScopeId,
    name:   String,
    origin: BindOrigin,    // Ast(AstId) for source bindings, Builtin(u32) for prelude
}

enum RefKind {
    Ref,           // binding is already in scope at the ref site
    FwdRef,        // binding comes later in the same module scope (mutual recursion)
    SelfRef,       // fn references its own name
    Unresolved,    // no binding found anywhere
}

struct RefInfo {
    kind:    RefKind,
    name:    String,
    bind_id: BindId,
    depth:   u32,          // scope levels between ref and bind (0 = same scope)
    ast_id:  AstId,        // the ref's AST node
}

enum ScopeEvent {
    Bind(BindId),
    Ref(RefInfo),
    ChildScope(ScopeId),
}
```

`scope_events` preserves source order per scope — later passes (notably CPS lowering) walk it to emit bindings and refs in the same order they appear in source.

## Classification

`RefKind` encodes both the classic "captured vs local" question and the forward/self-ref distinctions that matter for mutual recursion:

| Relationship | `RefKind` | Notes |
|---|---|---|
| Ref and bind in same scope, bind already seen | `Ref` | ordinary local |
| Ref in child scope, bind in ancestor | `Ref` with `depth > 0` | captured — counted in `depth` |
| Ref in same module scope, bind comes later | `FwdRef` | mutual recursion |
| Fn body references its own name | `SelfRef` | self-recursion |
| No binding found | `Unresolved` | diagnostics |

`depth` counts scope levels between ref and bind. Consumers that care about **capture boundaries** (lifting) filter to `Fn`-kind ancestors in the parent chain.

## Example — nested capture

```fink
outer = fn a:
  middle = fn b:
    inner = fn c:
      a + b + c
```

| Ref | `RefKind` | `depth` | Bind |
|---|---|---|---|
| `a` | `Ref` | 2 | `bind_a` in `s_outer` |
| `b` | `Ref` | 1 | `bind_b` in `s_middle` |
| `c` | `Ref` | 0 | `bind_c` in `s_inner` |

## Example — module-level mutual recursion

```fink
foo = fn: bar 1
bar = fn x: x + 1
```

`foo`'s ref to `bar` is pre-registered in the module scope before walking bodies, so it resolves as `FwdRef` to `bar`'s `BindId`.

## Integration with CPS

The CPS transform reads `ScopeResult` before lowering:

- Every `BindId` gets its `CpsId` pre-allocated via the identity mapping `CpsId(bind_id.0)`, stored in `CpsResult.bind_to_cps`.
- Ref resolution goes `ref AstId → ScopeResult.resolution[ast_id] → BindId → bind_to_cps[BindId] → CpsId`.
- The lowering emits `Ref::Synth(cps_id)` — no string lookup at the CPS level.

This is what makes forward refs and mutual recursion work at arbitrary nesting depth: the ref side of a mutual pair can emit a `Ref::Synth` at lowering time even though the bind side hasn't been constructed yet.

## Downstream use

- **CPS lowering** — reads `resolution` and `bind_to_cps` to emit resolved refs.
- **Lifting** — walks the `depth` + scope chain to find captured variables and thread them as extra params.
- **Diagnostics** — `Unresolved` refs become name-error diagnostics.
