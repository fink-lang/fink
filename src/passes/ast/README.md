# `src/passes/ast` — Lexer, Parser, AST

Source-of-truth for Fink's surface syntax. Tokenises and parses `.fnk`
source into a flat-arena AST that downstream passes consume.

## Contracts

- [arena-contract.md](arena-contract.md) — append-only flat arena, two-handle
  rule (read snapshot + write builder), side-table id stability. **Every pass
  that touches the AST must honour this.**
