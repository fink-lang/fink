//! WASM codegen — lifted CPS IR → self-contained, debuggable WASM binary.
//!
//! Stage order:
//!
//! ```text
//!   collect → emit → dwarf → link → fmt
//! ```
//!
//! `collect` walks the lifted CPS and builds `Module` / `CollectedFn`;
//! `emit` produces the binary via `wasm-encoder` plus byte-offset
//! mappings; `dwarf` appends line tables; `link` merges the runtime and
//! rewrites `@fink/` imports; `fmt` renders back to WAT with the native
//! source map for the playground and `fink wat`.
//!
//! The unified `$Fn2(captures, args)` calling convention, single-
//! `_apply` dispatch, closure layout, and how spread / varargs fit in
//! are all specified in `calling-convention.md` next to this module.
//!
//! Structural source locations (func headers, params, globals, exports)
//! are carried alongside the binary as `StructuralLoc` — they don't
//! correspond to code-section byte offsets and can't live in DWARF.
//!
//! Compiler-generated helpers use the `$_` prefix; the WAT formatter
//! hides them from test output.

pub mod builtins;
pub mod collect;
pub mod dwarf;
pub mod emit;
pub mod fmt;
pub mod ir;
pub mod ir_emit;
pub mod ir_fmt;
pub mod ir_link;
pub mod ir_lower;
pub mod link;
pub mod runtime_contract;
pub mod sourcemap;

#[cfg(feature = "run")]
pub mod compile;

#[cfg(test)]
mod tests {
  /// Round-trip gen_wat: CPS → emit (WASM binary) → format (WAT text + source map).
  fn gen_wat(src: &str) -> String {
    // Catch panics from emit/link/format so failing tests can still produce a
    // blessable string showing the panic message.
    let src_owned = src.to_string();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || gen_wat_inner(&src_owned))) {
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

  fn gen_wat_inner(src: &str) -> String {
    let (lifted, desugared) = crate::to_lifted(src, "test").unwrap_or_else(|e| panic!("{e}"));

    // Collect + emit WASM binary.
    let ir_ctx = super::collect::IrCtx::new(&lifted.result.origin, &desugared.ast);
    let module = super::collect::collect(&lifted.result.root, &ir_ctx, &lifted.result.module_locals, lifted.result.module_imports.clone());
    let ir_ctx = ir_ctx.with_globals(module.globals.clone());
    let mut result = super::emit::emit(&module, &ir_ctx, None);

    // Emit DWARF and append to binary.
    let dwarf_sections = super::dwarf::emit_dwarf("test", Some(src), &result.offset_mappings);
    super::dwarf::append_dwarf_sections(&mut result.wasm, &dwarf_sections);

    // Link: merge core runtime + user code into a standalone binary.
    static RUNTIME_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime.wasm"));

    let link_inputs = vec![
      super::link::LinkInput { module_name: "@fink/runtime".into(), wasm: RUNTIME_WASM.to_vec() },
      super::link::LinkInput { module_name: "@fink/user".into(), wasm: result.wasm },
    ];
    let linked = super::link::link(&link_inputs);

    // Format WASM → WAT with native source map (including structural locs).
    let (wat_output, wat_srcmap) = super::fmt::format_mapped_native(
      &linked.wasm, &result.structural_locs,
    );
    let wat_b64 = wat_srcmap.encode_base64url();

    format!("{}\n;; sm:{wat_b64}", wat_output.trim())
  }

  /// CPS → IR `Fragment` → WAT. No wasm-encoder, no linker, no runtime
  /// filtering. Drives the tracer-phase tests.
  fn ir_wat(src: &str) -> String {
    let src_owned = src.to_string();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || ir_wat_inner(&src_owned))) {
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

  fn ir_wat_inner(src: &str) -> String {
    let (lifted, desugared) = crate::to_lifted(src, "test").unwrap_or_else(|e| panic!("{e}"));
    let user_frag = super::ir_lower::lower(&lifted.result, &desugared.ast);
    // Single-module programs today: the link step is a passthrough,
    // but routing through it keeps the tracer test surface honest
    // when multi-fragment merge arrives.
    let linked = super::ir_link::link(&[user_frag]);
    let (wat, sm) = super::ir_fmt::fmt_fragment_with_sm(&linked);
    let b64 = sm.encode_base64url();
    format!("{}\n;; sm:{b64}", wat.trim())
  }


  /// Run the IR pipeline end-to-end and validate the emitted module.
  ///
  /// This is the tracer bullet for the new pipeline: the output is
  /// real, spec-valid WASM bytes with runtime-ir.wasm spliced in and
  /// all user imports resolved to concrete runtime indices.
  #[cfg(test)]
  fn ir_emit_for(src: &str) -> Vec<u8> {
    let (lifted, desugared) = crate::to_lifted(src, "test").unwrap_or_else(|e| panic!("{e}"));
    let user_frag = super::ir_lower::lower(&lifted.result, &desugared.ast);
    let linked = super::ir_link::link(&[user_frag]);
    super::ir_emit::emit(&linked)
  }

  #[cfg(test)]
  fn validate_and_collect_exports(bytes: &[u8]) -> Vec<String> {
    let mut validator = wasmparser::Validator::new_with_features(
      wasmparser::WasmFeatures::all(),
    );
    validator.validate_all(bytes)
      .unwrap_or_else(|e| panic!("ir_emit validation failed: {e}"));

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
  fn ir_emit_produces_valid_wasm_for_int_literal() {
    let bytes = ir_emit_for("42");
    let exports = validate_and_collect_exports(&bytes);

    // User's fink_module is exported.
    assert!(exports.contains(&"fink_module".to_string()),
      "missing fink_module export. got: {exports:?}");

    // Runtime exports are passed through (with <url>:<name> qualification).
    assert!(exports.contains(&"rt/apply.wat:_apply".to_string()),
      "missing rt/apply.wat:_apply passthrough");
    assert!(exports.contains(&"std/list.wat:args_empty".to_string()),
      "missing std/list.wat:args_empty passthrough");

    // Interop exports stay bare (host contract).
    assert!(exports.contains(&"_run_main".to_string()),
      "missing _run_main passthrough");
  }

  #[test]
  fn ir_emit_produces_valid_wasm_for_int_sum() {
    let bytes = ir_emit_for("42 + 123");
    let exports = validate_and_collect_exports(&bytes);

    assert!(exports.contains(&"fink_module".to_string()));
    assert!(exports.contains(&"rt/protocols.wat:op_plus".to_string()),
      "missing rt/protocols.wat:op_plus passthrough (needed for a+b)");
  }

  /// Instantiate ir_emit's output in wasmtime with trivial host stubs.
  /// Proves the bytes aren't just spec-valid but also load into the
  /// real engine we'll run programs in.
  ///
  /// Doesn't call fink_module — the CPS entry expects an args list
  /// containing a done continuation, which needs runtime-exported
  /// helpers (args_empty + args_prepend) to construct. That full
  /// execution handshake is exercised by ir_emit_executes_42 below.
  #[cfg(feature = "run")]
  #[test]
  fn ir_emit_instantiates_in_wasmtime() {
    use wasmtime::{Config, Engine, Module, Store, Linker, Error, ExternType};

    let bytes = ir_emit_for("42");

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

  /// End-to-end: compile `42`, run fink_module in wasmtime, assert the
  /// host-side done callback receives the expected Num.
  ///
  /// Uses the inbound-callable contract in interop/rust.wat:
  /// - host_invoke_cont(id, args) : env import — dispatches to the
  ///   Rust callback registered under `id`.
  /// - wrap_host_cont(id) -> anyref : exported factory — wraps a
  ///   WAT-authored $Fn2 adapter in a $Closure with captures=[i31(id)].
  ///   The opaque anyref is what the host hands WASM as a continuation.
  #[cfg(feature = "run")]
  #[test]
  fn ir_emit_executes_42() {
    use std::sync::{Arc, Mutex};
    use wasmtime::{Config, Engine, Module, Store, Linker, Error, ExternType, Val};

    let bytes = ir_emit_for("42");

    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_function_references(true);
    config.wasm_tail_call(true);
    let engine = Engine::new(&config).unwrap();
    let module = Module::new(&engine, &bytes).unwrap();
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);

    // Host-side callback registry + capture slot.
    let captured: Arc<Mutex<Option<f64>>> = Arc::new(Mutex::new(None));

    for imp in module.imports() {
      if imp.module() == "env"
        && let ExternType::Func(ft) = imp.ty()
      {
        let name = imp.name().to_string();
        match name.as_str() {
          "host_invoke_cont" => {
            // params: (i32 id, anyref args). id=1 is our done callback.
            let captured_clone = captured.clone();
            linker.func_new("env", &name, ft, move |mut caller, params, _results| {
              let id = params[0].unwrap_i32();
              assert_eq!(id, 1, "unexpected callback id {id}");
              // args is a $Cons. Pull args[0] = $Num(42).
              let args_any = params[1].unwrap_anyref()
                .ok_or_else(|| Error::msg("host_invoke_cont: null args"))?;
              let cons = args_any.unwrap_struct(&caller)?;
              let head = cons.field(&mut caller, 0)?;
              let head_any = head.unwrap_anyref()
                .ok_or_else(|| Error::msg("host_invoke_cont: null head"))?;
              let num_struct = head_any.unwrap_struct(&caller)?;
              let val_f = num_struct.field(&mut caller, 0)?;
              *captured_clone.lock().unwrap() = Some(val_f.unwrap_f64());
              Ok(())
            }).unwrap();
          }
          _ => {
            // Other env imports shouldn't fire during this test.
            let name_for_msg = name.clone();
            linker.func_new("env", &name, ft, move |_c, _p, _r| {
              Err(Error::msg(format!("host stub `{name_for_msg}` fired unexpectedly")))
            }).unwrap();
          }
        }
      }
    }

    let instance = linker.instantiate(&mut store, &module).unwrap();

    let wrap_host_cont = instance.get_func(&mut store, "wrap_host_cont")
      .expect("wrap_host_cont export missing");
    let args_empty = instance.get_func(&mut store, "std/list.wat:args_empty")
      .expect("args_empty export missing");
    let args_prepend = instance.get_func(&mut store, "std/list.wat:args_prepend")
      .expect("args_prepend export missing");
    let fink_module = instance.get_func(&mut store, "fink_module")
      .expect("fink_module export missing");

    // Wrap host callback id=1 as a fink continuation.
    let mut done_closure = [Val::AnyRef(None)];
    wrap_host_cont.call(&mut store, &[Val::I32(1)], &mut done_closure)
      .expect("wrap_host_cont failed");

    // Build args = [done_closure].
    let mut empty = [Val::AnyRef(None)];
    args_empty.call(&mut store, &[], &mut empty).expect("args_empty failed");
    let mut args_list = [Val::AnyRef(None)];
    args_prepend.call(&mut store, &[done_closure[0].clone(), empty[0].clone()], &mut args_list)
      .expect("args_prepend failed");

    // Call fink_module((null, args)).
    fink_module.call(&mut store, &[Val::AnyRef(None), args_list[0].clone()], &mut [])
      .expect("fink_module failed");

    // Done callback should have captured 42.
    assert_eq!(*captured.lock().unwrap(), Some(42.0),
      "host callback didn't receive 42");
  }

  /// Same flow as `ir_emit_executes_42` but for `42 + 123`. Exercises
  /// the op_plus protocol dispatch through rt/protocols, which then
  /// tail-calls the done cont with Num(165) instead of going through
  /// apply-path plumbing.
  #[cfg(feature = "run")]
  #[test]
  fn ir_emit_executes_42_plus_123() {
    use std::sync::{Arc, Mutex};
    use wasmtime::{Config, Engine, Module, Store, Linker, Error, ExternType, Val};

    let bytes = ir_emit_for("42 + 123");

    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_function_references(true);
    config.wasm_tail_call(true);
    let engine = Engine::new(&config).unwrap();
    let module = Module::new(&engine, &bytes).unwrap();
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);

    let captured: Arc<Mutex<Option<f64>>> = Arc::new(Mutex::new(None));

    for imp in module.imports() {
      if imp.module() == "env"
        && let ExternType::Func(ft) = imp.ty()
      {
        let name = imp.name().to_string();
        match name.as_str() {
          "host_invoke_cont" => {
            let captured_clone = captured.clone();
            linker.func_new("env", &name, ft, move |mut caller, params, _results| {
              let args_any = params[1].unwrap_anyref()
                .ok_or_else(|| Error::msg("host_invoke_cont: null args"))?;
              let cons = args_any.unwrap_struct(&caller)?;
              let head = cons.field(&mut caller, 0)?;
              let head_any = head.unwrap_anyref()
                .ok_or_else(|| Error::msg("host_invoke_cont: null head"))?;
              let num_struct = head_any.unwrap_struct(&caller)?;
              let val_f = num_struct.field(&mut caller, 0)?;
              *captured_clone.lock().unwrap() = Some(val_f.unwrap_f64());
              Ok(())
            }).unwrap();
          }
          _ => {
            let name_for_msg = name.clone();
            linker.func_new("env", &name, ft, move |_c, _p, _r| {
              Err(Error::msg(format!("host stub `{name_for_msg}` fired unexpectedly")))
            }).unwrap();
          }
        }
      }
    }

    let instance = linker.instantiate(&mut store, &module).unwrap();

    let wrap_host_cont = instance.get_func(&mut store, "wrap_host_cont").unwrap();
    let args_empty = instance.get_func(&mut store, "std/list.wat:args_empty").unwrap();
    let args_prepend = instance.get_func(&mut store, "std/list.wat:args_prepend").unwrap();
    let fink_module = instance.get_func(&mut store, "fink_module").unwrap();

    let mut done_closure = [Val::AnyRef(None)];
    wrap_host_cont.call(&mut store, &[Val::I32(1)], &mut done_closure).unwrap();

    let mut empty = [Val::AnyRef(None)];
    args_empty.call(&mut store, &[], &mut empty).unwrap();
    let mut args_list = [Val::AnyRef(None)];
    args_prepend.call(&mut store, &[done_closure[0].clone(), empty[0].clone()], &mut args_list).unwrap();

    fink_module.call(&mut store, &[Val::AnyRef(None), args_list[0].clone()], &mut [])
      .expect("fink_module(42+123) failed");

    assert_eq!(*captured.lock().unwrap(), Some(165.0),
      "host callback didn't receive 42 + 123 = 165");
  }

  test_macros::include_fink_tests!("src/passes/wasm/test_bindings.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_literals.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_operators.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_functions.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_strings.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_records.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_fink_module.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_ir.fnk");
}
