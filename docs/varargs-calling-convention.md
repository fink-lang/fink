# Varargs Calling Convention

## Problem

Fink supports variadic arguments (`fn a, ..rest:`) and call-site spread
(`f a, ..b, c`). The four permutations require runtime flexibility:

```
f = fn a, ..rest:       # varargs callee
f a, b                  # rest = [b]
f a, ..b, c             # rest = [..b, c]

g = fn a, b:            # fixed-arity callee
g a, b                  # normal
g a, ..b, c             # spread unpacked, must yield exactly 2 total args
```

Neither the caller nor the callee knows the other's shape at compile time
(closures, higher-order functions, callbacks). Lifting can wrap any function
(including continuations and matcher funcs) in a `$Closure` when it captures
variables, so all functions must share one callable signature.

Full flow analysis is not practical. A uniform calling convention is needed.

## Design

### Universal array-based calling convention

All functions use a single WASM signature: `$Fn(ref $VarArgs)`.

Everything — value args, continuations — is packed into the `$VarArgs`
array. The continuation (when present) is the last element.

```
fn a, b:     → $Fn(ref $VarArgs)  where args = [a, b, cont]
fn ·v_14:    → $Fn(ref $VarArgs)  where args = [v_14]  (cont with one value)
match arm    → $Fn(ref $VarArgs)  where args = [subject, ...]
```

This is required because lifting can turn any of these into a closure,
and `_croc` dispatch needs a uniform target signature.

#### Call site

The caller packs all arguments (including the continuation) into a
`$VarArgs` array:

- **Normal args**: push each value into the array
- **Spread `..x`**: unpack the sequence and append each element
- **Continuation**: push as the last element

```wat
;; f a, b, c  →  args = [a, b, c, cont]
(array.new_fixed $VarArgs 4 (a) (b) (c) (cont))

;; f a, ..b, c  →
;; build array dynamically: [a, ..unpack(b), c, cont]
```

For calls without spread, `array.new_fixed` builds the array inline (same
as `StrFmt` today). For calls with spread, a dynamic builder is needed
(since the spread length is unknown at compile time).

#### Callee

The function unpacks named params from the array via `array.get`:

```wat
;; fn a, b:  →  cont is args[2]
(local.set $a (array.get $VarArgs (local.get $args) (i32.const 0)))
(local.set $b (array.get $VarArgs (local.get $args) (i32.const 1)))
(local.set $cont (array.get $VarArgs (local.get $args) (i32.const 2)))

;; fn a, ..rest:  →  cont is args[len-1], rest is args[1..len-1]
(local.set $a (array.get $VarArgs (local.get $args) (i32.const 0)))
;; rest = args[1..len-1] converted to Fink list (cons cells)
;; cont = args[len-1]
```

For `..rest`, the remaining elements (between fixed params and cont) are
converted to a Fink list so that `rest` behaves as a normal sequence value —
pattern matching, spread, iteration all work.

### Closures

`$Closure` stays as `(struct funcref, (ref null $Captures))`.

Closure dispatch prepends captures into the `$VarArgs` array before
forwarding:

```
1. Unbox $Closure → funcref, captures
2. If captures is null: forward $VarArgs as-is
3. If captures is non-null: build new $VarArgs = [..captures, ..args]
4. return_call_ref funcref(new_args)
```

All `_croc_N` variants collapse into a single `_croc`. All `$FnN` types
collapse into a single `$Fn`.

### Builtins

Builtins (`op_add`, `str_fmt`, `seq_prepend`, etc.) keep their current
fixed-arity signatures. They are called directly by the emitter via
`return_call`, not through closure dispatch. No changes needed.

### Tagged templates

Tagged templates naturally fall out of this design. `tag'hello ${x} world'`
packs `['hello ', x, ' world', cont]` into a `$VarArgs` and calls the tag
function. The tag function is `fn ..parts:` and receives the parts as a
list — raw string segments and interpolated values interleaved.

## Trade-offs

### Costs
- Every function call allocates a `$VarArgs` array
- Every function entry unpacks params via `array.get`
- Closure dispatch may allocate a second array (captures + args)

### Benefits
- Uniform — one calling convention handles all permutations
- No flow analysis needed
- No `$FnN` type proliferation (single `$Fn`)
- `_croc` collapses to a single function
- Varargs and spread work everywhere, including higher-order and closures
- Any function can be lifted to a closure without signature changes

### Future optimisations
- **Direct calls**: when the callee is statically known and has no spread,
  bypass the array and call directly with fixed params
- **Inlining**: eliminate the array entirely for small known-target calls
- **Arity checking**: validate arg count at call site or callee entry as a
  debug/development aid

## Migration

This is a breaking change to the emitter and closure dispatch. Affected:

1. `emit.rs` — function signatures, call emission, closure construction
2. `collect.rs` — param collection (all functions become single `$Fn`)
3. `_croc_N` → single `_croc` in runtime
4. `$FnN` types → single `$Fn`
5. Closure construction — captures prepended to `$VarArgs` at dispatch time
6. CPS `Param::Spread` / `Arg::Spread` — emitter handles them for real
7. WAT snapshot tests — all need re-blessing
8. End-to-end runner tests — should pass unchanged (semantics preserved)
