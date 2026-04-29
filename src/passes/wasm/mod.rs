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

  #[cfg(test)]
  fn validate_and_collect_exports(bytes: &[u8]) -> Vec<String> {
    let mut validator = wasmparser::Validator::new_with_features(
      wasmparser::WasmFeatures::all(),
    );
    validator.validate_all(bytes)
      .unwrap_or_else(|e| panic!("emit validation failed: {e}"));

    let mut exports = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
      if let wasmparser::Payload::ExportSection(reader) = payload.unwrap() {
        for exp in reader {
          exports.push(exp.unwrap().name.to_string());
        }
      }
    }
    exports
  }

  #[test]
  fn emit_produces_valid_wasm_for_int_literal() {
    let bytes = emit_for("42");
    let exports = validate_and_collect_exports(&bytes);

    // User's fink_module is exported.
    assert!(exports.contains(&"fink_module".to_string()),
      "missing fink_module export. got: {exports:?}");

    // Runtime exports are passed through (with <url>:<name> qualification).
    assert!(exports.contains(&"rt/apply.wat:apply".to_string()),
      "missing rt/apply.wat:apply passthrough");
    assert!(exports.contains(&"std/fn.fnk:args_empty".to_string()),
      "missing std/fn.fnk:args_empty passthrough");

    // Interop exports stay bare (host contract).
    assert!(exports.contains(&"wrap_host_cont".to_string()),
      "missing wrap_host_cont passthrough");

    // stdio protocol dispatchers — exposed under the virtual std/io.fnk
    // namespace. Importing 'std/io.fnk' resolves to these.
    assert!(exports.contains(&"std/io.fnk:stdout".to_string()),
      "missing std/io.fnk:stdout dispatcher");
    assert!(exports.contains(&"std/io.fnk:stderr".to_string()),
      "missing std/io.fnk:stderr dispatcher");
    assert!(exports.contains(&"std/io.fnk:stdin".to_string()),
      "missing std/io.fnk:stdin dispatcher");
    assert!(exports.contains(&"std/io.fnk:read".to_string()),
      "missing std/io.fnk:read dispatcher");
  }

  #[test]
  fn emit_produces_valid_wasm_for_int_sum() {
    let bytes = emit_for("42 + 123");
    let exports = validate_and_collect_exports(&bytes);

    assert!(exports.contains(&"fink_module".to_string()));
    assert!(exports.contains(&"std/operators.fnk:op_plus".to_string()),
      "missing std/operators.fnk:op_plus passthrough (needed for a+b)");
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

    // fink_module is exported and is a func.
    let fink_module = instance.get_func(&mut store, "fink_module")
      .expect("fink_module export missing");
    let ty = fink_module.ty(&store);
    assert_eq!(ty.params().len(), 2, "fink_module should take (caps, args)");
    assert_eq!(ty.results().len(), 0, "fink_module should return nothing (CPS tail call)");
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
