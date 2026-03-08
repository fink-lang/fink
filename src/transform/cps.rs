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

/// A plain source name — used for references to existing bindings (e.g. free_vars).
pub type Name<'src> = &'src str;

/// A binding site — introduces a name into scope.
/// `User` carries the original source name; `Gen` carries a counter (no prefix string).
/// The formatter is responsible for rendering Gen as `·v_N` / `·fn_N` etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindName<'src> {
  User(Name<'src>),  // name from source: `foo`, `x`, `result`
  GenVal(u32),       // compiler-generated value temp: rendered as ·v_N
  GenFn(u32),        // compiler-generated function name: rendered as ·fn_N
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
  Name(Name<'src>),  // user-defined name: foo, add, x
  Prim(Prim),        // known runtime builtin — no scope resolution needed
  Op(&'src str),     // operator symbol: +, ==, .
}

/// Runtime builtin functions referenced in the IR.
/// Emitted by the transform for built-in operations; resolved to runtime
/// globals by codegen. Never appear as binding sites — reference only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prim {
  RangeExcl,   // 0..10
  RangeIncl,   // 0...10
  SeqAppend,   // [a, b, c] element construction
  SeqConcat,   // [..xs, ..ys] spread merge
  RecPut,      // {key: val} field construction
  RecMerge,    // {..rec} spread merge
  StrFmt,      // 'hello ${name}' interpolated string
  StrRaw,      // fmt'...' raw tagged template
}

impl Prim {
  pub fn as_str(self) -> &'static str {
    match self {
      Prim::RangeExcl => "range_excl",
      Prim::RangeIncl => "range_incl",
      Prim::SeqAppend => "seq_append",
      Prim::SeqConcat => "seq_concat",
      Prim::RecPut    => "rec_put",
      Prim::RecMerge  => "rec_merge",
      Prim::StrFmt    => "str_fmt",
      Prim::StrRaw    => "str_raw",
    }
  }
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
    free_vars: Vec<Name<'src>>,  // references to outer bindings, not definitions
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
  /// Pattern lowering to matcher primitives is a separate later pass.
  LetPat {
    pat: Box<Pat<'src>>,
    val: Box<Val<'src>>,
    body: Box<Expr<'src>>,
  },

  /// Pattern match — scrutinee against a list of arms.
  /// Arms are tried in order; first match wins.
  /// Pattern lowering to matcher primitives is a separate later pass.
  /// Type inference and semantic analysis work on the Pat tree directly.
  Match {
    scrutinee: Box<Val<'src>>,
    arms: Vec<Arm<'src>>,
    result: BindName<'src>,
    body: Box<Expr<'src>>,
  },

  /// Tail position — return value to current continuation.
  Ret(Box<Val<'src>>),
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
      PatKind::Seq { elems, spread } => {
        for elem in elems {
          match elem {
            SeqElem::Pat(p) => p.collect_bindings(out),
            SeqElem::Spread(s) => {
              if let Some(n) = s.name { out.push(n); }
              if let Some(n) = s.bind { out.push(n); }
            }
          }
        }
        if let Some(s) = spread {
          if let Some(n) = s.name { out.push(n); }
          if let Some(n) = s.bind { out.push(n); }
        }
      }
      PatKind::Rec { fields, spread } => {
        for f in fields { f.pattern.collect_bindings(out); }
        if let Some(s) = spread {
          if let Some(n) = s.name { out.push(n); }
          if let Some(n) = s.bind { out.push(n); }
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
