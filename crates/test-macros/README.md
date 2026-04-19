# `crates/test-macros` — `include_fink_tests!` proc macro

A small function-like proc macro that turns a `.fnk` file full of test
cases into Rust `#[test]` functions at compile time. Lets us write
compiler tests in Fink (the language being implemented) instead of in
Rust string literals.

Used widely under `src/passes/*` and `src/runner/` — see existing call
sites for examples.

## Usage

```rust
test_macros::include_fink_tests!("src/passes/cps/test_literals.fnk");
```

Path is relative to the call site's `CARGO_MANIFEST_DIR` (i.e. the
crate root the macro is invoked in, **not** the test-macros crate
itself). The macro reads the file at compile time and emits one
`#[test]` per `test '...', fn:` block found.

## Test-file shape

Each test in the `.fnk` file has the form:

```fink
test 'descriptive name', fn:
  expect <test-helper-fn> ƒink:
    <input fink source>

  | equals ƒink:
    <expected output>
```

- The `<test-helper-fn>` is a Rust function in the calling crate's
  `tests` module that takes `&str` and returns `String` (e.g. `cps`,
  `gen_wat`, `fmt`). The macro generates `let actual = <fn>(<src>);`.
- Both `ƒink: ...` blocks are extracted as raw indented text and stripped.
- The generated `#[test]` compares `actual` against the expected text;
  on mismatch it panics with a diff (or, under `BLESS=1`, rewrites the
  expected block in place — see below).

The macro also emits a hidden `const _: &str = include_str!(<file>);`
so cargo's incremental rebuild picks up `.fnk` changes.

## `BLESS=1` — accept current output as the new expected

When a test fails, run with `BLESS=1` to overwrite the `| equals
ƒink: ...` block with whatever the helper actually produced. The
project's `make bless` target wraps this:

```sh
make bless          # touches lib.rs, runs `BLESS=1 cargo test -j1`
```

**Two non-obvious gotchas the Makefile target handles for you:**

1. **`-j1` is required.** The macro rewrites the `.fnk` file in place
   from inside each generated `#[test]`. Concurrent test threads racing
   on the same file produce corrupt output (interleaved partial writes).
2. **Touching `crates/test-macros/src/lib.rs` is required** before
   re-running. Proc macros don't declare the `.fnk` files they read as
   build inputs, so cargo won't re-expand the macro just because the
   `.fnk` file changed. Touching the proc-macro crate forces re-expansion.

If you BLESS without going through `make bless`, do both manually:

```sh
touch crates/test-macros/src/lib.rs && BLESS=1 cargo test -j1
```

## Diagnostics

Parse errors in the `.fnk` file are formatted via
`fink::errors::format_diagnostic` and surfaced as a compile-time panic
with source context — the failing line and a hint, just like a normal
compiler diagnostic. So a malformed test file fails the build with a
useful message rather than an opaque proc-macro error.

## Where it sits in the codebase

This crate is a workspace member, not a sub-module of the main `fink`
crate. It's a `proc-macro = true` crate, so it has its own dependency
on `syn` / `quote` / `proc_macro2` and on `fink` itself (which it uses
to parse `.fnk` source at compile time). That gives us a single source
of truth for the `.fnk` parser — the same parser that compiles user
code parses test fixtures.
