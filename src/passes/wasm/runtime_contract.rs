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

use crate::passes::cps::ir::{Arg, BuiltIn, Callable, Cont, CpsResult, Expr, ExprKind, Lit, ValKind};

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
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sym {
  // ── value types ────────────────────────────────────────────────
  Num,
  Fn2,

  // ── functions ──────────────────────────────────────────────────
  ArgsHead,
  ArgsEmpty,
  ArgsPrepend,
  Apply,
  OpPlus,
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
  match b {
    BuiltIn::Add => &[Sym::OpPlus, Sym::Num],
    // TODO: as lowering grows, fill in the actual mappings. For now
    // only the Builtins lowering actually encounters matter.
    BuiltIn::FinkModule => &[],
    _ => &[],
  }
}

// ──────────────────────────────────────────────────────────────────────
// Handles — typed accessors after declaration
// ──────────────────────────────────────────────────────────────────────

/// Handles to every declared runtime-contract symbol. Populated by
/// [`declare`]; read (not re-declared) by lowering. Signature types
/// (`fn_any_to_any` etc.) are local types declared by `declare`,
/// not imports — see [`Sym`] for the distinction.
#[derive(Default)]
pub struct Runtime {
  // imported value types
  num: Option<TypeSym>,
  fn2: Option<TypeSym>,
  // locally-declared function signature types (structural)
  fn_any_to_any:  Option<TypeSym>,
  fn_nil_to_list: Option<TypeSym>,
  fn_prepend_any: Option<TypeSym>,
  fn_bin_op:      Option<TypeSym>,
  // functions
  args_head:    Option<FuncSym>,
  args_empty:         Option<FuncSym>,
  args_prepend: Option<FuncSym>,
  apply:            Option<FuncSym>,
  op_plus:          Option<FuncSym>,
}

impl Runtime {
  pub fn num(&self)              -> TypeSym { self.num.expect("rt: Num not declared") }
  pub fn fn2(&self)              -> TypeSym { self.fn2.expect("rt: Fn2 not declared") }
  pub fn args_head(&self)    -> FuncSym { self.args_head.expect("rt: args_head not declared") }
  pub fn args_empty(&self)         -> FuncSym { self.args_empty.expect("rt: args_empty not declared") }
  pub fn args_prepend(&self) -> FuncSym { self.args_prepend.expect("rt: args_prepend not declared") }
  pub fn apply(&self)            -> FuncSym { self.apply.expect("rt: _apply not declared") }
  pub fn op_plus(&self)          -> FuncSym { self.op_plus.expect("rt: op_plus not declared") }
}

// ──────────────────────────────────────────────────────────────────────
// Declare
// ──────────────────────────────────────────────────────────────────────

/// Per-`Sym` fragment URL + export name.
///
/// The emitter emits `(import "<url>" "<name>" ...)` — after merge
/// (via build.rs textual splice today, ir_link tomorrow), the runtime
/// bundle exports every referenced name qualified as `<url>:<name>`
/// in its export table. `ir_emit` composes the same string to look up
/// the concrete function/type index in `runtime-ir.wasm` and rewrite
/// the user fragment's call sites.
///
/// Reserved roots:
/// * `rt/*`   — compiler-level ABI. Not user-importable.
/// * `std/*`  — user-facing stdlib. Built on top of `rt`.
/// * `interop/*` — host bridge. Target-selected at link time.
/// * `./*`    — user's relative imports.
/// * `https://...`, `reg:*` — future third-party packages.
fn import_key(sym: Sym) -> (&'static str, &'static str) {
  match sym {
    Sym::Num             => ("rt/types.wat",     "Num"),
    Sym::Fn2             => ("rt/types.wat",     "Fn2"),
    Sym::Apply           => ("rt/apply.wat",     "_apply"),
    Sym::OpPlus          => ("rt/protocols.wat", "op_plus"),
    Sym::ArgsHead        => ("std/list.wat",     "args_head"),
    Sym::ArgsEmpty       => ("std/list.wat",     "args_empty"),
    Sym::ArgsPrepend     => ("std/list.wat",     "args_prepend"),
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
  // `types.wasm` at emit time.
  if needed.contains(&Sym::Num) || needed.contains(&Sym::OpPlus) {
    let (m, n) = import_key(Sym::Num);
    rt.num = Some(ty_import(frag, m, n, AbsHeap::Any));
  }
  if needed.contains(&Sym::Fn2) || needed.contains(&Sym::Apply) || always_need_fn2(usage) {
    let (m, n) = import_key(Sym::Fn2);
    rt.fn2 = Some(ty_import(frag, m, n, AbsHeap::Func));
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

  // Name convention: each function gets its own signature type
  // `$<url>:Fn_<fnname>` — monomorphic-first, mirrors how type
  // inference would start before generalisation. Reusable
  // calling-convention signatures (currently `rt/types.wat:Fn2`)
  // stay as value-type imports.
  if needed.contains(&Sym::ArgsHead) {
    let sig = ty_func(frag,
      vec![anyref_n.clone()],
      vec![anyref_n.clone()],
      "std/list.wat:Fn_args_head");
    rt.fn_any_to_any = Some(sig);
    let (m, n) = import_key(Sym::ArgsHead);
    rt.args_head = Some(import_func(frag, sig, m, n));
  }
  if needed.contains(&Sym::ArgsEmpty) {
    let sig = ty_func(frag,
      vec![],
      vec![anyref_n.clone()],
      "std/list.wat:Fn_args_empty");
    rt.fn_nil_to_list = Some(sig);
    let (m, n) = import_key(Sym::ArgsEmpty);
    rt.args_empty = Some(import_func(frag, sig, m, n));
  }
  if needed.contains(&Sym::ArgsPrepend) {
    let sig = ty_func(frag,
      vec![anyref_n.clone(), anyref_n.clone()],
      vec![anyref_n.clone()],
      "std/list.wat:Fn_args_prepend");
    rt.fn_prepend_any = Some(sig);
    let (m, n) = import_key(Sym::ArgsPrepend);
    rt.args_prepend = Some(import_func(frag, sig, m, n));
  }
  if needed.contains(&Sym::Apply) {
    // `_apply`'s signature is genuinely `rt/types.wat:Fn2` — reuse
    // the shared-identity type import directly.
    let (m, n) = import_key(Sym::Apply);
    rt.apply = Some(import_func(frag, rt.fn2.expect("Fn2 must be declared"), m, n));
  }
  if needed.contains(&Sym::OpPlus) {
    let sig = ty_func(frag,
      vec![anyref_n.clone(), anyref_n.clone(), anyref_n.clone()],
      vec![],
      "rt/protocols.wat:Fn_op_plus");
    rt.fn_bin_op = Some(sig);
    let (m, n) = import_key(Sym::OpPlus);
    rt.op_plus = Some(import_func(frag, sig, m, n));
  }

  rt
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

  scan_expr(&cps.root, &mut usage);
  usage
}

/// True if the fink_module body's tail App calls a user continuation
/// (requiring the `_apply` + list bring-up path) rather than a direct
/// builtin (`op_plus` etc.).
fn tail_uses_apply(root: &Expr) -> bool {
  // Find the fink_module body — `App(FinkModule, [Cont::Expr { body }])`.
  let ExprKind::App { func: Callable::BuiltIn(BuiltIn::FinkModule), args } = &root.kind else {
    return true; // unknown shape — assume apply path
  };
  let Some(Arg::Cont(Cont::Expr { body, .. })) = args.first() else {
    return true;
  };
  // The tail is body itself (lifted CPS has a single flat expression).
  matches!(&body.kind,
    ExprKind::App { func: Callable::Val(_), .. }
  )
}

fn scan_expr(expr: &Expr, usage: &mut RuntimeUsage) {
  match &expr.kind {
    ExprKind::LetVal { val, cont, .. } => {
      scan_val_kind(&val.kind, usage);
      scan_cont(cont, usage);
    }
    ExprKind::LetFn { fn_body, cont, .. } => {
      scan_expr(fn_body, usage);
      scan_cont(cont, usage);
    }
    ExprKind::App { func, args } => {
      match func {
        Callable::Val(v) => scan_val_kind(&v.kind, usage),
        Callable::BuiltIn(b) => {
          for &sym in syms_for_builtin(*b) { usage.mark(sym); }
        }
      }
      for a in args { scan_arg(a, usage); }
    }
    ExprKind::If { cond, then, else_ } => {
      scan_val_kind(&cond.kind, usage);
      scan_expr(then, usage);
      scan_expr(else_, usage);
    }
  }
}

fn scan_cont(cont: &Cont, usage: &mut RuntimeUsage) {
  if let Cont::Expr { body, .. } = cont {
    scan_expr(body, usage);
  }
}

fn scan_arg(arg: &Arg, usage: &mut RuntimeUsage) {
  match arg {
    Arg::Val(v) | Arg::Spread(v) => scan_val_kind(&v.kind, usage),
    Arg::Cont(c) => scan_cont(c, usage),
    Arg::Expr(e) => scan_expr(e, usage),
  }
}

fn scan_val_kind(kind: &ValKind, usage: &mut RuntimeUsage) {
  match kind {
    ValKind::Lit(Lit::Int(_) | Lit::Float(_) | Lit::Decimal(_)) => {
      usage.mark(Sym::Num);
    }
    ValKind::BuiltIn(b) => {
      for &sym in syms_for_builtin(*b) { usage.mark(sym); }
    }
    _ => {}
  }
}
