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
/// in canonical *display* order: value types first, signature types
/// next, functions last.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sym {
  // ── value types ────────────────────────────────────────────────
  Num,

  // ── signature types ────────────────────────────────────────────
  Fn2,
  FnAnyToAny,
  FnNilToList,
  FnPrependAny,
  /// `(func (param (ref any) (ref any) (ref any)))` — two operands
  /// plus a continuation. Used for binary numeric operators.
  FnBinOp,

  // ── functions ──────────────────────────────────────────────────
  ListHeadAny,
  ListNil,
  ListPrependAny,
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
/// [`declare`]; read (not re-declared) by lowering.
#[derive(Default)]
pub struct Runtime {
  // value types
  num: Option<TypeSym>,
  // signature types
  fn2: Option<TypeSym>,
  fn_any_to_any:  Option<TypeSym>,
  fn_nil_to_list: Option<TypeSym>,
  fn_prepend_any: Option<TypeSym>,
  fn_bin_op:      Option<TypeSym>,
  // functions
  list_head_any:    Option<FuncSym>,
  list_nil:         Option<FuncSym>,
  list_prepend_any: Option<FuncSym>,
  apply:            Option<FuncSym>,
  op_plus:          Option<FuncSym>,
}

impl Runtime {
  pub fn num(&self)              -> TypeSym { self.num.expect("rt: Num not declared") }
  pub fn fn2(&self)              -> TypeSym { self.fn2.expect("rt: Fn2 not declared") }
  pub fn list_head_any(&self)    -> FuncSym { self.list_head_any.expect("rt: list_head_any not declared") }
  pub fn list_nil(&self)         -> FuncSym { self.list_nil.expect("rt: list_nil not declared") }
  pub fn list_prepend_any(&self) -> FuncSym { self.list_prepend_any.expect("rt: list_prepend_any not declared") }
  pub fn apply(&self)            -> FuncSym { self.apply.expect("rt: _apply not declared") }
  pub fn op_plus(&self)          -> FuncSym { self.op_plus.expect("rt: op_plus not declared") }
}

// ──────────────────────────────────────────────────────────────────────
// Declare
// ──────────────────────────────────────────────────────────────────────

/// Reserved module root for the compiler-level runtime ABI.
///
/// Every fink-emitted fragment imports from this module; any runtime
/// backend (WAT today, WASI-adapter tomorrow, browser tomorrow+1) must
/// export the set of names the emitter depends on.
///
/// Reserved roots:
/// * `rt/*`   — compiler-level ABI (this one). Not user-importable.
/// * `std/*`  — user-facing stdlib. Built on top of `rt`.
/// * `./*`    — user's relative imports.
/// * `https://...`, `reg:*` — future third-party packages.
const RUNTIME_MOD: &str = "rt";

/// Declare every symbol in `usage` as an import on `frag`, in the
/// canonical ordering given by `Sym`'s variant order. Pulls in any
/// transitively-required types (e.g. a function's signature type is
/// imported even if the program doesn't mention it directly).
pub fn declare(frag: &mut Fragment, usage: &RuntimeUsage) -> Runtime {
  let mut rt = Runtime::default();

  // Expand to include transitive type deps that functions need.
  let mut needed = usage.used.clone();
  if needed.contains(&Sym::ListHeadAny)    { needed.insert(Sym::FnAnyToAny); }
  if needed.contains(&Sym::ListNil)        { needed.insert(Sym::FnNilToList); }
  if needed.contains(&Sym::ListPrependAny) { needed.insert(Sym::FnPrependAny); }
  if needed.contains(&Sym::Apply)          { needed.insert(Sym::Fn2); }
  if needed.contains(&Sym::OpPlus)         { needed.insert(Sym::FnBinOp); }

  // Types first (BTreeSet iteration is in `Sym` declaration order,
  // so value types come before signature types naturally).
  for sym in &needed {
    match sym {
      Sym::Num          => rt.num           = Some(ty_import(frag, RUNTIME_MOD, "Num",          AbsHeap::Any)),
      Sym::Fn2          => rt.fn2           = Some(ty_import(frag, RUNTIME_MOD, "Fn2",          AbsHeap::Func)),
      Sym::FnAnyToAny   => rt.fn_any_to_any  = Some(ty_import(frag, RUNTIME_MOD, "FnAnyToAny",   AbsHeap::Func)),
      Sym::FnNilToList  => rt.fn_nil_to_list = Some(ty_import(frag, RUNTIME_MOD, "FnNilToList",  AbsHeap::Func)),
      Sym::FnPrependAny => rt.fn_prepend_any = Some(ty_import(frag, RUNTIME_MOD, "FnPrependAny", AbsHeap::Func)),
      Sym::FnBinOp      => rt.fn_bin_op      = Some(ty_import(frag, RUNTIME_MOD, "FnBinOp",      AbsHeap::Func)),
      _ => {}
    }
  }

  // Functions, in canonical order.
  for sym in &needed {
    match sym {
      Sym::ListHeadAny    => rt.list_head_any    = Some(import_func(frag, rt.fn_any_to_any.unwrap(),  RUNTIME_MOD, "list_head_any")),
      Sym::ListNil        => rt.list_nil         = Some(import_func(frag, rt.fn_nil_to_list.unwrap(), RUNTIME_MOD, "list_nil")),
      Sym::ListPrependAny => rt.list_prepend_any = Some(import_func(frag, rt.fn_prepend_any.unwrap(), RUNTIME_MOD, "list_prepend_any")),
      Sym::Apply          => rt.apply            = Some(import_func(frag, rt.fn2.unwrap(),            RUNTIME_MOD, "_apply")),
      Sym::OpPlus         => rt.op_plus          = Some(import_func(frag, rt.fn_bin_op.unwrap(),      RUNTIME_MOD, "op_plus")),
      _ => {}
    }
  }

  rt
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
  // that type is always needed. `list_head_any` is always needed
  // because bring-up always pops `done` out of `_args`.
  usage.mark(Sym::Fn2);
  usage.mark(Sym::ListHeadAny);

  // Bring-up further uses `_apply` (with `list_nil` + `list_prepend_any`)
  // only when the tail call is a user value or continuation — i.e.
  // when the fink_module body's tail is `App(ContRef(_), ...)`. A
  // direct-style tail call into a builtin (e.g. `op_plus`) skips the
  // apply mechanism and doesn't need those symbols. Introspect the
  // body to decide.
  if tail_uses_apply(&cps.root) {
    usage.mark(Sym::ListNil);
    usage.mark(Sym::ListPrependAny);
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
