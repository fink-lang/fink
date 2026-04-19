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

The WASM binary contains: WasmGC types, imported builtins, defined
functions, globals, exports, name section, and DWARF debug info. The
formatter reads it back to produce human-readable WAT with source maps
for the playground and `fink wat` CLI.

Structural source locations (func headers, params, globals, exports) are
passed alongside the binary via `StructuralLoc`, since they don't
correspond to code section byte offsets and can't be in DWARF.

## Module layout

| File | Purpose |
|---|---|
| `collect.rs` | Shared collect phase (lifted CPS → `Module`/`CollectedFn`) |
| `emit.rs` | wasm-encoder binary emitter + byte offset tracking |
| `dwarf.rs` | gimli::write DWARF line table emission |
| `fmt.rs` | Custom WASM→WAT formatter (wasmparser + gimli::read) |
| `link.rs` | Static WASM linker (merges runtime + user code) |
| `sourcemap.rs` | `WasmMapping` type (used by DAP) |
| `builtins.rs` | Rust-side builtin scaffolding (currently inert; all builtins live in WAT runtime files) |
| `compile.rs` | WAT text → WASM binary (wat crate wrapper, `run` feature only) |

## Contracts and design

- [calling-convention.md](calling-convention.md) — function ABI:
  `$Fn2` / `$Fn3`, `$Closure`, `_apply` / `_apply_cont` dispatch,
  capture struct layout. **The authoritative description of how a Fink
  call lowers to WASM.** Read this first.

## Closure representation and calling convention (legacy notes)

> **Status:** the prose below describes a superseded per-arity
> `$FnN` / `$ClosureN` model. It was relocated verbatim from `mod.rs`'s
> `//!` block in Phase 1b so the source comment could be trimmed. Phase 1c
> will rewrite this section against the current single-`$Fn2` /
> `$Closure(funcref, $Captures)` design implemented in
> [`src/runtime/types.wat`](../../runtime/types.wat) and described in
> [`calling-convention.md`](calling-convention.md). Until then, treat
> [`calling-convention.md`](calling-convention.md) as the source of
> truth and read the section below as historical context only.

After lifting, every lambda with captures becomes a top-level function
with extra leading params for the captured values, plus an `·fn_closure`
call at the original site that packages the funcref + captures into a
closure value.

### WasmGC types

Universal value type: `(ref null any)` — WASM GC built-in. All value
slots use this. Structs are implicitly subtypes of `any`.

Plain functions use `$FnN` types (one per arity):

```wat
(type $Fn2 (func (param (ref any) (ref any))))
```

Closures use `$ClosureN` struct types (one per capture count N):

```wat
(type $Closure0 (struct (field funcref)))           ;; bare funcref wrapper
(type $Closure1 (struct
  (field funcref)         ;; funcref to lifted fn (arity = call_arity + N)
  (field (ref any))       ;; capture 0
))
```

`$Closure0` replaces the old `$FuncBox` — wraps a raw funcref so it can
flow through `(ref null any)` slots (funcrefs are not subtypes of `any`
in the WASM GC spec).

Numbers are `$Num` structs (f64 field). Booleans are i31ref (0/1).

### Construction: `$_closure_N` helper

The `·fn_closure` builtin compiles to a call to `$_closure_N` (N = number
of captures + 1 for the funcref). This is an emitted helper function:

```wat
(func $_closure_2 (param funcref) (param (ref any))
  (struct.new $Closure1 (local.get 0) (local.get 1))
)
```

It takes the funcref + N captures and returns the boxed struct as
`(ref any)`.

### Dispatch: `$_apply_N` helper (call-ref-or-closure)

At every `Callable::Val` call site (indirect call through an `(ref any)`
value), we don't statically know whether the callee is a plain funcref
or a closure struct. Instead of a static type inference pass, we use
WasmGC's `br_on_cast` for runtime dispatch.

For each call-site arity N, an emitted helper `$_apply_N` tries each
`$ClosureK` type that exists in the module:

```wat
(func $_apply_2
  (param $a0 (ref any)) (param $a1 (ref any)) (param $callee (ref any))
  (block $try_clos1
    (br_on_cast_fail $try_clos1 (ref any) (ref $Closure1) (local.get $callee))
    ;; it's $Closure1 — extract funcref + 1 capture, call with arity 3
    (struct.get $Closure1 1)   ;; capture 0
    (local.get $a0)
    (local.get $a1)
    (struct.get $Closure1 0)   ;; funcref
    (return_call_ref $Fn3)
  )
  ;; fallthrough: plain $Closure0 — unbox and call directly
  (return_call_ref $Fn2 (local.get $a0) (local.get $a1)
    (ref.cast (ref $Fn2) (struct.get $Closure0 0 (local.get $callee))))
)
```

This is correct by construction — no static analysis needed. A future
type inference pass can eliminate branches where the type is known.

### Internal naming convention

All compiler-generated helper functions use the `$_` prefix to
distinguish them from user-defined functions. The formatter hides
`$_`-prefixed functions from test output.

### Arity tracking

The set of `$ClosureN` types to emit is determined by scanning for
`·fn_closure` call sites during collection. The set of `$_apply_N`
helpers is determined by `Callable::Val` call site arities (already
tracked by `scan_call_arities`).

## Notes

- `varargs-calling-convention.md` is a sibling holding the rejected
  unified-array design plus still-load-bearing spread / `$SpreadArgs`
  content. Phase 1c folds that content into `calling-convention.md` and
  deletes the standalone file.
