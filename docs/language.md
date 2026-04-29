# ƒink Language

ƒink is a small, functional, indentation-based language. Values are immutable, types are inferred, IO goes through channels.

Features not yet reachable in the compiler live in [roadmap.md](roadmap.md). For the execution model — what effects are, how modules run, how mutual recursion and IO fit — see [execution-model.md](execution-model.md).

---

## Quickstart

Save as `hello.fnk`:

```fink
{stdin, stdout, stderr} = import 'std/io.fnk'

main = fn ..args:
  'Hello, ƒink!' >> stdout
  0
```

Run it:

```bash
fink hello.fnk
```

`fink <file>` is shorthand for `fink run <file>` — `run` is the default subcommand.

You'll see `Hello, ƒink!` on stdout. `main` returns an exit code — `0` for success.

---

## Comments

```fink
# end-of-line comment

---
block comment
---
```

---

## Literals

### Booleans

```fink
true
false
```

### Integers

Integer types are inferred from the literal's value and sign (the values below show the inferred type — type information isn't surfaced in tooling yet). Underscores separate digit groups and are ignored.

```fink
1_234_567              # u32
+1                     # i8
-1                     # i8
0xFF                   # u8
+0xFF                  # i8
0xFfFf                 # u16
0xFFFF_FFFF            # u32
0xFFFF_FFFF_FFFF_FFFF  # u64
0o_1234_5670           # octal
0b_0101_1111           # binary
```

### Floats and decimals

Floats are sized the same way (`f32` / `f64`). Decimals are a distinct type and don't mix with floats.

```fink
1.0                    # f32
1.0e100_000            # f64
1.0d                   # decimal
```

### Strings

Single-quoted. A string with `${expr}` inside is a **template string** — the expression is evaluated and interpolated. Escape sequences work in any string.

```fink
'hello world'

'result: ${1 + 2}'

'line one\nline two'
```

Multiline strings start with `'` alone on a line and indent the content:

```fink
'
  one
  two
  three
'
```

Block strings begin with `":` and end when the indent drops back; template interpolation and embedded single-quotes need no escaping:

```fink
":
  template interpolation ${name}
  and 'quotes' without escaping
```

Escape sequences:

```fink
'
  \n      - new line
  \r      - carriage return
  \v      - vertical tab
  \t      - tab
  \b      - backspace
  \f      - formfeed
  \\      - backslash
  \'      - single quote
  \$      - dollar sign
  \x0f    - hex byte (exactly 2 hex digits)
  \u{ff}  - Unicode code point between U+0000 and U+10FFFF
  \u{10_ff_ff} - underscores allowed for readability
'
```

### Sequences

Ordered, zero-indexed.

```fink
[]
[1, 2, 3]
```

Multiline:

```fink
numbers = [
  1
  2
  3
]
```

### Records

Keys are known at compile time. They can be identifiers, string literals (for keys with spaces or unusual characters), or parenthesised expressions the compiler can resolve at compile time.

```fink
{}
{foo: 1, bar: 2}
{'foo bar': 42}
{(1 + 1): 'two'}

point = {x: 1, y: 2}
```

---

## Identifiers and wildcards

Identifiers are sequences of UTF-8 graphemes. Hyphens and underscores are fine inside a name (whitespace around operators disambiguates from subtraction).

```fink
foo
foo-bar
foo_bar
ni_1234
```

`_` is the wildcard — a non-binding placeholder, not a name. Use it in patterns and parameter positions to discard.

```fink
_                        # in a pattern, discard
fn _, b: b               # ignore the first argument
[_, x] = [1, 2]          # discard first element
```

---

## Operators

### Arithmetic

```fink
-a                       # unary minus
a + b
a - b
a * b
a / b
a // b                   # integer divide
a ** b                   # power
a % b                    # remainder (sign follows dividend)
a %% b                   # true modulus (sign follows divisor)
a /% b                   # divmod — returns [quotient, remainder]
```

### Comparison

Comparison operators produce a `bool` and chain naturally:

```fink
a > b
a >= b
a < b
a <= b
a == b
a != b
a >< b         # disjoint — a and b have no element in common

1 < x < 10     # chained
```

### Ordering

Shorthand for comparsion.

```fink
a <=> b  # returns LT, EQ, GT
```


### Logical

Operate on booleans and return a boolean.

```fink
not a
a and b
a or b
a xor b
```

### Bitwise

Shared symbols with logical; dispatch is by value type.

```fink
not 0b0101_0101          # 0b1010_1010
0b1100 and 0b1010        # 0b0000_1000
0b1100 or  0b1010        # 0b0000_1110
0b1100 xor 0b1010        # 0b0000_0110

a << b                   # shift left
a >> b                   # shift right
a <<< b                  # rotate left
a >>> b                  # rotate right
```

### Ranges

```fink
0..10                    # 0 inclusive, 10 exclusive
0...10                   # 0 inclusive, 10 inclusive
```

Range literals are first-class values.

### Membership

`in` / `not in` test membership across any container that supports it — ranges, sequences, record keys, dict keys:

```fink
5  in 0..10              # range
'x' in {x: 1, y: 2}      # record key
2  in [1, 2, 3]          # sequence element
5  not in 0..3           # negated form
```

### Member access

By name:

```fink
point.x
foo.bar.spam
```

By expression — the expression must be resolvable at compile time, or the operand's type must implement `.`:

```fink
[10, 20, 30].(0)         # 10

key = 'x'
point.(key)              # point.x

point.'x'                # shorthand for .(str key)
```

### Spread

Destructures on the left, splices on the right.

```fink
[head, ..tail] = [1, 2, 3]

greet = fn name, ..titles: '${name} — ${titles}'

both = [..left, ..right]
merged = {..a, ..b}
```

---

## Precedence and grouping

Parentheses group, newlines separate statements, and `;` is a strong separator for stacking expressions on one line.

```fink
15 == (1 + 2) * (2 + 3)

[3, 7] == seq
  add 1, 2
  add 3, 4

[3, 7] == [add 1, 2; add 3, 4]
```

---

## Bindings

ƒink bindings use pattern matching — the left side is a pattern, the right side is the value.

### Left-hand

```fink
foo = 1

[a, b] = [1, 2]
{x, y} = point
{x, y: z} = point        # bind x to point.x and y to point.z
```

### Right-hand

Useful for results of multi-line expressions:

```fink
foo
  arg1
  arg2
|= result
```

### Guards

Any pattern position accepts a guard — a boolean expression that must hold for the pattern to match.

```fink
[x, y > 2] = [1, 3]
[x, is_even y] = [1, 4]      # assumes a user-defined `is_even`
```

### Nesting and spread

```fink
[a, [b, c]] = [1, [2, 3]]

{a, b: {c, d}} = {a: 1, b: {c: 2, d: 3}}

[head, ..tail] = [1, 2, 3, 4]
[head, ..middle, end] = [1, 2, 3, 4]
```

### Records match partially; sequences match exactly

```fink
{a} = {a: 1, b: 2}       # fine — records match partially
[a] = [1, 2]             # fails — sequence has extra elements
[a, ..] = [1, 2]         # fine — ..  discards the rest
```

---

## Functions

Defined with `fn args: body`. Zero args is `fn: body`.

```fink
add = fn a, b:
  result = a + b
  result
```

A single-line form is also fine when the body is short:

```fink
add = fn a, b: a + b

greet = fn: 'hello'
```

### Pattern-matched parameters

Same pattern language as bindings:

```fink
sum = fn {x, y}: x + y
head = fn [head, ..]: head
```

### Varargs

One trailing `..rest` parameter captures the rest of the arguments as a sequence.

```fink
log = fn prefix, ..parts:
  '${prefix}: ${parts}'
```

### `fn match`

Syntactic sugar for `fn args: match args:`. Use when the whole function body is a `match` on the parameter.

```fink
classify = fn match n:
  n > 0: 'positive'
  n < 0: 'negative'
  _:     'zero'
```

is the same as

```fink
classify = fn n: match n:
  n > 0: 'positive'
  n < 0: 'negative'
  _:     'zero'
```

### Higher-order, closures, recursion

Functions are values. They close over their enclosing scope. Module-level functions can refer to each other in any order (mutual recursion).

```fink
is_even = fn n:
  match n:
    0: true
    _: is_odd  n - 1

is_odd = fn n:
  match n:
    0: false
    _: is_even n - 1
```

---

## Application

Apply arguments to a function by writing them after it, separated by commas. `;` is a stronger-binding comma for packing expressions inline.

```fink
log 'hello'
add 1, 2

add
  mul 2, 3
  mul 3, 4
# same as:
add (mul 2, 3), (mul 3, 4)

add mul 2, 3; mul 3, 4
```

Nested application is right-to-left:

```fink
foo bar spam
# same as:
foo (bar spam)
```

### Tagged postfix application

A literal followed by a function name applies the function to the literal. Useful for unit-like wrappers and other post-fix conversions:

```fink
10sec                    # sec 10
10.5min                  # min 10.5
(foo)min                 # min foo
```

### Partial application with `?`

`?` in an expression stands for a hole that, taken together with the expression's scope, becomes a function of one argument.

```fink
add5 = add 5, ?
# same as:
add5 = fn $: add 5, $

inc = ? + 1
# same as:
inc = fn $: $ + 1
```

`?` bubbles up to the nearest scope boundary. The boundaries are:

- a parenthesised group `(...)`,
- a pipe segment (everything between two `|`s, or from a `|` to the start of the statement),
- the right-hand side of a binding (`lhs = rhs` — the bubble stops at `rhs`, never engulfs the `=`),
- a standalone top-level expression.

All `?` in the same scope refer to the same single parameter.

```fink
[?, ?]                   # fn $: [$, $]
{foo: ?, bar: ?}         # fn $: {foo: $, bar: $}

(foo ?.(1), ?.(2))       # fn $: foo $.(1), $.(2)  — one input, used twice
```

Parenthesise to narrow the scope:

```fink
{bar: (? + 2), spam: (? + 3)}
# same as:
{bar: fn $: $ + 2, spam: fn $: $ + 3}
```

---

## Pipes

`|` applies left-to-right. Each pipe segment is its own partial-application scope.

```fink
'hello' | capitalize | log
# same as:
log capitalize 'hello'
```

With partial application, each segment uses `?` for the incoming value:

```fink
add = fn a, b: a + b

result = 2
  | add 3, ?           # add 3 to 2
  | add 10, ?          # then add 10
# result == 15
```

Use `..?` to splat a sequence into multiple arguments:

```fink
[1, 2] | add ..?         # add 1, 2
```

---

## Pattern matching

`match` tries each arm top-to-bottom; the first that matches wins. Bindings from the matching pattern are in scope for the arm's body.

```fink
classify = fn match n:
  0:                  'zero'
  n > 0 and n < 10:   'small positive'
  n > 0:              'large positive'
  _:                  'negative'
```

Deep structural matching:

```fink
describe = fn match point:
  {x: 0, y: 0}:         'origin'
  {x: 0, y}:            'on y-axis at ${y}'
  {x, y: 0}:            'on x-axis at ${x}'
  {x, y}:               '(${x}, ${y})'
```

Sequence and record patterns support spread:

```fink
match items:
  []:              'empty'
  [x]:             'one: ${x}'
  [x, ..rest]:     'head ${x}, rest ${rest}'

match config:
  {}:                  'empty'
  {debug: true, ..}:   'debug mode'
  {..anything}:        'some config'
```

String patterns capture holes in a template:

```fink
match 'hello world':
  'hello ${rest}': rest      # 'world'
  _: ''
```

---

## Modules

A file is a module. Bind the exports you care about from the result of `import`.

```fink
{read, write} = import './files.fnk'
{add, mul}    = import './math.fnk'
```

Names bound at the top level of a module are exported by default; destructuring imports picks a subset. Module paths are relative to the importing file.

---

## Concurrency and IO

ƒink programs are cooperative — tasks yield at I/O and scheduler points. Values flow between tasks through channels.

### `main` and the IO channels

The runner calls `main` with `args, stdin, stdout, stderr` — CLI argv followed by the three IO channels. `main` returns an exit code.

```fink
{stdin, stdout, stderr} = import 'std/io.fnk'

main = fn ..args:
  'Hello, world' >> stdout
  0
```

### Sending to a channel

`>>` sends the left-hand value to the right-hand channel; `<<` sends the right-hand value to the left-hand channel. Both work; pick whichever reads better in context.

```fink
'line one' >> stdout
stdout << 'line two'
```

> **Note on precedence.** `<<` and `>>` today double as bitwise shift. Dispatch is by value type — a channel on one side, bits on both.

### Receiving from a channel

`receive` parks the current task until a message arrives:

```fink
line = receive stdin
```

### Spawning and awaiting

`spawn` creates a task from a zero-arg function; `await` blocks on its result.

```fink
future = spawn fn:
  compute_something

result = await future
```

### Reading raw bytes

`read stream, n` reads up to `n` bytes from a host stream (stdin, typically):

```fink
bytes = read stdin, 1024
```

---

## Block scoping

Every indented body is its own scope; bindings inside don't leak out.

```fink
result = (
  tmp = 10 + 20
  tmp * 2
)
# tmp is not in scope here
# result == 60
```

Record field bodies, match arm bodies, and function bodies behave the same way. Module scope is the only place where bindings are mutually recursive — order of definition does not matter inside a module.

---

## Indentation

Indented lines continue the preceding construct. A decrease in indent ends the construct.

```fink
add
  mul 2, 3
  mul 3, 4

# continuation after a comma is fine
foo bar,
  spam
```

Use `;` for a strong inline separator when you need several expressions on one line:

```fink
[add 1, 2; add 3, 4]
```

---

## Further reading

- [execution-model.md](execution-model.md) — how a ƒink program runs.
- [debugging.md](debugging.md) — running ƒink under a debugger.
- [roadmap.md](roadmap.md) — designed features not yet reachable.
- [../CONTRIBUTING.md](../CONTRIBUTING.md) — build, test, contribute.
- [../src/README.md](../src/README.md) — how the compiler is organised.
