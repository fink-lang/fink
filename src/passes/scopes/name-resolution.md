# Name Resolution — Scope Graph Design

This pass walks the desugared AST and computes a scope graph: every
binding gets a `BindId`; every reference resolves to one. Lifting and the
CPS transform consume the resulting tables to emit
`Ref::Synth(target_cps_id)` instead of name-based references, so later
passes can rearrange the tree freely without breaking scope.

The whole pass is AST-keyed. Names are resolved before CPS lowering.

## Typed IDs

```rust
pub struct ScopeId(pub u32);   // dense; index into ScopeResult.scopes
pub struct BindId(pub u32);    // dense; index into ScopeResult.binds
```

Both are debug-printed as `S<n>` and `B<n>`. Live in
[`mod.rs`](mod.rs).

## Scope kinds

A scope is created at one of three syntactic boundaries:

```rust
pub enum ScopeKind {
  Module,  // top of file — bindings are mutually recursive (forward refs OK)
  Fn,      // function body — sequential bindings (visible to later siblings only)
  Arm,     // match arm — pattern bindings visible in arm body
}
```

The kind drives both lookup behaviour (forward refs only inside `Module`)
and downstream policy (closure/capture analysis treats `Fn` as the
boundary that triggers a closure).

## Bindings and references

```rust
pub enum BindOrigin {
  Ast(AstId),    // user-source binding; AstId points at the binding node
  Builtin(u32),  // prelude / compiler-injected; u32 is a stable index
}

pub struct BindInfo {
  pub scope: ScopeId,
  pub name: String,
  pub origin: BindOrigin,
}

pub enum RefKind {
  Ref,          // normal — binding already in scope when ref is encountered
  FwdRef,       // forward — binding later in the same Module scope
  SelfRef,      // fn references its own binding by name
  Unresolved,   // no binding found; surfaces as a name-error diagnostic
}

pub struct RefInfo {
  pub kind: RefKind,
  pub name: String,
  pub bind_id: BindId,
  pub depth: u32,    // scope levels up (0 = same scope as the ref)
  pub ast_id: AstId,
}
```

`depth` counts every enclosing scope, not just `Fn` boundaries. The
`Fn`-boundary count (the number that matters for capture analysis) is
derived in the `lifting` pass by walking the parent chain and counting
`Fn` kinds — keeping that derivation outside this pass means scopes/
stays a structural index, with all "what does this mean for the runtime"
policy living downstream.

## ScopeResult

```rust
pub struct ScopeResult {
  pub scopes:       PropGraph<ScopeId, ScopeInfo>,
  pub binds:        PropGraph<BindId, BindInfo>,
  pub resolution:   PropGraph<AstId, Option<BindId>>,
  pub scope_events: PropGraph<ScopeId, Vec<ScopeEvent>>,
}
```

- `scopes` — every scope's `kind`, `parent` (`Option<ScopeId>`), and the
  `AstId` of the node that opened it.
- `binds` — every binding, in dense `BindId` order.
- `resolution` — sized to the full AST node count. For each AstId that is
  a reference, holds `Some(BindId)` or `None` (unresolved). Most slots
  are `None` — non-ref nodes never get an entry.
- `scope_events` — per-scope, in source order, the interleaved sequence
  of bindings, references, and child scopes encountered during the walk.
  This lets downstream passes (CPS, lifting) walk in source order without
  reparsing the AST.

```rust
pub enum ScopeEvent {
  Bind(BindId),
  Ref(RefInfo),
  ChildScope(ScopeId),
}
```

## Resolution algorithm

A single `name_stack: Vec<(String, BindId, ScopeId)>` is the working
state. Walk the AST in source order:

1. **Open a scope** (`Module`/`Fn`/`Arm`): push a `ScopeInfo`; remember
   the previous `name_stack` length so we can pop on close.
2. **For `Module` scopes**, pre-register every binding before walking
   any body — so forward refs inside the module resolve. Pre-registration
   adds to `name_stack` but does **not** emit a `ScopeEvent::Bind`; the
   event is emitted later when the binding is encountered in source
   order.
3. **For `Fn` and `Arm` scopes**, push bindings as they're encountered
   (sequential).
4. **For each reference**, walk `name_stack` from the top looking for the
   name. The match's distance from the top tells you `depth`. Classify:
   - Same `AstId` as the binding currently being defined → `SelfRef`.
   - Bound after the current source position in the same `Module` →
     `FwdRef`.
   - Otherwise → `Ref`.
   - No match → `Unresolved` (after also consulting the prelude
     `builtins` list).
5. **Close the scope**: truncate `name_stack` back to the saved length.

The pass never mutates the AST. Output is the four `PropGraph`s above.

## Downstream consumers

- **CPS transform** ([`../cps/`](../cps/)) reads `resolution` to emit
  `Ref::Synth(cps_id_of_binding)` instead of name-based refs. The map
  from `AstId` → pre-allocated `CpsId` is set up before lowering, so the
  CPS tree never has to look up a name.
- **Lifting** ([`../lifting/`](../lifting/)) walks `parent` chains in
  `scopes` and counts `ScopeKind::Fn` boundaries to compute capture
  depth. All capture/closure logic lives there, not here.
- **Diagnostics** flag `RefKind::Unresolved` as an error.

## Example — nested fns

```fink
outer = fn a:
  middle = fn b:
    inner = fn c:
      a + b + c
```

After scope analysis:

| Scope | Kind | Parent | Binds |
|---|---|---|---|
| `S0` | `Module` | — | `outer` |
| `S1` | `Fn` (outer's body) | `S0` | `a`, `middle` |
| `S2` | `Fn` (middle's body) | `S1` | `b`, `inner` |
| `S3` | `Fn` (inner's body) | `S2` | `c` |

References inside `inner`'s body resolve as:

| Ref | `RefKind` | `bind_id` | `depth` |
|---|---|---|---|
| `a` | `Ref` | `B(a)` | 3 |
| `b` | `Ref` | `B(b)` | 2 |
| `c` | `Ref` | `B(c)` | 0 |

`depth` here is the count of intervening scopes (any kind). Lifting
computes the *capture depth* (count of intervening `Fn` scopes) by
walking the parent chain.
