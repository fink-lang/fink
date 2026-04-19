# `src/strings` — string rendering and escape handling

Helpers for converting raw `LitStr` source bytes into the cooked byte
sequence that user code sees. `LitStr` AST nodes hold the literal source
exactly as written (including escape sequences); this module processes
the escapes at the boundary that needs them (codegen, eval, test
infrastructure).

## Fink strings are byte sequences

Following the C / Go / Python 2 model: a Fink string literal can hold
**arbitrary bytes**, not just valid UTF-8. `'\xFF'` is a valid 1-byte
string even though `0xFF` alone is not valid UTF-8.

The cooked output is `Vec<u8>`, not `String` — `String` would force
UTF-8 validation and lose the ability to round-trip arbitrary bytes.

A future `utf8` subtype will opt into codepoint-aware semantics. Until
then, everything is bytes; sister-runtime support lives in
[`src/runtime/str.wat`](../runtime/str.wat).

## Supported escapes

| Escape | Meaning |
|---|---|
| `\n` `\r` `\t` | newline / CR / tab |
| `\v` `\b` `\f` | vertical tab / backspace / form feed (archaic — TODO: review) |
| `\\` `\'` | literal backslash / single quote |
| `\$` | literal dollar (suppresses `${…}` interpolation in source) |
| `\xNN` | raw byte (2 hex digits — may produce invalid UTF-8) |
| `\u{NNNNNN}` | Unicode codepoint (1–6 hex, `_` allowed for grouping; encoded as UTF-8 bytes) |

## See also

- [`../runtime/str.wat`](../runtime/str.wat) — runtime string support
  (storage, hashing, slicing, equality, interpolation). Same byte-array
  model.
- [`docs/examples/lang-features.fnk`](../../docs/examples/lang-features.fnk)
  for the user-facing description of string literals and templates.
