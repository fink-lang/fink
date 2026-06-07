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

#[cfg(feature = "runtime")]
use crate::errors::Diagnostic;
#[cfg(feature = "runtime")]
use crate::passes::Wasm;

/// Map a wasmtime error to a Diagnostic via the linked binary's
/// MarkRecords. If we can't resolve the trap to a source location
/// (no backtrace, or no nearby MarkRecord), the trap reason still
/// becomes a useful one-line message anchored at the entry url.
#[cfg(feature = "runtime")]
pub fn diagnose(err: &wasmtime::Error, bundle: &Wasm, entry_url: &str) -> Diagnostic {
  let trap = err.downcast_ref::<wasmtime::Trap>();
  let bt = err.downcast_ref::<wasmtime::WasmBacktrace>();

  let message = trap_message(trap, err);
  let (url, loc) = resolve_source(bt, bundle, entry_url);

  Diagnostic { url, message, loc, hint: None }
}

/// Translate a `Trap` (or detect a known host-stub panic) into a
/// fink-flavoured one-line message. Falls back to the raw error
/// display when nothing matches.
#[cfg(feature = "runtime")]
fn trap_message(trap: Option<&wasmtime::Trap>, err: &wasmtime::Error) -> String {
  use wasmtime::Trap::*;
  if let Some(t) = trap {
    return match t {
      UnreachableCodeReached => "trap: unreachable code reached".to_string(),
      IntegerDivisionByZero  => "integer divide by zero".to_string(),
      IntegerOverflow        => "integer overflow".to_string(),
      BadConversionToInteger => "invalid conversion to integer".to_string(),
      NullReference          => "null reference".to_string(),
      BadSignature           => "indirect call type mismatch".to_string(),
      MemoryOutOfBounds      => "out of bounds memory access".to_string(),
      TableOutOfBounds       => "out of bounds table access".to_string(),
      ArrayOutOfBounds       => "out of bounds array access".to_string(),
      StackOverflow          => "call stack exhausted".to_string(),
      // WasmGC ref.cast failure. In fink today this surfaces when a
      // value flows into an op that expects a different type (e.g.
      // `1 + 'foo'` -- string into op_plus' $Num cast). Real fix is
      // either static type checking or runtime br_on_cast with a
      // reason-tagged panic per op; for now the message just signals
      // a type mismatch to the user.
      CastFailure            => "type mismatch".to_string(),
      other                  => format!("trap: {other}"),
    };
  }
  // host_panic delivers a wire reason code which the runner translates
  // into a message of the form "fink panic: <reason>". Walk the anyhow
  // chain looking for that prefix and strip it so the user sees the
  // bare reason string produced by `PanicReason::message()`.
  let mut cur: Option<&dyn std::error::Error> = Some(err.as_ref());
  while let Some(e) = cur {
    let m = e.to_string();
    if let Some(idx) = m.find("fink panic: ") {
      let tail = &m[idx + "fink panic: ".len()..];
      return tail.lines().next().unwrap_or("").trim().to_string();
    }
    cur = e.source();
  }
  // Fall back to the {e:#} form, which prints the chain in compact
  // shape. {e} alone often shows only the outermost wrapper.
  format!("{err:#}")
}

/// Walk the backtrace innermost-first; for the first frame whose
/// `module_offset` resolves to a MarkRecord, return that record's
/// (url, loc). Falls back to the entry url and a zero-loc.
#[cfg(feature = "runtime")]
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
#[cfg(feature = "runtime")]
fn nearest_mark(
  pc: u32,
  marks: &[crate::passes::debug_marks::MarkRecord],
) -> Option<&crate::passes::debug_marks::MarkRecord> {
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

/// SourceProvider impl that knows about modules in a compiled package.
///
/// In its simplest form (no entry source) it returns `None` for every
/// url, which makes `format_diagnostic` fall back to `format_oneline`.
/// The url+line+col from the MarkRecord is still correct -- the
/// fallback is just less pretty (no caret + context block).
///
/// When the entry's source is available (call `with_entry`), the
/// provider returns it for matching urls so `format_diagnostic`
/// renders the full caret+context block.
///
/// TODO: extend to multi-module by carrying the loader (or a snapshot
/// of all module sources). Today only the entry module gets rich
/// rendering; deps fall back to oneline.
#[cfg(feature = "runtime")]
pub struct PackageSourceProvider<'a> {
  bundle: &'a Wasm,
  entry: Option<(String, String)>,
}

#[cfg(feature = "runtime")]
impl<'a> PackageSourceProvider<'a> {
  pub fn new(bundle: &'a Wasm) -> Self {
    Self { bundle, entry: None }
  }

  /// Attach the entry module's url and source as a fallback for compiles
  /// that don't populate `bundle.module_sources` (e.g. single-file paths).
  /// Package compiles serve every module's source from the bundle, so this
  /// is only consulted when the bundle has no entry for the url.
  pub fn with_entry(mut self, url: String, src: String) -> Self {
    self.entry = Some((url, src));
    self
  }
}

#[cfg(feature = "runtime")]
impl crate::errors::SourceProvider for PackageSourceProvider<'_> {
  fn source(&self, url: &str) -> Option<&str> {
    // Prefer the per-module source captured during package compilation --
    // this is what lets a trap inside a dependency render its own source
    // line + caret. Fall back to the explicitly-attached entry source.
    if let Some(src) = self.bundle.module_sources.get(url) {
      return Some(src);
    }
    match &self.entry {
      Some((entry_url, entry_src)) if entry_url == url => Some(entry_src),
      _ => None,
    }
  }
}


#[cfg(all(test, feature = "runtime"))]
mod tests {
  use super::*;
  use crate::passes::debug_marks::MarkRecord;
  use crate::passes::wasm::ir::ModuleId;
  use crate::passes::cps::ir::CpsId;
  use crate::lexer::{Loc, Pos};

  fn loc(line: u32, col: u32) -> Loc {
    Loc {
      start: Pos { idx: 0, line, col },
      end:   Pos { idx: 0, line, col },
    }
  }

  fn mark(pc: u32, line: u32, col: u32, id: ModuleId) -> MarkRecord {
    MarkRecord { wasm_pc: pc, cps_id: CpsId(0), source: loc(line, col), module_id: id }
  }

  #[test]
  fn nearest_mark_finds_closest_at_or_before() {
    let marks = vec![
      mark(100, 1, 0, ModuleId(0)),
      mark(200, 2, 0, ModuleId(0)),
      mark(300, 3, 0, ModuleId(0)),
    ];
    assert_eq!(nearest_mark(50,  &marks).map(|m| m.wasm_pc), None);
    assert_eq!(nearest_mark(100, &marks).map(|m| m.wasm_pc), Some(100));
    assert_eq!(nearest_mark(150, &marks).map(|m| m.wasm_pc), Some(100));
    assert_eq!(nearest_mark(199, &marks).map(|m| m.wasm_pc), Some(100));
    assert_eq!(nearest_mark(200, &marks).map(|m| m.wasm_pc), Some(200));
    assert_eq!(nearest_mark(1000, &marks).map(|m| m.wasm_pc), Some(300));
  }

  #[test]
  fn nearest_mark_empty_returns_none() {
    assert!(nearest_mark(100, &[]).is_none());
  }

  #[test]
  fn nearest_mark_unsorted_input_still_works() {
    // The scan is O(n); order does not matter.
    let marks = vec![
      mark(300, 3, 0, ModuleId(0)),
      mark(100, 1, 0, ModuleId(0)),
      mark(200, 2, 0, ModuleId(0)),
    ];
    assert_eq!(nearest_mark(250, &marks).map(|m| m.wasm_pc), Some(200));
  }

  #[test]
  fn trap_message_maps_known_variants() {
    use wasmtime::Trap::*;
    let dummy = wasmtime::Error::msg("ignored");
    assert_eq!(trap_message(Some(&IntegerDivisionByZero), &dummy), "integer divide by zero");
    assert_eq!(trap_message(Some(&UnreachableCodeReached), &dummy), "trap: unreachable code reached");
    assert_eq!(trap_message(Some(&NullReference),          &dummy), "null reference");
    assert_eq!(trap_message(Some(&StackOverflow),          &dummy), "call stack exhausted");
    assert_eq!(trap_message(Some(&CastFailure),            &dummy), "type mismatch");
  }

  #[test]
  fn trap_message_strips_fink_panic_prefix() {
    // host_panic delivers messages of the form "fink panic: <reason>".
    // The translator strips the prefix so the user sees the bare
    // reason produced by `PanicReason::message()`.
    let irrefutable = wasmtime::Error::msg("fink panic: irrefutable pattern failed");
    let no_match    = wasmtime::Error::msg("fink panic: match exhausted: no arm matched");
    let nested      = wasmtime::Error::msg("some context\nfink panic: match exhausted: no arm matched\ntrailing");
    assert_eq!(trap_message(None, &irrefutable), "irrefutable pattern failed");
    assert_eq!(trap_message(None, &no_match),    "match exhausted: no arm matched");
    assert_eq!(trap_message(None, &nested),      "match exhausted: no arm matched");
  }

  #[test]
  fn package_source_provider_entry_fallback() {
    use crate::errors::SourceProvider;
    let bundle = Wasm {
      binary: Vec::new(),
      mappings: Vec::new(),
      marks: Vec::new(),
      id_to_url: std::collections::BTreeMap::new(),
      module_sources: std::collections::BTreeMap::new(),
    };
    let provider = PackageSourceProvider::new(&bundle)
      .with_entry("./test.fnk".to_string(), "hello".to_string());
    assert_eq!(provider.source("./test.fnk"), Some("hello"));
    assert_eq!(provider.source("./other.fnk"), None);
  }

  #[test]
  fn package_source_provider_serves_every_module() {
    use crate::errors::SourceProvider;
    let mut module_sources = std::collections::BTreeMap::new();
    module_sources.insert("./test.fnk".to_string(), "entry-src".to_string());
    module_sources.insert("./dep.fnk".to_string(), "dep-src".to_string());
    let bundle = Wasm {
      binary: Vec::new(),
      mappings: Vec::new(),
      marks: Vec::new(),
      id_to_url: std::collections::BTreeMap::new(),
      module_sources,
    };
    let provider = PackageSourceProvider::new(&bundle);
    assert_eq!(provider.source("./test.fnk"), Some("entry-src"));
    assert_eq!(provider.source("./dep.fnk"), Some("dep-src"));
    assert_eq!(provider.source("./missing.fnk"), None);
  }

  #[test]
  fn package_source_provider_without_entry_returns_none() {
    use crate::errors::SourceProvider;
    let bundle = Wasm {
      binary: Vec::new(),
      mappings: Vec::new(),
      marks: Vec::new(),
      id_to_url: std::collections::BTreeMap::new(),
      module_sources: std::collections::BTreeMap::new(),
    };
    let provider = PackageSourceProvider::new(&bundle);
    assert_eq!(provider.source("./test.fnk"), None);
  }
}
