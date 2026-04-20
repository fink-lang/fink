# Documentation Conventions

The rules for writing docs in the ƒink compiler repo. Scope: the `fink` repo for now; the same conventions are expected to apply to sister repos (`playground`, `vscode-extension`) as they adopt them.

Docs serve three audiences: language users, compiler integrators (embedders), and compiler contributors. The conventions below are the agreements that keep docs readable for all three across two rendering surfaces: GitHub (raw Markdown) and fink-lang.org (compiled).

---

## 1. Where docs live

**Rule:** a doc file lives next to the thing it describes.

| Describes | Lives in |
|---|---|
| The ƒink language — spec, semantics, examples | `docs/` |
| Compiler architecture — pipeline, passes, IR shape, invariants, design contracts | `src/**/README.md` and sibling `*.md` files |
| Rust-specific implementation details — module structure, function contracts, types, edge cases | Rustdoc `///` / `//!` inside the `.rs` files themselves (see Section 2 for the public-API / internal split) |
| Contribution process, dev setup, release flow | `CONTRIBUTING.md` at repo root |
| Repo overview, install, onward links | `README.md` at repo root |

### Two layers in code

Implementation docs come in **two layers**, and the layer determines the format:

- **Architecture-level design** — the *shape* of the compiler: pipeline stages, IR structure, pass contracts, invariants, rationale for architectural choices. Lives as Markdown files under `src/**`. Type names and signatures are fine where they carry architectural meaning; the contract itself would survive a rewrite in another language.
- **Implementation-level detail** — how a particular Rust module works: lifetime juggling, trait bounds, `Box` vs `Arc`, edge cases in a single function. Lives in Rustdoc comments (`///`, `//!`) inside the `.rs` file. Disappears when the code moves.

Rule: the architecture doc is always more abstract than the code it describes. If a Markdown file starts explaining Rust-specific mechanics, that content belongs in Rustdoc. If a Rustdoc comment starts explaining cross-module architecture, that content belongs in the nearest `README.md` or sibling design doc.

### Summary

Rule of thumb: if the file describes *what ƒink is*, it's in `docs/`. If it describes *how the compiler is built* at the architecture level, it's a Markdown file under `src/**`. If it describes *how a specific Rust module works*, it's Rustdoc in the `.rs` file.

`docs/` starts flat. Add subdirectories only when flat becomes unwieldy.

### Design notes

A **design note** is a standalone Markdown file that captures rationale, historical context, or exploratory thinking that doesn't fit inside a reference doc. Design notes live:

- Next to the code they explain, as a sibling `*.md` under `src/**/` (for compiler-architecture rationale).
- Under `docs/` (for language-level design).

Filenames carry a purpose suffix so their role is obvious at a glance: `-contract.md` (invariants and pass contracts), `-design.md` (architectural design), `-rationale.md` (why a decision was made), `-plan.md` (work-in-progress implementation plan). The list is not exhaustive — the top heading of the file is the authoritative marker of what it is.

---

## 2. Audience

Every doc has a primary audience, picked from:

- **user** — a developer writing `.fnk` code. Wants syntax, semantics, examples, how to install, how to run.
- **embedder** — a developer building on top of the compiler (language server, playground host, editor tooling, alternative runtime). Needs the language docs *and* the compiler's public API, but not its internals.
- **contributor** — a developer (or agent) working on the compiler itself. Reads everything above, plus internals.

Each audience is a superset of the one before it. A contributor is also an embedder and a user; an embedder is also a user.

**The audience is implicit from location:**

- `docs/` — user-facing (language spec, examples, quickstart).
- `src/**/*.md` — contributor-facing (architecture, pass contracts, IR design).
- Rustdoc on **public items** (anything that appears in `cargo doc` — items with `pub` visibility reachable from the crate root without crossing a non-`pub` boundary) — embedder-facing. Documents the consumer-visible contract: what the item does, invariants callers must uphold, error modes.
- Rustdoc on **non-public items** — contributor-facing. Documents implementation detail for other compiler contributors: shape, edge cases, why the code is the way it is.
- `CONTRIBUTING.md` — contributor-facing (process, setup, release).

A file has a primary audience. Content that genuinely serves more than one audience is fine — don't split artificially. But if a user doc is growing internals, that's a signal the internals want to move to a contributor doc.

Contributor docs may assume the reader has read the user docs. User docs must never require internal knowledge. Embedder docs sit on the public API surface and link *into* the language docs, not the internals.

---

## 3. Links

Every doc page must render correctly in two places: on GitHub (where contributors browse the raw source) and on fink-lang.org (where the website renders the same Markdown). The website is a rendering of the repo, not a separate source — docs never link *to* the website from inside a repo, because the reader is already looking at the content the website would render.

**Within a repo:** relative paths, resolved from the source file's directory (the Markdown standard — how GitHub and the website both render). From the repo root, `[CPS README](src/passes/cps/README.md)`. From `src/passes/ast/README.md`, `[CPS README](../cps/README.md)`. No absolute URLs for in-repo links — not even to the fink-lang.org version of the same page.

**Across repos:** only from a repo's root `README.md`. Deep docs never cross repo boundaries. The root README may link to:

- `https://fink-lang.org/...` — for onward reading by users (the canonical destination for a user who wants docs).
- `https://github.com/fink-lang/<repo>/...` — for sister-repo READMEs (playground, vscode-extension, etc.) when the target has no fink-lang.org equivalent.

If two repos end up needing the same reference content, extract it into one canonical home (usually `fink`'s `docs/`) and have the other repos' root READMEs link to the fink-lang.org rendering.

Never link to a branch or a commit. `main` is production. Exception: when deliberately referencing historical code (e.g. "the pre-refactor layout" in a design note), pin the exact commit and say so in the link text.

**Code references in prose:** always a Markdown link of the form `[filename.ext:NN](path/to/filename.ext#LNN)`. The `filename.ext:NN` display form is what IDEs recognise; the `#LNN` anchor is how GitHub and the website jump to the line. Example in prose: the CPS transform starts at [src/passes/cps/transform.rs:42](src/passes/cps/transform.rs#L42). Inside a code block, bare `src/passes/cps/transform.rs:42` is fine.

---

## 4. Voice and length

**Present tense. Describe what is.** No "we will…", no "previously…". If behaviour has changed, the doc reflects the current state; historical notes go in git history or in a clearly labelled design note.

**Short.** Rough targets, not hard limits — pragmatic beats dogmatic:

- README.md (repo root or subsystem): aim for one screen. Split into sibling files if it grows past that.
- Contract / invariant docs: as short as they can be while still complete.
- Reference docs (e.g. `docs/language.md`): every section earns its space.

If a doc grows, ask whether it splits cleanly. Long docs don't get read.

**No roadmap inside feature docs.** Roadmap lives in `docs/roadmap.md`. Design rationale — *why we built it this way* — is allowed inside a feature doc only when knowing it changes how a reader uses or extends the feature. Otherwise it goes in a design note.

**No meta-commentary.** Don't write about the doc itself ("this document explains…"). Just explain.

---

## 5. File naming and structure

- Markdown only. `.md` extension.
- `kebab-case.md` for filenames. `README.md` is the fixed exception.
- Every directory that contains docs has a `README.md` as its index. No orphan files.
- Headings use ATX (`#`, `##`, …). Top heading is the doc title. One `#` per file.
- Code blocks are always fenced with a language tag. Canonical tags: `fink` (ƒink), `rust`, `wat` (WebAssembly text), `bash` (shell commands), `text` (plain output, logs, file listings). Use these — not variants like `fnk`, `sh`, `shell`, or `console`. An un-tagged fence is a bug.

---

## 6. Implemented / designed / open

The ƒink language spec (`docs/language.md`) documents **what is implemented** only. Anything designed-but-not-implemented lives in `docs/roadmap.md`. Features move from roadmap into the spec when they ship.

No inline `designed` or `open` tags in the spec. A feature is either in `docs/language.md` (implemented) or in `docs/roadmap.md` (not).

---

## 7. Writing for the first-time user

The top user audience is a developer who just discovered ƒink and is deciding whether to spend time on it. Every user-facing doc is written to **not waste their time**. Concrete rules:

- The repo `README.md` must answer three things within one screen: what ƒink is, how to install it, where to go next.
- `docs/language.md` must let a reader write a working function within 5 minutes. Quickstart first, reference second.
- Code examples must be valid, complete ƒink that parses and runs. The website renders every `fink`-tagged code block with an *Open in Playground* button — the example is the input. "Complete" means the top level parses and the playground runs it. Keep the example minimal: no setup code beyond what the snippet actually demonstrates. A single top-level expression, a function definition, or a short module are all fair game. If an example needs substantial scaffolding to run, the example is teaching the wrong thing — rework it.
- No prerequisites before the first example. No "first, understand CPS" anywhere in user-facing docs.

---

## 8. Writing for coding agents

Agents read the same docs as humans, plus `CLAUDE.md`. The convention for agents:

- `CLAUDE.md` contains agent-specific rules that don't belong in user or contributor docs (e.g. "never run X", workflow conventions).
- Anything else an agent needs to know goes in the normal docs. Don't duplicate.
- Write docs that are skimmable: headings are load-bearing, lists beat paragraphs, examples beat prose.

---

## 9. Review personas

Doc batches are reviewed under these six personas. A **batch** is any PR that introduces a new doc, materially rewrites an existing doc, or touches more than a handful of files at once. Small edits (typo fixes, link updates, a paragraph tweak) don't need a persona run.

Each review is a separate pass; findings are triaged into Must / Should / Nice before moving on.

1. **First-time user (hardest, first)** — a developer who just discovered ƒink and wants to build something in it. Question: "does this make me want to stay, or do I bounce?"
2. **First-time contributor** — a developer who wants to help. Question: "can I find the part I want to work on and make a change without getting lost?"
3. **Compiler integrator** — a developer building on top of ƒink: a language server, an editor extension, an alternative playground, a tool that embeds the compiler. Reads language docs *and* the compiler's public API, never internals. Question: "can I build on ƒink without reverse-engineering it?"
4. **Coding agent writing ƒink** — an agent given a brief and asked to produce `.fnk` code. Question: "do the docs give me enough to write correct code without guessing?"
5. **Coding agent contributing to the compiler** — an agent given a brief and asked to make a compiler change. Question: "can I locate the relevant subsystem, understand its contract, and make a change that doesn't violate invariants?"
6. **Senior engineer on ƒink** — the author or an equivalent. Question: "is this accurate, consistent, free of drift, and maintainable?"

Each persona is run as a sub-agent with a brief prompt that sets the persona and asks for a review. The reviewer returns Must / Should / Nice findings.

**Sign-off rule:** the convention itself is ratified when personas 1, 3, and 4 give a clean pass. Personas 2, 5, and 6 are then run per-batch as docs are written or revised under the convention.

---

## 10. What this convention is not

- Not a style guide for code comments (those live under "Code Style" in `CLAUDE.md`).
- Not a grammar or prose guide — write clearly; common sense applies.
- Not a set of things to retrofit into every existing doc overnight. It is applied as docs are touched, and in planned bulk-retrofit passes over the existing code and language docs.
