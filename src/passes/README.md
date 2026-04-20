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

The stage chain lives in [mod.rs](mod.rs). Most callers enter via the `to_ast` / `to_cps` / `to_wasm` / `run` helpers in [../lib.rs](../lib.rs) — they route through this chain. `compile_package` itself lives in [wasm-link/mod.rs](wasm-link/mod.rs), re-exported from the crate root.

## Stages

- [ast/](ast/) — lexer, parser, AST arena, formatter, `Transform` trait.
- [partial/](partial/) — partial-application desugaring (`a | add ?`).
- [scopes/](scopes/) — name resolution, scope graph, capture/recursion classification.
- [cps/](cps/) — AST → CPS lowering and the CPS IR.
- [lifting/](lifting/) — unified closure + continuation lifting.
- [wasm/](wasm/) — per-module WASM codegen: collect, emit, DWARF, WAT formatter, sourcemap.
- [wasm-link/](wasm-link/) — package-level orchestration: walks the import graph, invokes `wasm/` per module, links the resulting binaries into a single WASM with the runtime.
- [modules/](modules/) — host-neutral `SourceLoader` trait used by `wasm-link` to read module sources (file, in-memory, future browser host).

## Pass contract

Passes that take and produce `CpsResult` uphold the rules in [cps/transform-contract.md](cps/transform-contract.md). Passes that take and produce `Ast` uphold [ast/arena-contract.md](ast/arena-contract.md).

## Entry point

Read [mod.rs](mod.rs) for the stage chain and the result types. Then drop into whichever stage you're working on.
