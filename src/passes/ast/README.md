# AST

Lexer, parser, AST types, formatter, and the `Transform` trait for rewriting. The AST is a flat append-only arena.

## Key files

- [lexer.rs](lexer.rs) — tokens, positions, source locations.
- [parser.rs](parser.rs) — Pratt-style recursive descent; builds the arena via `AstBuilder`.
- [mod.rs](mod.rs) — `Ast`, `AstId`, `AstBuilder`, `NodeKind`, `Exprs`, plus the `appended_only` invariant check.
- [fmt.rs](fmt.rs) — pretty-print AST back to source-like text.
- [transform.rs](transform.rs) — the `Transform` trait every rewrite pass implements.

## Contracts

- [arena-contract.md](arena-contract.md) — append-only invariant, the two-handle rule, why side-tables composed over `AstId` survive every pass.

## Entry point

Read [mod.rs](mod.rs) first for the `Ast` / `AstBuilder` shape, then [parser.rs](parser.rs) to see how parsing populates the arena. Pass authors start from [transform.rs](transform.rs) and [arena-contract.md](arena-contract.md).
