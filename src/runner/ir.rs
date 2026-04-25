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
    Str(Vec<u8>),
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
      Ok(TestResult::Str(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
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
              // Args list may be empty (`$Nil` — 0 fields). Treat as
              // "done called with no result" — capture None and return
              // cleanly instead of trapping.
              let head = match cons.field(&mut caller, 0) {
                Ok(h) => h,
                Err(_) => {
                  *captured_clone.lock().unwrap() = Some(TestResult::None);
                  return Ok(());
                }
              };
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
              // String types: $Str subtypes are GC structs.
              //   $StrEmpty:    no fields           — empty string.
              //   $StrDataImpl: (i32 offset, i32 len) — read from memory 0.
              //   $StrBytesImpl: (ref $ByteArray)    — read array elements.
              // Detect by struct field shape.
              if let Ok(Some(st)) = head_any.as_struct(&caller) {
                let field0 = st.field(&mut caller, 0);
                match field0 {
                  // $Num: f64.
                  Ok(Val::F64(bits)) => {
                    *captured_clone.lock().unwrap() =
                      Some(TestResult::Num(f64::from_bits(bits)));
                    return Ok(());
                  }
                  // $StrDataImpl: i32 offset + i32 length.
                  Ok(Val::I32(offset)) => {
                    if let Ok(Val::I32(length)) = st.field(&mut caller, 1) {
                      let mem = caller.get_export("memory")
                        .and_then(|e| e.into_memory());
                      if let Some(mem) = mem {
                        let data = mem.data(&caller);
                        let off = offset as usize;
                        let len = length as usize;
                        if off + len <= data.len() {
                          let bytes = data[off..off + len].to_vec();
                          *captured_clone.lock().unwrap() =
                            Some(TestResult::Str(bytes));
                          return Ok(());
                        }
                      }
                    }
                  }
                  // $StrBytesImpl: a (ref $ByteArray) — read element-wise.
                  Ok(Val::AnyRef(Some(_))) => {
                    if let Ok(Val::AnyRef(Some(ar))) = st.field(&mut caller, 0)
                      && let Ok(Some(arr)) = ar.as_array(&caller)
                    {
                      let len = arr.len(&caller).unwrap_or(0);
                      let mut bytes = Vec::with_capacity(len as usize);
                      for i in 0..len {
                        if let Ok(Val::I32(b)) = arr.get(&mut caller, i) {
                          bytes.push(b as u8);
                        }
                      }
                      *captured_clone.lock().unwrap() =
                        Some(TestResult::Str(bytes));
                      return Ok(());
                    }
                  }
                  // $StrEmpty: no field 0 — index errors. Treat as empty string.
                  Err(_) => {
                    *captured_clone.lock().unwrap() =
                      Some(TestResult::Str(Vec::new()));
                    return Ok(());
                  }
                  _ => {}
                }
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

    // Entry-module contract: if the source defined a top-level
    // `main`, invoke it with a fresh done cont and capture its
    // result instead of the module body's final value. The module
    // body's done has already fired by now (registering bindings,
    // settling globals); we discard whatever it captured.
    //
    // Currently we only pass `[done]` — works for `main = fn:`. The
    // optional-args shape (`main = fn args:`) needs cli_args wiring,
    // not yet implemented.
    if let Some(main_global) = instance.get_global(&mut store, "main") {
      // Reset captured to take the *new* done's value.
      *captured.lock().unwrap() = None;

      let main_clo = main_global.get(&mut store);
      let apply_func = instance.get_func(&mut store, "rt/apply.wat:_apply")
        .ok_or("no '_apply' export")?;
      let mut empty2 = [Val::AnyRef(None)];
      args_empty.call(&mut store, &[], &mut empty2)
        .map_err(|e| format!("args_empty: {e}"))?;
      let mut main_args = [Val::AnyRef(None)];
      args_prepend.call(&mut store, &[done_closure[0].clone(), empty2[0].clone()], &mut main_args)
        .map_err(|e| format!("args_prepend (main): {e}"))?;
      apply_func.call(&mut store, &[main_args[0].clone(), main_clo], &mut [])
        .map_err(|e| format!("_apply(main): {e}"))?;
    }

    Ok(captured.lock().unwrap().take().unwrap_or(TestResult::None))
  }

  // Shared fixtures — same .fnk files the main runner uses. Tests
  // tagged `skip-ir` are the ones the new pipeline can't handle yet;
  // they emit `#[ignore = "skip-ir"]` and stay visible in the test
  // count as a coverage-gap indicator.
  test_macros::include_fink_tests!("src/runner/test_literals.fnk",  skip-ir);
  test_macros::include_fink_tests!("src/runner/test_operators.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_bindings.fnk",  skip-ir);
  test_macros::include_fink_tests!("src/runner/test_functions.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_ranges.fnk",    skip-ir);
  test_macros::include_fink_tests!("src/runner/test_records.fnk",   skip-ir);
  test_macros::include_fink_tests!("src/runner/test_strings.fnk",   skip-ir);
  test_macros::include_fink_tests!("src/runner/test_patterns.fnk",  skip-ir);
  test_macros::include_fink_tests!("src/runner/test_formatting.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_tasks.fnk",     skip-ir);
  test_macros::include_fink_tests!("src/runner/test_ir_main.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_ir.fnk");
}
