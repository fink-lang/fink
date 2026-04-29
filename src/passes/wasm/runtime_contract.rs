//! The runtime ABI the emitter depends on.
//!
//! # Layers
//!
//! * **CPS** declares *what operations* a program uses — via
//!   `BuiltIn` variants (`Add`, `SeqPop`, `FinkModule`, ...) and
//!   literal types. CPS knows nothing about WASM or runtime names.
//! * **Emitter** knows the mapping from CPS operations to WASM-level
//!   runtime symbols. For each `BuiltIn`, it lists the runtime
//!   functions and types required to lower it. That mapping lives
//!   *here*.
//! * **Linker** must supply a runtime that exports every symbol the
//!   emitter imports. Different backends (hand-written WAT runtime
//!   today, WASI-adapter tomorrow, browser-adapter the day after)
//!   are alternative implementations of the same ABI.
//!
//! The ABI itself is the set of names in `@fink/runtime:*` — stable
//! across backends. Runtimes may forward those names to any
//! underlying mechanism they like, but must expose the same ABI.
//!
//! # Flow
//!
//! 1. **Prepass (`scan`)** — walk the CPS once to collect the set of
//!    `BuiltIn`s used, plus any symbol directly implied by the CPS
//!    shape (every module that reaches the bring-up path needs the
//!    bring-up helpers). Produces a [`RuntimeUsage`].
//! 2. **Declare (`declare`)** — emit WASM imports on the fragment
//!    for every symbol in usage (plus transitive type deps), in a
//!    canonical order. Returns a [`Runtime`] of typed handles.
//! 3. **Lower** — reads handles from `Runtime`; never touches the
//!    import list directly.
//!
//! Only symbols the program actually uses get imported.
//!
//! # Growing the ABI
//!
//! Adding a new CPS-level operation that needs runtime support:
//!
//! * Add the `Sym` variant (if the runtime function isn't already
//!   there).
//! * Add a handle field on [`Runtime`] and getter.
//! * Add an entry in `syms_for_builtin` mapping the `BuiltIn` to
//!   the required `Sym`s.
//! * Add the declaration arm in `declare`.
//! * The runtime (WAT source today) must export the new name with a
//!   compatible signature.
//!
//! # Separation from type inference
//!
//! `scan` is a usage-collecting pass that overlaps conceptually with
//! what a future type inferencer will compute. Keeping it as its
//! own phase means it can fold into type inference later without
//! entangling with lowering.

use std::collections::BTreeSet;

use crate::passes::cps::ir::{Arg, BuiltIn, Callable, Cont, CpsResult, Expr, ExprKind, Lit, Param, ParamInfo, ValKind};

use super::ir::*;

// ──────────────────────────────────────────────────────────────────────
// ABI — named runtime symbols the emitter can import
// ──────────────────────────────────────────────────────────────────────

/// A runtime-provided symbol the emitter might import. Variants are
/// in canonical *display* order: value types first, functions last.
///
/// **No function-signature types here.** Only value types (with
/// supertyping relationships that need shared identity across
/// fragments) cross the ABI as type imports. Function signatures
/// (e.g. the signature of `args_head`) are *local* types
/// declared by the emitter at fragment level — WASM structural
/// equivalence handles matching at link time.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Sym {
  // ── value types ────────────────────────────────────────────────
  Num,
  Fn2,
  Closure,
  Captures,
  VarArgs,
  // Function-signature type for the per-module host wrapper.
  // `(ref null any, i32) -> ()`. Declared in `rt/types.wat` so all
  // modules share one nominal type instead of emitting a local
  // copy per fragment.
  FnHostWrapper,

  // ── calling-convention primitives (std/list.wat today) ────────
  ArgsHead,
  ArgsTail,
  ArgsEmpty,
  ArgsPrepend,
  ArgsConcat,

  // ── application (rt/apply.wat) ────────────────────────────────
  Apply,

  // ── polymorphic protocol operators (rt/protocols.wat) ─────────
  // All binary operators share the shape (anyref, anyref, anyref)
  // → unit (i.e. Fn_op_binary); Not is the only unary today
  // (anyref, anyref) → unit.
  OpPlus, OpMinus, OpMul, OpDiv, OpIntDiv, OpRem, OpIntMod,
  OpEq, OpNeq, OpLt, OpLte, OpGt, OpGte, OpDisjoint,
  OpAnd, OpOr, OpXor, OpNot,
  OpShl, OpShr,
  OpRngex, OpRngin, OpIn, OpNotIn, OpDot,
  // Polymorphic predicate — same `(any, cont)` unary CPS shape as
  // OpNot. Used by `BuiltIn::Empty` and (future) other unary
  // predicates.
  OpEmpty,
  // Seq construction — `(item, seq, cont)` ternary CPS shape (same
  // as binary protocol ops). Used by `BuiltIn::SeqPrepend` for list
  // literals and pattern-match recursion.
  SeqPrepend,
  // Rec merge — `(dest, src, cont)`. Same ternary CPS shape. Used
  // for `{..r1, ..r2, k: v}` record spreads.
  RecMerge,
  // Type guards / destructuring — all share the `(any, any, any) -> ()`
  // shape with binary protocol ops; the second/third args are
  // continuations rather than values, but the WASM signature is the
  // same. RecPop has a different 4-arg shape; not yet wired.
  IsSeqLike, IsRecLike,
  SeqPop, SeqPopBack,
  // String construction. `Str` wraps a data-section pointer:
  //   `str(offset, len) -> $Str`. `StrEmpty` is a singleton constant.
  Str, StrEmpty,
  // Polymorphic string formatter — `(varargs, cont)` where varargs
  // is a `$VarArgs` array of segment values. Same shape as
  // `Fn_op_unary`. Used by `BuiltIn::StrFmt` for `'${...}'` templates.
  StrFmt,
  // Polymorphic string-template pattern matcher —
  // `(subj, prefix, suffix, fail, succ)`. 5-arg shape; used by
  // `BuiltIn::StrMatch` for `'foo${x}bar' = 'fooBARbar'` patterns.
  StrMatch,
  // Cooperative-multitasking primitives — all `(any, any) -> ()`,
  // same shape as Fn_op_unary.
  Yield, Spawn, Await,
  // Channels — same shape.
  Channel, Receive,
  // Rec primitives — 4-arg shape `(any, any, any, any) -> ()`. Used
  // for record construction (`rec_set`) and pattern destructure
  // (`rec_pop`).
  RecPut, RecPop,
  // Empty rec singleton — `() -> anyref`. Used for `{}`.
  RecEmpty,
  // Direct-style rec field setter — `(any, any, any) -> any`. Used by
  // the BuiltIn::Import handler to build the import rec at module-load
  // time without going through the CPS-style rec_set chain.
  RecSetField,
  // `panic` runtime function — `Fn2` shape. Used when `BuiltIn::Panic`
  // appears in value position (passed as fail continuation in
  // pattern-match dispatch). Wrapped in a no-capture `$Closure` at
  // the call site.
  Panic,
  // `std/modules.fnk:pub` — direct call, `(mod_url, name, val) -> ()`.
  // Emitted at every `·ƒpub` site to register the binding into the
  // module's exports rec in the runtime registry. No return value;
  // CPS continues inline after the call.
  ModulesPub,
  // `std/modules.fnk:import` — CPS, `(url, mod_clos, cont) -> ()`.
  // Emitted at every user-fragment `import 'url'` site. mod_clos is
  // a no-capture `$Closure` over the producer's `<url>:fink_module`
  // funcref, built inline at the call site so we don't need to
  // pass funcrefs through the anyref-typed call ABI (funcrefs and
  // anyrefs are disjoint hierarchies in WasmGC).
  ModulesImport,
  // `std/modules.fnk:init_module` — CPS, host-facing module init.
  // `(mod_url, mod_clos, key, cont) -> ()`. Idempotent run-then-
  // lookup. Tail-applies cont with `(last_expr, val)` where val is
  // the full exports rec or `rec[key]` if key is non-null. Emitted
  // by the per-module wrapper that lower synthesises for each
  // fragment; the wrapper is exported under the module's canonical
  // FQN so any host can call it as the unified module-init API.
  ModulesInitModule,
  // `interop/rust.wat:wrap_host_cont` — `(i32) -> anyref`. Wraps a
  // host-registered callback id into an opaque fink-side
  // continuation. Used by the per-module host wrapper to convert
  // the host's i32 cont_id into a fink anyref before tail-calling
  // `init_module`.
  WrapHostCont,
  // `std/str.wat:_str_wrap_bytes` — `(anyref) -> anyref`. Wraps a
  // GC `$ByteArray` into a fink `$Str`. Used by the per-module
  // host wrapper to convert the host's raw byte-array key into a
  // fink string before tail-calling `init_module`.
  StrWrapBytes,
}

// ──────────────────────────────────────────────────────────────────────
// Usage — the emitter's per-program requirement list
// ──────────────────────────────────────────────────────────────────────

/// Set of runtime symbols used by a program. Output of [`scan`],
/// input to [`declare`].
#[derive(Default, Debug)]
pub struct RuntimeUsage {
  used: BTreeSet<Sym>,
}

impl RuntimeUsage {
  pub fn mark(&mut self, sym: Sym) { self.used.insert(sym); }
  pub fn has(&self, sym: Sym) -> bool { self.used.contains(&sym) }
}

/// Which runtime symbols a given `BuiltIn` requires at lowering
/// time. Table-driven — the emitter consults this to translate CPS
/// operation usage into runtime ABI requirements.
///
/// Transitive type deps (a function's signature type) are handled
/// in [`declare`], so this only needs to list the *direct* symbol(s)
/// the BuiltIn lowers to.
///
/// Empty list = no runtime import needed (operation lowers purely
/// structurally; e.g. `FinkModule` is handled by the module-root
/// framing, not a BuiltIn call).
fn syms_for_builtin(b: BuiltIn) -> &'static [Sym] {
  // All polymorphic operators take `(any, any, any)` — operand types
  // don't appear in the operator import's signature. `Sym::Num` is
  // marked independently via `scan_val_kind` when a numeric literal
  // appears, so we don't need to co-mark it here.
  match b {
    // Arithmetic
    BuiltIn::Add    => &[Sym::OpPlus],
    BuiltIn::Sub    => &[Sym::OpMinus],
    BuiltIn::Mul    => &[Sym::OpMul],
    BuiltIn::Div    => &[Sym::OpDiv],
    BuiltIn::IntDiv => &[Sym::OpIntDiv],
    BuiltIn::Mod    => &[Sym::OpRem],
    BuiltIn::IntMod => &[Sym::OpIntMod],
    // Comparison
    BuiltIn::Eq  => &[Sym::OpEq],
    BuiltIn::Neq => &[Sym::OpNeq],
    BuiltIn::Lt  => &[Sym::OpLt],
    BuiltIn::Lte => &[Sym::OpLte],
    BuiltIn::Gt  => &[Sym::OpGt],
    BuiltIn::Gte => &[Sym::OpGte],
    BuiltIn::Disjoint => &[Sym::OpDisjoint],
    // Logic / bitwise (polymorphic — runtime dispatches on bool vs int)
    BuiltIn::And => &[Sym::OpAnd],
    BuiltIn::Or  => &[Sym::OpOr],
    BuiltIn::Xor => &[Sym::OpXor],
    BuiltIn::Not => &[Sym::OpNot],
    // Shifts
    BuiltIn::Shl => &[Sym::OpShl],
    BuiltIn::Shr => &[Sym::OpShr],
    // Range / membership / field-access (polymorphic)
    BuiltIn::Range     => &[Sym::OpRngex],
    BuiltIn::RangeIncl => &[Sym::OpRngin],
    BuiltIn::In        => &[Sym::OpIn],
    BuiltIn::NotIn     => &[Sym::OpNotIn],
    BuiltIn::Get       => &[Sym::OpDot],
    BuiltIn::Empty     => &[Sym::OpEmpty],
    BuiltIn::StrFmt    => &[Sym::StrFmt, Sym::VarArgs],
    BuiltIn::StrMatch  => &[Sym::StrMatch],
    BuiltIn::Yield     => &[Sym::Yield],
    BuiltIn::Spawn     => &[Sym::Spawn],
    BuiltIn::Await     => &[Sym::Await],
    BuiltIn::Channel   => &[Sym::Channel],
    BuiltIn::Receive   => &[Sym::Receive],
    BuiltIn::SeqPrepend => &[Sym::SeqPrepend],
    BuiltIn::RecMerge   => &[Sym::RecMerge],
    BuiltIn::IsSeqLike  => &[Sym::IsSeqLike],
    BuiltIn::IsRecLike  => &[Sym::IsRecLike],
    BuiltIn::SeqPop     => &[Sym::SeqPop],
    BuiltIn::SeqPopBack => &[Sym::SeqPopBack],
    BuiltIn::RecPut     => &[Sym::RecPut],
    BuiltIn::RecPop     => &[Sym::RecPop],
    // `·ƒpub name, val, cont` lowers inline to `global.set $<fqn>:<name>`
    // plus a direct call to `std/modules.fnk:pub (<fqn>, <name>, val)`
    // plus the cont continuation. The url + name strings are interned
    // at lowering time, so `Sym::Str` is needed to materialise them.
    BuiltIn::Pub        => &[Sym::ModulesPub, Sym::Str],
    // Import has two lowering paths:
    //
    //   - Virtual stdlib (`std/io.fnk` etc.): per-name `<url>:<name>`
    //     accessor calls + inline rec build. Needs `RecEmpty`,
    //     `RecSetField`, `Str`.
    //   - User fragment (`./foo.fnk`): tail-call `std/modules.fnk:import`
    //     with [url_str, mod_clos, cont]. The mod_clos is a no-capture
    //     `$Closure` over the producer's `<url>:fink_module`, built
    //     inline at the call site — needs `Closure` + `Captures` types.
    //
    // We mark the union since `scan` can't tell which path will fire
    // (the URL string is a Lit::Str arg, not a Sym). Unused imports
    // are harmless — the runtime side is small.
    BuiltIn::Import     => &[
      Sym::RecEmpty, Sym::RecSetField, Sym::Str,
      Sym::ModulesImport, Sym::Closure, Sym::Captures,
    ],
    // Closure construction needs both $Closure struct + $Captures array types.
    BuiltIn::FnClosure => &[Sym::Closure, Sym::Captures],
    // Not yet lowered — add mappings when lower gains coverage.
    BuiltIn::FinkModule => &[],
    _ => &[],
  }
}

// ──────────────────────────────────────────────────────────────────────
// Handles — typed accessors after declaration
// ──────────────────────────────────────────────────────────────────────

/// Handles to every declared runtime-contract symbol. Populated by
/// [`declare`]; read (not re-declared) by lowering. Signature types
/// are local types declared by `declare`, not imports — see [`Sym`]
/// for the distinction.
#[derive(Default)]
pub struct Runtime {
  // imported value types
  num: Option<TypeSym>,
  fn2: Option<TypeSym>,
  closure: Option<TypeSym>,
  captures: Option<TypeSym>,
  varargs: Option<TypeSym>,
  fn_host_wrapper: Option<TypeSym>,
  // Locally-declared function signature type used by the
  // virtual-stdlib import codegen path in lower (still allocates
  // `import_func` placeholders for source-named accessors). To be
  // removed when those accessors get promoted to first-class Sym
  // variants.
  fn_nil_to_list: Option<TypeSym>,  // () -> anyref
  // calling-convention funcs
  args_head:    Option<FuncSym>,
  args_tail:    Option<FuncSym>,
  args_empty:   Option<FuncSym>,
  args_prepend: Option<FuncSym>,
  args_concat:  Option<FuncSym>,
  apply:        Option<FuncSym>,
  // polymorphic protocol operators
  op_plus:    Option<FuncSym>,
  op_minus:   Option<FuncSym>,
  op_mul:     Option<FuncSym>,
  op_div:     Option<FuncSym>,
  op_intdiv:  Option<FuncSym>,
  op_rem:     Option<FuncSym>,
  op_intmod:  Option<FuncSym>,
  op_eq:      Option<FuncSym>,
  op_neq:     Option<FuncSym>,
  op_lt:      Option<FuncSym>,
  op_lte:     Option<FuncSym>,
  op_gt:      Option<FuncSym>,
  op_gte:     Option<FuncSym>,
  op_disjoint: Option<FuncSym>,
  op_and:     Option<FuncSym>,
  op_or:      Option<FuncSym>,
  op_xor:     Option<FuncSym>,
  op_not:     Option<FuncSym>,
  op_empty:   Option<FuncSym>,
  seq_prepend: Option<FuncSym>,
  rec_merge:   Option<FuncSym>,
  is_seq_like: Option<FuncSym>,
  is_rec_like: Option<FuncSym>,
  seq_pop:     Option<FuncSym>,
  seq_pop_back: Option<FuncSym>,
  // string constructors
  str_:         Option<FuncSym>,
  str_empty:    Option<FuncSym>,
  // 4-arg rec primitives.
  rec_put:      Option<FuncSym>,
  rec_pop:      Option<FuncSym>,
  rec_empty:    Option<FuncSym>,
  rec_set_field: Option<FuncSym>,
  panic:        Option<FuncSym>,
  str_fmt:      Option<FuncSym>,
  str_match:    Option<FuncSym>,
  // Scheduling — all `(any, any) -> ()` (Fn_op_unary shape).
  yield_:       Option<FuncSym>,
  spawn:        Option<FuncSym>,
  await_:       Option<FuncSym>,
  channel:      Option<FuncSym>,
  receive:      Option<FuncSym>,
  op_shl:     Option<FuncSym>,
  op_shr:     Option<FuncSym>,
  op_rngex:   Option<FuncSym>,
  op_rngin:   Option<FuncSym>,
  op_in:      Option<FuncSym>,
  op_notin:   Option<FuncSym>,
  op_dot:     Option<FuncSym>,
  // std/modules.fnk: protocol — direct-call primitives (no CPS cont).
  // `pub` shares the `Fn_op_binary` signature shape (3 anyref params,
  // no result).
  modules_pub:    Option<FuncSym>,
  modules_import: Option<FuncSym>,
  modules_init_module: Option<FuncSym>,
  wrap_host_cont:  Option<FuncSym>,
  str_wrap_bytes:  Option<FuncSym>,
}

impl Runtime {
  pub fn num(&self)          -> TypeSym { self.num.expect("rt: Num not declared") }
  pub fn fn2(&self)          -> TypeSym { self.fn2.expect("rt: Fn2 not declared") }
  pub fn closure(&self)      -> TypeSym { self.closure.expect("rt: Closure not declared") }
  pub fn captures(&self)     -> TypeSym { self.captures.expect("rt: Captures not declared") }
  pub fn varargs(&self)      -> TypeSym { self.varargs.expect("rt: VarArgs not declared") }
  pub fn fn_host_wrapper(&self) -> TypeSym { self.fn_host_wrapper.expect("rt: Fn_host_wrapper not declared") }
  pub fn args_head(&self)    -> FuncSym { self.args_head.expect("rt: args_head not declared") }
  pub fn args_tail(&self)    -> FuncSym { self.args_tail.expect("rt: args_tail not declared") }
  pub fn args_empty(&self)   -> FuncSym { self.args_empty.expect("rt: args_empty not declared") }
  pub fn args_prepend(&self) -> FuncSym { self.args_prepend.expect("rt: args_prepend not declared") }
  pub fn args_concat(&self)  -> FuncSym { self.args_concat.expect("rt: args_concat not declared") }
  pub fn str_(&self)         -> FuncSym { self.str_.expect("rt: str not declared") }
  pub fn str_empty(&self)    -> FuncSym { self.str_empty.expect("rt: str_empty not declared") }
  pub fn rec_put(&self)      -> FuncSym { self.rec_put.expect("rt: rec_put not declared") }
  pub fn modules_pub(&self)    -> FuncSym { self.modules_pub.expect("rt: modules_pub not declared") }
  pub fn modules_import(&self) -> FuncSym { self.modules_import.expect("rt: modules_import not declared") }
  pub fn modules_init_module(&self) -> FuncSym { self.modules_init_module.expect("rt: modules_init_module not declared") }
  pub fn wrap_host_cont(&self)       -> FuncSym { self.wrap_host_cont.expect("rt: wrap_host_cont not declared") }
  pub fn str_wrap_bytes(&self)       -> FuncSym { self.str_wrap_bytes.expect("rt: str_wrap_bytes not declared") }
  pub fn rec_pop(&self)      -> FuncSym { self.rec_pop.expect("rt: rec_pop not declared") }
  pub fn rec_empty(&self)    -> FuncSym { self.rec_empty.expect("rt: rec_empty not declared") }
  pub fn rec_set_field(&self) -> FuncSym { self.rec_set_field.expect("rt: rec_set_field not declared") }
  /// `() -> anyref` signature type. Shared by `args_empty`, `rec_new`,
  /// and the BuiltIn::Import virtual-stdlib accessors.
  pub fn fn_nil_to_list_sig(&self) -> TypeSym {
    self.fn_nil_to_list.expect("rt: fn_nil_to_list sig not declared")
  }
  pub fn panic(&self)        -> FuncSym { self.panic.expect("rt: panic not declared") }
  pub fn str_fmt(&self)      -> FuncSym { self.str_fmt.expect("rt: str_fmt not declared") }
  pub fn str_match(&self)    -> FuncSym { self.str_match.expect("rt: str_match not declared") }
  pub fn yield_(&self)       -> FuncSym { self.yield_.expect("rt: yield not declared") }
  pub fn spawn(&self)        -> FuncSym { self.spawn.expect("rt: spawn not declared") }
  pub fn await_(&self)       -> FuncSym { self.await_.expect("rt: await not declared") }
  pub fn channel(&self)      -> FuncSym { self.channel.expect("rt: channel not declared") }
  pub fn receive(&self)      -> FuncSym { self.receive.expect("rt: receive not declared") }
  pub fn apply(&self)        -> FuncSym { self.apply.expect("rt: _apply not declared") }

  /// Look up the runtime func for a protocol operator `Sym`. Panics
  /// if the Sym wasn't declared — lowering should scan → declare
  /// every Sym it plans to read.
  pub fn op(&self, sym: Sym) -> FuncSym {
    let f = match sym {
      Sym::OpPlus   => self.op_plus,
      Sym::OpMinus  => self.op_minus,
      Sym::OpMul    => self.op_mul,
      Sym::OpDiv    => self.op_div,
      Sym::OpIntDiv => self.op_intdiv,
      Sym::OpRem    => self.op_rem,
      Sym::OpIntMod => self.op_intmod,
      Sym::OpEq     => self.op_eq,
      Sym::OpNeq    => self.op_neq,
      Sym::OpLt     => self.op_lt,
      Sym::OpLte    => self.op_lte,
      Sym::OpGt     => self.op_gt,
      Sym::OpGte    => self.op_gte,
      Sym::OpDisjoint => self.op_disjoint,
      Sym::OpAnd    => self.op_and,
      Sym::OpOr     => self.op_or,
      Sym::OpXor    => self.op_xor,
      Sym::OpNot    => self.op_not,
      Sym::OpEmpty  => self.op_empty,
      Sym::SeqPrepend => self.seq_prepend,
      Sym::RecMerge   => self.rec_merge,
      Sym::IsSeqLike  => self.is_seq_like,
      Sym::IsRecLike  => self.is_rec_like,
      Sym::SeqPop     => self.seq_pop,
      Sym::SeqPopBack => self.seq_pop_back,
      Sym::OpShl    => self.op_shl,
      Sym::OpShr    => self.op_shr,
      Sym::OpRngex  => self.op_rngex,
      Sym::OpRngin  => self.op_rngin,
      Sym::OpIn     => self.op_in,
      Sym::OpNotIn  => self.op_notin,
      Sym::OpDot    => self.op_dot,
      Sym::StrFmt   => self.str_fmt,
      Sym::Yield    => self.yield_,
      Sym::Spawn    => self.spawn,
      Sym::Await    => self.await_,
      Sym::Channel  => self.channel,
      Sym::Receive  => self.receive,
      _ => panic!("rt.op: {:?} is not a protocol-operator Sym", sym),
    };
    f.unwrap_or_else(|| panic!("rt: {:?} not declared", sym))
  }
}

// ──────────────────────────────────────────────────────────────────────
// Declare
// ──────────────────────────────────────────────────────────────────────

/// Per-`Sym` fragment URL + export name.
///
/// The emitter emits `(import "<url>" "<name>" ...)` — after merge
/// (via build.rs textual splice today, link tomorrow), the runtime
/// bundle exports every referenced name qualified as `<url>:<name>`
/// in its export table. `emit` composes the same string to look up
/// the concrete function/type index in `runtime-ir.wasm` and rewrite
/// the user fragment's call sites.
///
/// Reserved roots:
/// * `rt/*`   — compiler-level ABI. Not user-importable.
/// * `std/*`  — user-facing stdlib. Built on top of `rt`.
/// * `interop/*` — host bridge. Target-selected at link time.
/// * `./*`    — user's relative imports.
/// * `https://...`, `reg:*` — future third-party packages.
pub(super) fn import_key(sym: Sym) -> (&'static str, &'static str) {
  match sym {
    Sym::Num             => ("rt/types.wat",     "Num"),
    Sym::Fn2             => ("rt/types.wat",     "Fn2"),
    Sym::Closure         => ("rt/types.wat",     "Closure"),
    Sym::Captures        => ("rt/types.wat",     "Captures"),
    Sym::VarArgs         => ("rt/types.wat",     "VarArgs"),
    Sym::FnHostWrapper   => ("rt/types.wat",     "Fn_host_wrapper"),
    Sym::Apply           => ("rt/apply.wat",     "rt/apply.wat:apply"),
    Sym::ArgsHead        => ("std/list.wat", "std/fn.fnk:args_head"),
    Sym::ArgsTail        => ("std/list.wat", "std/fn.fnk:args_tail"),
    Sym::ArgsEmpty       => ("std/list.wat", "std/fn.fnk:args_empty"),
    Sym::ArgsPrepend     => ("std/list.wat", "std/fn.fnk:args_prepend"),
    Sym::ArgsConcat      => ("std/list.wat", "std/fn.fnk:args_concat"),
    Sym::OpPlus          => ("rt/protocols.wat", "std/operators.fnk:op_plus"),
    Sym::OpMinus         => ("rt/protocols.wat", "std/operators.fnk:op_minus"),
    Sym::OpMul           => ("rt/protocols.wat", "std/operators.fnk:op_mul"),
    Sym::OpDiv           => ("rt/protocols.wat", "std/operators.fnk:op_div"),
    Sym::OpIntDiv        => ("rt/protocols.wat", "std/operators.fnk:op_intdiv"),
    Sym::OpRem           => ("rt/protocols.wat", "std/operators.fnk:op_rem"),
    Sym::OpIntMod        => ("rt/protocols.wat", "std/operators.fnk:op_intmod"),
    Sym::OpEq            => ("rt/protocols.wat", "std/operators.fnk:op_eq"),
    Sym::OpNeq           => ("rt/protocols.wat", "std/operators.fnk:op_neq"),
    Sym::OpLt            => ("rt/protocols.wat", "std/operators.fnk:op_lt"),
    Sym::OpLte           => ("rt/protocols.wat", "std/operators.fnk:op_lte"),
    Sym::OpGt            => ("rt/protocols.wat", "std/operators.fnk:op_gt"),
    Sym::OpGte           => ("rt/protocols.wat", "std/operators.fnk:op_gte"),
    Sym::OpDisjoint      => ("rt/protocols.wat", "std/operators.fnk:op_disjoint"),
    Sym::OpAnd           => ("rt/protocols.wat", "std/operators.fnk:op_and"),
    Sym::OpOr            => ("rt/protocols.wat", "std/operators.fnk:op_or"),
    Sym::OpXor           => ("rt/protocols.wat", "std/operators.fnk:op_xor"),
    Sym::OpNot           => ("rt/protocols.wat", "std/operators.fnk:op_not"),
    Sym::OpShl           => ("rt/protocols.wat", "std/operators.fnk:op_shl"),
    Sym::OpShr           => ("rt/protocols.wat", "std/operators.fnk:op_shr"),
    Sym::OpRngex         => ("std/range.wat", "std/range.fnk:excl"),
    Sym::OpRngin         => ("std/range.wat", "std/range.fnk:incl"),
    Sym::OpIn            => ("rt/protocols.wat", "std/operators.fnk:op_in"),
    Sym::OpNotIn         => ("rt/protocols.wat", "std/operators.fnk:op_notin"),
    Sym::OpDot           => ("rt/protocols.wat", "std/operators.fnk:op_dot"),
    Sym::OpEmpty         => ("rt/protocols.wat", "std/operators.fnk:op_empty"),
    Sym::SeqPrepend      => ("std/list.wat", "std/seq.fnk:prepend"),
    Sym::RecMerge        => ("std/dict.wat", "std/rec.fnk:merge"),
    Sym::IsSeqLike       => ("rt/protocols.wat", "std/operators.fnk:is_seq_like"),
    Sym::IsRecLike       => ("rt/protocols.wat", "std/operators.fnk:is_rec_like"),
    Sym::SeqPop          => ("rt/protocols.wat", "std/seq.fnk:pop"),
    Sym::SeqPopBack      => ("rt/protocols.wat", "std/seq.fnk:pop_back"),
    Sym::Str             => ("std/str.wat", "std/str.fnk:str"),
    Sym::StrEmpty        => ("std/str.wat", "std/str.fnk:str_empty"),
    Sym::StrFmt          => ("std/str.wat", "std/str.fnk:fmt"),
    Sym::StrMatch        => ("std/str.wat", "std/str.fnk:match"),
    Sym::Yield           => ("std/async.wat", "std/async.fnk:yield"),
    Sym::Spawn           => ("std/async.wat", "std/async.fnk:spawn"),
    Sym::Await           => ("std/async.wat", "std/async.fnk:await"),
    Sym::Channel         => ("std/channel.wat", "std/channels.fnk:channel"),
    Sym::Receive         => ("rt/protocols.wat", "std/channels.fnk:receive"),
    Sym::RecPut          => ("std/dict.wat", "std/rec.fnk:put"),
    Sym::RecPop          => ("std/dict.wat", "std/rec.fnk:pop"),
    Sym::RecEmpty        => ("std/dict.wat", "std/rec.fnk:new"),
    Sym::RecSetField     => ("std/dict.wat", "std/rec.fnk:_set_field"),
    Sym::Panic           => ("rt/protocols.wat", "std/interop.fnk:panic"),
    Sym::ModulesPub        => ("rt/modules.wat",   "std/modules.fnk:pub"),
    Sym::ModulesImport     => ("rt/modules.wat",   "std/modules.fnk:import"),
    Sym::ModulesInitModule => ("rt/modules.wat",   "std/modules.fnk:init_module"),
    Sym::WrapHostCont      => ("interop/rust.wat", "wrap_host_cont"),
    Sym::StrWrapBytes      => ("std/str.wat",      "std/str.wat:_str_wrap_bytes"),
  }
}

/// Declare every symbol in `usage` as an import on `frag`, in the
/// canonical ordering given by `Sym`'s variant order. Pulls in any
/// transitively-required types (e.g. a function's signature type is
/// imported even if the program doesn't mention it directly).
pub fn declare(frag: &mut Fragment, usage: &RuntimeUsage) -> Runtime {
  let mut rt = Runtime::default();
  let needed = &usage.used;

  // Value-type imports — `rt/types.wat:Num` / `rt/types.wat:Fn2`. Shared
  // identity across the ABI: user struct.new instances must match
  // runtime's concrete type indices. Emit resolves them against
  // `types-ir.wasm` at emit time.
  //
  // `Sym::Num` is marked in `scan_val_kind` whenever a numeric
  // literal appears (which is the only place we construct $Num
  // values via `struct.new`). Operators take anyref operands — they
  // don't need `$Num` unless there's a numeric literal in scope.
  // Runtime value types — referenced via `TypeSym::Runtime(sym)`,
  // resolved by emit at byte time. No fragment-level placeholder
  // allocation; the Sym variant is the reference.
  if needed.contains(&Sym::Num) {
    rt.num = Some(TypeSym::Runtime(Sym::Num));
  }
  if needed.contains(&Sym::Fn2) || needed.contains(&Sym::Apply) || always_need_fn2(usage) {
    rt.fn2 = Some(TypeSym::Runtime(Sym::Fn2));
  }
  if needed.contains(&Sym::Captures) {
    rt.captures = Some(TypeSym::Runtime(Sym::Captures));
  }
  if needed.contains(&Sym::Closure) {
    rt.closure = Some(TypeSym::Runtime(Sym::Closure));
  }
  if needed.contains(&Sym::VarArgs) {
    rt.varargs = Some(TypeSym::Runtime(Sym::VarArgs));
  }
  if needed.contains(&Sym::FnHostWrapper) {
    rt.fn_host_wrapper = Some(TypeSym::Runtime(Sym::FnHostWrapper));
  }

  // Function-signature types and function imports.
  //
  // CPS/lower is the owner of the runtime ABI — the emitter
  // dictates the function signatures, the runtime WAT implements
  // them. That means signatures are **definitional** at this layer,
  // not derived from anywhere external. Each signature is declared
  // as a local type in the fragment with the `<url>:Fn_` prefix
  // (so the ABI boundary is visible in the rendered WAT) and used
  // to type each function import. WASM validates by structural
  // equivalence at link time, so the user fragment's locally-
  // declared `$std/list.wat:Fn_head_any` matches the runtime's own
  // signature of `head_any` by shape.
  let anyref_n = val_anyref(true);

  // Runtime functions — referenced via `FuncSym::Runtime(sym)`. No
  // local function-signature type declaration needed: emit resolves
  // each Sym to its merged-binary func index, which already carries
  // the correct signature in the runtime's type section.
  //
  // The sole remaining local-sig allocation is `fn_nil_to_list`,
  // consumed by the virtual-stdlib `import` codegen path in lower —
  // that path still uses the placeholder mechanism for accessor
  // functions whose names come from source. To be removed when those
  // accessors get promoted to first-class `Sym` variants.
  if needed.contains(&Sym::ArgsHead)    { rt.args_head    = Some(FuncSym::Runtime(Sym::ArgsHead)); }
  if needed.contains(&Sym::ArgsTail)    { rt.args_tail    = Some(FuncSym::Runtime(Sym::ArgsTail)); }
  if needed.contains(&Sym::ArgsEmpty)   { rt.args_empty   = Some(FuncSym::Runtime(Sym::ArgsEmpty)); }
  if needed.contains(&Sym::ArgsPrepend) { rt.args_prepend = Some(FuncSym::Runtime(Sym::ArgsPrepend)); }
  if needed.contains(&Sym::ArgsConcat)  { rt.args_concat  = Some(FuncSym::Runtime(Sym::ArgsConcat)); }
  if needed.contains(&Sym::Apply)       { rt.apply        = Some(FuncSym::Runtime(Sym::Apply)); }

  for sym in BINARY_OPS {
    if needed.contains(sym) {
      set_op(&mut rt, *sym, FuncSym::Runtime(*sym));
    }
  }
  for sym in UNARY_OPS {
    if needed.contains(sym) {
      set_op(&mut rt, *sym, FuncSym::Runtime(*sym));
    }
  }
  for sym in TERNARY_PRIMITIVES {
    if needed.contains(sym) {
      set_ternary_primitive(&mut rt, *sym, FuncSym::Runtime(*sym));
    }
  }

  if needed.contains(&Sym::Str)      { rt.str_      = Some(FuncSym::Runtime(Sym::Str)); }
  if needed.contains(&Sym::StrEmpty) { rt.str_empty = Some(FuncSym::Runtime(Sym::StrEmpty)); }
  if needed.contains(&Sym::StrFmt)   { rt.str_fmt   = Some(FuncSym::Runtime(Sym::StrFmt)); }
  if needed.contains(&Sym::StrMatch) { rt.str_match = Some(FuncSym::Runtime(Sym::StrMatch)); }

  for sym in SCHED_PRIMITIVES {
    if needed.contains(sym) {
      set_sched_primitive(&mut rt, *sym, FuncSym::Runtime(*sym));
    }
  }

  if needed.contains(&Sym::RecPut)  { rt.rec_put = Some(FuncSym::Runtime(Sym::RecPut)); }
  if needed.contains(&Sym::RecPop)  { rt.rec_pop = Some(FuncSym::Runtime(Sym::RecPop)); }
  if needed.contains(&Sym::Panic)   { rt.panic   = Some(FuncSym::Runtime(Sym::Panic)); }

  if needed.contains(&Sym::RecEmpty) {
    // `rec_new : () -> anyref` — virtual-stdlib `import` codegen
    // path needs this signature in the user fragment to type the
    // accessor placeholder funcs (lower.rs `import_func` site).
    if rt.fn_nil_to_list.is_none() {
      let s = ty_func(frag,
        vec![],
        vec![anyref_n.clone()],
        "std/dict.wat:Fn_rec_new");
      rt.fn_nil_to_list = Some(s);
    }
    rt.rec_empty = Some(FuncSym::Runtime(Sym::RecEmpty));
  }

  if needed.contains(&Sym::RecSetField) { rt.rec_set_field  = Some(FuncSym::Runtime(Sym::RecSetField)); }
  if needed.contains(&Sym::ModulesPub)  { rt.modules_pub    = Some(FuncSym::Runtime(Sym::ModulesPub)); }
  if needed.contains(&Sym::ModulesInitModule) { rt.modules_init_module = Some(FuncSym::Runtime(Sym::ModulesInitModule)); }
  if needed.contains(&Sym::WrapHostCont)       { rt.wrap_host_cont      = Some(FuncSym::Runtime(Sym::WrapHostCont)); }
  if needed.contains(&Sym::StrWrapBytes)       { rt.str_wrap_bytes      = Some(FuncSym::Runtime(Sym::StrWrapBytes)); }
  if needed.contains(&Sym::ModulesImport) {
    rt.modules_import = Some(FuncSym::Runtime(Sym::ModulesImport));
    // Virtual-stdlib `import` codegen path in lower allocates
    // `import_func` placeholders for accessor functions (their names
    // come from source, not a Sym variant), so the user fragment
    // needs the `() -> anyref` signature locally to type them.
    // Remove when accessors get promoted to first-class Sym variants.
    if rt.fn_nil_to_list.is_none() {
      let s = ty_func(frag,
        vec![],
        vec![anyref_n.clone()],
        "std/dict.wat:Fn_rec_new");
      rt.fn_nil_to_list = Some(s);
    }
  }

  rt
}

/// All binary-protocol Syms — share `Fn_op_binary` signature.
const BINARY_OPS: &[Sym] = &[
  Sym::OpPlus, Sym::OpMinus, Sym::OpMul, Sym::OpDiv, Sym::OpIntDiv, Sym::OpRem, Sym::OpIntMod,
  Sym::OpEq, Sym::OpNeq, Sym::OpLt, Sym::OpLte, Sym::OpGt, Sym::OpGte, Sym::OpDisjoint,
  Sym::OpAnd, Sym::OpOr, Sym::OpXor,
  Sym::OpShl, Sym::OpShr,
  Sym::OpRngex, Sym::OpRngin, Sym::OpIn, Sym::OpNotIn, Sym::OpDot,
];

/// All unary-protocol Syms — share `Fn_op_unary` signature.
const UNARY_OPS: &[Sym] = &[Sym::OpNot, Sym::OpEmpty];

/// Scheduling / channel primitives — all `(any, any) -> ()` shape,
/// same as `Fn_op_unary`. Distinct from operator unaries because they
/// have side effects (queue manipulation, host channel I/O) rather
/// than pure dispatch.
const SCHED_PRIMITIVES: &[Sym] = &[
  Sym::Yield, Sym::Spawn, Sym::Await,
  Sym::Channel, Sym::Receive,
];

fn set_sched_primitive(rt: &mut Runtime, sym: Sym, f: FuncSym) {
  let slot = match sym {
    Sym::Yield   => &mut rt.yield_,
    Sym::Spawn   => &mut rt.spawn,
    Sym::Await   => &mut rt.await_,
    Sym::Channel => &mut rt.channel,
    Sym::Receive => &mut rt.receive,
    _ => panic!("set_sched_primitive: {:?} is not a sched primitive", sym),
  };
  *slot = Some(f);
}

/// Seq/rec primitives that share the `Fn_op_binary` signature shape
/// (`(any, any, any) -> ()`). Each is a 3-arg CPS function.
const TERNARY_PRIMITIVES: &[Sym] = &[
  Sym::SeqPrepend,
  Sym::RecMerge,
  Sym::IsSeqLike, Sym::IsRecLike,
  Sym::SeqPop, Sym::SeqPopBack,
];

fn set_ternary_primitive(rt: &mut Runtime, sym: Sym, f: FuncSym) {
  let slot = match sym {
    Sym::SeqPrepend => &mut rt.seq_prepend,
    Sym::RecMerge   => &mut rt.rec_merge,
    Sym::IsSeqLike  => &mut rt.is_seq_like,
    Sym::IsRecLike  => &mut rt.is_rec_like,
    Sym::SeqPop     => &mut rt.seq_pop,
    Sym::SeqPopBack => &mut rt.seq_pop_back,
    _ => panic!("set_ternary_primitive: {:?} is not a ternary primitive", sym),
  };
  *slot = Some(f);
}

/// Store the handle for a declared binary-protocol Sym back into the
/// Runtime's typed slot. Mirrors the enum spread in `Runtime::op`.
fn set_op(rt: &mut Runtime, sym: Sym, f: FuncSym) {
  let slot = match sym {
    Sym::OpPlus   => &mut rt.op_plus,
    Sym::OpMinus  => &mut rt.op_minus,
    Sym::OpMul    => &mut rt.op_mul,
    Sym::OpDiv    => &mut rt.op_div,
    Sym::OpIntDiv => &mut rt.op_intdiv,
    Sym::OpRem    => &mut rt.op_rem,
    Sym::OpIntMod => &mut rt.op_intmod,
    Sym::OpEq     => &mut rt.op_eq,
    Sym::OpNeq    => &mut rt.op_neq,
    Sym::OpLt     => &mut rt.op_lt,
    Sym::OpLte    => &mut rt.op_lte,
    Sym::OpGt     => &mut rt.op_gt,
    Sym::OpGte    => &mut rt.op_gte,
    Sym::OpDisjoint => &mut rt.op_disjoint,
    Sym::OpAnd    => &mut rt.op_and,
    Sym::OpOr     => &mut rt.op_or,
    Sym::OpXor    => &mut rt.op_xor,
    Sym::OpShl    => &mut rt.op_shl,
    Sym::OpShr    => &mut rt.op_shr,
    Sym::OpRngex  => &mut rt.op_rngex,
    Sym::OpRngin  => &mut rt.op_rngin,
    Sym::OpIn     => &mut rt.op_in,
    Sym::OpNotIn  => &mut rt.op_notin,
    Sym::OpDot    => &mut rt.op_dot,
    Sym::OpNot    => &mut rt.op_not,
    Sym::OpEmpty  => &mut rt.op_empty,
    _ => panic!("set_op: {:?} is not a protocol Sym", sym),
  };
  *slot = Some(f);
}

/// Fn2 is required by every fink_module definition. Without a
/// dedicated marker we always declare it when the scan added any
/// bring-up helpers.
fn always_need_fn2(usage: &RuntimeUsage) -> bool {
  usage.has(Sym::ArgsHead) || usage.has(Sym::Apply)
}

// ──────────────────────────────────────────────────────────────────────
// Scan — prepass
// ──────────────────────────────────────────────────────────────────────

/// Scan the lifted CPS for every runtime symbol the emitter will
/// need. Called once before lowering.
///
/// The logic has two parts:
/// 1. **Structural requirements.** Any program that reaches the
///    fink-module bring-up path uses `Apply`, `Fn2`, and the list
///    helpers. These are unconditional today; revisit when lowering
///    grows to handle fragments that don't emit `fink_module`.
/// 2. **BuiltIn-driven requirements.** For each `Callable::BuiltIn`
///    encountered, consult `syms_for_builtin` and mark those symbols.
/// 3. **Literal-driven requirements.** Numeric literals mark `Num`.
pub fn scan(cps: &CpsResult) -> RuntimeUsage {
  let mut usage = RuntimeUsage::default();

  // Every well-formed module is a `Fn2`-shaped `fink_module`, so
  // that type is always needed. `args_head` is always needed
  // because bring-up always pops `done` out of `_args`.
  usage.mark(Sym::Fn2);
  usage.mark(Sym::ArgsHead);

  // Bring-up further uses `_apply` (with `args_empty` + `args_prepend`)
  // only when the tail call is a user value or continuation — i.e.
  // when the fink_module body's tail is `App(ContRef(_), ...)`. A
  // direct-style tail call into a builtin (e.g. `op_plus`) skips the
  // apply mechanism and doesn't need those symbols. Introspect the
  // body to decide.
  if tail_uses_apply(&cps.root) {
    usage.mark(Sym::ArgsEmpty);
    usage.mark(Sym::ArgsPrepend);
    usage.mark(Sym::Apply);
  }

  scan_expr(&cps.root, cps, &mut usage);
  usage
}

/// True if any function body in the program — module body or any
/// lifted fn body — has a tail App that lowers via the `_apply`
/// path (`Callable::Val`) rather than a direct builtin call.
///
/// We scan every `LetFn`'s `fn_body` plus the module-root body. Each
/// fn_body's tail is the relevant signal — emit will lower at most
/// one tail per fn, but apply usage in any one of them requires the
/// full bring-up symbol set.
fn tail_uses_apply(root: &Expr) -> bool {
  // Find the fink_module body — `App(FinkModule, [Cont::Expr { body }])`.
  let ExprKind::App { func: Callable::BuiltIn(BuiltIn::FinkModule), args } = &root.kind else {
    return true; // unknown shape — assume apply path
  };
  let Some(Arg::Cont(Cont::Expr { body, .. })) = args.first() else {
    return true;
  };
  any_fn_uses_apply(body)
}

/// Recursive: returns true if `body`'s own tail is apply-path, or if
/// any nested LetFn's `fn_body` does.
fn any_fn_uses_apply(body: &Expr) -> bool {
  if tail_is_apply_path(body) { return true; }
  walk_fn_bodies(body, &mut |fb| tail_is_apply_path(fb))
}

/// Walk every nested LetFn's `fn_body` (transitively) and return true
/// if `pred` returns true for any of them.
fn walk_fn_bodies(expr: &Expr, pred: &mut dyn FnMut(&Expr) -> bool) -> bool {
  match &expr.kind {
    ExprKind::LetVal { cont, .. } => walk_cont_fns(cont, pred),
    ExprKind::LetFn { fn_body, cont, .. } => {
      if pred(fn_body) { return true; }
      if walk_fn_bodies(fn_body, pred) { return true; }
      walk_cont_fns(cont, pred)
    }
    ExprKind::App { args, .. } => {
      args.iter().any(|a| match a {
        Arg::Cont(c) => walk_cont_fns(c, pred),
        Arg::Expr(e) => walk_fn_bodies(e, pred),
        _ => false,
      })
    }
    ExprKind::If { then, else_, .. } => {
      walk_fn_bodies(then, pred) || walk_fn_bodies(else_, pred)
    }
  }
}

fn walk_cont_fns(cont: &Cont, pred: &mut dyn FnMut(&Expr) -> bool) -> bool {
  if let Cont::Expr { body, .. } = cont {
    walk_fn_bodies(body, pred)
  } else { false }
}

fn tail_is_apply_path(expr: &Expr) -> bool {
  match &expr.kind {
    // Value bindings / function definitions are transparent — recurse
    // into the cont body to find the tail.
    ExprKind::LetVal { cont, .. } | ExprKind::LetFn { cont, .. } => {
      match cont {
        Cont::Expr { body, .. } => tail_is_apply_path(body),
        Cont::Ref(_) => true, // tail is _apply(args, cont_ref)
      }
    }
    // Pub is a no-op wrapper — recurse into its cont body.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::Pub), args } => {
      if let Some(Arg::Cont(Cont::Expr { body, .. })) = args.get(1) {
        tail_is_apply_path(body)
      } else {
        true
      }
    }
    // FnClosure: the cont can be `Cont::Ref` (tail-apply) or
    // `Cont::Expr` (continue inline). For Cont::Ref the lowered code
    // emits `_apply([new_clo], cont)`, which needs the apply-path
    // bring-up. For Cont::Expr, recurse into the body.
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::FnClosure), args } => {
      match args.last() {
        Some(Arg::Cont(Cont::Expr { body, .. })) => tail_is_apply_path(body),
        Some(Arg::Cont(Cont::Ref(_)))            => true,
        _ => false,
      }
    }
    // Direct tail: App where callee is a Val (user cont) → apply path.
    // App with BuiltIn (op_plus etc.) → direct, no apply needed.
    ExprKind::App { func: Callable::Val(_), .. } => true,
    ExprKind::App { func: Callable::BuiltIn(_), .. } => false,
    ExprKind::If { .. } => true, // conservative — apply on at least one branch
  }
}

fn scan_expr(expr: &Expr, cps: &CpsResult, usage: &mut RuntimeUsage) {
  match &expr.kind {
    ExprKind::LetVal { val, cont, .. } => {
      scan_val_kind(&val.kind, usage);
      scan_cont(cont, cps, usage);
    }
    ExprKind::LetFn { params, fn_body, cont, .. } => {
      // Mark `args_tail` if the lifted fn's prologue will need to peel
      // more than one entry from $:params, OR if a spread is present
      // (where preceding params still need to advance the cursor before
      // the spread captures the remaining tail). User-param-shaped
      // entries here are anything that's NOT a `Cap` — `Param`, `Cont`,
      // and ungilded params all unpack from $:params. See
      // `lower::lower_fn` for the matching split.
      let mut unpack_count = 0usize;
      let mut has_spread = false;
      for p in params {
        let (pid, is_spread) = match p {
          Param::Name(b)   => (b.id, false),
          Param::Spread(b) => (b.id, true),
        };
        let info = cps.param_info.try_get(pid).and_then(|o| *o);
        let is_cap = matches!(info, Some(ParamInfo::Cap(_)));
        if !is_cap {
          unpack_count += 1;
          if is_spread { has_spread = true; }
        }
      }
      if unpack_count > 1 || has_spread {
        usage.mark(Sym::ArgsTail);
      }
      // Any LetFn in the program means at least one $Closure value will
      // exist at runtime (either via `App(FnClosure)` or via a no-capture
      // `Ref→Closure` materialisation in `emit_val_into`). Mark both the
      // type imports here so lower never finds them missing.
      usage.mark(Sym::Closure);
      usage.mark(Sym::Captures);
      scan_expr(fn_body, cps, usage);
      scan_cont(cont, cps, usage);
    }
    ExprKind::App { func, args } => {
      match func {
        Callable::Val(v) => scan_val_kind(&v.kind, usage),
        Callable::BuiltIn(b) => {
          for &sym in syms_for_builtin(*b) { usage.mark(sym); }
        }
      }
      for a in args { scan_arg(a, cps, usage); }
    }
    ExprKind::If { cond, then, else_ } => {
      scan_val_kind(&cond.kind, usage);
      scan_expr(then, cps, usage);
      scan_expr(else_, cps, usage);
    }
  }
}

fn scan_cont(cont: &Cont, cps: &CpsResult, usage: &mut RuntimeUsage) {
  if let Cont::Expr { body, .. } = cont {
    scan_expr(body, cps, usage);
  }
}

fn scan_arg(arg: &Arg, cps: &CpsResult, usage: &mut RuntimeUsage) {
  match arg {
    Arg::Val(v) => scan_val_kind(&v.kind, usage),
    Arg::Spread(v) => {
      // Spread args at call sites are `..rest` — lower via
      // `args_concat` instead of `args_prepend`.
      usage.mark(Sym::ArgsConcat);
      scan_val_kind(&v.kind, usage);
    }
    Arg::Cont(c) => scan_cont(c, cps, usage),
    Arg::Expr(e) => scan_expr(e, cps, usage),
  }
}

fn scan_val_kind(kind: &ValKind, usage: &mut RuntimeUsage) {
  match kind {
    ValKind::Lit(Lit::Int(_) | Lit::Float(_) | Lit::Decimal(_)) => {
      usage.mark(Sym::Num);
    }
    ValKind::Lit(Lit::Seq) => {
      // Empty seq `[]` reuses `args_empty` from std/list.wat (exported
      // under both `args_empty` and `list_nil`).
      usage.mark(Sym::ArgsEmpty);
    }
    ValKind::Lit(Lit::Str(s)) => {
      if s.is_empty() { usage.mark(Sym::StrEmpty); }
      else            { usage.mark(Sym::Str); }
    }
    ValKind::Lit(Lit::Rec) => {
      usage.mark(Sym::RecEmpty);
    }
    ValKind::BuiltIn(b) => {
      for &sym in syms_for_builtin(*b) { usage.mark(sym); }
      // When a builtin appears in *value* position (e.g. `panic` as a
      // fail-cont arg), the lowering also needs the runtime symbol
      // for the underlying `Fn2`, plus the Closure/Captures types to
      // wrap it.
      if matches!(b, BuiltIn::Panic) {
        usage.mark(Sym::Panic);
        usage.mark(Sym::Closure);
        usage.mark(Sym::Captures);
      }
    }
    _ => {}
  }
}
