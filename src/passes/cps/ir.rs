// Compiler-internal CPS IR.
//
// Designed from the compiler's perspective — no runtime plumbing (env handles,
// state threading, ƒ_cont strings). Those are output formatting conventions only,
// synthesized by the pretty-printer and codegen from the structural IR.
//
// Scope is structural (nesting). Env and state are implicit.
// Every function has an explicit name (user or synthetic).
// Ident references are annotated with their resolution kind after SCC analysis.

// ---------------------------------------------------------------------------
// Node identity
// ---------------------------------------------------------------------------

/// Unique identifier for a CPS expression node, assigned by the transform.
/// Used as a key into property graphs for attaching pass-computed metadata
/// (types, resolution, etc.) without modifying the IR structure.
///
/// The CPS transform produces a `PropGraph<CpsId, Option<AstId>>` (in `CpsResult.origin`)
/// mapping each node back to the AST expression it was synthesized from. This provides
/// AST origin tracking for all nodes — user bindings, refs, and compiler-generated
/// temps alike — without encoding provenance in the IR.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CpsId(pub u32);

impl std::fmt::Debug for CpsId {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "cps#{}", self.0)
  }
}

impl From<CpsId> for usize {
  fn from(id: CpsId) -> usize { id.0 as usize }
}

impl From<usize> for CpsId {
  fn from(n: usize) -> CpsId { CpsId(n as u32) }
}

/// Output of the CPS transform — the IR tree plus metadata.
pub struct CpsResult<'src> {
  pub root: Expr<'src>,
  /// Maps each CPS node back to the AST expression it was synthesized from.
  /// Compiler-generated nodes with no direct AST origin have `None`.
  /// Node count is `origin.len()`.
  pub origin: crate::propgraph::PropGraph<CpsId, Option<crate::ast::AstId>>,
}

// ---------------------------------------------------------------------------
// Names and references
// ---------------------------------------------------------------------------

/// A plain source name — used for references to existing bindings.
pub type Name<'src> = &'src str;

/// A free variable captured from an outer scope.
pub type FreeVar<'src> = Name<'src>;

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
  Name(Bind<'src>),
  Spread(Bind<'src>),
}

/// A call-site argument — either a plain value or a spread (`..items`).
/// Restricting spread to this type (rather than `ValKind`) prevents spread
/// from appearing in positions where it has no meaning (e.g. `LetVal`, `Ret`).
#[derive(Debug, Clone)]
pub enum Arg<'src> {
  Val(Val<'src>),
  Spread(Val<'src>),
}

/// A reference to a binding — how a name is used from scope.
/// TODO [deprecated]: collapse into just `RefKind` — `resolution` moves to prop graph,
/// `Ref` becomes unnecessary wrapper.
#[derive(Debug, Clone)]
pub struct Ref<'src> {
  pub kind: RefKind<'src>,
  /// TODO [deprecated]: move to `PropGraph<CpsId, Option<Resolution>>`.
  pub resolution: Option<Resolution>,
}

/// The variant of a reference — how the name is stored and looked up.
///
/// All names — user-defined, operators, prims — are `Name`. They are
/// distinguished only by their string content, not by RefKind variant.
/// Operators and prims are pre-seeded into scope; a separate shadowing
/// pass protects them from accidental override.
///
/// TODO [deprecated]: once `Ref` collapses (resolution moves to prop graph),
/// inline `RefKind` directly into `ValKind` — `ValKind::Ref(RefKind)` instead
/// of `ValKind::Ref(Ref { kind: RefKind, .. })`.
///
/// TODO: with `CpsId→AstId` origin map, `Name(&'src str)` may not need to carry
/// the string at all — the name is recoverable from the AST via the origin map.
/// The CPS IR would become purely structural, with no strings except
/// `BindName::User` at binding sites. Operators, prims, and user refs would all
/// be just a `CpsId` pointing back to their AST origin.
#[derive(Debug, Clone)]
pub enum RefKind<'src> {
  Name(Name<'src>),      // any name: user ("foo"), operator ("+"), prim ("·seq_append")
  Bind(BindName<'src>),  // typed scope reference — load this binding (avoids string materialisation for Gen temps)
}

/// Whether a range pattern is exclusive (`..`) or inclusive (`...`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeKind {
  Excl,  // `..`  — exclusive upper bound
  Incl,  // `...` — inclusive upper bound
}

// ---------------------------------------------------------------------------
// Compiler-known operations
// ---------------------------------------------------------------------------

/// A compiler-known operation — resolved statically, not by scope lookup.
/// Covers source operators, data construction, and string formatting.
/// No runtime value — only valid in the func position of App/MatchApp/MatchIf.
///
/// NOTE: Consider renaming `Op` → `BuiltIn` — these aren't just operators,
/// they include data construction (SeqAppend, RecPut) and string formatting.
/// `BuiltIn` better conveys "compiler-known callable with a fixed protocol".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
  // Arithmetic
  Add, Sub, Mul, Div, IntDiv, Mod, IntMod, DivMod, Pow,
  // Comparison
  Eq, Neq, Lt, Lte, Gt, Gte, Cmp,
  // Logical
  And, Or, Xor, Not,
  // Bitwise
  BitAnd, BitXor, Shl, Shr, RotL, RotR, BitNot,
  // Range
  Range, RangeIncl, In, NotIn,
  // Member access
  Get,
  // Data construction
  SeqAppend, SeqConcat, RecPut, RecMerge,
  // String interpolation
  StrFmt,
}

impl Op {
  /// Map a source operator string to its `Op` variant.
  /// Returns `None` for unknown operators (user-defined functions).
  pub fn from_str(s: &str) -> Option<Op> {
    match s {
      // Arithmetic
      "+"   => Some(Op::Add),
      "-"   => Some(Op::Sub),
      "*"   => Some(Op::Mul),
      "/"   => Some(Op::Div),
      "//"  => Some(Op::IntDiv),
      "%"   => Some(Op::Mod),
      "%%"  => Some(Op::IntMod),
      "/%"  => Some(Op::DivMod),
      "**"  => Some(Op::Pow),
      // Comparison
      "=="  => Some(Op::Eq),
      "!="  => Some(Op::Neq),
      "<"   => Some(Op::Lt),
      "<="  => Some(Op::Lte),
      ">"   => Some(Op::Gt),
      ">="  => Some(Op::Gte),
      "><"  => Some(Op::Cmp),
      // Logical
      "and" => Some(Op::And),
      "or"  => Some(Op::Or),
      "xor" => Some(Op::Xor),
      "not" => Some(Op::Not),
      // Bitwise
      "&"   => Some(Op::BitAnd),
      "^"   => Some(Op::BitXor),
      "<<"  => Some(Op::Shl),
      ">>"  => Some(Op::Shr),
      "<<<" => Some(Op::RotL),
      ">>>" => Some(Op::RotR),
      "~"   => Some(Op::BitNot),
      // Range
      ".."  => Some(Op::Range),
      "..." => Some(Op::RangeIncl),
      "in"  => Some(Op::In),
      "not in" => Some(Op::NotIn),
      // Member access
      "."   => Some(Op::Get),
      _     => None,
    }
  }
}

/// What an App/MatchApp/MatchIf calls — either a runtime value or a known op.
/// `Op` has no CpsId — it's a compile-time tag, not an IR node. The enclosing
/// `App` node's CpsId carries the AST origin for the operation.
#[derive(Debug, Clone)]
pub enum Callable<'src> {
  Val(Val<'src>),
  Op(Op),
}


/// How a name reference resolves — populated by the resolve pass.
///
/// Every variant carries the CpsId of the Bind node at the definition site,
/// so downstream passes go straight from use → definition.
/// Absence of resolution (None in the PropGraph) = unresolved name error.
/// No Global variant — scope is closed; builtins are pre-seeded Bind nodes.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
  Local(CpsId),      // Bind node in current scope, already initialized
  Captured(CpsId),   // Bind node, across a fn boundary
  Recursive(CpsId),  // LetRec Bind, behind a fn boundary (valid)
  ForwardRef(CpsId), // LetRec Bind, not behind a fn boundary (compile error)
}

// ---------------------------------------------------------------------------
// Node — generic shell shared by Val and Expr
// ---------------------------------------------------------------------------

/// A CPS IR node — generic over its kind type.
/// All three node types — `Val`, `Bind`, and `Expr` — share the same `CpsId`
/// space, so every node is addressable by property graphs.
/// The kind parameter enforces at compile time which positions hold which nodes.
#[derive(Debug, Clone)]
pub struct Node<K> {
  pub id: CpsId,
  pub kind: K,
}

// ---------------------------------------------------------------------------
// Values — already-computed things
// ---------------------------------------------------------------------------

/// An already-computed value — a literal, a local binding reference, or a scope key.
pub type Val<'src> = Node<ValKind<'src>>;

/// A definition site — introduces a name into scope.
/// Has its own `CpsId` so name resolution can point directly at the binding.
pub type Bind<'src> = Node<BindName<'src>>;

/// TODO: once `Ref` collapses and `RefKind` inlines here, consider merging
/// `Ident(BindName)` and the former `RefKind::Bind(BindName)` — both reference
/// a `BindName`, and the distinction (direct use vs scope load) is a
/// runtime/codegen concern, not semantic. The resolve pass could classify
/// everything as just a name, with the prop graph recording how it resolves.
#[derive(Debug, Clone)]
pub enum ValKind<'src> {
  Ident(BindName<'src>),  // a locally bound name (param or let-binding)
  Ref(Ref<'src>),         // a reference to a binding (user name or operator)
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

/// A CPS expression node — computation with continuations.
pub type Expr<'src> = Node<ExprKind<'src>>;

#[derive(Debug, Clone)]
pub enum ExprKind<'src> {
  /// Bind a value to a name; visible in body.
  LetVal {
    name: Bind<'src>,
    val: Box<Val<'src>>,
    body: Box<Expr<'src>>,
  },

  /// Bind a function; name NOT visible in fn_body (non-recursive).
  /// Anonymous fns get a compiler-generated synthetic name.
  /// `free_vars` is populated by the free-variable analysis pass; empty until then.
  /// Contains names read from outer scope (loads not covered by params/locals),
  /// in first-encounter order. Used by cps_fmt to emit `{..·scope, name, …}`.
  LetFn {
    name: Bind<'src>,
    params: Vec<Param<'src>>,
    /// TODO [deprecated]: remove once resolve pass exists — free vars are derivable
    /// from `Resolution::Captured` entries in the prop graph.
    free_vars: Vec<FreeVar<'src>>,
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
    func: Callable<'src>,
    args: Vec<Arg<'src>>,
    result: Bind<'src>,
    body: Box<Expr<'src>>,
  },

  /// Branch on cond.
  If {
    cond: Box<Val<'src>>,
    then: Box<Expr<'src>>,
    else_: Box<Expr<'src>>,
  },

  // ---------------------------------------------------------------------------
  // Pattern lowering primitives — produced by the pattern lowering pass.
  // All primitives carry an explicit `fail` continuation (·panic or a ·ƒ_fail ref).
  // ---------------------------------------------------------------------------

  /// Bind an extracted val to a name; always succeeds.
  /// Parallel to LetVal but with an explicit fail cont (for structural uniformity).
  /// Emitted for bare-ident pattern positions: `x = foo` → MatchLetVal(foo, name=x, body).
  MatchLetVal {
    name: Bind<'src>,
    val: Box<Val<'src>>,
    fail: Box<Expr<'src>>,
    body: Box<Expr<'src>>,
  },

  /// Apply `func` to `args`; bind result to `result`; `fail` if tag is wrong.
  /// Used for constructor/extractor patterns: `Ok b`, `Some x`.
  /// Parallel to App but with an explicit fail cont.
  MatchApp {
    func: Callable<'src>,
    args: Vec<Val<'src>>,
    fail: Box<Expr<'src>>,
    result: Bind<'src>,
    body: Box<Expr<'src>>,
  },

  /// Apply `func` to `args`; call `fail` if result is falsy; no result binding.
  /// Used for guard predicates: `is_even x`, `a > 0`.
  /// Fuses apply + boolean test into one node; no intermediate temp exposed.
  MatchIf {
    func: Callable<'src>,
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
    elem: Bind<'src>,
    body: Box<Expr<'src>>,
  },

  /// Assert `val` (cursor) is exhausted; `fail` if elements remain.
  /// Forwards the matched value to `result` in the continuation.
  MatchDone {
    val: Box<Val<'src>>,
    /// TODO: formatting hack — remove when codegen no longer needs readable cursor names.
    cursor: u32,
    fail: Box<Expr<'src>>,
    result: Bind<'src>,
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
    result: Bind<'src>,
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
    elem: Bind<'src>,
    body: Box<Expr<'src>>,
  },

  /// Pattern match block — tries arms in order; first match wins.
  /// `params` are the values passed into each arm (one per subject).
  /// `arm_params` are the names each arm receives them as (parallel vec).
  /// `fail` is the exhaustion continuation (·panic, or outer ·ƒ_fail in nested matches).
  /// Each arm expr is a lowered Match* primitive chain ending in ·ƒ_cont.
  /// `result` names the value received by the result cont from whichever arm succeeds.
  MatchBlock {
    params: Vec<Val<'src>>,
    fail: Box<Expr<'src>>,
    arm_params: Vec<Bind<'src>>,
    arms: Vec<Expr<'src>>,
    result: Bind<'src>,
    body: Box<Expr<'src>>,
  },

  // ---------------------------------------------------------------------------
  // Suspension
  // ---------------------------------------------------------------------------

  /// Yield — suspend execution, passing `value` to the scheduler.
  /// The continuation receives the resumed value bound to `result`.
  /// Later passes use Yield nodes to color the continuation graph:
  /// every continuation reachable from a Yield is "suspendable."
  Yield {
    value: Box<Val<'src>>,
    result: Bind<'src>,
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

/// A single named function binding in a `LetRec` group.
#[derive(Debug, Clone)]
pub struct Binding<'src> {
  pub name: Bind<'src>,
  pub params: Vec<Param<'src>>,
  pub fn_body: Box<Expr<'src>>,
}

