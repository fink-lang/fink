# `src/` — Source map

The Fink compiler is laid out by responsibility. This page is the entry
point for a contributor — it tells you what each subsystem does and where
to find the design docs (when they exist).

## Pipeline order

```
tokenize → parse → desugar (partial + scopes) → lower (CPS)
                                              ↓
                                            lift
                                              ↓
                                       compile_package
                                       (collect → emit → DWARF → link)
```

Each stage produces a typed result that gates the next — you cannot skip
or misorder passes. See [passes/mod.rs](passes/mod.rs) for the typed stage
chain.

## Subsystems

| Path | Purpose | README? |
|---|---|---|
| [bin/](bin/) | CLI entry points (`fink`, `finkrt`) | — |
| [compile/](compile/) | High-level compile entry points (single-module + package) | — |
| [dap/](dap/) | Debug Adapter Protocol server (`fink dap`) | — |
| [errors/](errors/) | Diagnostic formatter | — |
| [fmt/](fmt/) | Canonical Fink-source pretty-printer (Stage-2 layout + print) | — |
| [passes/](passes/) | Compiler passes — see below | — |
| [runner/](runner/) | wasmtime-based runner for `fink run` | — |
| [runtime/](runtime/) | WAT runtime sources, merged at build time | — |
| [sourcemap/](sourcemap/) | Native byte-offset source-map format | — |
| [strings/](strings/) | String rendering and escape handling | — |

## Passes

| Path | Purpose | README? |
|---|---|---|
| [passes/ast/](passes/ast/) | Lexer, parser, AST types, formatter, transform | [README](passes/ast/README.md) + [arena-contract](passes/ast/arena-contract.md) |
| [passes/partial/](passes/partial/) | Partial-application desugaring (`?`) | — |
| [passes/scopes/](passes/scopes/) | AST-level scope analysis (`BindOrigin`/`ScopeId`/`BindId`) | [README](passes/scopes/README.md) + [name-resolution](passes/scopes/name-resolution.md) |
| [passes/cps/](passes/cps/) | CPS IR + transform | [README](passes/cps/README.md) + [transform-contract](passes/cps/transform-contract.md), [ir-design](passes/cps/ir-design.md), [node-unification](passes/cps/node-unification.md) |
| [passes/lifting/](passes/lifting/) | Unified closure + cont lifting (iterative until convergence) | — |
| [passes/modules/](passes/modules/) | Module-level helpers shared by lower & wasm-link | — |
| [passes/wasm/](passes/wasm/) | Codegen: collect, emit, DWARF, fmt, link | [README](passes/wasm/README.md) + [calling-convention](passes/wasm/calling-convention.md) |
| [passes/wasm-link/](passes/wasm-link/) | Multi-module package compiler + linker | — |

## Conventions

- **Doc comments:** `//!` for module overview (≤40 lines or one paragraph
  + one short list), `///` for items. Anything longer moves to a sibling
  `README.md`.
- **Sibling READMEs:** created only when there's real prose to write. A
  missing README is a *visible* signal of an undescribed gap — see the
  "—" entries above. If you understand a subsystem well enough to write
  half a page about it, open a PR adding its README and update this table.
- **Tests:** live in the file that implements the feature
  (`#[cfg(test)] mod tests` at the bottom), or in a sibling `.fnk` file
  loaded via `test_macros::include_fink_tests!`.

## See also

- [/CLAUDE.md](../CLAUDE.md) — project conventions and rules
- [/docs/](../docs/) — language-level docs (spec, semantics, terminology)
  that describe Fink rather than the implementation
