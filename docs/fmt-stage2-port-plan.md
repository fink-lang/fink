# Stage-2 Formatter Port Plan

**Status:** design note, no code. Written 2026-04-15.

The Stage-2 source-code formatter at [src/fmt/layout.rs](../src/fmt/layout.rs)
and [src/fmt/print.rs](../src/fmt/print.rs) was stubbed in commit `9bfbcc5`
during the flat-AST refactor. `fmt2` CLI still works but delegates to
`ast::fmt`, producing s-expression-style output instead of canonical
Fink source layout. This document plans the real port.

Legacy source lives at `git show 9bfbcc5^:src/fmt/layout.rs` (1476 lines),
`git show 9bfbcc5^:src/fmt/print.rs` (459 lines), and the two fixture files
`git show 9bfbcc5^:src/fmt/test_fmt.fnk` (15 tests) / `test_print.fnk`
(47 tests).

## Shape change

Legacy operated on an owning `&Node<'src>` tree and returned owning
`Node<'src>` values built via `Node::new(kind, loc)` + `Box::new`. The
flat AST follows the two-handle rule — see
[docs/ast-arena-contract.md](ast-arena-contract.md):

- **Read:** `ast: &'src Ast<'src>` — a snapshot taken at pass entry.
- **Write:** `builder: &mut AstBuilder<'src>` — append-only, returns
  `AstId` for each appended node.

The port applies the **gen-field-injection pattern** from
[src/passes/cps/transform.rs](../src/passes/cps/transform.rs): put both
handles on `Ctx` as fields instead of threading them through every call.
Helpers take `id: AstId` and read the payload via a one-line
`self.node_of(id)`.

### Ctx

```rust
struct Ctx<'cfg, 'src> {
    cfg: &'cfg FmtConfig,
    block_col: u32,
    ast: &'src Ast<'src>,
    builder: AstBuilder<'src>,
    origin: PropGraph<FmtId, Option<AstId>>,
}
```

- `ast` — read the input tree.
- `builder` — append new nodes.
- `origin` — **new concern.** Every `builder.append` is paired with an
  `origin.push(input_id_or_none)` so `FmtResult.origin` ends up aligned
  with `FmtResult.ast.nodes` one-to-one.

### Function signatures

| Legacy | Flat |
|---|---|
| `pub fn layout(root: &Node, cfg: &FmtConfig) -> Node` | `pub fn layout(ast: &Ast, cfg: &FmtConfig) -> FmtResult` |
| `fn fix(&mut self, node: &Node) -> Node` | `fn fix(&mut self, id: AstId) -> AstId` |
| `fn fix_children(&mut self, node: &Node) -> Node` | `fn fix_children(&mut self, id: AstId) -> AstId` |
| `fn node(&mut self, node: &Node, at: Pos) -> Node` | `fn node(&mut self, id: AstId, at: Pos) -> AstId` |
| `fn fix_exprs(&mut self, exprs: &Exprs) -> Exprs` | unchanged (`Exprs.items: Box<[AstId]>`, recursion returns `Vec<AstId>.into_boxed_slice()`) |
| `fn inline_width_node(node: &Node) -> u32` | `fn inline_width_node(ast: &Ast, id: AstId) -> u32` — free function, takes `ast` explicitly (no `Ctx` available at call sites) |
| `fn place_tok(tok: &Token, at: Pos) -> Token` | unchanged — tokens are still inline in `NodeKind`, no arena |

All the `Pos`/`Loc` math (`newline_pos`, `advance_pos`, `space_after`,
`loc`) is pure functions of `Pos` and carries over unchanged.

## Origin map: new design

Legacy had no origin tracking. The `FmtResult { ast, origin:
PropGraph<FmtId, Option<AstId>> }` type was added as scaffolding during
the flat-AST refactor. The port defines the contract:

- **`FmtId` and `AstId` index the same arena.** `FmtResult.ast.nodes` is
  the builder's finished arena (extended with any new nodes the layout
  pass appended). `FmtId(n)` = the node at index `n` in that arena.
- **`origin[FmtId(n)]`** records the input `AstId` the node at `n` was
  derived from. For nodes that existed before the pass (the "unchanged
  subtree" shortcut), `origin[FmtId(n)] = Some(AstId(n))` — identity.
  For appended nodes, `origin` carries the id of the input node the
  layout decision was driven by. For purely synthesised nodes (e.g. the
  Module wrapper inserted by canonical mode if there is one), `None`.

**One-line rule:** every `self.builder.append(kind, loc)` is followed
by `self.origin.push(Some(src_id))` (or `None`). Encapsulate in
`Ctx::append(src: Option<AstId>, kind, loc) -> AstId`.

### Preserve-mode shortcut ("unchanged node keeps original locs")

Legacy `fix_children` clones the input subtree; if nothing changed the
clone has the same locs and the printer treats it as a no-op.

In the flat port: if `fix_children(id)` finds that every recursed child
returned its own input id **and** no structural rewrite was needed,
return `id` unchanged — **no append**. Because `FmtId = AstId` for the
input arena, `origin[FmtId(id)]` is automatically `Some(id)` (identity
initialisation — see below). Cheap optimisation, matches legacy
semantics.

**Arena initialisation:** at pass start, seed `origin` with identity
mappings for every existing `AstId` in the input arena:

```rust
let mut builder = AstBuilder::from_ast(ast); // starts at nodes.len()
let mut origin = PropGraph::with_size(builder.len());
for i in 0..builder.len() { origin.set(FmtId(i as u32), Some(AstId(i as u32))); }
```

Then any new append pushes one more slot to both arrays, keeping them
aligned. Unchanged subtrees return an `AstId` whose origin slot is
already the identity — no extra work.

## Preserve vs canonical mode

Entry dispatches on whether the input has real locs
(`root.loc.start.idx == 0 && root.loc.start.line <= 1` → canonical,
otherwise preserve). This logic is pure loc inspection, no storage
dependency, carries over verbatim.

**Hard rules** that trigger rewrite even in preserve mode — enforcement
sites stay the same, just read children via `self.ast.nodes.get(child_id)`:

1. Wrong indent depth — `fix_fn` / `fix_match` / `fix_arm` compare
   `body_item_loc.start.col` to `block_col + indent_width`.
2. Ambiguous `Apply` (≥2 direct ungrouped args, any itself ungrouped) —
   `should_expand_apply`.
3. Apply whose single arg is multi-arg bare Apply — `should_expand_apply`.
4. Line width > max — `inline_width_*` measurement + `at.col + w >
   max_width` checks.
5. `LitRec` field value contains `Fn` with block body —
   `rec_item_needs_expand`.

## print.rs rewrite strategy

Print is a **read-only walker** over the flat AST — no builder. Shape
matches [src/passes/ast/fmt.rs](../src/passes/ast/fmt.rs): thread
`ast: &Ast` as a parameter, no Gen struct needed.

### Switch `Writer` → `MappedWriter`

Legacy `Writer` hand-rolls its source-map accumulator with
`Vec<(u32,u32,u32,u32)>`, `out_line`, `out_col`, and a `track_output`
helper that walks char by char. This is ~50 lines of position arithmetic
that `src/sourcemap.rs::MappedWriter` already provides — it's what
`ast::fmt` uses.

**Port decision: switch to `MappedWriter`.** Simplifies:
- `track_output` disappears (`MappedWriter::push_str` advances the
  cursor automatically).
- `mark` becomes `writer.mark(src_line, src_col)` before each token.
- Block-string multi-line writes get newline tracking for free.

Print-only entry points stay the same public surface:
```rust
pub fn print(ast: &Ast) -> String;
pub fn print_mapped(ast: &Ast, source_name: &str) -> (String, SourceMap);
pub fn print_mapped_with_content(ast: &Ast, source_name: &str, content: &str) -> (String, SourceMap);
```

### Gap-filling semantics (unchanged strategy)

Print walks the tree in document order and, for each leaf token, emits
whitespace from the current cursor up to the token's `loc.start`. The
layout pass is responsible for producing locs that are monotonically
non-decreasing in document order and self-consistent; print is the
identity observer.

`Writer::gap(target)`: if `target.line > self.line`, emit
`(target.line - self.line)` newlines then `target.col` spaces;
otherwise emit `(target.idx - self.idx)` spaces. `MappedWriter` handles
the newline/column accounting so the `gap` logic shrinks to the emit
decisions.

### Edge cases preserved from legacy

- **Keywords (`fn`, `match`, `try`) are never stored as tokens.**
  `Writer::keyword(target, "fn")` writes the keyword at `node.loc.start`
  directly.
- **Interpolation close `}` delimiters** in string templates are
  sometimes reconstructed from `close.loc.start.idx - 1` in
  `templ_child`. Same logic carries over.
- **Block string content** (`write_block_str_content`) splits by `\n`,
  skips leading/trailing empty parts, re-emits each line with the
  correct indent. Carries over unchanged — `MappedWriter` handles the
  newline tracking that `Writer` had to do manually.

## Test fixtures

Restore both fixture files from `9bfbcc5^`:

```bash
git show 9bfbcc5^:src/fmt/test_fmt.fnk > src/fmt/test_fmt.fnk
git show 9bfbcc5^:src/fmt/test_print.fnk > src/fmt/test_print.fnk
```

62 test cases total (15 layout + 47 print). They use Fink block-string
templates with `expect fmt ƒink: … equals ƒink: …` or `equals 'NO-DIFF'`
when the input is already canonical.

Loaded via `test_macros::include_fink_tests!("src/fmt/test_fmt.fnk")`
at the bottom of each Rust source file. The helper `fn fmt(src: &str)
-> String` inside the `tests` module:

```rust
fn fmt(src: &str) -> String {
    let ast = parser::parse(src, "test")
        .unwrap_or_else(|e| panic!("parse error: {}", e.message));
    let cfg = FmtConfig::default();
    let laid = layout(&ast, &cfg);
    let output = print::print(&laid.ast);
    if output == src { "NO-DIFF".into() } else { output }
}
```

Same for `print.rs::tests::print` with `parser::parse_with_blocks`.

## Risk areas

### Multi-line string `content_end` math

`Ctx::lit_str` computes the end position of a block string by walking
`content.split('\n')` and accounting for the opening `":"` delimiter.
This is the fiddliest part of the legacy code and the most likely
source of off-by-one bugs during the port. Preserve the exact
arithmetic; don't "simplify" it.

### `str_templ` position relocation

Template string children (interpolations + raw segments) must stay in
their original relative positions within the template. Legacy
`Ctx::str_templ` walks children and shifts each by a fixed delta
computed from the outer placement. The same logic ports 1:1 — children
are already `Box<[AstId]>`, iteration is identical.

### Exprs construction

Legacy constructs new `Exprs { items: Vec<Node>, seps: Vec<Token> }`
with owned children. Flat `Exprs.items` is `Box<[AstId]>`, so
`fix_exprs` / collection rebuilders collect `Vec<AstId>` and call
`.into_boxed_slice()`. Pattern is used in dozens of places; make it a
helper on `Ctx` to avoid scattered `.into_boxed_slice()` calls.

## Effort breakdown

Per the pre-plan survey (see [src/passes/cps/transform.rs](../src/passes/cps/transform.rs)
and [src/passes/ast/fmt.rs](../src/passes/ast/fmt.rs) for reference
shapes):

| Phase | Hours |
|---|---|
| `Ctx` struct + origin map wiring + `Ctx::append` helper | 0.5 |
| `fix` + `fix_children` + `fix_exprs` + `fix_{fn,match,arm,apply,infix}` | 1.5 |
| `node` + `lit_str` + `collection*` (3 variants) + `str_templ` | 1.5 |
| Operators: unary/infix/chained_cmp/spread/member/group/bind/bind_right | 0.5 |
| `apply*` + `pipe` + `fn_node` + `patterns` + `match_node` + `arm` + `try_node` + `block_node` + Module | 1.5 |
| Free helpers (width, decisions, Pos math) | 0.5 |
| **layout.rs subtotal** | **6.0** |
| `print.rs` — Writer state, `MappedWriter` switch, 24-arm `node` match, `templ_child`, `write_block_str_content`, `exprs`, keyword paths | 2.0 |
| Origin map design review + doc note in `src/fmt/mod.rs` | 0.5 |
| Restore fixtures + adapt Rust test helpers to new signatures | 0.5 |
| Test blessing + debugging (expect 3–5 `Pos` off-by-ones, mainly in block-string + str-templ) | 2.0 |
| Delete stubs, remove "TODO: deferred port" comments, update MEMORY.md | 0.25 |
| **Total** | **11.25 h** |

Lower bound ~8 h if blessing goes first-try on most fixtures; upper
bound ~16 h if the `Pos` arithmetic bugs bite.

## Order of work

Suggested sequence when the port is picked up:

1. **Restore test fixtures first** (git show into src/fmt/). They serve
   as the acceptance target from day one.
2. **Port `Ctx` + origin wiring** — compile the skeleton before any
   recursion logic. Get a green `fn layout(ast) -> FmtResult {
   FmtResult { ast: ast.clone(), origin: identity } }` building.
3. **Port `fix_children` + `fix_exprs`** — the default recursion.
   Leaves and boring containers work first.
4. **Port `node` variant by variant**, smallest first (leaves), running
   `make test-full` after each batch. The 24-arm match is natural
   batching.
5. **Port the preserve-mode `fix_*` family** once `node` is done.
6. **Port `print.rs`** in one pass (smaller file, read-only, reference
   is `ast::fmt`).
7. **Bless** the test fixtures one at a time, reading each diff before
   accepting. Don't bulk-bless — Pos math bugs produce plausible-looking
   output that's wrong.
8. **Delete the stub files' deferred-port comments** and remove the
   "functional impact" note from memory.

## Non-goals

- Not adding new formatter features (width config, new wrap styles,
  etc.). Pure port.
- Not touching `ast::fmt` — it's a separate s-expression printer used
  by CLI `fmt` and debug output.
- Not changing the `fmt2` CLI surface.
- Not switching the layout pass to use the `Transform` trait. Layout's
  two-mode dispatcher doesn't fit the trait shape cleanly and the
  legacy function-based approach is clearer for this pass.
