# `src/errors` — diagnostic formatter

A single `Diagnostic { message, loc, hint }` type and a
`format_diagnostic(src, diag, opts)` function that renders it as a
multi-line string with the source line, a caret span, and optional
context lines around the error.

Used everywhere a parse / scope / lowering error needs to surface to
the user: the CLI binary catches errors at the top of each subcommand
and pretty-prints them; the proc macro
([`crates/test-macros/`](../../crates/test-macros/)) calls
`format_diagnostic` to turn `.fnk` parse errors into useful
compile-time messages.

## Output shape

```
error: <message>
  --> <path>:<line>:<col>
   |
 N | <source line>
   |   ^^^^^ <hint, if any>
```

Number of context lines before/after is controlled by
`FormatOptions { lines_before, lines_after }`. The default is 1 / 0.

## Why a separate module

- All diagnostic rendering goes through one path — same shape for parse
  errors, scope errors, lowering errors, runtime trap descriptions.
- The compiler core stays render-policy-free: passes return
  `Diagnostic`s; only the binary or test-macro driver decides how to
  format them.
