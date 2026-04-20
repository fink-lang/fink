# CPS IR Design

This document describes the CPS IR as implemented in [ir.rs](ir.rs). Output-formatting artifacts (rendered names, `·ret_N`, state threading) are intentionally out of scope here — those are synthesized by the pretty-printer from the structural IR.

---

## Core principles

- Every intermediate result has an explicit name (`BindNode`).
- Control flow is explicit — continuations, not implicit returns.
- Scope is structural (nesting), not a runtime object.
- Every function has an explicit name (user-originated or compiler-synthesised).
- Source names are never stored on IR nodes. Refs point at their binding by `CpsId`; name recovery goes through the origin map.
- Trees stay trees (`Box<Expr>`). Per-node metadata lives in property graphs keyed by `CpsId`, not on the nodes themselves.

---

## Metadata strategy

Pass-computed metadata (origins, param roles, synth aliases, resolution) is stored in **property graphs** — `PropGraph<CpsId, T>` — rather than on IR nodes. Each pass reads upstream property graphs and may produce its own.

Identifier spaces:

- `AstId` (`u32`) — assigned by the parser to every AST node.
- `BindId` (`u32`) — assigned by scope analysis to every source-level binding.
- `CpsId` (`u32`) — assigned by the CPS transform to every IR node (both `Val` and `Expr`).

All three are newtype indices over `PropGraph`. See [../../propgraph.rs](../../propgraph.rs).

Both `Val` and `Expr` are type aliases for `Node<K>`, which carries a `CpsId`. This gives every CPS node a uniform id so property graphs keyed by `CpsId` can annotate values and expressions alike.

---

## Names and references

Source names are never stored in the IR. Every binding and every reference carries only a `CpsId`; the source name (if any) is recovered via the origin map `CpsId → AstId → Ident("foo") | SynthIdent(n)`.

```text
Bind                            -- a definition site (introduces a name into scope)
  SynthName                     -- source-level binding; CpsId pre-allocated from scope analysis
  Synth                         -- compiler-generated temp; no source origin
  Cont(ContKind)                -- continuation parameter; role = Ret | Succ | Fail

Ref                             -- a use site (references a binding)
  Synth(CpsId)                  -- resolved: points at the BindNode with this CpsId
  Unresolved(CpsId)             -- scope analysis found no binding; CpsId carries the ref's AstId for display

BindNode = Node<Bind>           -- definition site with its own CpsId

Param                           -- function parameter
  Name(BindNode)                -- plain param
  Spread(BindNode)              -- varargs: ..rest (only one, trailing position)

Arg                             -- call-site argument
  Val(Val)                      -- plain argument
  Spread(Val)                   -- spread: ..items
  Cont(Cont)                    -- continuation (CPS-plumbed; not a user-visible value)
  Expr(Box<Expr>)               -- nested expression (inlined at the call site)
```

### Pre-allocated CpsIds for source bindings

Source bindings get their `CpsId` allocated *before* the CPS transform runs, using the scope analysis output. This is what lets `Ref::Synth(cps_id)` be emitted at ref sites before the target `BindNode` has been constructed — and therefore what makes mutual recursion and forward references work at any nesting depth.

Mechanics: `BindId` is a dense 0..n index into `ScopeResult.binds`. The CPS allocator starts at n, so `CpsId(bind_id.0)` is the pre-allocated id for each scope bind — the mapping is the identity function. `CpsResult.bind_to_cps: PropGraph<BindId, CpsId>` stores it explicitly so downstream passes don't depend on the offset convention.

### Continuation references as values

Continuations are not refs in the `Ref` enum — they are a separate first-class form:

```text
Cont                            -- a continuation
  Expr { args, body }           -- inline cont body
  Ref(CpsId)                    -- reference to a named cont parameter (e.g. a fail cont passed as arg)
```

---

## Compiler-known operations

```text
BuiltIn                         -- resolved statically, not by scope lookup
  Add, Sub, Mul, Div, ...       -- arithmetic
  Eq, Neq, Lt, Lte, ...         -- comparison
  And, Or, Xor, Not             -- logical
  Shl, Shr, RotL, RotR          -- shifts / rotations
  Range, RangeIncl, In, NotIn   -- range
  Get                           -- member access (.)
  SeqPrepend, SeqConcat         -- [] value construction
  RecPut, RecMerge              -- {} value construction
  StrFmt                        -- string interpolation
  FnClosure                     -- closure construction (used post-lifting)
  IsSeqLike, IsRecLike,
  SeqPop, RecPop, Empty         -- pattern-match primitives
  StrMatch                      -- string-template pattern matching
  Yield, Spawn, Await           -- scheduling
  Channel, Receive              -- channels
  Read                          -- host IO
  Pub, Import, FinkModule       -- module-level markers
  Panic                         -- irrefutable-pattern failure sentinel
  Export                        -- legacy; see ir.rs

Callable                        -- what an App calls
  Val(Val)                      -- runtime value (function reference)
  BuiltIn(BuiltIn)              -- compile-time tag; no CpsId
```

---

## Values

```text
Node<K> { id: CpsId, kind: K }  -- generic shell
Val = Node<ValKind>             -- trivial value (has CpsId)

ValKind
  Ref(Ref)                      -- reference to a binding
  Lit(Lit)                      -- literal value

Lit
  Bool(bool)
  Int(i64)
  Float(f64)
  Decimal(f64)                  -- distinct from Float for the type system
  Str(&str)
  Seq                           -- empty sequence []
  Rec                           -- empty record {}
  Range(RangeKind, ...)         -- literal range value
```

---

## Expressions

```text
Expr = Node<ExprKind>           -- computation node (has CpsId)
```

### Core variants

```text
LetVal { name, val, cont }
  -- bind val to name; visible in cont

LetFn { name, params, fn_body, cont, fn_kind }
  -- bind a function; name visible in fn_body via pre-allocated CpsId
  -- fn_kind: CpsFunction (receives k from caller) vs CpsClosure (closes over k)
  --   see ../wasm/calling-convention.md for the runtime implication

App { func: Callable, args }
  -- call func with args; the cont arg (if any) is the result continuation
  -- func is either a Val (runtime dispatch) or a BuiltIn (compile-time tag)

If { cond, then, else_ }
  -- branch on cond

Ret { value }
  -- terminal: return a value
```

### Pattern matching

All patterns lower to **PatternMatch** — a matcher function applied to a subject. The CPS transform emits `LetFn` + `App` directly; no dedicated match IR nodes exist.

```text
PatternMatch structure (emitted as LetFn + App):

  LetFn body = fn(bind_names...): <continuation>
  LetFn matcher = fn(subj, succ, fail): <matcher_body>
  matcher(subject, body, panic)
```

The matcher tests with temps only; on success it calls `succ(values...)` to forward extracted values to the body. On failure it calls `fail()`.

**Literal / guard patterns** use `BuiltIn::Eq`, `BuiltIn::Gt`, etc. with `If`.

**Match blocks** form a fail-chain: each arm's `fail` calls the next arm's matcher:
`mp_1(subj, k, fn: mp_2(subj, k, fn: panic))`.

### Collection primitives for destructuring

Used inside matcher bodies:

- `SeqPop(seq, fail, cont(head, tail))` — pop head; fail if empty.
- `RecPop(rec, name, fail, cont(value, rest))` — extract named field; fail if missing.
- `Empty(collection, cont(bool))` — predicate; caller branches with `If`.
- `IsSeqLike(v, succ, fail)` / `IsRecLike(v, succ, fail)` — type guards.

Arg order is fail-before-cont so the cont lambda renders last in output.

---

## CpsResult — the pass output

`CpsResult` carries the IR tree plus its metadata side-tables:

| Field | Keyed by | What it holds |
|---|---|---|
| `root: Expr` | — | Tree root. |
| `origin: PropGraph<CpsId, Option<AstId>>` | `CpsId` | AST node each CPS node was synthesised from, or `None` for pure synth. |
| `bind_to_cps: PropGraph<BindId, CpsId>` | `BindId` | Pre-allocated CpsId for each source binding. |
| `synth_alias: PropGraph<CpsId, Option<CpsId>>` | `CpsId` | Populated by lifting — maps new capture-param CpsId back to the original captured binding. |
| `param_info: PropGraph<CpsId, Option<ParamInfo>>` | `CpsId` | Semantic role of each param: `Cap`, `Param`, `Cont`. Populated by lifting. |
| `module_locals: Vec<(CpsId, String)>` | — | Every module-level binding leaf — the authoritative "which CpsIds become WASM globals". |
| `module_imports: BTreeMap<String, Vec<String>>` | — | `import` declarations collected from the AST before lowering. |

---

## Settled design decisions

### First-pass CPS — no load/store/scope

First-pass CPS has no forward refs at the Rust level (they are handled by `bind_to_cps` pre-allocation). The formatter outputs structural CPS directly. Load/store/scope synthesis is deferred to later passes that need it.

### Closure vs LetFn

All fns are potentially closures. The IR uses `LetFn` with implicit captures determined by structural nesting. The lifting pass rewrites `LetFn` to carry explicit capture params and emits `BuiltIn::FnClosure` at the call site for closures that capture values.

### Env / scope / state

All three are output conventions — none are part of the analysis IR:

- **Scope** — compiler concept; structural binding chain; disappears after analysis.
- **Env** — runtime concept; heap record a lifted closure carries; synthesised during lifting.
- **State** — effect/mutation threading; synthesised by the pretty-printer and codegen.

### Yield as a first-class primitive

`Yield` is a core `BuiltIn`, not sugar. It's the foundation for async IO (implicit futures via yield to the scheduler), generators / producers / transducers, channels, and tasks (spawn, join, race, cancel reduce to yield + scheduler). Later passes can color the continuation graph from `Yield` nodes — every continuation reachable from a `Yield` is "suspendable".

---

## Output conventions (pretty-printer only)

The pretty-printer synthesizes these from the structural IR:

**Rendered names:**

- Source bindings (via origin map `Ident("foo")`) → `·foo_<cps_id>`.
- Synthetic source bindings (`SynthIdent(n)`) → `·$_<n>_<cps_id>`.
- Compiler temps (`Bind::Synth`, no origin) → `·v_<cps_id>`.
- Continuation params (`Bind::Cont(kind)`) → `·ret_<cps_id>`, `·succ_<cps_id>`, or `·fail_<cps_id>`.
- Builtins → `·op_eq`, `·seq_pop`, etc.

**Rendering templates:**

```text
LetVal  → ·let val, fn name: cont
LetFn   → ·fn fn params: fn_body, fn name: cont
App     → func args, fn result: cont
If      → ·if cond, fn: then, fn: else
```
