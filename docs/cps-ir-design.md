# CPS IR Design — Compiler Perspective

This document describes the CPS IR as implemented in `src/passes/cps/ir.rs`.
Output formatting (state threading, ƒ_cont naming) is intentionally
ignored here — those are rendering artifacts synthesized by the pretty-printer
and codegen from the structural IR.

---

## Core principles

- Every intermediate result has an explicit name
- Control flow is explicit — continuations, not implicit returns
- Scope is structural (nesting), not a runtime object
- Every function has an explicit name (user or compiler-generated)
- No stringly-typed logic — all internal branching uses typed enums
- Trees stay trees (`Box<Expr>`) — metadata lives in property graphs, not on nodes

---

## Metadata strategy

Pass-computed metadata (types, resolution, source locations) is stored
in **property graphs** — typed `Vec<Option<T>>` indexed by node IDs — rather than
on IR nodes directly. Each pass reads upstream property graphs and writes its own.

- `AstId(u32)` — assigned by the parser to every AST node
- `CpsId(u32)` — assigned by the CPS transform to every node (both `Val` and `Expr`)
- `PropGraph<Id, T>` — generic property graph type (`src/propgraph.rs`)

Both `Val` and `Expr` are type aliases for `Node<K>`, which carries a `CpsId`.
This gives every CPS node a uniform ID in a single address space, so property
graphs keyed by `CpsId` can annotate values and expressions alike.

See also: `memory/project_property_graphs.md` for the full design rationale.

---

## Names and references

```
Bind                          -- a definition site (introduces a name into scope)
  User                        -- from source; name recoverable via origin map
  Gen                         -- compiler-generated temp: rendered as ·v_{cps_id}

Ref                           -- a use site (references a binding)
  Name                        -- user ref; name recoverable via origin map
  Gen(CpsId)                  -- compiler-generated temp: carries CpsId of the Bind::Gen it refers to

BindNode = Node<Bind>         -- definition site with its own CpsId

Param                         -- function parameter
  Name(BindNode)              -- plain param
  Spread(BindNode)            -- varargs: ..rest (only one, trailing position)

Arg                           -- call-site argument
  Val(Val)                    -- plain argument
  Spread(Val)                 -- spread: ..items
```

## Compiler-known operations

```
BuiltIn                       -- resolved statically, not by scope lookup
  Add, Sub, Mul, Div, ...    -- arithmetic
  Eq, Neq, Lt, Lte, ...      -- comparison
  And, Or, Xor, Not          -- logical
  BitAnd, BitXor, Shl, ...   -- bitwise
  Range, RangeIncl, In, NotIn -- range
  Get                         -- member access (.)
  SeqPrepend, SeqConcat       -- [] value construction
  RecPut, RecMerge            -- {} value construction
  StrFmt                      -- string interpolation

Callable                      -- what an App calls
  Val(Val)                    -- runtime value (function reference)
  BuiltIn(BuiltIn)           -- compile-time tag, no CpsId
```

## Resolution — populated by resolve pass

```
Resolution
  Local(CpsId)                -- bound in current scope
  Captured { bind: CpsId, depth: u32 }
                              -- free variable from outer scope; depth = fn boundary crossings
  Recursive(CpsId)            -- self-reference within same fn (recursive call)
  Unresolved                  -- no binding found (name error)
```

Every variant (except `Unresolved`) carries the CpsId of the Bind node at the definition site.
Stored in `PropGraph<CpsId, Option<Resolution>>` keyed by the Ref node's CpsId — not on the IR nodes.
Complete: produced by `src/passes/name_res/`.

---

## Values — already-computed things

```
Node<K> { id: CpsId, kind: K }  -- generic shell
Val = Node<ValKind>              -- trivial value (has CpsId)

ValKind
  Ref(Ref)                    -- reference to a binding (user or compiler temp)
  Lit(Lit)                    -- literal value

Lit
  Bool(bool)
  Int(i64)
  Float(f64)
  Decimal(f64)                -- distinct from Float for the type system
  Str(&str)
  Seq                         -- empty sequence []
  Rec                         -- empty record {}
```

---

## Expressions

```
Expr = Node<ExprKind>                        -- computation node (has CpsId)
```

### Core

```
LetVal { name, val, cont }
  -- bind val to name; visible in cont

LetFn { name, params, fn_body, cont, fn_kind }
  -- bind a function; name NOT visible in fn_body (non-recursive)
  -- captures resolved by name resolution (Resolution::Captured entries)
  -- fn_kind: CpsFunction (takes k from caller) vs CpsClosure (closes over k)
  --   see docs/calling-convention-v2.md for full design

App { func: Callable, args }
  -- call func with args; last Arg::Cont is the result continuation
  -- func is either a Val (runtime) or BuiltIn (compile-time)

If { cond, then, else_ }
  -- branch on cond
```

### Pattern matching

All patterns lower to **PatternMatch** — a matcher function applied to the subject.
The CPS transform emits `LetFn` + `App` directly; no dedicated match IR nodes exist.

```
PatternMatch structure (emitted as LetFn + App):

  LetFn body = fn(bind_names...): <continuation>
  LetFn matcher = fn(subj, succ, fail): <matcher_body>
  matcher(subject, body, panic)
```

The matcher tests with temps only; on success it calls `succ(values...)` to forward
extracted values to the body. On failure it calls `fail()`.

**Literal/guard patterns:** matcher body uses `op_eq`, `op_gt`, etc. with `If`.

**Match blocks:** fail-chain of matchers — each arm's fail tries the next arm:
`mp_1(subj, k, fn: mp_2(subj, k, fn: panic))`

### Collection primitives

Used inside matcher bodies for sequence and record destructuring:

```
SeqPop(seq, fail, cont(head, tail))
  -- pop head element from sequence; call fail if empty

RecPop(rec, name, fail, cont(value, rest))
  -- extract named field from record; call fail if missing

Empty(collection, cont(bool))
  -- predicate: is the collection empty? Caller branches with If
```

Arg order: fail-before-cont (so the continuation lambda renders last in output).
The "cursor" is the collection itself with items removed — immutable, no iterator protocol.

---

## Settled design decisions

### First-pass CPS — no load/store/scope
First-pass CPS has no forward refs — all names are in scope by construction.
The formatter outputs structural CPS directly (·let, ·fn, ·apply, ·ƒ_cont).
Load/store/scope synthesis is deferred to later passes that need it (closure
conversion, codegen).

### Closure vs LetFn
All fns are potentially closures. The analysis IR uses `LetFn` with implicit
captures from nesting. Name resolution (complete) identifies captures via
`Resolution::Captured { bind, depth }`. Closure hoisting (next pass) will
rewrite `LetFn` nodes to carry explicit capture lists for codegen.

### Env / Scope / State
All three are output conventions — none in the analysis IR:
- **Scope** — compiler concept; structural binding chain; disappears after analysis
- **Env** — runtime concept; heap record a lifted closure carries; synthesized during hoisting
- **State** — effect/mutation threading; synthesized by pretty-printer and codegen

### Yield as first-class primitive
`yield` is a core IR node, not sugar. It's the foundation for:
- Async IO (implicit futures via yield to scheduler)
- Generators / producers / transducers
- Channels
- Tasks (spawn, join, race, cancel reduce to yield + scheduler)

Later passes use Yield nodes to color the continuation graph — every
continuation reachable from a Yield is "suspendable."

---

## Output conventions (pretty-printer only)

The pretty-printer synthesizes these from the structural IR:

**Variable sigils:**
- User vars — plain: `foo`, `bar`
- Compiler temps — `·v_{cps_id}`
- Continuations — `·ƒ_cont`
- Runtime/injected — `·state`

**Rendering conventions:**
```
LetVal  → ·let val, fn name: cont
LetFn   → ·fn fn params: fn_body, fn name: cont
App     → func args, fn result: cont   (builtins: ·op_eq, ·seq_pop, etc.)
If      → ·if cond, fn: then, fn: else
```
