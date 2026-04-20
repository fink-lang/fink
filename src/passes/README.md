# Passes

The compiler pipeline. Each subdirectory is one stage. Stages are chained by typed result structs — skipping or misordering a stage is a type error.

## Pipeline

```text
parse(src, url)            → Ast
desugar(Ast)               → DesugaredAst   (partial application + scopes)
lower(DesugaredAst)        → Cps
lift(Cps, DesugaredAst)    → LiftedCps      (closure + cont lifting)
compile_package(entry, …)  → Wasm           (collect → emit → DWARF → link)
```

The stage chain lives in [mod.rs](mod.rs); the public entry points there are what callers use.

## Stages

- [ast/](ast/) — lexer, parser, AST arena, formatter, `Transform` trait.
- [partial/](partial/) — partial-application desugaring (`a | add ?`).
- [scopes/](scopes/) — name resolution, scope graph, capture/recursion classification.
- [cps/](cps/) — AST → CPS lowering and the CPS IR.
- [lifting/](lifting/) — unified closure + continuation lifting.
- [modules/](modules/) — module-level concerns shared across stages.
- [wasm/](wasm/) — WASM codegen (collect, emit, DWARF, WAT formatter, linker).
- [wasm-link/](wasm-link/) — package-level compile orchestration (entry + deps → linked binary).

## Pass contract

Passes that take and produce `CpsResult` uphold the rules in [cps/transform-contract.md](cps/transform-contract.md). Passes that take and produce `Ast` uphold [ast/arena-contract.md](ast/arena-contract.md).

## Entry point

Read [mod.rs](mod.rs) for the stage chain and the result types. Then drop into whichever stage you're working on.
