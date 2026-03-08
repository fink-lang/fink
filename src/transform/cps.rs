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

/// A bound name — parameters, let-bindings, synthetic temporaries.
/// Sigil conventions (·foo, ƒ_cont) are output-only; plain names here.
pub type Name<'src> = &'src str;

/// A function parameter — either a plain name or a varargs spread (`..rest`).
/// Only one `Spread` is valid, and only in trailing position; enforced by the transform.
#[derive(Debug, Clone)]
pub enum Param<'src> {
  Name(Name<'src>),
  Spread(Name<'src>),
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
  Name(Name<'src>),  // user-defined name: foo, add, x
  Op(&'src str),     // operator symbol: +, ==, .
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
  Ident(Name<'src>),  // a locally bound name (param or let-binding)
  Key(Key<'src>),     // a scope lookup (user name or operator)
  Lit(Lit<'src>),     // a literal value
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
    name: Name<'src>,
    val: Box<Val<'src>>,
    body: Box<Expr<'src>>,
  },

  /// Bind a function; name NOT visible in fn_body (non-recursive).
  /// Anonymous fns get a compiler-generated synthetic name.
  LetFn {
    name: Name<'src>,
    params: Vec<Param<'src>>,
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
    result: Name<'src>,
    body: Box<Expr<'src>>,
  },

  /// Branch on cond.
  If {
    cond: Box<Val<'src>>,
    then: Box<Expr<'src>>,
    else_: Box<Expr<'src>>,
  },

  /// Pattern match — scrutinee against a list of arms.
  /// Arms are tried in order; first match wins.
  /// Pattern lowering to matcher primitives is a separate later pass.
  /// Type inference and semantic analysis work on the Pat tree directly.
  Match {
    scrutinee: Box<Val<'src>>,
    arms: Vec<Arm<'src>>,
    result: Name<'src>,
    body: Box<Expr<'src>>,
  },

  /// Tail position — return value to current continuation.
  Ret(Box<Val<'src>>),
}

/// A single binding in a LetRec group.
#[derive(Debug, Clone)]
pub struct Binding<'src> {
  pub name: Name<'src>,
  pub params: Vec<Param<'src>>,
  pub fn_body: Box<Expr<'src>>,
  pub meta: Meta,
}

/// A single match arm — pattern + body.
/// `bindings` lists names introduced by the pattern, available in fn_body.
#[derive(Debug, Clone)]
pub struct Arm<'src> {
  pub pattern: Pat<'src>,
  pub bindings: Vec<Name<'src>>,  // names introduced by pattern, for scope analysis
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
  Bind(Name<'src>),

  /// 42, true, 'hello' — equality check against literal
  Lit(Lit<'src>),

  /// [a, b, ..rest] — sequence pattern with optional spread
  Seq {
    elems: Vec<SeqElem<'src>>,
    spread: Option<Box<Spread<'src>>>,
  },

  /// {foo, bar: x, ..rest} — record pattern with optional spread
  Rec {
    fields: Vec<RecField<'src>>,
    spread: Option<Box<Spread<'src>>>,
  },

  /// 'hello ${..rest}' — string interpolation pattern
  Str(Vec<StrPat<'src>>),

  /// 'a'...'z' or 0..10 — range pattern
  Range {
    op: &'src str,   // ".." or "..."
    start: Box<Pat<'src>>,
    end: Box<Pat<'src>>,
  },

  /// guard: `head > 3` or `is_odd head` — pattern with predicate
  Guard {
    pat: Box<Pat<'src>>,
    guard: Box<Val<'src>>,
  },
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
  pub guard: Option<Box<Val<'src>>>,  // None = bare `..rest`
  pub bind: Option<Name<'src>>,       // `|= name` binding
  pub name: Option<Name<'src>>,       // `..rest` name
  pub meta: Meta,
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
