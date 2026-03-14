// Compiler-internal CPS IR.
//
// Designed from the compiler's perspective — no runtime plumbing (env handles,
// state threading, ƒ_cont strings). Those are output formatting conventions only,
// synthesized by the pretty-printer and codegen from the structural IR.
//
// Scope is structural (nesting). Env and state are implicit.
// Every function has an explicit name (user or synthetic).
// Ref nodes carry `Ref::Name` (user) or `Ref::Gen(CpsId)` (compiler temp,
// pointing at the Bind::Gen node); resolution is a side-table populated
// by the resolve pass.

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

/// A free variable captured from an outer scope — identified by the CpsId
/// of the Ref node at the capture site. The name is recoverable from the
/// origin map (CpsId → AstId → AST node → ident string).
pub type FreeVar = CpsId;

/// A definition site — introduces a name into scope.
/// `User` marks a source-level binding; the name is recoverable from the
/// origin map (CpsId → AstId → AST ident). `Gen` marks a compiler-generated
/// temp; the formatter renders it as `·v_{cps_id}` using the node's own CpsId.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bind {
  User,  // name from source: recoverable via origin map
  Gen,   // compiler-generated temp: rendered as ·v_{cps_id}
}

/// A use site — references a binding. `Name` for user names (identity from
/// origin map), `Gen(CpsId)` for compiler temps (carries the CpsId of the
/// `Bind::Gen` node it refers to — the only link, since Gen has no name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ref {
  Name,          // user ref: name recoverable from origin map
  Gen(CpsId),    // compiler-generated temp: refers to Bind::Gen at the given CpsId
}

impl Ref {
  /// Convert a use-site Ref to the corresponding definition-site Bind.
  pub fn to_bind(self) -> Bind {
    match self {
      Ref::Name => Bind::User,
      Ref::Gen(_) => Bind::Gen,
    }
  }
}

/// A function parameter — either a plain name or a varargs spread (`..rest`).
/// Only one `Spread` is valid, and only in trailing position; enforced by the transform.
#[derive(Debug, Clone)]
pub enum Param {
  Name(BindNode),
  Spread(BindNode),
}

/// A call-site argument — either a plain value or a spread (`..items`).
/// Restricting spread to this type (rather than `ValKind`) prevents spread
/// from appearing in positions where it has no meaning (e.g. `LetVal`, `Ret`).
#[derive(Debug, Clone)]
pub enum Arg<'src> {
  Val(Val<'src>),
  Spread(Val<'src>),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltIn {
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

impl BuiltIn {
  /// Map a source operator string to its `BuiltIn` variant.
  /// Panics on unknown operators — every operator the parser emits must be
  /// covered here. Error recovery can be added later if needed.
  pub fn from_op_str(s: &str) -> BuiltIn {
    match s {
      // Arithmetic
      "+"   => BuiltIn::Add,
      "-"   => BuiltIn::Sub,
      "*"   => BuiltIn::Mul,
      "/"   => BuiltIn::Div,
      "//"  => BuiltIn::IntDiv,
      "%"   => BuiltIn::Mod,
      "%%"  => BuiltIn::IntMod,
      "/%"  => BuiltIn::DivMod,
      "**"  => BuiltIn::Pow,
      // Comparison
      "=="  => BuiltIn::Eq,
      "!="  => BuiltIn::Neq,
      "<"   => BuiltIn::Lt,
      "<="  => BuiltIn::Lte,
      ">"   => BuiltIn::Gt,
      ">="  => BuiltIn::Gte,
      "><"  => BuiltIn::Cmp,
      // Logical
      "and" => BuiltIn::And,
      "or"  => BuiltIn::Or,
      "xor" => BuiltIn::Xor,
      "not" => BuiltIn::Not,
      // Bitwise
      "&"   => BuiltIn::BitAnd,
      "^"   => BuiltIn::BitXor,
      "<<"  => BuiltIn::Shl,
      ">>"  => BuiltIn::Shr,
      "<<<" => BuiltIn::RotL,
      ">>>" => BuiltIn::RotR,
      "~"   => BuiltIn::BitNot,
      // Range
      ".."  => BuiltIn::Range,
      "..." => BuiltIn::RangeIncl,
      "in"  => BuiltIn::In,
      "not in" => BuiltIn::NotIn,
      // Member access
      "."   => BuiltIn::Get,
      _     => panic!("BuiltIn::from_op_str: unknown operator {:?}", s),
    }
  }
}

/// What an App/MatchApp/MatchIf calls — either a runtime value or a built-in.
/// `BuiltIn` has no CpsId — it's a compile-time tag, not an IR node. The
/// enclosing `App` node's CpsId carries the AST origin for the operation.
#[derive(Debug, Clone)]
pub enum Callable<'src> {
  Val(Val<'src>),
  BuiltIn(BuiltIn),
}


// ---------------------------------------------------------------------------
// Node — generic shell shared by Val and Expr
// ---------------------------------------------------------------------------

/// A CPS IR node — generic over its kind type.
/// All three node types — `Val`, `BindNode`, and `Expr` — share the same `CpsId`
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

/// A definition-site node — introduces a name into scope.
/// Has its own `CpsId` so name resolution can point directly at the binding.
pub type BindNode = Node<Bind>;

#[derive(Debug, Clone)]
pub enum ValKind<'src> {
  Ref(Ref),           // a reference to a binding (user name or compiler temp)
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

/// A CPS expression node — computation with continuations.
pub type Expr<'src> = Node<ExprKind<'src>>;

#[derive(Debug, Clone)]
pub enum ExprKind<'src> {
  /// Bind a value to a name; visible in body.
  LetVal {
    name: BindNode,
    val: Box<Val<'src>>,
    body: Box<Expr<'src>>,
  },

  /// Bind a function; name NOT visible in fn_body (non-recursive).
  /// Anonymous fns get a compiler-generated synthetic name.
  /// `free_vars` is populated by the free-variable analysis pass; empty until then.
  /// Contains CpsIds of Ref nodes at capture sites — names recoverable from origin map.
  /// In first-encounter order.
  LetFn {
    name: BindNode,
    params: Vec<Param>,
    /// TODO [deprecated]: remove once resolve pass exists — free vars are derivable
    /// from `Resolution::Captured` entries in the prop graph.
    free_vars: Vec<FreeVar>,
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
    result: BindNode,
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
    name: BindNode,
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
    result: BindNode,
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
    elem: BindNode,
    body: Box<Expr<'src>>,
  },

  /// Assert `val` (cursor) is exhausted; `fail` if elements remain.
  /// Forwards the matched value to `result` in the continuation.
  MatchDone {
    val: Box<Val<'src>>,
    /// TODO: formatting hack — remove when codegen no longer needs readable cursor names.
    cursor: u32,
    fail: Box<Expr<'src>>,
    result: BindNode,
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
    result: BindNode,
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
    field: &'src str,
    fail: Box<Expr<'src>>,
    elem: BindNode,
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
    arm_params: Vec<BindNode>,
    arms: Vec<Expr<'src>>,
    result: BindNode,
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
    result: BindNode,
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
  pub name: BindNode,
  pub params: Vec<Param>,
  pub fn_body: Box<Expr<'src>>,
}

