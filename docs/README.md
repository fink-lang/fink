# `docs/` — language documentation

Docs for users of the Fink language (not for compiler contributors —
those live next to the code under [`src/`](../src/), see
[`src/README.md`](../src/README.md)).

## What's here

- [**language.md**](language.md) — by-example syntax reference. Every
  construct with a runnable snippet, marked `implemented` / `designed`
  / `open` so a reader knows what's solid vs what's still landing.
  This is the primary user-facing surface and the source of truth for
  the language. Eventually consumed by [fink-lang.org](https://fink-lang.org)
  in place of the hand-authored language reference there.
- [**grammar.md**](grammar.md) — formal-ish grammar of the surface
  syntax (lexical structure, operator precedence, EBNF-flavoured rules
  for expressions, patterns, modules). Companion to `language.md` for
  readers who want the structural rules, not the by-example tour.
- [**semantics.md**](semantics.md) — evaluation model: values,
  immutability, scope, control flow, modules, error propagation,
  cooperative concurrency. Cross-language analogies (≈ OCaml, ≈ Rust,
  ≈ Haskell, ≈ JS) are inlined where Fink uses a familiar word with
  a non-obvious meaning.
- [**terminology.md**](terminology.md) — alphabetical glossary of
  Fink terms. Lookup index, complementary to `language.md` /
  `semantics.md` (which introduce terms in concept order).
- [**roadmap.md**](roadmap.md) — features that are designed but not
  yet implemented, plus active design questions. Each section anchors
  to a stable kebab-case slug (`#types`, `#protocols`, …) so
  cross-links stay stable as the file grows.
- [**examples/**](examples/) — `.fnk` files. `lang-features.fnk` is
  the syntax tour `language.md` is converted from (kept until the
  conversion is fully cross-referenced). The four WIP files
  (`type-system.fnk`, `protocols.fnk`, `macros.fnk`, `unresolved.fnk`)
  hold the original syntax sketches; their settled content has been
  folded into `language.md` / `roadmap.md`.

## Audience

Everything here describes **the language**, not the current Rust
implementation. The compiler will eventually self-host (Fink-on-Fink),
so anything in this directory should survive an implementation rewrite.
Implementation-specific documentation (calling convention, IR design,
arena contract, etc.) lives next to the code it describes — see
[`src/passes/`](../src/passes/) and [`src/README.md`](../src/README.md).

## See also

- [`/README.md`](../README.md) — repo entry point. Install + Quickstart.
- [fink-lang.org](https://fink-lang.org) — published documentation, in-browser playground.
- [`/CONTRIBUTING.md`](../CONTRIBUTING.md) — contributor entry point.
