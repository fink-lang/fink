# Fink / Larix: A History

## Part 2: The Next Generation

### A Fresh Start

The first Fink compiler was a proof of concept that exceeded its own brief. By the time it was done, it had bootstrapped — the Fink compiler was written in Fink itself, compiling to JavaScript. That milestone validated the core design: the language was expressive enough to write real, non-trivial programs. But it also made clear that the foundation had cracks. Early design decisions had calcified. Syntax pain points accumulated. The type system was absent entirely.

Rather than patch the existing implementation, the decision was made to start clean. The bootstrapped compiler had done its job: not to be maintained, but to teach. Everything learned from writing it — what was elegant, what chafed, what the language wanted to be — would feed directly into the next generation.

The new compiler would be written in Rust. It would target WebAssembly with garbage collection (WasmGC) or LLVM IR. And it would have types from the start.

The language was also renamed. *Fink* had a naming conflict with an existing macOS package manager, and the GitHub org needed to be clean. After exploring options across English, German, Esperanto, and Latin, the answer turned out to already be present in the existing codebase: *Larix* — the genus of the larch tree, which had been used as the name of the AST library in the first compiler. The GitHub org became *hackmatack*, another name for the larch, which happened to be available and had a satisfying sound to it.

---

### Designing the Language, For Real This Time

The clean slate began with the language specification — working through each feature area systematically, making concrete decisions before writing any code.

**Arithmetic and operators** came first. The core arithmetic operators were straightforward, but the edge cases required thought. Integer division got its own operator (`//`). Remainder (`%`) follows the dividend's sign; true modulus (`mod`) follows the divisor's. Both exist because they serve different use cases, and conflating them is a common source of bugs. Divmod (`/%`) returns a tuple. Power is `**`.

Bitwise operators reuse the logical keywords — `and`, `or`, `xor`, `not` — dispatching based on operand types rather than having a separate set of symbols. This keeps the operator space clean.

**The type system** was the largest design surface. The language settled on variant types for sum types, product types with a dedicated `type:` syntax, generics, and a form of dependent types. Open record types (`{..}`) allow structural typing at the call site. Protocols provide the dispatch mechanism for polymorphism — abstract function types rather than interfaces or typeclasses, though the concept is similar.

**Pattern matching** got a thorough treatment: destructuring for all type forms, guards, type matching, range constraints using `..` syntax, and exhaustiveness checking. The compiler ensures no case goes unhandled.

**Number literals** surfaced an interesting detail. Decimal literals use a `d` suffix (`1.0d`, `1.0d9`). An `int + decimal` promotes safely, but `float + decimal` is always a compile error — the different precision models are incompatible by design, and the compiler enforces this rather than silently coercing.

Tagged literals — where a numeric literal is immediately followed by an identifier without whitespace, as in `1.5sec` — were a subtle but useful feature. The tag is called as a function, receiving the raw integer components rather than the computed float. So `1.5sec` calls `sec(1, 5)`, giving the tag function exact control over interpretation.

**Set operators** were worked out in full. Union is `+` (ordered, left-to-right), intersection is `and` (with `&` deferred until the pipe operator finds a new home), difference is `-`, symmetric difference is `^`, cartesian product is `*`. Subset and superset relationships use the comparison operators. Disjointness uses `><`.

**The `?` placeholder** creates implicit lambdas scoped to the nearest argument boundary. It threads through expressions naturally and was a point of earlier design tension — in the first compiler, `?` had been treated as an operator with a binding power in the Pratt parser, which caused problems. The resolution was to not treat it as an operator at all.

**Pipelines** use `|` with a strict rule: the right-hand side must be a single-argument function, and the left-hand side must be a value. The pipe operator strictly requires these types, and the parser enforces the rule syntactically: `|` continuation at a line start is a special case rather than a general infix operator. Custom infix tagging operators were considered and ruled out — the ambiguity between identifiers and operators made the parsing intractable in a clean way.

---

### Building the Compiler

With the specification taking shape, implementation began on the new compiler in Rust.

**The tokenizer** was designed to be lazy, with one-token lookahead, and zero-copy: tokens are not strings but lightweight views into the source buffer — a start position and an end position. The full source is kept in memory, and every downstream phase can slice into it for free. This makes error messages precise at every stage of compilation without any additional bookkeeping.

Indentation sensitivity is handled by emitting synthetic `BlockStart`, `BlockCont`, and `BlockEnd` tokens rather than exposing raw whitespace to the parser. The parser never has to think about indentation directly.

User-definable operators required care in the tokenizer. The operator registry is populated on-demand: as the parser encounters new operator definitions, it registers the byte sequences with the tokenizer, which can then recognize them going forward. Built-in and custom operators share the same `OpId`-based registry.

String interpolation, bracket nesting, and error recovery all live in the tokenizer. Two snapshot test formatters — compact and verbose — make the tokenizer's output inspectable, with visual indentation mirroring the block structure of the source.

**The parser** uses a two-level architecture. A Pratt parser handles true operators — infix, prefix, and postfix — with binding powers. Everything else is dispatched through null-denotation handlers into dedicated recursive descent parse functions. Keyword-led constructs (`if`, `match`, `fn`, `let`, and so on) each get their own parse function, called from the Pratt table's nud dispatch rather than being shoehorned into the operator precedence machinery.

This was a direct lesson from the first compiler. Pratt parsers work elegantly for operator precedence, but become painful when extended to non-operator constructs. The `?` debacle from the first implementation was the clearest example: assigning a binding power to something that wasn't really an operator created subtle bugs and convoluted grammar rules. Keeping the Pratt table small and letting recursive descent handle everything else produced a much cleaner result.

**The intermediate representation** is continuation-passing style (CPS) rather than static single assignment (SSA). The choice was deliberate: SSA is LLVM's concern. At the compiler's level, CPS gives a clean representation of control flow and closures without the phi-node complexity that SSA requires. LLVM and Binaryen both perform SSA construction and optimization downstream; there's no value in duplicating that work.

The CPS environment graph uses a parent/child structure rather than flat key-value maps. Looking up a name walks from child to parent, which gives lexical scoping and shadowing for free. Environments are populated eagerly per scope, which handles forward references and mutual recursion without needing special `letrec` markers or two-pass resolution. Whether a reference is recursive or a genuine closure capture is determined purely by which environment level it resolves from — a clean invariant that falls out of the structure naturally.

Inlining is deferred entirely to LLVM/Binaryen. There's no value in implementing it at the CPS level when the downstream optimizer does it better with more information.

The CPS representation can be rendered back into Larix source syntax, with IL metadata in comments. This enables snapshot testing that is both human-readable and executable — the rendered output can be fed back through the compiler. Compiling already-transformed CPS should produce an isomorphic graph, an idempotency property that serves as a structural correctness check. This was noted as interesting but deferred to post-v1.

---

### Working with Claude

A significant part of the language design and implementation work for the next generation was done in collaboration with Claude, Anthropic's AI assistant, via claude.ai. The workflow was practical: design sessions covered one feature area at a time, working toward concrete decisions, with Claude producing summaries suitable for passing to Claude Code for implementation work.

This was genuinely useful. Having an interlocutor that could follow the technical context across long sessions, hold multiple design constraints in mind simultaneously, and quickly prototype examples or trace through implications of a decision compressed the design timeline substantially. Sessions that might have taken days of solo thinking — sketching, discarding, re-sketching — could be worked through in hours.

**The challenges were real, though.**

The most persistent issue was context. Claude has no memory between conversations by default. Every session had to re-establish where things stood: what decisions had been made, what was still open, what the relevant constraints were. Early on this was friction. The mitigation was to treat each session's output as a document — summaries of decisions made, specifications of components designed — so that the next session could start from a concrete artifact rather than from scratch. Memory features in claude.ai helped with high-level continuity across sessions, but the detailed technical state still needed to be brought back explicitly.

A related issue was that Claude would sometimes produce confident-sounding answers to design questions that turned out to be wrong — not wrong in a factual sense, but wrong for *this* language, for reasons that depended on earlier decisions that weren't fully in context. The resolution was to treat Claude's suggestions as proposals to evaluate rather than conclusions to accept. The human in the loop remained the decision-maker; Claude was accelerating the process of generating and pressure-testing options.

The two-level parser architecture — Pratt plus recursive descent — is a good example of this dynamic. The initial instinct was to extend the Pratt parser to handle everything, which is a natural impulse and something Claude initially worked within. It was only when the `?` problem surfaced as a concrete failure case that the conversation shifted to the cleaner architecture. Claude was useful in that conversation precisely because it could quickly trace through why the binding-power approach was breaking down and articulate the alternative clearly. The insight came from the back-and-forth, not from either party in isolation.

**What worked well:**

Working through feature areas systematically, one at a time, with Claude as a sounding board. The language covers a lot of ground — arithmetic, type system, pattern matching, set operators, number literals, pipeline semantics, tagged operators, CPS IR design, tokenizer architecture, parser architecture — and having a persistent interlocutor for each area that could hold the prior decisions in mind and flag contradictions was genuinely valuable.

Code generation worked well too. Pseudocode examples, Rust sketches, and JavaScript prototypes could be produced quickly and iterated on within a session. The JS style preferences (snake_case, const, arrow functions, template strings) were captured as project instructions and applied consistently, which reduced the editing overhead.

The snapshot test formatter for the tokenizer is a small example of this: a design decision was made in conversation, Claude produced a working sketch, the sketch was refined, and the result fed directly into the implementation. The same pattern repeated across components.

**The broader lesson** was that AI assistance is most useful as an amplifier of good process, not a replacement for it. Keeping decisions documented, working systematically, and maintaining the human as the final arbiter of design choices made the collaboration productive. Treating it as an oracle or a shortcut to skip the hard thinking would have produced worse results — and would have been slower in the end, when the accumulated bad decisions had to be unpicked.

---

### Where Things Stand

The tokenizer is complete. The parser architecture is settled. The CPS IR design is specified. The language specification covers all major feature areas, with a small set of things explicitly deferred to after v1 — the idempotency property of the CPS transform, custom operator definition, macros, and a few dependent type features that need real usage to drive design decisions.

The next step is AST construction, building on the completed tokenizer. After that, the CPS transform, and then code generation targeting WasmGC or LLVM IR.

The tooling picture is also taking shape: a VSCode extension with a graph viewer panel, bidirectional source-to-graph navigation via source maps, and property graph export to Neo4j or ArangoDB for Cypher queries in debug mode. The CPS snapshot format, being executable Larix source, makes the compiler's internals unusually transparent — a property that started as a testing strategy but may turn out to be useful as a teaching tool for compiler concepts.

The clean slate was the right call.