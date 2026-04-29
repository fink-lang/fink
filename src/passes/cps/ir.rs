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
#[derive(Clone)]
pub struct CpsResult {
  pub root: Expr,
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
  /// Semantic role of each function parameter — Cap, Param, or Cont.
  /// Populated by the lifting pass. Keyed by the param's CpsId.
  /// Downstream passes (formatter, WASM emitter) read this to distinguish
  /// param origins without reverse-engineering from call sites.
  pub param_info: crate::propgraph::PropGraph<CpsId, Option<ParamInfo>>,
  /// Every module-level binding leaf: `(cps_id, source_name)` for each Ident
  /// bound at module scope. Includes destructure leaves (e.g. `x` from
  /// `{x} = ...`). The authoritative source of "which CpsIds become WASM
  /// globals" — not all are exported (see `·ƒpub` for that).
  pub module_locals: Vec<(CpsId, String)>,
  /// Imports declared at module scope: url → [name, ...].
  /// Collected from the AST before CPS lowering, so names are available even
  /// after lifting scatters the rec_pop continuation chain into separate fns.
  pub module_imports: std::collections::BTreeMap<String, Vec<String>>,
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
/// `Cont(ContKind)` marks a continuation parameter with its semantic role.
/// Rendered as `·ret_N`, `·succ_N`, or `·fail_N` depending on the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bind {
  SynthName,       // source-level binding: pre-allocated CpsId, name via origin map
  Synth,           // compiler-generated temp: rendered as ·v_{cps_id}
  Cont(ContKind),  // continuation parameter with semantic role
}

/// Semantic role of a continuation parameter.
///
/// Used by the formatter for readable names and by the emitter to understand
/// the calling convention. Set by the CPS transform at creation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContKind {
  /// Return continuation — where to send the function's result.
  /// Created for every user function and match wrapper.
  Ret,
  /// Success continuation — called when a pattern match succeeds.
  Succ,
  /// Failure continuation — called when a pattern match fails, tries next arm.
  Fail,
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
  pub fn is_cont(self) -> bool {
    matches!(self, Bind::Cont(_))
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

/// Semantic role of a function parameter, populated by the lifting pass.
///
/// Captures the distinction between user-written params, captured variables
/// (threaded as extra params by lifting), and the continuation param added
/// by the CPS transform. The origin CpsId points back to the original
/// binding — for captures this is the binding being closed over, for user
/// params it's the pre-allocated CpsId from scope analysis.
///
/// Stored in `CpsResult.param_info: PropGraph<CpsId, Option<ParamInfo>>`.
/// The WASM emitter can read this directly instead of reverse-engineering
/// param roles from call-site patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamInfo {
  /// A captured variable threaded as an extra param by lifting.
  /// Origin is the CpsId of the original binding being captured.
  Cap(CpsId),
  /// An original user-written function parameter.
  /// Origin is the pre-allocated CpsId from scope analysis.
  Param(CpsId),
  /// The continuation parameter added by the CPS transform.
  Cont,
}

/// A call-site argument — either a plain value or a spread (`..items`).
/// Restricting spread to this type (rather than `ValKind`) prevents spread
/// from appearing in positions where it has no meaning (e.g. `LetVal`, `Ret`).
#[derive(Debug, Clone)]
pub enum Arg {
  Val(Val),
  Spread(Val),
  Cont(Cont),
  Expr(Box<Expr>),
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
  Eq, Neq, Lt, Lte, Gt, Gte, Disjoint,
  // Logical
  And, Or, Xor, Not,
  // Shifts / rotations
  Shl, Shr, RotL, RotR,
  // Range
  Range, RangeIncl, In, NotIn,
  // Member access
  Get,
  // Data construction
  SeqPrepend, SeqConcat, RecPut, RecMerge,
  // String interpolation
  StrFmt,
  // Closure construction — partially applies a lifted fn with its captures.
  // Args: lifted_fn, cap_0, cap_1, ...; result is a closure value.
  FnClosure,
  // Collection primitives — used inside pattern matchers for seq/rec destructuring.
  // IsSeqLike(value, succ(value), fail()) — type guard; succ if seq-like (list)
  // IsRecLike(value, succ(value), fail()) — type guard; succ if rec-like (rec, dict)
  // SeqPop(seq, fail, cont(head, tail)) — pop head element; fail if empty
  // RecPop(rec, name, fail, cont(value, rest)) — extract named field; fail if missing
  // Empty(collection, cont(bool)) — predicate; caller branches with If
  IsSeqLike, IsRecLike, SeqPop, RecPop, Empty,
  // StrMatch(subj, prefix, suffix, fail(), succ(capture)) — string template pattern matching.
  // Checks subj starts with prefix, ends with suffix (non-overlapping), binds the middle slice.
  StrMatch,
  // Scheduling — cooperative multitasking primitives.
  // Yield(value, cont) — suspend current task, switch to next; value for future message passing.
  // Spawn(task_fn, cont) — create new task from task_fn; cont receives future.
  // Await(future, cont) — wait for future to settle; cont receives settled value.
  Yield, Spawn, Await,
  // Channels — multi-message async communication between tasks (point-to-point).
  // Channel(tag, cont) — create new channel; cont receives channel value.
  // Receive(channel, cont) — park receiver; cont receives message when matched.
  // Send is not a builtin — `msg >> ch` dispatches via op_shr to channel.wat's $send.
  Channel, Receive,
  // IO — host-mediated async read. Used by the OLD pipeline only.
  // The IR pipeline imports `read` from `std/io.fnk` instead; an
  // ident named `read` resolves to this builtin only when not
  // shadowed by an import. Plan: remove once the old pipeline goes
  // away. See [.brain/.scratch/read-as-imported-fn.md].
  Read,
  // Module export — terminal App in a module body. Args are the exported
  // bindings. Replaces anonymous ContRef at module level.
  // Legacy: replaced by Pub for new CPS shape. TODO: remove.
  Export,
  // Per-binding export — side effect that registers a name as public.
  // Args: exported value, cont (no args — pure side effect).
  // Emitted by lower_module after each module-level binding that is exported.
  Pub,
  // Module import — `import './foo.fnk'` is a builtin function at module level.
  Import,
  // Module entry point — the root App of every compiled module.
  // `lower_module` emits `App(FinkModule, [Cont::Expr { args: [ƒret], body }])`
  // as the CPS root. The module body is the cont's body, ending with a
  // tail call to ƒret. At runtime, the host provides `fink_module` which
  // invokes the cont with a done continuation.
  FinkModule,
  // Irrefutable-pattern failure sentinel. The compiler wires this in as the
  // "fail continuation" at match sites with no alternative. At runtime it
  // delegates through operators.wat → interop-rust.wat → host_panic, which
  // traps the WASM instance. Zero args today; future work: pass a reason
  // string / source location for diagnostics.
  Panic,
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
      "><"  => BuiltIn::Disjoint,
      // Logical
      "and" => BuiltIn::And,
      "or"  => BuiltIn::Or,
      "xor" => BuiltIn::Xor,
      "not" => BuiltIn::Not,
      // Shifts / rotations
      "<<"  => BuiltIn::Shl,
      ">>"  => BuiltIn::Shr,
      "<<<" => BuiltIn::RotL,
      ">>>" => BuiltIn::RotR,
      // Range
      ".."  => BuiltIn::Range,
      "..." => BuiltIn::RangeIncl,
      "in"  => BuiltIn::In,
      "not in" => BuiltIn::NotIn,
      // Member access
      "."   => BuiltIn::Get,
      // Scheduling
      "yield" => BuiltIn::Yield,
      "spawn" => BuiltIn::Spawn,
      "await" => BuiltIn::Await,
      // Channels
      "channel" => BuiltIn::Channel,
      "receive" => BuiltIn::Receive,
      // IO
      // Module
      "import" => BuiltIn::Import,
      "read" => BuiltIn::Read,
      _     => panic!("BuiltIn::from_builtin_str: unknown name {:?}", s),
    }
  }
}

/// What an App calls — either a runtime value or a built-in.
/// `BuiltIn` has no CpsId — it's a compile-time tag, not an IR node. The
/// enclosing `App` node's CpsId carries the AST origin for the operation.
#[derive(Debug, Clone)]
pub enum Callable {
  Val(Val),
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
pub type Val = Node<ValKind>;

/// A definition-site node — introduces a name into scope.
/// Has its own `CpsId` so name resolution can point directly at the binding.
pub type BindNode = Node<Bind>;

#[derive(Debug, Clone)]
pub enum ValKind {
  Ref(Ref),           // a reference to a binding (user name or compiler temp)
  Lit(Lit),     // a literal value
  ContRef(CpsId),     // reference to a continuation as a value (for fail args)
  BuiltIn(BuiltIn),   // a compiler-known op used as a value
}

#[derive(Debug, Clone)]
pub enum Lit {
  Bool(bool),
  Int(i64),
  Float(f64),
  Decimal(f64),       // distinct from Float for the type system
  /// Byte sequence. Fink strings are byte arrays, not UTF-8 strings — `\xFF`
  /// is a valid 1-byte string literal. Using `Vec<u8>` avoids Rust's UTF-8
  /// validation at the CPS boundary.
  Str(Vec<u8>),
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
/// (e.g. SeqPop/RecPop which yield value + rest_cursor) use two args.
/// The `CpsId` of each bind is used by the formatter to render compiler-generated
/// temps as `·v_N`. No pass indexes into any table by these ids.
#[derive(Debug, Clone)]
pub enum Cont {
  Ref(CpsId),
  Expr { args: Vec<BindNode>, body: Box<Expr> },
}

impl Cont {
  /// Return the inline body if this is `Cont::Expr`, else `None`.
  pub fn body(&self) -> Option<&Expr> {
    match self {
      Cont::Ref(_) => None,
      Cont::Expr { body, .. } => Some(body),
    }
  }

  /// Unwrap the inline body, panicking if this is `Cont::Ref`.
  /// Only use where `Cont::Ref` is structurally impossible.
  pub fn unwrap_body(self) -> (Vec<BindNode>, Box<Expr>) {
    match self {
      Cont::Expr { args, body } => (args, body),
      Cont::Ref(_) => panic!("Cont::unwrap_body called on Cont::Ref"),
    }
  }
}

// ---------------------------------------------------------------------------
// CPS function classification
// ---------------------------------------------------------------------------

/// Distinguishes CPS functions from CPS closures at the IR level.
///
/// The distinction is about the calling convention: CpsFunction is called
/// with `Arg::Cont` at the call site (the cont is a separate WASM param
/// or prepended to the args list in unified $Fn2 mode). CpsClosure is
/// never called with `Arg::Cont` — it receives any continuation values
/// as regular `Arg::Val` arguments or captures.
///
/// Set once by the CPS transform at creation time. Preserved through lifting.
/// See `../wasm/calling-convention.md` for the WASM-level design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpsFnKind {
  /// Called with `Arg::Cont` at the call site. Includes user-defined
  /// functions, match wrappers (m_0), and match matchers (mp_N).
  /// At the WASM level, the cont is prepended to the args list ($Fn2).
  CpsFunction,

  /// Never called with `Arg::Cont`. Includes compiler-generated
  /// continuations (inline cont bodies), match arm bodies (mb_N),
  /// PatternMatch bodies and matchers, and success wrappers.
  /// Continuation values arrive as regular `Arg::Val` or captures.
  CpsClosure,
}


// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

/// A CPS expression node — computation with continuations.
pub type Expr = Node<ExprKind>;

#[derive(Debug, Clone)]
pub enum ExprKind {
  /// Bind a value to a name; visible in body.
  LetVal {
    name: BindNode,
    val: Box<Val>,
    cont: Cont,
  },

  /// Bind a function; name NOT visible in fn_body (non-recursive).
  /// Anonymous fns get a compiler-generated synthetic name.
  ///
  /// `fn_kind` distinguishes functions called with `Arg::Cont` (CpsFunction)
  /// from those that never receive a cont via `Arg::Cont` (CpsClosure).
  /// Set by the CPS transform at creation time. See `../wasm/calling-convention.md`.
  LetFn {
    name: BindNode,
    params: Vec<Param>,
    fn_kind: CpsFnKind,
    // TODO: rename to body
    fn_body: Box<Expr>,
    cont: Cont,
  },

  /// Call func with args; the last `Arg::Cont` is the result continuation.
  App {
    func: Callable,
    args: Vec<Arg>,
  },

  /// Branch on cond.
  If {
    cond: Box<Val>,
    // TODO: investigate whether then/else_ should be Cont (structurally same as App cont — "what comes next")
    then: Box<Expr>,
    else_: Box<Expr>,
  },

  // ---------------------------------------------------------------------------
  // Pattern matching — all patterns lower to PatternMatch (LetFn + App).
  // Type guards (IsSeqLike, IsRecLike) wrap matcher entries; collection primitives
  // (SeqPop, RecPop, Empty) are emitted inside matcher bodies.
  //
  // Matcher invariant: matchers work with synthetic temps only (Bind::Synth).
  // No named bindings are created inside a matcher — if a pattern fails,
  // nothing should be in scope. Temps are forwarded to succ on success;
  // the body's params give them user-visible names.
  // ---------------------------------------------------------------------------

}

// ---------------------------------------------------------------------------
// Bind kind collection — for the formatter's ref rendering
// ---------------------------------------------------------------------------

/// Walk the CPS tree and collect bind kinds into a prop graph.
/// Used by the formatter to render refs with semantic cont names.
pub fn collect_bind_kinds(expr: &Expr) -> crate::propgraph::PropGraph<CpsId, Option<Bind>> {
  let mut bk: crate::propgraph::PropGraph<CpsId, Option<Bind>> = crate::propgraph::PropGraph::new();
  collect_bk_expr(expr, &mut bk);
  bk
}

fn collect_bk_bind(bind: &BindNode, bk: &mut crate::propgraph::PropGraph<CpsId, Option<Bind>>) {
  let idx: usize = bind.id.into();
  while bk.len() <= idx { bk.push(None); }
  bk.set(bind.id, Some(bind.kind));
}

fn collect_bk_expr(expr: &Expr, bk: &mut crate::propgraph::PropGraph<CpsId, Option<Bind>>) {
  match &expr.kind {
    ExprKind::LetVal { name, cont, .. } => {
      collect_bk_bind(name, bk);
      collect_bk_cont(cont, bk);
    }
    ExprKind::LetFn { name, params, fn_body, cont, .. } => {
      collect_bk_bind(name, bk);
      for p in params {
        let b = match p { Param::Name(b) | Param::Spread(b) => b };
        collect_bk_bind(b, bk);
      }
      collect_bk_expr(fn_body, bk);
      collect_bk_cont(cont, bk);
    }
    ExprKind::App { args, .. } => {
      for arg in args {
        if let Arg::Cont(c) = arg { collect_bk_cont(c, bk); }
        if let Arg::Expr(e) = arg { collect_bk_expr(e, bk); }
      }
    }
    ExprKind::If { then, else_, .. } => {
      collect_bk_expr(then, bk);
      collect_bk_expr(else_, bk);
    }
  }
}

fn collect_bk_cont(cont: &Cont, bk: &mut crate::propgraph::PropGraph<CpsId, Option<Bind>>) {
  if let Cont::Expr { args, body } = cont {
    for a in args { collect_bk_bind(a, bk); }
    collect_bk_expr(body, bk);
  }
}

