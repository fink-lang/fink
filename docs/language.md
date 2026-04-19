# ƒink — Language Reference

By-example reference for the Fink language. Each section shows the
current syntax with runnable snippets.

> **Status convention.** Each section is one of:
>
> - **implemented** — the parser and runtime accept it today.
> - **designed** — syntax + semantics settled; parser/runtime doesn't accept yet.
> - **open** — active design questions; see [roadmap.md](roadmap.md) for details.
>
> The type system, protocols, macros, async/concurrency, and effects
> are all designed but not yet implemented. They live in
> [roadmap.md](roadmap.md) and the WIP example files alongside this one.

---

## Comments

> **Status:** implemented

```fink
# end-of-line comment

---
block comment
---
```

---

## Literals

> **Status:** implemented (except where noted)

### Booleans

```fink
true
false
```

### Integers

Type is inferred from the literal value and sign.

```fink
1_234_567               # u32
+1                      # i8
-1                      # i8
0xFF                    # u8
+0xFF                   # i8
0xFfFf                  # u16
0xFFFF_FFFF             # u32
0xFFFF_FFFF_FFFF_FFFF   # u64
0o_1234_5670            # octal
0b_0101_1111            # binary
```

### Floats and decimals

```fink
1.0             # f32
1.0e100_000     # f64 — too big for f32

1.0d            # decimal — cannot be mixed with floats
1.0d-100
```

### Tagged literals

Postfix function application — `<value><name>` reads as `<name> <value>`.

```fink
10sec      # == sec 10
10.5min    # == min 10.5
(foo)min   # == min foo
```

### Strings

Fink strings are byte sequences (not validated UTF-8). See
[`src/strings/README.md`](../src/strings/README.md) for the rationale and
the full escape-sequence table.

```fink
'hello world'

# multiline
'
  hello
  world
'

# interpolation
'hello ${1 + 2}'

# escape sequences (excerpt)
'\n \t \\ \' \$ \x0f \u{ff} \u{10_ff_ff}'
```

#### String blocks

`":` opens an indented multiline string. The colon ends the open
delimiter; the indented block is the content. Block strings support
interpolation and don't require escaping `'`.

```fink
":
  supports templating ${bar}
  no need to escape 'spam'
```

#### Tagged templates

A tag function receives the raw string parts and the interpolated
values, interleaved as a single argument list.

```fink
fmt'hello ${1 + 2}'
sql'SELECT * FROM users WHERE name = ${name}'
raw'foo \n \t bar'        # raw — no escape processing
raw":
  foo \n
  bar
```

### Sequences

```fink
[]
[1, 2, 3]
seq 1, 2, 3
seq
  1
  2
  seq 3, 4
```

### Records

Static, compile-time-known field names.

```fink
{}
{foo: 1, bar: 2}
{foo: 1, 'ni na': 2, (a + b): 3}   # computed keys must be compile-time resolvable

# multiline
{
  foo: 1
  bar: {
    spam: 3
  }
  ham:
    ni = own-scoped-block
    ni * 2     # value of ham field
}
```

### Dictionaries

Like records, but with runtime keys (any hashable key, any value).

```fink
dict {foo: 1, 'bar': 2, (key): 3}
```

### Sets

```fink
set 1, 2, 3, 3                   # == set 3, 2, 1
ordered_set 3, 2, 3, 1           # == ordered_set 3, 2, 1
                                 # != ordered_set 1, 2, 3
```

### Identifiers

Any sequence of UTF-8 graphemes excluding registered operators,
separators, and terminators. Most operators sit between whitespace,
so `-` is fine in the middle of an identifier (a prefix `-` has no
whitespace to its right, so the parser disambiguates).

```fink
foo
foo-bar
foo_bar
ni_1234
```

`_` is the **wildcard** — a non-binding placeholder, not a regular
identifier.

```fink
_                      # discard in pattern or fn-param position
fn _, b: b             # ignore first arg
[_, x] = [1, 2]        # discard first element
```

---

## Operators

> **Status:** implemented (except where noted)
>
> Each operator has a protocol; types implementing the protocol get
> overloaded behaviour. (Protocol declarations are
> [designed](roadmap.md#protocols), not yet implemented.)

### Arithmetic

```fink
-a          # unary minus
a + b       # add
a - b       # subtract
a * b       # multiply
a / b       # divide
a // b      # integer divide
a ** b      # power
a %  b      # remainder — sign follows dividend
a %% b      # true modulus — sign follows divisor
a /% b      # divmod — returns [quotient, remainder]
```

### Logical

Operands must be `bool`; result is `bool`.

```fink
not a
a and b
a or b
a xor b
```

### Comparison

Always returns `bool`. Comparison can chain.

```fink
a >  b
a >= b
a <  b
a <= b
a == b
a != b              # not equal — neither identity nor value
a >  b >  c         # chained
a >= b >= c         # chained
a >< b              # disjoint — for types, sets, lists
```

### Bitwise

Polymorphic — `not`/`and`/`or`/`xor` operate bitwise on integers and
boolean-wise on `bool`s.

```fink
not 0b0101_0101    == 0b1010_1010
0b1100 and 0b1010  == 0b0000_1000
0b1100 or  0b1010  == 0b0000_1110
0b1100 xor 0b1010  == 0b0000_0110
a >>  b            # shift right
a <<  b            # shift left
a >>> b            # rotate right
a <<< b            # rotate left
```

### Set operators

```fink
a or b      # union (left-to-right merge)
a xor b     # symmetric difference
a and b     # intersection
a -   b     # difference
a *   b     # cartesian product

# comparison — always returns bool
a == b      # equality
a != b      # inequality
a <  b      # proper subset
a <= b      # subset or equal
a >  b      # proper superset
a >= b      # superset or equal
a >< b      # disjoint

a in     b  # membership
a not in b  # non-membership
```

### Spread

```fink
[head, ..tail]
[head, .. tail]            # whitespace-tolerant
[..seq1, ..seq2]           # concat

{foo: bar, ..rest}
{..rec1, ..rec2}           # merge — right wins on conflict

foo bar, ..spam, ni        # spread in call site
fn x, ..ys, z: _           # spread in params

[..]    # matches non-empty seq
{..}    # matches non-empty rec
```

### Ranges

`..` is exclusive on the upper bound; `...` is inclusive.

```fink
0..10              # 0 inclusive, 10 exclusive
0...10             # 0 inclusive, 10 inclusive
'a'...'z'          # char range inclusive
start..end
start...end
(1 + 2)..(3 + 4)
```

> _Open:_ commonly-used steps (e.g. `1..10..2`) — see
> [roadmap.md](roadmap.md).

### Member access

```fink
# by name literal
foo.bar
foo.bar-spam
foo.bar.spam == (foo.bar).spam

# by member/key expression
# expression must be compile-time static, OR L and R must implement the . protocol
foo.(expr)

[1, 2, 3].(0) == 1
[0, 1, 2, 3, 4].(2...3) == [2, 3]

{foo: 123}.foo == 123
{'ni nu': 123}.('ni ni') == 123
{x: 1, y: 2, z: 3}.(['x', 'y']) == {x: 1, y: 1}
```

> _Open:_ shorthand forms `foo.[…]` and `foo.'…'` for indexing — see
> [roadmap.md](roadmap.md).

---

## Precedence

> **Status:** implemented

### Grouping

```fink
15 == (1 + 2) * (2 + 3)
[3, 7] == [(add 1, 2), (add 3, 4)]
```

### Newline as a strong expression separator

```fink
[3, 7] == seq
  add 1, 2
  add 3, 4
```

### `;` as a strong `,` separator

`;` terminates argument lists harder than `,` does, removing the need
for explicit parentheses in many call patterns.

```fink
[3, 7] == [add 1, 2; add 3, 4]
       == [(add 1, 2), (add 3, 4)]
```

---

## Bindings

> **Status:** implemented
>
> Every binding is a pattern match. If the pattern doesn't match, the
> failure is reported at compile time when statically detectable;
> otherwise it traps at runtime.

### Left-hand binding

```fink
foo = 1

[a, b] = [1, 2]
{x, y} = point
{x, y: z} = point      # bind x to point.x, y to point.y

# nested structures
[a, [b, c]] = [1, [2, 3]]
{a, b: {c, d}} = {a: 1, b: {2, 3}}

# patterns must match — these fail at compile time
[x, 3] = [1, 2]        # 3 doesn't match 2
[x]    = [1, 2]        # right side has too many items
```

### Pattern guards

```fink
[x, y >= 2]      = [1, 2]
[x, is_even y]   = [1, 2]
```

### Records match partially; sequences match exactly

```fink
{a}        = {a, b}    # record: extra fields ignored
{a, ..{}}  = {a, b}    # explicit empty rest — fails because {b} isn't empty
[a]        = [1, 2]    # sequence: error, sizes don't match
[a, ..]    = [1, 2]    # use rest-discard for partial match
```

### Spread in patterns

```fink
[head, ..]                       = [1]
[head, ..tail]                   = [1, 2, 3, 4]
[head, ..middle, end]            = [1, 2, 3, 4]
[head > 3, ..tail]               = [4, 5, 6]
[(is_odd head), ..tail]          = [3, 4, 5]
[head, ..(0..2), ..rest]         = [1, 2, 3, 4, 5]
[..(is_odd), ..evens]            = [1, 2, 3, 4, 5]   # evens == [2, 4]
```

### String matching

```fink
'start ${middle} end' = 'foo bar spam'
# middle == ' bar '
```

### Right-hand binding

`|=` is `=` with the direction reversed. Useful for capturing the result
of a multi-line expression or a binding within nested structures.

```fink
123 |= b

foo
  arg1
  arg2
|= result

# binding nested values, each with their own pattern
[a |= [b, 2], [3, 4]] = [[1, 2], [3, 4]]
# a == [1, 2], b == 1

# binding results of spread guards
[..(str) |= strings, ..other] = [1, '2', 3, '4']
# strings == ['2', '4'], other == [1, 3]
```

---

## Pattern matching (`match`)

> **Status:** implemented
>
> Any binding pattern works on the left of a match arm. Arms are tried
> top-to-bottom; the first matching one wins. Bindings created by the
> match are visible in that arm's body.

```fink
# value matching against literals
match foo:
  1: 'one'
  2: 'two'
  _: 'other'

# binding
match foo:
  [head, ..tail]: head
  []: 'empty'

# guards inline
match foo:
  n > 0 and n < 10: 'small positive ${n}'
  n > 0:            'large positive ${n}'
  even n:           'even number ${n}'
  _:                'other number ${foo}'

# deep structural
match foo:
  {x, y}: x + y

# string patterns
match s:
  'hello ${..rest}': rest
  'a'..'z':          'lowercase letter'

# sequence patterns
match items:
  []:                  'empty'
  [..]:                'non empty'
  [x]:                 'one element'
  [x, y]:              'two elements ${x} ${y}'
  [x, ..]:             'unbound non-empty rest'
  [x, ..rest]:         'bound rest'

# record patterns
match foobar:
  {}:                  'empty'
  {..}:                'non empty'
  {foo: 1}:            'has foo=1, may have other fields'
  {foo: 1, ..}:        'unbound non-empty rest'
  {foo: 1, ..{}}:      'unbound empty rest'
  {foo: 1, ..rest}:    'bound rest'
  {x: 1, y: {z}}:      'matched z: ${z}'
```

> _Designed but not implemented:_ matching on a type
> (`match foo: str s: ...`, `u8 n: ...`, `[..str |= ss]: ...`) and
> number-mask patterns (`0b_xx11_xx11`, `0x_FF_xx`). See
> [roadmap.md](roadmap.md#types) and
> [docs/examples/type-system.fnk](examples/type-system.fnk).

---

## Functions

> **Status:** implemented

```fink
fn a, b:
  a + b

# single line
fn a, b: a + b

# bound to a name
add = fn a, b:
  result = a + b
  result

# no args
greet = fn: 'hello'

# default args
greet = fn name='world':
  'hello ${name}'

# pattern matching in args follows standard pattern matching
foo = fn {x, y}: x + y
bar = fn [head, ..tail]: head
baz = fn arg, ..rest_args: arg + rest_args
```

### `fn match` sugar

`fn match` is a one-arg function whose body is a `match` on that arg:

```fink
classify = fn match n:
  n > 0: 'positive'
  n < 0: 'negative'
  _:     'zero'

# equivalent to:
classify = fn n: match n:
  n > 0: 'positive'
  n < 0: 'negative'
  _:     'zero'
```

For multi-arg variants, `fn match a, ..b:` matches against the full arg
list:

```fink
foo = fn match a, ..b:
  1, ..:        1
  2, x, ..y:    2
```

### Closures and higher-order

```fink
map = fn map_fn: fn match items:
  []:              []
  [item, ..rest]:  [(map_fn item), ..(map map_fn) rest]
```

### Mutual recursion

Module-level bindings are mutually recursive — forward references work
freely.

```fink
is_even = fn n:
  match n:
    0: true
    _: is_odd n - 1

is_odd = fn n:
  match n:
    0: false
    _: is_even n - 1
```

---

## Application

> **Status:** implemented

```fink
log 'hello'
add 1, 2

# multiline args
add
  mul 2, 3
  mul 3, 4
# equivalent to
add (mul 2, 3), (mul 3, 4)
```

### `;` as a strong terminator

`;` ends an argument list harder than `,` — handy to avoid parens
inline.

```fink
add mul 2, 3; mul 3, 4
```

### Right-to-left nested application

```fink
foo bar spam ham
# equivalent to
foo (bar (spam ham))
# equivalent to
foo
  bar
    spam
      ham
```

### Postfix tagged application

```fink
(foo)min          # == min foo
[1, 2, 3]foo      # == foo [1, 2, 3]
{foo: bar}spam    # == spam {foo: bar}
123sec
```

### Partial application (`?`)

`?` placeholders bubble up to the nearest enclosing scope boundary and
become a single param `$`. See
[`src/passes/partial/README.md`](../src/passes/partial/README.md) for
the full scoping rules.

```fink
add5 = add 5, ?
add5 = ? + 5
add5 = fn b: add 5, b

(?)             == fn $: $
(1 + ?)         == fn $: 1 + $
[1, ?]          == fn $: [1, $]
[1, ..?]        == fn $: [1, ..$]
{foo, ..?}      == fn $: {foo, ..$}
(?.foo)         == fn $: $.foo
(?.(expr))      == fn $: $.(expr)
(foo ?, bar)    == fn $: foo $, bar
(foo ..?)       == fn $: foo ..$
```

`Group (...)` is the only explicit scope boundary; pipe segments and
statement tops are natural outer boundaries; everything else (`Apply`,
`InfixOp`, `Member`, `Range`, `Spread`, `LitSeq`, `LitRec`) is
transparent. All `?`s in the same scope refer to the same single `$`.

```fink
# seq/rec transparent — both ? are the same $
[?, ?]          == fn $: [$, $]
{foo: ?, bar: ?} == fn $: {foo: $, bar: $}

# Group narrows scope
(foo ?.(1), ?.(2))   == fn $: foo $.(1), $.(2)
(foo (bar ?))        == foo (fn $: bar $)
```

---

## Pipes

> **Status:** implemented

`|` flips application left-to-right. Each pipe segment is its own scope
(important for `?`).

```fink
foo | bar | spam
# equivalent to
spam (bar foo)

# multiline pipe — recommended
'hello'
| capitalize
| log
# equivalent to
log (capitalize 'hello')

# partial in pipe — common case
1..10
| filter is_divisible ?, 2     # == filter (fn $: is_divisible $, 2)
| map ? * 2                    # == map (fn $: $ * 2)
| [..?]                        # == fn $: [..$]
|= even_nums

# spread the pipe input as multiple args
[1, 2] | add ..?
# equivalent to
[1, 2] | fn [a, b]: add a, b
```

---

## Error handling

> **Status:** implemented

```fink
# try — unwrap if Ok, otherwise return early with Err
fn foo:
  a = try bar _
  b = try baz a
  Ok a + b

# match — handle errors explicitly
fn foo:
  match bar _:
    Ok x:    x + 1
    Err err: log 'error: ${trace err}'

# error chaining
fn foo:
  match bar _:
    Ok x:  Ok x
    Err e: Err e, 'foo failed'
```

---

## Block scoping

> **Status:** implemented

A new block scope is created by parens, by record-field bodies, by
match arm bodies, and by function bodies. Bindings introduced inside a
block do not leak out.

```fink
spam = (                # parens create a block scope
  ni = ham 1
  ni                    # ni is the value of the (...) — does not leak
)

{
  foo: 12
  bar:                  # field body — its own scope
    ni = 123            # ni never leaks out of bar:
    shrub ni, 456       # the value bound to bar
}

match 123:
  a:                    # arm body — its own scope
    ni = foo a          # ni and a never leak
    ni + 1
```

Plain assignment without a block does not introduce a scope:

```fink
foo = spam = ham        # foo, spam, ham all bind to the same value
```

---

## Modules and imports

> **Status:** implemented

```fink
{foo, bar} = import './foobar.fnk'
```

The path is resolved relative to the importing module. Two consumers
reaching the same file via different relative paths share a single
linked instance. See
[`src/passes/wasm-link/README.md`](../src/passes/wasm-link/README.md)
for canonical-URL details and dep-init ordering.

---

## Indentation

> **Status:** implemented

Indentation groups multiline arguments. Single-line and multiline forms
of the same expression mean the same thing.

```fink
foo bar,
  spam
# equivalent to
foo bar, spam
# equivalent to
foo
  bar
  spam
```

---

## Not yet implemented

The following features have settled syntax but no parser/runtime
support yet. See [roadmap.md](roadmap.md) for the per-feature status
and the GitHub issues tracking active design questions.

- **Type system** — opaque, product, sum/variant, generic, dependent,
  union, type spread. Sketches in
  [docs/examples/type-system.fnk](examples/type-system.fnk).
- **Protocols** — abstract functions, per-type specialisation. Sketches
  in [docs/examples/protocols.fnk](examples/protocols.fnk).
- **Macros** — compile-time AST manipulation. Sketches in
  [docs/examples/macros.fnk](examples/macros.fnk).
- **Async / concurrency primitives at the language level** — `spawn`,
  `await_all`, implicit-await on access. (The runtime building blocks
  exist — see [`src/runtime/scheduler.wat`](../src/runtime/scheduler.wat)
  and [`src/runtime/channel.wat`](../src/runtime/channel.wat); the
  user-facing surface is the part that's not finalised.)
- **Context / effects** — `context`, `with`, `get_ctx`. Algebraic
  effects abstraction.
- **Patterns as first-class values** — sketches in
  [docs/examples/unresolved.fnk](examples/unresolved.fnk).
