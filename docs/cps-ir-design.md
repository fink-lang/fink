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
  SeqAppend, SeqConcat        -- [] value construction
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
LetVal { name, val, body }
  -- bind val to name; visible in body

LetFn { name, params, fn_body, body }
  -- bind a function; name NOT visible in fn_body (non-recursive)
  -- captures resolved by name resolution (Resolution::Captured entries)

LetRec { bindings: Vec<Binding>, body }
  -- mutually recursive group; all names visible in all fn_bodies
  -- Binding = { name, params, fn_body }

App { func: Callable, args, result, body }
  -- call func with args; bind result; visible in body
  -- func is either a Val (runtime) or BuiltIn (compile-time)

If { cond, then, else_ }
  -- branch on cond
```

### Suspension

```
Yield { value, result, body }
  -- suspend execution, yield value to scheduler
  -- continuation receives resumed value bound to result
  -- used by later passes to color continuation graphs:
     every continuation reachable from Yield is "suspendable"
```

### Terminal

```
Ret(val)                      -- return value to current continuation
Panic                         -- unconditional failure (irrefutable pattern fail)
FailCont                      -- delegate to enclosing ·ƒ_fail (inside MatchBlock arms)
```

### Pattern lowering primitives

All pattern nodes carry an explicit `fail` continuation (Panic or FailCont).

```
MatchLetVal { name, val, fail, body }
  -- bind val to name; always succeeds (structural uniformity with fail cont)

MatchApp { func: Callable, args, fail, result, body }
  -- apply func to args; fail if tag is wrong
  -- constructor/extractor patterns: Ok b, Some x

MatchIf { func: Callable, args, fail, body }
  -- apply func to args; fail if falsy; no result binding
  -- guard predicates: is_even x, a > 0

MatchValue { val, lit, fail, body }
  -- assert val equals literal; fail if not
  -- literal patterns: [a, 1], ['hello']
```

### Sequence pattern traversal

```
MatchSeq { val, cursor, fail, body }
  -- assert val is a sequence; open cursor

MatchNext { val, cursor, next_cursor, fail, elem, body }
  -- pop head from cursor; bind to elem; fail if empty

MatchDone { val, cursor, fail, result, body }
  -- assert cursor exhausted; forward matched value to result

MatchNotDone { val, cursor, fail, body }
  -- assert cursor non-empty; fail if exhausted

MatchRest { val, cursor, fail, result, body }
  -- bind remaining elements; zero-or-more; works on seq and rec cursors
```

### Record pattern traversal

```
MatchRec { val, cursor, fail, body }
  -- assert val is a record; open cursor

MatchField { val, cursor, next_cursor, field, fail, elem, body }
  -- extract named field; bind to elem; advance cursor
```

Note: `cursor` fields are `u32` formatting hacks — they render as `·m_N` in
the pretty-printer. Will be removed when codegen derives position from structure.

### Match block

```
MatchBlock { params, fail, arm_params, arms, result, body }
  -- try arms in order; first match wins
  -- params: values passed into each arm
  -- arm_params: names each arm receives them as
  -- fail: exhaustion continuation (Panic or outer FailCont)
  -- each arm: lowered Match* chain ending in ·ƒ_cont
  -- result: value received by result cont from winning arm
```

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
- Match cursors — `·m_N`
- Continuations — `·ƒ_cont`, `·ƒ_fail`
- Runtime/injected — `·state`

**Rendering conventions:**
```
LetVal  → ·let val, fn name: body
LetFn   → ·fn fn params: fn_body, fn name: body
App     → ·apply func, args, ·state, fn result, ·state: body
Ret     → ·ƒ_cont val, ·state
BuiltIn → rendered inline as ·op'sym' (operators) or ·prim (data construction)
```
