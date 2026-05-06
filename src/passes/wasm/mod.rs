//! WASM codegen — lifted CPS IR → self-contained, debuggable WASM binary.
//!
//! Stage order:
//!
//! ```text
//!   lower → link → emit (or fmt for WAT text)
//! ```
//!
//! `lower` walks the lifted CPS and builds an IR `Fragment`; `link`
//! merges per-fragment IR into a single linked Fragment; `emit`
//! produces a final standalone WASM binary with `runtime-ir.wasm`
//! spliced in; `fmt` renders the linked Fragment back to WAT for the
//! playground and `fink wat`. `compile_package` orchestrates BFS over
//! imports and drives `lower` per dep before linking.

pub mod compile_package;
pub mod emit;
pub mod fmt;
pub mod ir;
pub mod link;
pub mod lower;
pub mod runtime_contract;
pub mod sourcemap;

/// Translate any `function[N]` substring in a wasmtime / validator error
/// message to `function[N]: $<qualified-name>`. Names are looked up in
/// the supplied binary's name section first (covers user-fragment
/// functions), falling back to the runtime's name table for runtime
/// functions when the binary has no entry. If the error also names a
/// byte offset (`offset M`), append the WAT line + a small surrounding
/// window from `wasmprinter`'s offset-annotated dump of the runtime
/// binary, so a validation error pinpoints the source instruction
/// without re-running tooling externally.
///
/// Falls back to leaving unknown indices / offsets unchanged.
pub fn annotate_func_indices(err: &str, wasm: &[u8]) -> String {
  // Build a combined name table: binary's own name section first, then
  // runtime fallback for indices the binary doesn't name.
  let mut func_names = func_names_from_binary(wasm);
  for (idx, name) in emit::runtime_func_names() {
    func_names.entry(idx).or_insert(name);
  }
  if func_names.is_empty() {
    return err.to_string();
  }
  // Replace each `function[N]` and `<wasm function N>` with the
  // qualified name. Two formats: `function[42]` (translation/validation
  // errors) and `<wasm function 42>` (trap backtraces).
  let mut out = annotate_pattern(err, "function[", "]", &func_names);
  out = annotate_pattern(&out, "<wasm function ", ">", &func_names);

  // If the error pointed at an offset, render the SAME binary the
  // error came from to text WAT and show a window around the offset.
  // This is the WAT view of the failing instruction — fuzzy-matchable
  // against the test snapshot WAT to find the originating fixture line.
  if let Some(off) = parse_offset(&out)
    && let Some(snippet) = wat_window_at_offset(wasm, off)
  {
    out.push_str("\n--- WAT near offset ---\n");
    out.push_str(&snippet);
  }
  out
}

/// Read the function-name subsection of `wasm`'s `name` custom section
/// into a `idx → name` map. Empty map if the binary has no name section.
fn func_names_from_binary(wasm: &[u8]) -> std::collections::HashMap<u32, String> {
  use std::collections::HashMap;
  let mut out: HashMap<u32, String> = HashMap::new();
  if wasm.len() < 8 || !wasm.starts_with(b"\0asm") {
    return out;
  }
  for payload in wasmparser::Parser::new(0).parse_all(wasm).flatten() {
    if let wasmparser::Payload::CustomSection(c) = payload
      && let wasmparser::KnownCustom::Name(reader) = c.as_known()
    {
      for sub in reader.into_iter().flatten() {
        if let wasmparser::Name::Function(map) = sub {
          for entry in map.into_iter().flatten() {
            out.insert(entry.index, entry.name.to_string());
          }
        }
      }
    }
  }
  out
}

/// Replace each `<prefix>N<close>` substring with `<prefix>N<close>: $<name>`,
/// where N is a u32 looked up in `names`. Skipped when `<close>` is
/// already followed by `::` (wasmtime auto-annotates from the binary's
/// name section, so adding our own would duplicate).
fn annotate_pattern(err: &str, prefix: &str, close: &str, names: &std::collections::HashMap<u32, String>) -> String {
  let mut out = String::with_capacity(err.len());
  let mut rest = err;
  while let Some(pos) = rest.find(prefix) {
    out.push_str(&rest[..pos + prefix.len()]);
    let after = &rest[pos + prefix.len()..];
    let end = match after.find(close) { Some(c) => c, None => { out.push_str(after); return out; } };
    let num_str = &after[..end];
    out.push_str(num_str);
    out.push_str(close);
    let tail = &after[end + close.len()..];
    let already_named = tail.starts_with("::") || tail.starts_with(": $");
    if !already_named
      && let Ok(idx) = num_str.parse::<u32>()
      && let Some(name) = names.get(&idx)
    {
      out.push_str(": $");
      out.push_str(name);
    }
    rest = tail;
  }
  out.push_str(rest);
  out
}

/// Parse an `offset 0x...` or `offset N` substring out of an error.
fn parse_offset(err: &str) -> Option<usize> {
  let pos = err.find("offset ")?;
  let after = &err[pos + "offset ".len()..];
  let end = after.find(|c: char| !c.is_ascii_alphanumeric() && c != 'x').unwrap_or(after.len());
  let num = &after[..end];
  if let Some(hex) = num.strip_prefix("0x") {
    usize::from_str_radix(hex, 16).ok()
  } else {
    num.parse::<usize>().ok()
  }
}

/// Render the supplied wasm bytes to WAT with `print_offsets`, find
/// the line whose annotated offset is closest to `target` (without
/// going past), and return a window of ±5 lines. Used to give a
/// human-readable view of the failing instruction at error time —
/// fuzzy-grep the surrounding lines into the test snapshot files
/// to find which fixture line emitted that WASM.
fn wat_window_at_offset(bytes: &[u8], target: usize) -> Option<String> {
  if bytes.len() < 8 || !bytes.starts_with(b"\0asm") {
    return None;
  }
  let mut cfg = wasmprinter::Config::new();
  cfg.print_offsets(true);
  let wat = {
    let mut s = String::new();
    cfg.print(bytes, &mut wasmprinter::PrintFmtWrite(&mut s)).ok()?;
    s
  };

  // Walk lines. wasmprinter prefixes each item with `(;@<hex>   ;)`
  // when `print_offsets(true)` is set. Track the most recent line
  // whose offset is <= target.
  let lines: Vec<&str> = wat.lines().collect();
  let mut best: Option<usize> = None;
  for (i, line) in lines.iter().enumerate() {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("(;@") {
      let hex_end = rest.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(rest.len());
      let hex = &rest[..hex_end];
      if let Ok(off) = usize::from_str_radix(hex, 16) {
        if off <= target {
          best = Some(i);
        } else if best.is_some() {
          break;
        }
      }
    }
  }
  let i = best?;
  let lo = i.saturating_sub(5);
  let hi = (i + 6).min(lines.len());
  let mut window = String::new();
  for (j, line) in lines[lo..hi].iter().enumerate() {
    let marker = if lo + j == i { ">> " } else { "   " };
    window.push_str(marker);
    window.push_str(line);
    window.push('\n');
  }
  Some(window)
}

#[cfg(test)]
mod tests {
  /// CPS → IR `Fragment` → WAT. No wasm-encoder, no linker, no runtime
  /// filtering. Drives the tracer-phase tests.
  fn wat(src: &str) -> String {
    let src_owned = src.to_string();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || wat_inner(&src_owned))) {
      Ok(s) => s,
      Err(e) => {
        let msg = if let Some(s) = e.downcast_ref::<&str>() {
          (*s).to_string()
        } else if let Some(s) = e.downcast_ref::<String>() {
          s.clone()
        } else {
          "<unknown panic>".to_string()
        };
        format!("PANIC: {msg}")
      }
    }
  }

  fn wat_inner(src: &str) -> String {
    let (lifted, desugared) = crate::to_lifted(src, "test").unwrap_or_else(|e| panic!("{e}"));
    // Generic synthetic FQN — every fragment in the IR must have a
    // non-empty fqn_prefix; tests use `test:` so emitted exports
    // (per-module wrapper, etc.) are addressable by a stable name.
    let user_frag = super::lower::lower(&lifted.result, &desugared.ast, "test:");
    // Single-module programs today: the link step is a passthrough,
    // but routing through it keeps the tracer test surface honest
    // when multi-fragment merge arrives.
    let linked = super::link::link(&[user_frag]);
    let (wat, sm) = super::fmt::fmt_fragment_with_sm(&linked);
    let b64 = sm.encode_base64url();
    format!("{}\n;; sm:{b64}", wat.trim())
  }


  /// Multi-module variant of `wat` for the new package-compile
  /// pipeline. Lowers `src` as the entry module under a fixed test
  /// canonical URL (`./test.fnk`) so every emitted symbol carries
  /// a real FQN prefix, exercising the same code paths a real
  /// `compile_package` invocation would drive.
  ///
  /// Today: single-fragment only — no actual import resolution, just
  /// the FQN-prefix half of the multi-module pipeline. Once
  /// `compile_package` lands, this helper grows a `SourceLoader`
  /// and walks dep imports for real.
  #[allow(dead_code)]
  fn wat_pkg(src: &str) -> String {
    let src_owned = src.to_string();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || wat_pkg_inner(&src_owned))) {
      Ok(s) => s,
      Err(e) => {
        let msg = if let Some(s) = e.downcast_ref::<&str>() {
          (*s).to_string()
        } else if let Some(s) = e.downcast_ref::<String>() {
          s.clone()
        } else {
          "<unknown panic>".to_string()
        };
        format!("PANIC: {msg}")
      }
    }
  }

  /// Inline-entry hybrid loader for `wat_pkg`. The entry source is
  /// registered at a synthetic disk path (the test fixtures dir,
  /// `src/passes/wasm/`) so dep imports from it can resolve to real
  /// fixture files like `test_link/simple.fnk` via FileSourceLoader.
  #[cfg(test)]
  struct PkgTestLoader {
    entry_abs_path: std::path::PathBuf,
    entry_source: String,
    disk: crate::passes::modules::FileSourceLoader,
  }

  #[cfg(test)]
  impl crate::passes::modules::SourceLoader for PkgTestLoader {
    fn load(&mut self, path: &std::path::Path) -> Result<String, String> {
      if path == self.entry_abs_path {
        Ok(self.entry_source.clone())
      } else {
        crate::passes::modules::SourceLoader::load(&mut self.disk, path)
      }
    }
  }

  fn wat_pkg_inner(src: &str) -> String {
    // Anchor the entry at `src/passes/wasm/test.fnk` so relative
    // imports like `./test_link/simple.fnk` reach the real fixture
    // tree on disk.
    let entry_abs_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("src/passes/wasm/test.fnk");
    let mut loader = PkgTestLoader {
      entry_abs_path: entry_abs_path.clone(),
      entry_source: src.to_string(),
      disk: crate::passes::modules::FileSourceLoader::new(),
    };
    let pkg = super::compile_package::compile_package(&entry_abs_path, &mut loader)
      .unwrap_or_else(|e| panic!("compile_package: {e}"));
    let (wat, sm) = super::fmt::fmt_fragment_with_sm(&pkg.fragment);
    let b64 = sm.encode_base64url();
    format!("{}\n;; sm:{b64}", wat.trim())
  }


  /// Run the IR pipeline end-to-end and validate the emitted module.
  ///
  /// This is the tracer bullet for the new pipeline: the output is
  /// real, spec-valid WASM bytes with runtime-ir.wasm spliced in and
  /// all user imports resolved to concrete runtime indices.
  #[cfg(test)]
  fn emit_for(src: &str) -> Vec<u8> {
    let (lifted, desugared) = crate::to_lifted(src, "test").unwrap_or_else(|e| panic!("{e}"));
    let user_frag = super::lower::lower(&lifted.result, &desugared.ast, "test:");
    let linked = super::link::link(&[user_frag]);
    super::emit::emit(&linked)
  }

  /// Instantiate emit's output in wasmtime with trivial host stubs.
  /// Proves the bytes aren't just spec-valid but also load into the
  /// real engine we'll run programs in.
  ///
  /// Doesn't call fink_module — the CPS entry expects an args list
  /// containing a done continuation, which needs runtime-exported
  /// helpers (args_empty + args_prepend) to construct. That full
  /// execution handshake is exercised by emit_executes_42 below.
  #[cfg(feature = "run")]
  #[test]
  fn emit_instantiates_in_wasmtime() {
    use wasmtime::{Config, Engine, Module, Store, Linker, Error, ExternType};

    let bytes = emit_for("42");

    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_function_references(true);
    config.wasm_tail_call(true);
    let engine = Engine::new(&config).unwrap();
    let module = Module::new(&engine, &bytes).unwrap();
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);

    // Wire every env import as a trap-on-call stub — none get
    // invoked during instantiation.
    for imp in module.imports() {
      if imp.module() == "env"
        && let ExternType::Func(ft) = imp.ty()
      {
        let name = imp.name().to_string();
        let name_for_msg = name.clone();
        linker.func_new("env", &name, ft, move |_c, _p, _r| {
          Err(Error::msg(format!("host stub `{name_for_msg}` called (unexpected for smoke test)")))
        }).unwrap();
      }
    }

    let instance = linker.instantiate(&mut store, &module).unwrap();

    // The per-module wrapper is exported under the canonical FQN
    // (`test:` for the test driver) and has the host-friendly
    // signature `(cont: anyref) -> ()`.
    let wrapper = instance.get_func(&mut store, "test")
      .expect("'test' wrapper export missing");
    let ty = wrapper.ty(&store);
    assert_eq!(ty.params().len(), 1, "wrapper should take (cont)");
    assert_eq!(ty.results().len(), 0, "wrapper should return nothing (CPS tail call)");
  }

  // End-to-end execution tests live in `src/runner/mod.rs` test
  // module, driven by `test_*.fnk` fixtures.

  test_macros::include_fink_tests!("src/passes/wasm/test_literals.fnk", skip-ir);
  test_macros::include_fink_tests!("src/passes/wasm/test_operators.fnk", skip-ir);
  test_macros::include_fink_tests!("src/passes/wasm/test_bindings.fnk", skip-ir);
  test_macros::include_fink_tests!("src/passes/wasm/test_functions.fnk", skip-ir);
  test_macros::include_fink_tests!("src/passes/wasm/test_tasks.fnk", skip-ir);
  test_macros::include_fink_tests!("src/passes/wasm/test_records.fnk", skip-ir);
  test_macros::include_fink_tests!("src/passes/wasm/test_strings.fnk", skip-ir);
  test_macros::include_fink_tests!("src/passes/wasm/test_linking.fnk", skip-ir);
  test_macros::include_fink_tests!("src/passes/wasm/test_io.fnk", skip-ir);
  test_macros::include_fink_tests!("src/passes/wasm/test_sets.fnk", skip-ir);
}
