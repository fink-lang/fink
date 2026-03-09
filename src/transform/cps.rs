// Compiler-internal CPS IR.
//
// Designed from the compiler's perspective — no runtime plumbing (env handles,
// state threading, ƒ_cont strings). Those are output formatting conventions only,
// synthesized by the pretty-printer and codegen from the structural IR.
//
// Scope is structural (nesting). Env and state are implicit.
// Every function has an explicit name (user or synthetic).
// Ident references are annotated with their resolution kind after SCC analysis.

use crate::lexer::Loc;

// ---------------------------------------------------------------------------
// Metadata — attached to every IR node
// ---------------------------------------------------------------------------

/// Per-node metadata. Loc is the source span; type info is a placeholder for
/// the type inference pass. Both are Option so nodes can be constructed before
/// loc threading or type inference is complete.
#[derive(Debug, Clone)]
pub struct Meta {
  pub loc: Option<Loc>,
  pub ty: Option<()>,  // placeholder — replaced when type system is designed
}

impl Meta {
  pub fn none() -> Self {
    Meta { loc: None, ty: None }
  }

  pub fn at(loc: Loc) -> Self {
    Meta { loc: Some(loc), ty: None }
  }
}

// ---------------------------------------------------------------------------
// Names and keys
// ---------------------------------------------------------------------------

/// A plain source name — used for references to existing bindings.
pub type Name<'src> = &'src str;

/// A free variable captured from an outer scope.
/// Typed so the formatter can render each variant correctly without string inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FreeVar<'src> {
  Name(Name<'src>),  // user-defined name: foo, x
  Op(&'src str),     // operator symbol: +, ==, . (rendered as ·op_X in scope capture)
}

/// A binding site — introduces a name into scope.
/// `User` carries the original source name; `Gen` carries a counter (no prefix string).
/// The formatter renders Gen as `·v_N`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindName<'src> {
  User(Name<'src>),  // name from source: `foo`, `x`, `result`
  Gen(u32),          // compiler-generated temp: rendered as ·v_N
}

/// A function parameter — either a plain name or a varargs spread (`..rest`).
/// Only one `Spread` is valid, and only in trailing position; enforced by the transform.
#[derive(Debug, Clone)]
pub enum Param<'src> {
  Name(BindName<'src>),
  Spread(BindName<'src>),
}

/// A call-site argument — either a plain value or a spread (`..items`).
/// Restricting spread to this type (rather than `ValKind`) prevents spread
/// from appearing in positions where it has no meaning (e.g. `LetVal`, `Ret`).
#[derive(Debug, Clone)]
pub enum Arg<'src> {
  Val(Val<'src>),
  Spread(Val<'src>),
}

/// A lookup key — how a name is referenced from scope.
/// Annotated with resolution kind after SCC/semantic analysis.
#[derive(Debug, Clone)]
pub struct Key<'src> {
  pub kind: KeyKind<'src>,
  pub resolution: Option<Resolution>,
  pub meta: Meta,
}

#[derive(Debug, Clone)]
pub enum KeyKind<'src> {
  Name(Name<'src>),      // user-defined name: foo, add, x
  Bind(BindName<'src>),  // typed scope reference — load this binding (avoids string materialisation for Gen temps)
  Prim(Prim),            // known runtime builtin — no scope resolution needed
  Op(&'src str),         // operator symbol: +, ==, .
}

/// Runtime builtin functions referenced in the IR.
/// Emitted by the transform for built-in operations; resolved to runtime
/// globals by codegen. Never appear as binding sites — reference only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prim {
  SeqAppend,   // [a, b, c] element construction
  SeqConcat,   // [..xs, ..ys] spread merge
  RecPut,      // {key: val} field construction
  RecMerge,    // {..rec} spread merge
  StrFmt,      // 'hello ${name}' interpolated string
  StrRaw,      // fmt'...' raw tagged template
}

/// Whether a range pattern is exclusive (`..`) or inclusive (`...`).
/// Replaces the `op: &'src str` field in `PatKind::Range`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeKind {
  Excl,  // `..`  — exclusive upper bound
  Incl,  // `...` — inclusive upper bound
}


/// How a name reference resolves — populated by the semantic/SCC pass.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
  Local,      // bound in current scope, already initialized
  Captured,   // free variable from an outer scope
  Recursive,  // same LetRec group, behind a fn boundary (valid)
  ForwardRef, // same LetRec group, not behind a fn boundary (compile error)
  Global,     // module-level binding
}

// ---------------------------------------------------------------------------
// Values — already-computed things
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Val<'src> {
  pub kind: ValKind<'src>,
  pub meta: Meta,
}

#[derive(Debug, Clone)]
pub enum ValKind<'src> {
  Ident(BindName<'src>),  // a locally bound name (param or let-binding)
  Key(Key<'src>),         // a scope lookup (user name or operator)
  Lit(Lit<'src>),         // a literal value
}

#[derive(Debug, Clone)]
pub enum Lit<'src> {
  Bool(bool),
  Int(i64),
  Float(f64),
  Decimal(f64),       // distinct from Float for the type system
  Str(&'src str),
  Seq,                // empty sequence literal []
  Rec,                // empty record literal {}
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Expr<'src> {
  pub kind: ExprKind<'src>,
  pub meta: Meta,
}

#[derive(Debug, Clone)]
pub enum ExprKind<'src> {
  /// Bind a value to a name; visible in body.
  LetVal {
    name: BindName<'src>,
    val: Box<Val<'src>>,
    body: Box<Expr<'src>>,
  },

  /// Bind a function; name NOT visible in fn_body (non-recursive).
  /// Anonymous fns get a compiler-generated synthetic name.
  /// `free_vars` is populated by the free-variable analysis pass; empty until then.
  /// Contains names read from outer scope (loads not covered by params/locals),
  /// in first-encounter order. Used by cps_fmt to emit `{..·scope, name, …}`.
  LetFn {
    name: BindName<'src>,
    params: Vec<Param<'src>>,
    free_vars: Vec<FreeVar<'src>>,  // references to outer bindings, not definitions
    fn_body: Box<Expr<'src>>,
    body: Box<Expr<'src>>,
  },

  /// Mutually recursive group — all names visible in all fn_bodies.
  /// Each binding: (name, params, fn_body).
  /// Cross-refs not behind a fn boundary → ForwardRef error.
  LetRec {
    bindings: Vec<Binding<'src>>,
    body: Box<Expr<'src>>,
  },

  /// Call func with args; result bound to `result`, visible in body.
  App {
    func: Box<Val<'src>>,
    args: Vec<Arg<'src>>,
    result: BindName<'src>,
    body: Box<Expr<'src>>,
  },

  /// Branch on cond.
  If {
    cond: Box<Val<'src>>,
    then: Box<Expr<'src>>,
    else_: Box<Expr<'src>>,
  },

  /// Irrefutable pattern bind — deconstruct `val` against `pat`; names
  /// introduced by the pattern are available in `body`.
  /// Emitted by the transform for `[a, b] = foo` bind statements and for
  /// complex destructuring params (desugared to a bind in the fn body).
  /// TODO: remove once pattern lowering pass is complete; replaced by Match* primitives.
  LetPat {
    pat: Box<Pat<'src>>,
    val: Box<Val<'src>>,
    body: Box<Expr<'src>>,
  },

  /// Pattern match — scrutinee against a list of arms.
  /// Arms are tried in order; first match wins.
  /// TODO: remove once pattern lowering pass is complete; replaced by Match* primitives.
  Match {
    scrutinees: Vec<Val<'src>>,  // one for single-subject, many for multi-arg match
    arms: Vec<Arm<'src>>,
    result: BindName<'src>,
    body: Box<Expr<'src>>,
  },

  // ---------------------------------------------------------------------------
  // Pattern lowering primitives — produced by the pattern lowering pass.
  // LetPat and Match are eliminated; these replace them.
  // All primitives carry an explicit `fail` continuation (·panic or a ·ƒ_fail ref).
  // ---------------------------------------------------------------------------

  /// Bind an extracted val to a name; always succeeds.
  /// Parallel to LetVal but with an explicit fail cont (for structural uniformity).
  /// Emitted for bare-ident pattern positions: `x = foo` → MatchLetVal(foo, name=x, body).
  MatchLetVal {
    name: BindName<'src>,
    val: Box<Val<'src>>,
    fail: Box<Expr<'src>>,
    body: Box<Expr<'src>>,
  },

  /// Apply `func` to `args`; bind result to `result`; `fail` if tag is wrong.
  /// Used for constructor/extractor patterns: `Ok b`, `Some x`.
  /// Parallel to App but with an explicit fail cont.
  MatchApp {
    func: Box<Val<'src>>,
    args: Vec<Val<'src>>,
    fail: Box<Expr<'src>>,
    result: BindName<'src>,
    body: Box<Expr<'src>>,
  },

  /// Apply `func` to `args`; call `fail` if result is falsy; no result binding.
  /// Used for guard predicates: `is_even x`, `a > 0`.
  /// Fuses apply + boolean test into one node; no intermediate temp exposed.
  MatchIf {
    func: Box<Val<'src>>,
    args: Vec<Val<'src>>,
    fail: Box<Expr<'src>>,
    body: Box<Expr<'src>>,
  },

  /// Assert val equals a literal; `fail` if not.
  /// Used for literal element patterns: `[a, 1]`, `['hello']`.
  MatchValue {
    val: Box<Val<'src>>,
    lit: Lit<'src>,
    fail: Box<Expr<'src>>,
    body: Box<Expr<'src>>,
  },

  /// Assert `val` is a sequence; `fail` if not.
  MatchSeq {
    val: Box<Val<'src>>,
    /// TODO: formatting hack — remove when codegen no longer needs readable cursor names.
    /// The formatter renders this as `·seq_N`; codegen will derive position from structure.
    cursor: u32,
    fail: Box<Expr<'src>>,
    body: Box<Expr<'src>>,
  },

  /// Pop the head element from `val` (the current seq/cursor); bind to `elem`.
  /// `fail` if empty.
  MatchNext {
    val: Box<Val<'src>>,
    /// TODO: formatting hack — remove when codegen no longer needs readable cursor names.
    /// `cursor` = incoming position, `next_cursor` = advanced position (both render as `·seq_N`).
    cursor: u32,
    next_cursor: u32,
    fail: Box<Expr<'src>>,
    elem: BindName<'src>,
    body: Box<Expr<'src>>,
  },

  /// Assert `val` (cursor) is exhausted; `fail` if elements remain.
  /// Forwards the matched value to `result` in the continuation.
  MatchDone {
    val: Box<Val<'src>>,
    /// TODO: formatting hack — remove when codegen no longer needs readable cursor names.
    cursor: u32,
    fail: Box<Expr<'src>>,
    result: BindName<'src>,
    body: Box<Expr<'src>>,
  },

  /// Assert `val` (cursor) is non-empty; `fail` if exhausted.
  MatchNotDone {
    val: Box<Val<'src>>,
    /// TODO: formatting hack — remove when codegen no longer needs readable cursor names.
    cursor: u32,
    fail: Box<Expr<'src>>,
    body: Box<Expr<'src>>,
  },

  /// Bind remaining elements of `val` (cursor) as a value; zero-or-more.
  /// Works on both seq and rec cursors.
  MatchRest {
    val: Box<Val<'src>>,
    /// TODO: formatting hack — remove when codegen no longer needs readable cursor names.
    cursor: u32,
    fail: Box<Expr<'src>>,
    result: BindName<'src>,
    body: Box<Expr<'src>>,
  },

  /// Assert `val` is a record; `fail` if not.
  /// Entry point for rec pattern traversal.
  /// TODO: formatting hack — remove when codegen no longer needs readable cursor names.
  MatchRec {
    val: Box<Val<'src>>,
    /// TODO: formatting hack — mirrors MatchSeq; formatter renders this as `·rec_N`.
    cursor: u32,
    fail: Box<Expr<'src>>,
    body: Box<Expr<'src>>,
  },

  /// Extract named `field` from `val` (rec/cursor); bind extracted val to `elem`.
  /// Advances the cursor: `cursor` is the incoming position, `next_cursor` the advanced one.
  MatchField {
    val: Box<Val<'src>>,
    /// TODO: formatting hack — mirrors MatchNext cursor/next_cursor pair.
    cursor: u32,
    next_cursor: u32,
    field: Name<'src>,
    fail: Box<Expr<'src>>,
    elem: BindName<'src>,
    body: Box<Expr<'src>>,
  },

  /// Pattern match block — tries arms in order; first match wins.
  /// The runtime injects the scrutinee into each arm as the first param.
  /// `fail` is the exhaustion continuation (·panic, or outer ·ƒ_fail in nested matches).
  /// Each arm expr is a lowered Match* primitive chain ending in ·ƒ_cont.
  /// `result` names the value received by the result cont from whichever arm succeeds.
  MatchBlock {
    scrutinee: Box<Val<'src>>,
    /// Name injected into each arm as the scrutinee param (e.g. `·v_0`).
    scrutinee_param: BindName<'src>,
    fail: Box<Expr<'src>>,
    arms: Vec<Expr<'src>>,
    result: BindName<'src>,
    body: Box<Expr<'src>>,
  },

  /// Tail position — return value to current continuation.
  Ret(Box<Val<'src>>),

  /// Unconditional failure — pattern match with no recovery.
  /// Used as the `fail` expr for irrefutable patterns (·panic equivalent).
  /// Lets the compiler statically identify always-failing paths.
  Panic,

  /// Reference to the enclosing `·ƒ_fail` continuation.
  /// Used as the `fail` expr inside match arm bodies — failure delegates to next arm.
  /// Only valid inside a MatchBlock arm.
  FailCont,
}

/// A single binding in a LetRec group.
#[derive(Debug, Clone)]
pub struct Binding<'src> {
  pub name: BindName<'src>,
  pub params: Vec<Param<'src>>,
  pub fn_body: Box<Expr<'src>>,
  pub meta: Meta,
}

/// A single match arm — pattern + body.
/// `bindings` lists names introduced by the pattern, available in fn_body.
#[derive(Debug, Clone)]
pub struct Arm<'src> {
  pub pattern: Pat<'src>,
  pub bindings: Vec<BindName<'src>>,  // names introduced by pattern, for scope analysis
  pub fn_body: Box<Expr<'src>>,
  pub meta: Meta,
}

// ---------------------------------------------------------------------------
// Patterns — preserved from AST, lowered to matcher primitives only at codegen
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Pat<'src> {
  pub kind: PatKind<'src>,
  pub meta: Meta,
}

#[derive(Debug, Clone)]
pub enum PatKind<'src> {
  /// _ — discard
  Wildcard,

  /// foo — bind scrutinee to name
  Bind(BindName<'src>),

  /// 42, true, 'hello' — equality check against literal
  Lit(Lit<'src>),

  /// [a, b, ..rest] — sequence pattern; spreads appear as SeqElem::Spread in elems
  Seq(Vec<SeqElem<'src>>),

  /// (a, b, c) — fixed-arity positional pattern from multi-arg match/fn.
  /// No spread allowed. Arity must match the number of scrutinees.
  /// Distinct from Seq so the type checker can enforce arity statically.
  Tuple(Vec<Pat<'src>>),

  /// {foo, bar: x, ..rest} — record pattern; spreads appear as RecElem::Spread in elems
  Rec(Vec<RecElem<'src>>),

  /// 'hello ${..rest}' — string interpolation pattern
  Str(Vec<StrPat<'src>>),

  /// 'a'...'z' or 0..10 — range pattern
  Range {
    kind: RangeKind,
    start: Box<Pat<'src>>,
    end: Box<Pat<'src>>,
  },

  /// guard: `head > 3` or `is_odd head` — pattern with predicate
  Guard {
    pat: Box<Pat<'src>>,
    guard: Box<Val<'src>>,
  },
}

impl<'src> Pat<'src> {
  /// Collect all names bound by this pattern (depth-first, left-to-right).
  pub fn bindings(&self) -> Vec<BindName<'src>> {
    let mut names = vec![];
    self.collect_bindings(&mut names);
    names
  }

  fn collect_bindings(&self, out: &mut Vec<BindName<'src>>) {
    match &self.kind {
      PatKind::Wildcard | PatKind::Lit(_) | PatKind::Range { .. } => {}
      PatKind::Bind(name) => out.push(*name),
      PatKind::Guard { pat, .. } => pat.collect_bindings(out),
      PatKind::Tuple(pats) => {
        for p in pats { p.collect_bindings(out); }
      }
      PatKind::Seq(elems) => {
        for elem in elems {
          match elem {
            SeqElem::Pat(p) => p.collect_bindings(out),
            SeqElem::Spread(s) => {
              if let Some(n) = s.name { out.push(n); }
              if let Some(n) = s.bind { out.push(n); }
            }
          }
        }
      }
      PatKind::Rec(elems) => {
        for elem in elems {
          match elem {
            RecElem::Field(f) => f.pattern.collect_bindings(out),
            RecElem::Spread(s) => {
              if let Some(n) = s.name { out.push(n); }
              if let Some(n) = s.bind { out.push(n); }
            }
          }
        }
      }
      PatKind::Str(parts) => {
        for p in parts {
          if let StrPat::Spread(s) = p {
            if let Some(n) = s.name { out.push(n); }
            if let Some(n) = s.bind { out.push(n); }
          }
        }
      }
    }
  }
}

/// An element in a sequence pattern.
#[derive(Debug, Clone)]
pub enum SeqElem<'src> {
  Pat(Pat<'src>),
  Spread(Spread<'src>),
}

/// A spread element — `..rest`, `..(guard)`, `..(guard) |= name`.
#[derive(Debug, Clone)]
pub struct Spread<'src> {
  pub guard: Option<Box<Val<'src>>>,      // None = bare `..rest`
  pub bind: Option<BindName<'src>>,       // `|= name` binding
  pub name: Option<BindName<'src>>,       // `..rest` name
  pub meta: Meta,
}

/// An element in a record pattern — either a named field or a spread.
#[derive(Debug, Clone)]
pub enum RecElem<'src> {
  Field(RecField<'src>),
  Spread(Spread<'src>),
}

/// A field in a record pattern.
#[derive(Debug, Clone)]
pub struct RecField<'src> {
  pub key: Name<'src>,
  pub pattern: Pat<'src>,
  pub meta: Meta,
}

/// A segment in a string pattern.
#[derive(Debug, Clone)]
pub enum StrPat<'src> {
  Lit(&'src str),         // literal text segment
  Spread(Spread<'src>),   // `${..rest}` interpolation capture
}
