# CPS IR Design — Compiler Perspective

This document works through the CPS IR design from the compiler's point of view.
Output formatting (env handles, state threading, ƒ_cont naming) is intentionally
ignored here — those are rendering artifacts, not semantic content.

---

## Core principle

Every intermediate result has an explicit name. Control flow is explicit.
Scope is structural (nesting), not a runtime object.

---

## Settled decisions

### Store/Load
Only needed for two cases:
- **Mutual recursion** (`LetRec`) — names reference each other across bindings
- **Protocols** — collapse into dispatch; an operator is a dispatcher, no mutable slot needed

Everything else is structural scope — no Store/Load in the IR.

### Mutual recursion — `LetRec`
A `LetRec` block makes all its names mutually visible for name resolution purposes.
Cross-references within the group are valid **only if they appear inside a `fn` body**
(the function won't be called until after all bindings are initialized).
Bare value references to not-yet-initialized names in the same group are a compile error:

```
rec:
  x = y + 1        # error — y has no value at this point
  y = 42

rec:
  foo = fn: bar 1  # ok — bar is called later, after both are bound
  bar = fn n: foo n
```

### Env / scope
The compiler tracks scope as a chain of binding sets during the walk.
No Env node in the IR. Env handle threading is a rendering artifact for the pretty-printer.

### Closure vs LetFn
All fns are potentially closures; captures are inferred by the free-var pass.
Analysis IR uses plain `LetFn { name, params, fn_body, body }` — captures implicit from nesting.
Codegen IR adds explicit captures after the free-var pass.

Every function has a name — either user-provided or a compiler-generated synthetic.
Anonymous functions (e.g. `fn x: fn y: x + y`) get a fresh synthetic name at IR construction.
This makes debugging, tracing, error messages, and closure hoisting all easier.

---

## Scenarios to walk through

- [ ] Simple binding: `x = 42`
- [ ] Function definition and call: `add = fn a, b: a + b`
- [ ] Closure capture: `fn x: fn y: x + y`
- [ ] Pattern match
- [ ] Mutual recursion (`LetRec`)

### Visualization as correctness check

The pretty-printer derives `store`/`load` output from the clean IR annotations.
That output is valid executable Fink — so if it's correct, the IR semantics are sound.
The visualization doubles as a runtime spec. This was the original intent.

### Scope vs Env vs State

All three are output conventions only — none appear in the analysis IR:

- **Scope** — compiler concept; chain of binding sets, structural, implicit, disappears after analysis
- **Env** — runtime/codegen concept; heap-allocated record a lifted closure carries;
  synthesized from scope/closure structure during hoisting
- **State** — effect/mutation threading mechanism; synthesized by pretty-printer and codegen

### Ident annotation

After SCC analysis, each `Ident` reference is annotated with its resolution kind:
- `Local` — bound in current scope, already initialized
- `Recursive` — same `LetRec` group, behind a `fn` boundary (valid)
- `ForwardRef` — same `LetRec` group, not behind a `fn` boundary (compile error)
- `Captured` — from an outer scope (free variable)
- `Global` — module level

`Recursive` and `Global` refs need `store`/`load` in output; `Local` and `Captured` are structurally resolved.

---

## IR sketch

```
Name = &str           -- all names are strings; sigils (·, ƒ_) are output conventions only

Val:
  Ident(Name)         -- a bound name
  Lit(Lit)            -- literal value

Lit:
  Bool(bool)
  Int(i64)
  Float(f64)
  Str(&str)

Key:                  -- lookup key (for operators vs user names)
  Name(Name)          -- user-defined name: foo, add, x
  Op(&str)            -- operator symbol: +, ==, .

Expr:
  LetVal { name: Name, val: Val, body: Expr }
    -- bind `name` to `val`; visible in `body`

  LetFn { name: Name, params: Vec<Name>, fn_body: Expr, body: Expr }
    -- bind a function; NOT recursive (name not visible in fn_body)

  LetRec { bindings: Vec<(Name, Vec<Name>, Expr)>, body: Expr }
    -- mutually recursive group; all names visible in all fn_bodies
    -- each binding: (name, params, fn_body)
    -- cross-refs not behind fn boundary → compile error (ForwardRef)

  App { func: Val, args: Vec<Val>, result: Name, body: Expr }
    -- call func with args; result bound to `result`, visible in `body`

  If { cond: Val, then: Expr, else_: Expr }
    -- branch on cond

  Ret(Val)
    -- tail position: return value to current continuation
```


