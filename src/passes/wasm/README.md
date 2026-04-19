# `src/passes/wasm` — WASM codegen

Lowers lifted CPS IR into a self-contained, debuggable WASM binary.
WAT text is a derived view — the binary is canonical, the formatter
reads it back to produce human-readable output.

## Pipeline

```
Lifted CPS IR
    ↓
collect.rs  → Module / CollectedFn
    ↓
emit.rs     → WASM binary (wasm-encoder) + byte offset mappings
    ↓
dwarf.rs    → DWARF .debug_* sections appended to binary
    ↓
fmt.rs      → WAT text + native source map
```

## Contracts and design

- [calling-convention.md](calling-convention.md) — function ABI: `$Fn2` /
  `$Fn3`, `$Closure`, `_apply` / `_apply_cont` dispatch, capture struct
  layout. **The authoritative description of how a Fink call lowers to WASM.**

The long architecture summary that previously lived in the `mod.rs` `//!`
block will move here in Phase 1b.

## Notes

- `varargs-calling-convention.md` is a sibling holding the rejected
  unified-array design plus still-load-bearing spread / `$SpreadArgs`
  content. Phase 1c folds that content into `calling-convention.md` and
  deletes the standalone file.
