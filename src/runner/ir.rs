//! IR-pipeline runner test harness.
//!
//! Parallel to the main runner's `run` / `run_main` harnesses in
//! `src/runner/mod.rs`, but drives the **new** pipeline end-to-end:
//!
//!   source → to_lifted → ir_lower → ir_link → ir_emit → wasmtime
//!
//! `run(src) -> String` mirrors the existing `run` semantics:
//! compile a bare expression (no `main = fn:` wrapper), invoke
//! `fink_module` with a host-wrapped done continuation, capture the
//! value the done receives, stringify it matching the existing
//! convention (integer-valued floats rendered without `.0`).
//!
//! Used by `include_fink_tests!("src/runner/test_ir.fnk")` below.
//! The fixture set grows by demand as `ir_lower` gains coverage;
//! once it covers what the main runner tests exercise, we swap
//! `test_ir.fnk` for shared `test_literals.fnk` / `test_operators.fnk`
//! etc. imports.

#[cfg(test)]
mod tests {
  use std::sync::{Arc, Mutex};
  use wasmtime::{Config, Engine, Module, Store, Linker, Error, ExternType, Val};

  enum TestResult {
    Num(f64),
    Bool(bool),
    None,
  }

  /// Run a bare-expression Fink source through the new IR pipeline
  /// and return the value the done continuation receives, stringified.
  #[allow(unused)]
  fn run(src: &str) -> String {
    match exec_ir_module(src) {
      Ok(TestResult::Num(v)) => {
        if v == v.floor() && v.abs() < 1e15 {
          format!("{}", v as i64)
        } else {
          format!("{}", v)
        }
      }
      Ok(TestResult::Bool(b)) => format!("{}", b),
      Ok(TestResult::None) => String::new(),
      Err(e) => format!("ERROR: {}", e),
    }
  }

  fn exec_ir_module(src: &str) -> Result<TestResult, String> {
    let (lifted, desugared) = crate::to_lifted(src, "test")
      .map_err(|e| format!("compile: {e}"))?;
    let user_frag = crate::passes::wasm::ir_lower::lower(&lifted.result, &desugared.ast);
    let linked = crate::passes::wasm::ir_link::link(&[user_frag]);
    let bytes = crate::passes::wasm::ir_emit::emit(&linked);

    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_function_references(true);
    config.wasm_tail_call(true);
    let engine = Engine::new(&config).map_err(|e| e.to_string())?;
    let module = Module::new(&engine, &bytes).map_err(|e| format!("{e:#}"))?;
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);

    let captured: Arc<Mutex<Option<TestResult>>> = Arc::new(Mutex::new(None));

    for imp in module.imports() {
      if imp.module() == "env"
        && let ExternType::Func(ft) = imp.ty()
      {
        let name = imp.name().to_string();
        match name.as_str() {
          "host_invoke_cont" => {
            // done_cont fires with (i32 id, args). Pull args[0] and
            // inspect its shape to recover the result value.
            let captured_clone = captured.clone();
            linker.func_new("env", &name, ft, move |mut caller, params, _results| {
              let args_any = params[1].unwrap_anyref()
                .ok_or_else(|| Error::msg("host_invoke_cont: null args"))?;
              let cons = args_any.unwrap_struct(&caller)?;
              let head = cons.field(&mut caller, 0)?;
              let head_any = match head {
                Val::AnyRef(Some(r)) => r,
                _ => return Ok(()),
              };
              // Bools: i31ref (0 = false, 1 = true).
              if let Ok(Some(i31)) = head_any.as_i31(&caller) {
                *captured_clone.lock().unwrap() =
                  Some(TestResult::Bool(i31.get_i32() != 0));
                return Ok(());
              }
              // $Num: struct with f64 in field 0.
              if let Ok(Some(st)) = head_any.as_struct(&caller)
                && let Ok(Val::F64(bits)) = st.field(&mut caller, 0)
              {
                *captured_clone.lock().unwrap() =
                  Some(TestResult::Num(f64::from_bits(bits)));
              }
              Ok(())
            }).map_err(|e| e.to_string())?;
          }
          _ => {
            let name_for_msg = name.clone();
            linker.func_new("env", &name, ft, move |_c, _p, _r| {
              Err(Error::msg(format!("host stub `{name_for_msg}` fired unexpectedly")))
            }).map_err(|e| e.to_string())?;
          }
        }
      }
    }

    let instance = linker.instantiate(&mut store, &module)
      .map_err(|e| e.to_string())?;

    let wrap_host_cont = instance.get_func(&mut store, "wrap_host_cont")
      .ok_or("no 'wrap_host_cont' export")?;
    let args_empty = instance.get_func(&mut store, "std/list.wat:args_empty")
      .ok_or("no 'std/list.wat:args_empty' export")?;
    let args_prepend = instance.get_func(&mut store, "std/list.wat:args_prepend")
      .ok_or("no 'std/list.wat:args_prepend' export")?;
    let fink_module = instance.get_func(&mut store, "fink_module")
      .ok_or("no 'fink_module' export")?;

    let mut done_closure = [Val::AnyRef(None)];
    wrap_host_cont.call(&mut store, &[Val::I32(1)], &mut done_closure)
      .map_err(|e| format!("wrap_host_cont: {e}"))?;

    let mut empty = [Val::AnyRef(None)];
    args_empty.call(&mut store, &[], &mut empty)
      .map_err(|e| format!("args_empty: {e}"))?;
    let mut args_list = [Val::AnyRef(None)];
    args_prepend.call(&mut store, &[done_closure[0].clone(), empty[0].clone()], &mut args_list)
      .map_err(|e| format!("args_prepend: {e}"))?;

    fink_module.call(&mut store, &[Val::AnyRef(None), args_list[0].clone()], &mut [])
      .map_err(|e| format!("fink_module: {e}"))?;

    Ok(captured.lock().unwrap().take().unwrap_or(TestResult::None))
  }

  // Shared fixtures — same .fnk files the main runner uses. Tests
  // tagged `skip-ir` are the ones the new pipeline can't handle yet;
  // they emit `#[ignore = "skip-ir"]` and stay visible in the test
  // count as a coverage-gap indicator.
  test_macros::include_fink_tests!("src/runner/test_literals.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_operators.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_ir.fnk");
}
