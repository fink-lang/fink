// Wasmtime-based WASM runner.
//
// Runs compiled Fink modules in wasmtime with WasmGC support.
// All Fink functions are CPS — the host provides the initial continuation
// that receives the result.
//
// The module exports `_box_func` to box a funcref into $Closure0 (a struct
// subtype of any), so the host can create boxed continuations without
// needing direct access to GC struct types.
//
// Value representation:
//   - Numbers: $Num struct (f64 field)
//   - Booleans: i31ref (0 = false, 1 = true)
//   - Functions: $Closure0 (funcref field) or $ClosureN

use std::sync::{Arc, Mutex};

use wasmtime::*;

use super::RunOptions;

/// Execute a compiled Fink module with IO channels.
///
/// Calls _run_main which handles everything internally:
/// channel setup, scheduler, IO bridging, and exit.
/// The host provides host_exit, host_write_stdout, host_write_stderr.
///
/// stdout/stderr are injected so callers control where output goes
/// (real stdio for the CLI, buffers for tests).
pub fn run(
  opts: &RunOptions,
  wasm: &[u8],
  stdout: super::IoStream,
  stderr: super::IoStream,
) -> Result<i64, String> {
  let mut config = Config::new();
  config.wasm_gc(true);
  config.wasm_tail_call(true);
  config.wasm_function_references(true);
  if opts.debug {
    config.debug_info(true);
    config.cranelift_opt_level(OptLevel::None);
  }

  let engine = Engine::new(&config).map_err(|e| e.to_string())?;
  let module = Module::new(&engine, wasm).map_err(|e| e.to_string())?;

  // TODO: move exit code handling into _run_main (return i32 directly),
  // removing the need for host_exit import and this shared state.
  let exit_code: Arc<Mutex<i64>> = Arc::new(Mutex::new(0));
  let exit_code_clone = exit_code.clone();
  let mut store = Store::new(&engine, ());

  // Wire up "env" imports.
  let mut linker = Linker::new(&engine);
  for import in module.imports() {
    if import.module() == "env"
      && let ExternType::Func(ft) = import.ty()
    {
      let name = import.name().to_string();
      match name.as_str() {
        "host_exit" => {
          let code = exit_code_clone.clone();
          linker.func_new("env", &name, ft.clone(), move |_caller, params, _results| {
            *code.lock().unwrap() = params[0].unwrap_i32() as i64;
            Ok(())
          }).map_err(|e| e.to_string())?;
        }
        "host_write_stdout" => {
          let out = stdout.clone();
          linker.func_new("env", &name, ft.clone(), move |mut caller, params, _results| {
            let offset = params[0].unwrap_i32() as usize;
            let length = params[1].unwrap_i32() as usize;
            if let Some(memory) = caller.get_export("memory")
              && let Some(mem) = memory.into_memory()
            {
              let data = mem.data(&caller);
              if offset + length <= data.len() {
                let mut w = out.lock().unwrap();
                w.write_all(&data[offset..offset + length]).ok();
                w.write_all(b"\n").ok();
              }
            }
            Ok(())
          }).map_err(|e| e.to_string())?;
        }
        "host_write_stderr" => {
          let err = stderr.clone();
          linker.func_new("env", &name, ft.clone(), move |mut caller, params, _results| {
            let offset = params[0].unwrap_i32() as usize;
            let length = params[1].unwrap_i32() as usize;
            if let Some(memory) = caller.get_export("memory")
              && let Some(mem) = memory.into_memory()
            {
              let data = mem.data(&caller);
              if offset + length <= data.len() {
                let mut w = err.lock().unwrap();
                w.write_all(&data[offset..offset + length]).ok();
                w.write_all(b"\n").ok();
              }
            }
            Ok(())
          }).map_err(|e| e.to_string())?;
        }
        _ => {
          let err_name = name.clone();
          linker.func_new("env", &name, ft.clone(), move |_caller, _params, _results| {
            Err(Error::msg(format!("builtin '{}' not yet implemented", err_name)))
          }).map_err(|e| e.to_string())?;
        }
      }
    }
  }

  let instance = linker.instantiate(&mut store, &module).map_err(|e| e.to_string())?;

  // Look up the user's main function, box it, and pass to _run_main.
  let main_fn = instance.get_func(&mut store, "main")
    .ok_or("no 'main' export")?;
  let box_func = instance.get_func(&mut store, "_box_func")
    .ok_or("no '_box_func' export")?;
  let mut boxed_main = [Val::AnyRef(None)];
  box_func.call(&mut store, &[Val::FuncRef(Some(main_fn))], &mut boxed_main)
    .map_err(|e| format!("_box_func failed: {}", e))?;

  let run_main = instance.get_func(&mut store, "_run_main")
    .ok_or("no '_run_main' export")?;
  run_main.call(&mut store, &[boxed_main[0]], &mut [])
    .map_err(|e| format!("_run_main failed: {}", e))?;

  Ok(*exit_code.lock().unwrap())
}

