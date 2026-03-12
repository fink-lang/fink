# CPS IR Design — Compiler Perspective

This document describes the CPS IR as implemented in `src/transform/cps.rs`.
Output formatting (env handles, state threading, ƒ_cont naming) is intentionally
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

Pass-computed metadata (types, resolution, free vars, source locations) is stored
in **property graphs** — typed `Vec<Option<T>>` indexed by node IDs — rather than
on IR nodes directly. Each pass reads upstream property graphs and writes its own.

- `AstId(u32)` — assigned by the parser to every AST node
- `CpsId(u32)` — assigned by the CPS transform to every node (both `Val` and `Expr`)
- `PropGraph<Id, T>` — generic property graph type (`src/propgraph.rs`)

Both `Val` and `Expr` are type aliases for `Node<K>`, which carries a `CpsId`.
This gives every CPS node a uniform ID in a single address space, so property
graphs keyed by `CpsId` can annotate values and expressions alike.

CPS nodes still carry a `Meta { loc, ty }` field as a transitional measure.
`loc` may move to `PropGraph<CpsId, Loc>` in the future; `ty` is a placeholder.

See also: `memory/project_property_graphs.md` for the full design rationale.

---

## Names and keys

```
Name = &'src str              -- plain source name (reference to existing binding)

BindName                      -- a binding site (introduces a name into scope)
  User(Name)                  -- from source: `foo`, `x`, `result`
  Gen(u32)                    -- compiler-generated: rendered as ·v_N

FreeVar                       -- a captured variable from an outer scope
  Name(Name)                  -- user name: foo, x
  Op(&str)                    -- operator: +, ==, . (rendered as ·op_X)

Param                         -- function parameter
  Name(BindName)              -- plain param
  Spread(BindName)            -- varargs: ..rest (only one, trailing position)

Arg                           -- call-site argument
  Val(Val)                    -- plain argument
  Spread(Val)                 -- spread: ..items
```

## Key — scope lookup

```
Key { kind: KeyKind, resolution: Option<Resolution>, meta: Meta }

KeyKind
  Name(Name)                  -- user name: foo, add
  Bind(BindName)              -- typed scope ref (avoids string materialisation for Gen temps)
  Prim(Prim)                  -- runtime builtin (no scope resolution needed)
  Op(&str)                    -- operator symbol: +, ==, .

Prim                          -- runtime builtins (reference only, never binding sites)
  SeqAppend                   -- [a, b, c] element construction
  SeqConcat                   -- [..xs, ..ys] spread merge
  RecPut                      -- {key: val} field construction
  RecMerge                    -- {..rec} spread merge
  StrFmt                      -- 'hello ${name}' interpolation
  StrRaw                      -- fmt'...' tagged template
```

## Resolution — populated by semantic/SCC pass

```
Resolution
  Local                       -- bound in current scope, already initialized
  Captured                    -- free variable from outer scope
  Recursive                   -- same LetRec group, behind fn boundary (valid)
  ForwardRef                  -- same LetRec group, not behind fn boundary (compile error)
  Global                      -- module-level binding
```

`Recursive` and `Global` refs need store/load in output; `Local` and `Captured`
are structurally resolved.

---

## Values — already-computed things

```
Node<K> { id: CpsId, kind: K, meta: Meta }  -- generic shell
Val = Node<ValKind>                          -- trivial value (has CpsId)

ValKind
  Ident(BindName)             -- locally bound name (param or let-binding)
  Key(Key)                    -- scope lookup (user name, operator, or builtin)
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

LetFn { name, params, free_vars, fn_body, body }
  -- bind a function; name NOT visible in fn_body (non-recursive)
  -- free_vars populated by free-var pass; empty until then

LetRec { bindings: Vec<Binding>, body }
  -- mutually recursive group; all names visible in all fn_bodies
  -- Binding = { name, params, fn_body, meta }

App { func, args, result, body }
  -- call func with args; bind result; visible in body

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

MatchApp { func, args, fail, result, body }
  -- apply func to args; fail if tag is wrong
  -- constructor/extractor patterns: Ok b, Some x

MatchIf { func, args, fail, body }
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

### Store/Load
Only needed for mutual recursion (`LetRec`) and protocols (dispatch collapse).
Everything else is structural scope.

### Closure vs LetFn
All fns are potentially closures; captures inferred by free-var pass.
Analysis IR uses `LetFn` with implicit captures from nesting.
Codegen IR adds explicit captures after free-var analysis.

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
- Compiler temps — `·v_N`, `·fn_N`
- Match cursors — `·m_N`
- Continuations — `·ƒ_cont`, `·ƒ_err`, `·ƒ_ok`, `·ƒ_fail`
- Runtime/injected — `·scope`, `·state`, `·chld_scope`, `·op_plus`

**Core primitives rendered:**
`·store`, `·load`, `·apply`, `·closure`, `·scope`, `·seq_append`, `·seq_concat`,
`·rec_put`, `·rec_merge`, `·id`, `·op`, `·str_fmt`, `·str_raw`, `·yield`,
`·match_block`, `·match_branch`, `·match_store`, `·match_seq`, `·match_next`,
`·match_done`, `·match_not_done`, `·match_rest`, `·match_rec`, `·match_field`,
`·match_value`, `·match_if`, `·match_apply`, `·panic`, `·if`
