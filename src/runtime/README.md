# `src/runtime` — WAT runtime sources

Hand-written WebAssembly Text (WAT) modules that implement Fink's core
data structures (HAMT for records/dicts, cons-cell lists, channels,
strings, ranges) and the operators / scheduler / host-bridge that the
compiler's codegen emits calls into.

These files are **not** compiled by `rustc`. They're compiled to WASM at
build time by [`build.rs`](../../build.rs) and embedded in the compiler
binary via `include_bytes!`. End result: a single merged
`runtime.wasm` that the linker prepends to every user module.

## Why merged into one module

Inter-runtime calls (e.g. `operators.wat → str.wat::str_eq`) become
plain function calls within one WASM module — no import/export
resolution at link time. The compiler stays wasm32-safe (no runtime
dependency on the `wat` crate; that's build-time only).

## Files

| File | Lines | Purpose |
|---|---|---|
| [`types.wat`](types.wat) | 223 | **Canonical type hierarchy**: `(ref any)`, `$Num`, `$Str`, `$List`, `$Rec`, `$Dict`, `$Set`, `$Range`, `$Future`, `$Channel`, `$HostChannel`, `$VarArgs`, `$SpreadArgs`, `$Captures`, `$Closure`, `$Fn2`. Compiled standalone (separate from the merged module) and injected into every emitted user module so cross-module casts work. **Read this first** — every other runtime file builds on these types. |
| [`str.wat`](str.wat) | 2925 | String runtime: byte storage, hashing, slicing, equality, escape processing, `str_fmt` (interpolation), `_str_fmt_val` value formatter, `_str_fmt_val_repr` for container repr, `_str_wrap_bytes` for host-supplied bytes. |
| [`rec.wat`](rec.wat) | 1519 | Record + dict HAMT (Hash Array Mapped Trie). Backs both `{...}` records and `dict {...}` dicts; the wrapper types differ for future shape-optimisation. |
| [`set.wat`](set.wat) | 1366 | Immutable hash-set HAMT. Not yet wired into the merged module — only included when something starts using it. |
| [`operators.wat`](operators.wat) | 626 | CPS-shaped arithmetic / comparison / logic / bitwise / range / membership / member-access dispatchers. Polymorphic where the language is (`==`, `and`, `or`, `not` over `$Num` and i31ref). |
| [`list.wat`](list.wat) | 350 | Cons-cell `$List`: `seq_prepend`, `seq_concat`, `seq_pop`, `empty`. Used for both user `[…]` sequences and the args list passed by the [calling convention](../passes/wasm/calling-convention.md). |
| [`scheduler.wat`](scheduler.wat) | 276 | Cooperative multitasking: `$resume` (task loop), `$Future` (settled flag + waiters), `host_resume` for yielding to the host reactor. Backs `spawn` / `await` / `yield`. |
| [`interop-rust.wat`](interop-rust.wat) | 269 | Host bridge: `_run_main`, `host_resume`, `host_channel_send` (unified IO send), `host_panic` (irrefutable-pattern trap), `read` builtin. The boundary between WASM-internal scheduling and wasmtime-side IO. |
| [`channel.wat`](channel.wat) | 200 | Multi-message channels: `$Channel(messages, receivers, tag)` plus the internal `process_msg_q` task. Backs `>>` (send) and `<<` (receive). |
| [`range.wat`](range.wat) | 153 | Numeric ranges (`0..10`, `0...10`): construction, formatting, membership, match patterns. |
| [`int.wat`](int.wat) | 95 | Integer-specific helpers: `int_op_and/or/xor/not`, `int_op_div/rem/mod`, CPS-shaped `shl/shr/rotl/rotr`. |
| [`hashing.wat`](hashing.wat) | 82 | Hash primitives shared by `rec.wat` and `set.wat`. |
| [`dispatch.wat`](dispatch.wat) | 21 | `_apply` / `_apply_cont` runtime dispatch (closure unboxing). Tied to the [calling convention](../passes/wasm/calling-convention.md). |

## Build flow

`build.rs` does the merge in three steps:

1. Compile `types.wat` standalone → `$OUT_DIR/types.wasm`. The emitter
   loads this and injects the rec group into every user module so
   user-side type references point at the canonical types.
2. Extract the rec group definition from `types.wat` (it goes first in
   the merged module).
3. Concatenate every other `.wat` body, dedupe imports across modules,
   wrap in a `(module ...)`, compile to WASM → `$OUT_DIR/runtime.wasm`.
   This file is `include_bytes!`'d into the compiler and prepended to
   every user module by [`src/passes/wasm/link.rs`](../passes/wasm/link.rs).

`set.wat` is currently excluded from the merged module — the build-script
order list in `build.rs` will add it when something actually uses it.

## Tests

[`mod.rs`](mod.rs) holds a `#[cfg(test)] mod tests` block that loads the
merged `runtime.wasm` into wasmtime and exercises low-level runtime
behaviour (HAMT operations, list ops, hashing) directly — without going
through the compiler. End-to-end tests that compile Fink source and run
it live in [`src/runner/`](../runner/) instead.

## Adding a runtime function

1. Add the function to the appropriate `.wat` file (or a new file —
   register it in the `runtime_modules` list in `build.rs`).
2. If the codegen needs to emit calls to it, add the function name to
   the matching builtin enum in [`src/passes/cps/ir.rs`](../passes/cps/ir.rs)
   and wire emission in [`src/passes/wasm/emit.rs`](../passes/wasm/emit.rs).
3. Run `cargo build` — `build.rs` recompiles the merged runtime if any
   `.wat` file changed.

If you add an exported import (`(import "env" ...)`), it must be
satisfied by the runner's wasmtime host setup
([`src/runner/wasmtime_runner.rs`](../runner/wasmtime_runner.rs)) or
(for the DAP) [`src/dap/mod.rs`](../dap/mod.rs).
