// Runner: executes compiled WASM in Wasmtime.

use std::sync::{Arc, Mutex};

pub mod wasmtime_runner;

/// Shared, thread-safe IO stream (stdout or stderr).
pub type IoStream = Arc<Mutex<dyn std::io::Write + Send>>;

pub struct RunOptions {
  pub debug: bool,
  /// Source label shown in the debugger (e.g. the input file path).
  pub source_label: String,
}

impl Default for RunOptions {
  fn default() -> Self {
    Self { debug: false, source_label: "fink".into() }
  }
}

/// Compile source and run it. Returns the exit code from main.
pub fn run_source(
  mut opts: RunOptions,
  src: &str,
  path: &str,
  stdout: IoStream,
  stderr: IoStream,
) -> Result<i64, String> {
  if opts.source_label == "fink" {
    opts.source_label = path.to_string();
  }
  let wasm = crate::to_wasm(src, path)?;
  wasmtime_runner::run(&opts, &wasm.binary, stdout, stderr)
}

/// Read a file and run it. Supports .fnk source and .wasm binaries.
/// Returns the exit code from main.
pub fn run_file(
  mut opts: RunOptions,
  path: &str,
  stdout: IoStream,
  stderr: IoStream,
) -> Result<i64, String> {
  if opts.source_label == "fink" {
    opts.source_label = path.to_string();
  }

  if path.ends_with(".fnk") {
    let src = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    return run_source(opts, &src, path, stdout, stderr);
  }

  let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
  if bytes.starts_with(b"\0asm") {
    wasmtime_runner::run(&opts, &bytes, stdout, stderr)
  } else {
    Err("only .fnk and .wasm files are supported".into())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::{Arc, Mutex};
  use wasmtime::*;

  /// Result of calling an exported CPS function directly.
  enum TestResult {
    Num(f64),
    Bool(bool),
    Str(String),
    None,
  }

  /// Call a named export in a compiled WASM module and return the result.
  /// This is test infrastructure — it calls a CPS function directly,
  /// bypassing the IO protocol that _run_main uses.
  fn exec_export(wasm: &[u8], export_name: &str) -> Result<TestResult, String> {
    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_tail_call(true);
    config.wasm_function_references(true);

    let engine = Engine::new(&config).map_err(|e| e.to_string())?;
    let module = Module::new(&engine, wasm).map_err(|e| e.to_string())?;
    let mut store = Store::new(&engine, ());

    // Wire up "env" imports — trap with "not yet implemented".
    let mut linker = Linker::new(&engine);
    for import in module.imports() {
      if import.module() == "env"
        && let ExternType::Func(ft) = import.ty()
      {
        let name = import.name().to_string();
        let err_name = name.clone();
        linker.func_new("env", &name, ft.clone(), move |_caller, _params, _results| {
          Err(Error::msg(format!("builtin '{}' not yet implemented", err_name)))
        }).map_err(|e| e.to_string())?;
      }
    }

    let instance = linker.instantiate(&mut store, &module).map_err(|e| e.to_string())?;

    let test_fn = instance.get_func(&mut store, export_name)
      .ok_or_else(|| format!("no '{}' export", export_name))?;
    let box_func = instance.get_func(&mut store, "_box_func")
      .ok_or("no '_box_func' export")?;

    // Create the "done" continuation — receives the result.
    let result_val: Arc<Mutex<Option<TestResult>>> = Arc::new(Mutex::new(None));
    let result_clone = result_val.clone();

    let fn2_stub = instance.get_func(&mut store, "_fn2_stub")
      .ok_or("no '_fn2_stub' export")?;
    let done_ty = fn2_stub.ty(&store);
    let done = Func::new(&mut store, done_ty, move |mut caller, params, _results| {
      if let Some(Val::AnyRef(Some(args_list))) = params.get(1)
        && let Ok(Some(cons)) = args_list.as_struct(&caller)
        && let Ok(Val::AnyRef(Some(any_ref))) = cons.field(&mut caller, 0)
      {
        if let Ok(Some(i31)) = any_ref.as_i31(&caller) {
          *result_clone.lock().unwrap() = Some(TestResult::Bool(i31.get_i32() != 0));
        } else if let Ok(Some(struct_ref)) = any_ref.as_struct(&caller) {
          if let Ok(Val::F64(bits)) = struct_ref.field(&mut caller, 0) {
            *result_clone.lock().unwrap() = Some(TestResult::Num(f64::from_bits(bits)));
          } else if let Ok(Val::I32(offset)) = struct_ref.field(&mut caller, 0)
            && let Ok(Val::I32(length)) = struct_ref.field(&mut caller, 1)
          {
            if let Some(memory) = caller.get_export("memory")
              && let Some(mem) = memory.into_memory()
            {
              let data = mem.data(&caller);
              let start = offset as usize;
              let end = start + length as usize;
              if end <= data.len() {
                let s = String::from_utf8_lossy(&data[start..end]).into_owned();
                *result_clone.lock().unwrap() = Some(TestResult::Str(s));
              }
            }
          }
        }
      }
      Ok(())
    });

    let mut box_result = [Val::AnyRef(None)];
    box_func.call(&mut store, &[Val::FuncRef(Some(done))], &mut box_result)
      .map_err(|e| format!("_box_func failed: {}", e))?;

    let list_nil = instance.get_func(&mut store, "_list_nil")
      .ok_or("no '_list_nil' export")?;
    let mut nil = [Val::AnyRef(None)];
    list_nil.call(&mut store, &[], &mut nil)
      .map_err(|e| format!("_list_nil failed: {}", e))?;

    let list_prepend = instance.get_func(&mut store, "_list_prepend")
      .ok_or("no '_list_prepend' export")?;
    let mut args_with_cont = [Val::AnyRef(None)];
    list_prepend.call(&mut store, &[box_result[0], nil[0]], &mut args_with_cont)
      .map_err(|e| format!("_list_prepend failed: {}", e))?;

    test_fn.call(
      &mut store,
      &[Val::AnyRef(None), args_with_cont[0]],
      &mut [],
    ).map_err(|e| format!("{} failed: {}", export_name, e))?;

    Ok(result_val.lock().unwrap().take().unwrap_or(TestResult::None))
  }

  #[allow(unused)]
  fn run(src: &str) -> String {
    let wasm = crate::to_wasm(src, "test").expect("compilation failed");
    match exec_export(&wasm.binary, "test_main") {
      Ok(TestResult::Num(v)) => {
        if v == v.floor() && v.abs() < 1e15 {
          format!("{}", v as i64)
        } else {
          format!("{}", v)
        }
      }
      Ok(TestResult::Bool(b)) => format!("{}", b),
      Ok(TestResult::Str(s)) => s,
      Ok(TestResult::None) => String::new(),
      Err(e) => format!("ERROR: {}", e),
    }
  }

  #[allow(unused)]
  fn run_main(src: &str) -> String {
    let wasm = crate::to_wasm(src, "test").expect("compilation failed");
    let stdout_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));

    match wasmtime_runner::run(
      &RunOptions::default(),
      &wasm.binary,
      stdout_buf.clone(),
      stderr_buf.clone(),
    ) {
      Ok(exit_code) => {
        let stdout_bytes = stdout_buf.lock().unwrap();
        let stdout = String::from_utf8_lossy(&stdout_bytes);
        let stderr_bytes = stderr_buf.lock().unwrap();
        let stderr = String::from_utf8_lossy(&stderr_bytes);

        let mut out = format!("{}", exit_code);

        let stdout_lines: Vec<&str> = stdout.split('\n').filter(|s| !s.is_empty()).collect();
        if !stdout_lines.is_empty() {
          out.push_str("\nstdout == \":");
          for line in &stdout_lines {
            out.push_str(&format!("\n  {}", line));
          }
        }

        let stderr_lines: Vec<&str> = stderr.split('\n').filter(|s| !s.is_empty()).collect();
        if !stderr_lines.is_empty() {
          out.push_str("\nstderr == \":");
          for line in &stderr_lines {
            out.push_str(&format!("\n  {}", line));
          }
        }

        out.trim_end().to_string()
      }
      Err(e) => format!("ERROR: {}", e),
    }
  }

  test_macros::include_fink_tests!("src/runner/test_io.fnk");
  test_macros::include_fink_tests!("src/runner/test_literals.fnk");
  test_macros::include_fink_tests!("src/runner/test_bindings.fnk");
  test_macros::include_fink_tests!("src/runner/test_operators.fnk");
  test_macros::include_fink_tests!("src/runner/test_functions.fnk");
  test_macros::include_fink_tests!("src/runner/test_strings.fnk");
  test_macros::include_fink_tests!("src/runner/test_records.fnk");
  test_macros::include_fink_tests!("src/runner/test_formatting.fnk");
  test_macros::include_fink_tests!("src/runner/test_patterns.fnk");
  test_macros::include_fink_tests!("src/runner/test_ranges.fnk");
  test_macros::include_fink_tests!("src/runner/test_errors.fnk");
  test_macros::include_fink_tests!("src/runner/test_fn_match.fnk");
  test_macros::include_fink_tests!("src/runner/test_tasks.fnk");
}
