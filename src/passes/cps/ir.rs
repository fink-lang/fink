// Compiler-internal CPS IR.
//
// Designed from the compiler's perspective — no runtime plumbing (env handles,
// state threading, ƒ_cont strings). Those are output formatting conventions only,
// synthesized by the pretty-printer and codegen from the structural IR.
//
// Scope is structural (nesting). Env and state are implicit.
// Every function has an explicit name (user or synthetic).
//
// ---------------------------------------------------------------------------
// Identity and name resolution design
// ---------------------------------------------------------------------------
//
// All bindings — whether from source identifiers or compiler-generated — use
// `Bind::SynthName` or `Bind::Synth`. There is no `Bind::Name` variant; source
// names are never stored in the IR. All references use `Ref::Synth(CpsId)`,
// pointing at the `BindNode` by its CpsId.
//
// The formatter recovers source names by following: CpsId → origin map →
// AstId → AST node kind:
//   - Ident("foo")    → rendered as `·foo_<cps_id>`
//   - SynthIdent(n)   → rendered as `·$_<n>_<cps_id>`
//   - no AST origin   → rendered as `·v_<cps_id>`
//
// `Bind::SynthName` marks a bind node whose CpsId was pre-allocated before the
// CPS transform ran, using the scope analysis output. Pre-allocation solves
// forward references (mutual recursion at any nesting depth): at a ref site,
// scope resolution gives `ref_ast_id → BindId`, and `CpsResult.bind_to_cps`
// maps `BindId → CpsId`, so `Ref::Synth(cps_id)` can be emitted before the
// bind node is constructed.
//
// Pre-allocation mechanics: `BindId` is a dense index into `ScopeResult.binds`
// (0..n). The CPS allocator starts at n, so `CpsId(bind_id.0)` is the
// pre-allocated id for each scope bind — the mapping is the identity function.
// `CpsResult.bind_to_cps: PropGraph<BindId, CpsId>` stores this explicitly so
// downstream passes can query it without knowing the offset convention.
// The origin map is pre-filled for `CpsId(0)..CpsId(n)` from
// `ScopeResult.binds[i].origin` before the transform runs.
//
// `Bind::Synth` marks a compiler-generated temp with no source origin — e.g.
// intermediate results from operators, sequence cursors. These are allocated
// on-the-fly during the transform. Lifting also creates `Bind::Synth` nodes
// for capture params and hoisted continuations.
//
// `Bind::Cont` marks a continuation parameter. Rendered as `·v_<cps_id>`.
// TODO: collapse Bind::Cont into Bind::Synth — the distinction no longer affects rendering.
// `ContRef(CpsId)` references a continuation as a value (e.g. fail args).
//
// The `name_res` pass is no longer responsible for source-name resolution —
// that is handled by pre-allocation + `Ref::Synth`. It remains responsible for
// resolving refs introduced by lifting (hoisted fns reference their capture
// params by the original CpsId, which `synth_alias` maps to the new param).

// ---------------------------------------------------------------------------
// Module root representation
//
// `lower_module` produces a flat `LetFn`/`LetVal` chain with no outer wrapper.
// The chain terminates with `App { func: ContRef(v_0), args: [exports] }`:
//   e.g. `·v_0 ·foo_0, ·bar_1, ·baz_2`
// Only simple top-level `name = <non-import expr>` bindings are exported.
// Pattern destructures and imports are excluded.
// `v_0` is the implicit module-exit continuation (provided by the runtime/linker).
// Name resolution / import matching of those args is a separate pass.
// ---------------------------------------------------------------------------

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
  /// Maps each scope BindId to its pre-allocated CpsId.
  /// Populated before the transform runs. The mapping is CpsId(bind_id.0) by
  /// construction (the CPS allocator starts at scope_result.binds.len()), but
  /// stored explicitly so downstream passes don't depend on the offset convention.
  pub bind_to_cps: crate::propgraph::PropGraph<crate::passes::scopes::BindId, CpsId>,
  /// Synth capture aliases: maps new cap param CpsId → original captured bind CpsId.
  /// Populated by closure_lifting when creating synth cap params (fresh CpsId for
  /// a param that carries a value previously bound under a different CpsId).
  /// Consumed by name_res: registers the old CpsId as an alias in the synths scope
  /// so that Ref::Synth(old_id) in the hoisted fn body resolves to the new param.
  pub synth_alias: crate::propgraph::PropGraph<CpsId, Option<CpsId>>,
}

// ---------------------------------------------------------------------------
// Names and references
// ---------------------------------------------------------------------------

/// A definition site — introduces a name into scope.
///
/// `SynthName` marks a source-level binding whose CpsId was pre-allocated from
/// `ScopeResult.binds` before the transform ran. The source name is recoverable
/// via the origin map: CpsId → AstId → Ident("foo") | SynthIdent(n).
/// Formatter rendering: Ident → `·foo_<cps_id>`, SynthIdent → `·$_<n>_<cps_id>`.
///
/// `Synth` marks a compiler-generated temp with no source origin — intermediate
/// results, sequence cursors, hoisted cont params, etc. Rendered as `·v_<cps_id>`.
///
/// `Cont` marks a continuation parameter. Rendered as `·ƒ_<cps_id>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bind {
  SynthName,  // source-level binding: pre-allocated CpsId, name via origin map
  Synth,      // compiler-generated temp: rendered as ·v_{cps_id}
  Cont,       // continuation parameter: rendered as ·ƒ_{cps_id}
}

/// A use site — references a binding by the CpsId of its `BindNode`.
/// All references are `Ref::Synth(bind_cps_id)` — there is no string-based
/// `Ref::Name`. Source refs use the pre-allocated CpsId from `bind_to_cps`;
/// compiler-generated refs use the CpsId allocated on-the-fly.
/// `Ref::Unresolved(ast_id)` is used when scope resolution found no binding
/// for a name; the CpsId carries the origin AstId for display purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ref {
  Synth(CpsId),       // refers to the BindNode at the given CpsId
  Unresolved(CpsId),  // no binding found; CpsId derived from ref AstId for display
}

impl Ref {
  /// Convert a use-site Ref to the corresponding definition-site Bind kind.
  /// Source refs (pre-allocated) → SynthName; compiler temps → Synth.
  /// Callers that need the actual bind kind should look it up in the IR directly.
  pub fn to_bind(self) -> Bind {
    Bind::Synth
  }
}

impl Bind {
  /// True if this bind introduces a continuation parameter.
  // TODO: remove once Bind::Cont is collapsed into Bind::Synth.
  pub fn is_cont(self) -> bool {
    matches!(self, Bind::Cont)
  }

  /// True if this bind is a source-level name (pre-allocated from scope analysis).
  pub fn is_synth_name(self) -> bool {
    matches!(self, Bind::SynthName)
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
  Cont(Cont<'src>),
  Expr(Box<Expr<'src>>),
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
/// No runtime value — only valid in the func position of App.
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
  // Closure construction — partially applies a lifted fn with its captures.
  // Args: lifted_fn, cap_0, cap_1, ...; result is a closure value.
  FnClosure,
  // Pattern matching primitives — emitted directly by the CPS transform.
  // Each takes val + fail as args; cont receives match results.
  // MatchValue/MatchBlock/MatchArm/MatchIf have been eliminated — literals,
  // match arms, and guards are lowered to plain PatternMatch (LetFn + App + If).
  MatchSeq, MatchNext, MatchDone, MatchNotDone,
  MatchRest, MatchRec, MatchField,
  // Yield — suspend execution, passing a value to the scheduler.
  // Args: value; cont receives the resumed value.
  Yield,
  // Module export — terminal App in a module body. Args are the exported
  // bindings. Replaces anonymous ContRef at module level.
  Export,
  // Module import — `import './foo.fnk'` is a builtin function at module level.
  Import,
}

impl BuiltIn {
  /// Map a source name to its `BuiltIn` variant.
  /// Covers operators, keywords, and builtin functions like `import`.
  /// Panics on unknown names — every builtin the pipeline emits must be
  /// covered here.
  pub fn from_builtin_str(s: &str) -> BuiltIn {
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
      // Module
      "import" => BuiltIn::Import,
      _     => panic!("BuiltIn::from_builtin_str: unknown name {:?}", s),
    }
  }
}

/// What an App calls — either a runtime value or a built-in.
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
  Panic,              // fail sentinel — irrefutable pattern failure (unreachable)
  ContRef(CpsId),     // reference to a continuation as a value (for fail args)
  BuiltIn(BuiltIn),   // a compiler-known op used as a value
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
// Continuations
// ---------------------------------------------------------------------------

/// A continuation — either a reference to an existing function, or an inline
/// expression with one or more result bindings.
///
/// `Ref(id)` — tail call: pass the result directly to the binding at `id`
/// (always a `Bind::Cont` or `Bind::Synth` node).
/// `Expr { args, body }` — inline: bind results to `args` in order, then evaluate `body`.
///
/// Single-result continuations use `args: vec![bind]`. Multi-result continuations
/// (e.g. MatchNext/MatchField which yield elem + next_cursor) use two args.
/// The `CpsId` of each bind is used by the formatter to render compiler-generated
/// temps as `·v_N`. No pass indexes into any table by these ids.
#[derive(Debug, Clone)]
pub enum Cont<'src> {
  Ref(CpsId),
  Expr { args: Vec<BindNode>, body: Box<Expr<'src>> },
}

impl<'src> Cont<'src> {
  /// Return the inline body if this is `Cont::Expr`, else `None`.
  pub fn body(&self) -> Option<&Expr<'src>> {
    match self {
      Cont::Ref(_) => None,
      Cont::Expr { body, .. } => Some(body),
    }
  }

  /// Unwrap the inline body, panicking if this is `Cont::Ref`.
  /// Only use where `Cont::Ref` is structurally impossible.
  pub fn unwrap_body(self) -> (Vec<BindNode>, Box<Expr<'src>>) {
    match self {
      Cont::Expr { args, body } => (args, body),
      Cont::Ref(_) => panic!("Cont::unwrap_body called on Cont::Ref"),
    }
  }
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
    cont: Cont<'src>,
  },

  /// Bind a function; name NOT visible in fn_body (non-recursive).
  /// Anonymous fns get a compiler-generated synthetic name.
  /// For user fns the last param is `Param::Name(Bind::Cont)` — the return
  /// continuation. Lifted continuations may have no cont param at all.
  LetFn {
    name: BindNode,
    params: Vec<Param>,
    // TODO: rename to body
    fn_body: Box<Expr<'src>>,
    cont: Cont<'src>,
  },

  /// Call func with args; the last `Arg::Cont` is the result continuation.
  App {
    func: Callable<'src>,
    args: Vec<Arg<'src>>,
  },

  /// Branch on cond.
  If {
    cond: Box<Val<'src>>,
    // TODO: investigate whether then/else_ should be Cont (structurally same as App cont — "what comes next")
    then: Box<Expr<'src>>,
    else_: Box<Expr<'src>>,
  },

  // ---------------------------------------------------------------------------
  // Pattern matching — Match* primitives are emitted as App { BuiltIn::Match*, args }.
  // Fail conts are encoded as ValKind::Panic or ValKind::ContRef in args.
  //
  // Matcher invariant: matchers work with synthetic temps only (Bind::Synth).
  // No named bindings are created inside a matcher — if a pattern fails,
  // nothing should be in scope. Temps are forwarded to succ on success;
  // the body's params give them user-visible names.
  // ---------------------------------------------------------------------------

}

