# ƒink — Grammar

Concrete syntactic structure of the language. This is the formal
companion to [language.md](language.md): where the reference shows the
syntax by example, this file shows what the parser actually accepts.

The parser is hand-rolled (Pratt for infix operators); the grammar
below is described informally in EBNF-flavoured rules but the
implementation is the source of truth.

> **Status:** the grammar is settled and composable. Adding planned
> features (explicit types, protocol-impl syntax, macros) extends it
> rather than reshaping it. See [roadmap.md](roadmap.md) for what's
> queued.

---

## Lexical structure

### Significant whitespace

Indentation is significant. A multiline argument list, a block body,
or a multiline collection are all indentation-delimited:

```fink
foo
  arg1
  arg2
```

is `foo arg1, arg2`. Lines at the same indent level are sibling
statements; deeper indent means a child of the previous line.

### Tokens

| Category | Examples |
|---|---|
| Identifier | `foo`, `foo-bar`, `foo_bar`, `ni_1234`, any UTF-8 graphemes excluding registered operators / separators / terminators |
| Wildcard | `_` (non-binding placeholder; not an identifier) |
| Integer literal | `123`, `1_234_567`, `0xFF`, `0o_1234`, `0b_0101`, `+1`, `-1` |
| Float literal | `1.0`, `1.0e100_000` |
| Decimal literal | `1.0d`, `1.0d-100` |
| String literal | `'…'` (single-line, escapes + interpolation), `":` (block) |
| Tagged literal | `<value><name>` — postfix function application: `10sec` ≡ `sec 10` |
| Tagged template | `<name>'…'` or `<name>":` — `fmt'hello ${x}'`, `sql":` |
| Separator | `,` `;` `:` (`;` is a stronger `,`) |
| Bracket | `(` `)` `[` `]` `{` `}` |
| Operator | see precedence table below |

### Comments

```fink
# end-of-line

---
block comment
---
```

Block comments use a `---` line as both opener and closer.

---

## Operator precedence

From the parser's binding-power table (lower binds looser):

| BP | Operators | Associativity |
|---:|---|---|
| 20 / 21 | `or`, `xor` | left |
| 30 / 31 | `and` | left |
| 40 / 41 | `in` (and `not in`) | left |
| 50 / 51 | `..`, `...` | left |
| 60 / 61 | `==`, `!=`, `<`, `<=`, `>`, `>=`, `><` | chainable |
| 90 / 91 | `>>`, `<<`, `>>>`, `<<<` | left |
| 100 / 101 | `+`, `-` | left |
| 110 / 111 | `*`, `/`, `//`, `%`, `%%`, `/%` | left |
| 121 / 120 | `**` | **right** |
| 140 / 141 | `.` (member access) | left |

Comparison operators chain: `a < b < c` parses as `a < b and b < c`
without re-evaluating `b`.

`<<` and `>>` precedence is currently set for the bitwise-shift
interpretation (above `+`/`-`). The channel-send interpretation
wants looser precedence (below `==`); resolution is an
[open question](roadmap.md#chan-op-precedence).

---

## Expressions

### Atoms

```ebnf
atom         := literal
              | identifier
              | wildcard
              | sequence
              | record
              | dict
              | set
              | grouped
              | fn-expr
              | match-expr
              | string-template
              | tagged-template

grouped      := '(' expr ')'
sequence     := '[' (expr (',' expr)*)? ']'
              | 'seq' (expr (',' expr)*)?     # one-line
              | 'seq' indent expr+ dedent      # multiline
record       := '{' (field (',' field)*)? '}'
field        := name ':' expr
              | name                            # shorthand: name: name
              | '(' expr ')' ':' expr           # computed key — must be compile-time static
dict         := 'dict' record
set          := 'set' (expr (',' expr)*)?
              | 'ordered_set' (expr (',' expr)*)?
```

### Application

`apply` is the bare prefix-call form. Whitespace separates the function
from its first argument; `,` separates subsequent arguments.

```ebnf
apply        := callable arg (',' arg)*
              | callable indent arg+ dedent       # multiline args (one per line)
callable     := identifier | grouped | member | apply
arg          := expr | spread-arg
spread-arg   := '..' expr | '...' expr            # exclusive vs inclusive (range-shaped)
```

`;` is a stronger argument terminator than `,`:

```fink
foo a, b; bar c, d  # foo (a, b), bar (c, d)
```

### Pipe

```ebnf
pipe         := expr ('|' expr)+
```

Each `|` segment is its own scope for partial application.

### Partial application

`?` placeholders are not a node category — they're a marker that gets
desugared into an `fn` whose param is `$`. Scope boundaries for `?`:
`(...)`, each pipe segment, statement top. See
[`src/passes/partial/README.md`](../src/passes/partial/README.md) for
the full rules.

### Functions

```ebnf
fn-expr      := 'fn' params ':' body
              | 'fn' ':' body                    # zero-arg
              | 'fn' 'match' params ':' arms     # fn match sugar
params       := param (',' param)*
param        := pattern                          # any pattern, plus optional default
              | pattern '=' expr
              | '..' pattern                     # spread / variadic param
body         := expr
              | indent stmt+ dedent
```

### Bindings

```ebnf
binding      := pattern '=' expr                 # left-hand
              | expr '|=' pattern                # right-hand (direction reversed)
```

The pattern is the LHS of the binding. Any pattern that's valid in
`match` is valid as a binding LHS — see Patterns below.

### Match

```ebnf
match-expr   := 'match' expr (',' expr)* ':' arms
arms         := indent arm+ dedent
arm          := pattern ':' body
```

### Member access

```ebnf
member       := expr '.' name                    # name literal
              | expr '.' '(' expr ')'            # computed (compile-time-static or via . protocol)
```

---

## Patterns

A pattern is anything that can appear on the LHS of `=` / `|=` or in a
`match` arm.

```ebnf
pattern      := literal-pat                      # exact value match
              | identifier                       # binding (introduces a name)
              | wildcard                         # `_`
              | sequence-pat                     # `[a, b, ..rest]`
              | record-pat                       # `{x, y, ..rest}`
              | guarded-pat                      # `n > 0`, `is_even n`
              | spread-pat                       # `..pat`, `..pat |= name`
              | range-pat                        # `0..10`, `'a'...'z'`
              | string-pat                       # `'hello ${rest}'`
sequence-pat := '[' (pattern (',' pattern)*)? ']'
record-pat   := '{' (record-pat-field (',' record-pat-field)*)? '}'
record-pat-field := name                         # shorthand — bind name to field
                  | name ':' pattern             # field has its own pattern
                  | '..' identifier              # bind rest to a name
                  | '..' '{}'                    # explicit empty rest
```

Records match **partially** by default (extra fields ignored). Use
`{a, ..{}}` to require an empty rest. Sequences match **exactly** by
default; use `[a, ..]` to allow a tail.

> _Designed but not implemented:_ type-prefix patterns
> (`str s`, `u8 n`, `[..str |= ss]`) and number-mask patterns
> (`0b_xx11_xx11`, `0x_FF_xx`). Both depend on the type system landing
> — see [roadmap.md#types](roadmap.md#types).

---

## Modules

A `.fnk` file is a module. Top-level statements are mutually recursive
— forward references work without forward declarations.

```ebnf
module       := stmt*
stmt         := binding | expr
import       := '{' (name (',' name)*)? '}' '=' 'import' string-literal
```

The path in `import` is resolved relative to the importing module. See
[`src/passes/wasm-link/README.md`](../src/passes/wasm-link/README.md)
for canonical-URL semantics and dep-init ordering.

---

## Implementation pointer

The grammar above is informal — the authoritative source is the parser
([`src/passes/ast/parser.rs`](../src/passes/ast/parser.rs)). When this
file and the parser disagree, the parser wins; please file a docs
issue or open a PR with the fix.
