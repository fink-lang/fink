//! Translate a wasmtime `Error` (the result of a trap) into a
//! `Diagnostic` users can read.
//!
//! The translation has three pieces:
//! 1. Downcast the error to `wasmtime::Trap` for the trap kind
//!    (UnreachableCodeReached, IntegerDivisionByZero, NullReference,
//!    BadSignature, etc.). Each maps to a plain-language fink message.
//! 2. Downcast to `wasmtime::WasmBacktrace` to get the failing frame's
//!    PC offset (`module_offset`) into the linked binary.
//! 3. Find the `MarkRecord` whose `wasm_pc` is closest at-or-before that
//!    PC. The MarkRecord carries the source `Loc` and `module_id`; the
//!    `id_to_url` map turns the id into a fink url. That triple is the
//!    Diagnostic.
//!
//! Tail calls (`return_call*`) collapse the wasm stack so the backtrace
//! is usually a single innermost frame — exactly the site we want
//! pointed at.

#[cfg(feature = "run")]
use crate::errors::Diagnostic;
#[cfg(feature = "run")]
use crate::passes::Wasm;

/// Map a wasmtime error to a Diagnostic via the linked binary's
/// MarkRecords. If we can't resolve the trap to a source location
/// (no backtrace, or no nearby MarkRecord), the trap reason still
/// becomes a useful one-line message anchored at the entry url.
#[cfg(feature = "run")]
pub fn diagnose(err: &wasmtime::Error, bundle: &Wasm, entry_url: &str) -> Diagnostic {
  let trap = err.downcast_ref::<wasmtime::Trap>();
  let bt = err.downcast_ref::<wasmtime::WasmBacktrace>();

  let message = trap_message(trap, err);
  let (url, loc) = resolve_source(bt, bundle, entry_url);

  Diagnostic { url, message, loc, hint: None }
}

/// Translate a `Trap` (or fall back to the raw error display) into a
/// fink-flavoured one-line message.
#[cfg(feature = "run")]
fn trap_message(trap: Option<&wasmtime::Trap>, err: &wasmtime::Error) -> String {
  use wasmtime::Trap::*;
  match trap {
    Some(UnreachableCodeReached) => "trap: unreachable code reached".to_string(),
    Some(IntegerDivisionByZero)  => "integer divide by zero".to_string(),
    Some(IntegerOverflow)        => "integer overflow".to_string(),
    Some(BadConversionToInteger) => "invalid conversion to integer".to_string(),
    Some(NullReference)          => "null reference".to_string(),
    Some(BadSignature)           => "indirect call type mismatch".to_string(),
    Some(MemoryOutOfBounds)      => "out of bounds memory access".to_string(),
    Some(TableOutOfBounds)       => "out of bounds table access".to_string(),
    Some(ArrayOutOfBounds)       => "out of bounds array access".to_string(),
    Some(StackOverflow)          => "call stack exhausted".to_string(),
    Some(other)                  => format!("trap: {other}"),
    None                         => err.to_string(),
  }
}

/// Walk the backtrace innermost-first; for the first frame whose
/// `module_offset` resolves to a MarkRecord, return that record's
/// (url, loc). Falls back to the entry url and a zero-loc.
#[cfg(feature = "run")]
fn resolve_source(
  bt: Option<&wasmtime::WasmBacktrace>,
  bundle: &Wasm,
  entry_url: &str,
) -> (String, crate::lexer::Loc) {
  let fallback_loc = crate::lexer::Loc {
    start: crate::lexer::Pos { idx: 0, line: 1, col: 0 },
    end:   crate::lexer::Pos { idx: 0, line: 1, col: 0 },
  };

  let Some(bt) = bt else {
    return (entry_url.to_string(), fallback_loc);
  };

  for frame in bt.frames() {
    let Some(off) = frame.module_offset() else { continue };
    let pc = off as u32;
    if let Some(mark) = nearest_mark(pc, &bundle.marks) {
      let url = bundle.id_to_url.get(&mark.module_id)
        .cloned()
        .unwrap_or_else(|| entry_url.to_string());
      return (url, mark.source);
    }
  }
  (entry_url.to_string(), fallback_loc)
}

/// Closest at-or-before MarkRecord for `pc`. Linear scan; the mark
/// list is typically short and only consulted at trap time.
#[cfg(feature = "run")]
fn nearest_mark<'a>(
  pc: u32,
  marks: &'a [crate::passes::debug_marks::MarkRecord],
) -> Option<&'a crate::passes::debug_marks::MarkRecord> {
  let mut best: Option<&crate::passes::debug_marks::MarkRecord> = None;
  for m in marks {
    if m.wasm_pc <= pc {
      match best {
        Some(b) if b.wasm_pc > m.wasm_pc => {}
        _ => best = Some(m),
      }
    }
  }
  best
}

/// SourceProvider impl that knows about all modules in a compiled
/// package. Today returns `None` for every url since `Wasm` doesn't
/// carry source bytes; that makes `format_diagnostic` fall back to
/// `format_oneline`, which is the right behaviour: we still get
/// `ERROR: url:line:col: message` with the right url+loc from the
/// MarkRecord, without needing the source bytes embedded.
///
/// When a real source-bearing variant is needed (CLI rendering full
/// caret+context), the loader can be threaded in and consulted here.
#[cfg(feature = "run")]
pub struct PackageSourceProvider<'a> {
  _bundle: &'a Wasm,
}

#[cfg(feature = "run")]
impl<'a> PackageSourceProvider<'a> {
  pub fn new(bundle: &'a Wasm) -> Self {
    Self { _bundle: bundle }
  }
}

#[cfg(feature = "run")]
impl crate::errors::SourceProvider for PackageSourceProvider<'_> {
  fn source(&self, _url: &str) -> Option<&str> {
    None
  }
}
