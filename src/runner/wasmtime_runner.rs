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

/// Result of executing a Fink module.
#[derive(Debug, Clone, PartialEq)]
pub enum FinkResult {
  /// Numeric result (f64 from $Num struct).
  Num(f64),
  /// Boolean result (i31ref: 0 = false, 1 = true).
  Bool(bool),
  /// String result ($StrDataImpl: offset + length into linear memory).
  Str(String),
  /// No result returned.
  None,
}

/// Execute a compiled WASM module and return the result.
pub fn exec(opts: &RunOptions, wasm: &[u8]) -> Result<FinkResult, String> {
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

  // Find the main function.
  let main_fn = instance.get_func(&mut store, "main")
    .ok_or("no 'main' export")?;

  // Find _box_func: (func (param funcref) (result (ref null any))).
  let box_func = instance.get_func(&mut store, "_box_func")
    .ok_or("no '_box_func' export — module may be from an older compiler")?;

  // Create the "done" continuation — receives the result.
  let result_val: Arc<Mutex<Option<FinkResult>>> = Arc::new(Mutex::new(None));
  let result_clone = result_val.clone();

  // The done func must use the module's $Fn2 type (from the canonical rec group).
  // _fn2_stub is a dummy function exported solely for its type.
  let fn2_stub = instance.get_func(&mut store, "_fn2_stub")
    .ok_or("no '_fn2_stub' export")?;
  let done_ty = fn2_stub.ty(&store);
  let done = Func::new(&mut store, done_ty, move |mut caller, params, _results| {
    // params[0] = captures (ignore), params[1] = args list ($Cons).
    // The result value is the head (field 0) of the args list.
    if let Some(Val::AnyRef(Some(args_list))) = params.get(1)
      && let Ok(Some(cons)) = args_list.as_struct(&caller)
      && let Ok(Val::AnyRef(Some(any_ref))) = cons.field(&mut caller, 0)
    {
          // Try i31ref first (booleans), then $Num struct (f64 field),
          // then $StrDataImpl (two i32 fields: offset, length).
          if let Ok(Some(i31)) = any_ref.as_i31(&caller) {
            *result_clone.lock().unwrap() = Some(FinkResult::Bool(i31.get_i32() != 0));
          } else if let Ok(Some(struct_ref)) = any_ref.as_struct(&caller) {
            if let Ok(Val::F64(bits)) = struct_ref.field(&mut caller, 0) {
              *result_clone.lock().unwrap() = Some(FinkResult::Num(f64::from_bits(bits)));
            } else if let Ok(Val::I32(offset)) = struct_ref.field(&mut caller, 0)
              && let Ok(Val::I32(length)) = struct_ref.field(&mut caller, 1)
            {
              // $StrDataImpl — read bytes from linear memory.
              if let Some(memory) = caller.get_export("memory")
                && let Some(mem) = memory.into_memory()
              {
                let data = mem.data(&caller);
                let start = offset as usize;
                let end = start + length as usize;
                if end <= data.len() {
                  let s = String::from_utf8_lossy(&data[start..end]).into_owned();
                  *result_clone.lock().unwrap() = Some(FinkResult::Str(s));
                }
              }
            }
          }
    }
    Ok(())
  });

  // Box the done funcref via _box_func → $Closure (a struct subtype of any).
  let mut box_result = [Val::AnyRef(None)];
  box_func.call(&mut store, &[Val::FuncRef(Some(done))], &mut box_result)
    .map_err(|e| format!("_box_func failed: {}", e))?;

  // Unified $Fn2: main is $Fn2(captures, args). Cont is first element of args.
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

  // Call main: (null_caps, args_with_cont).
  main_fn.call(
    &mut store,
    &[Val::AnyRef(None), args_with_cont[0]],
    &mut [],
  ).map_err(|e| format!("main failed: {}", e))?;

  // Extract the result.
  Ok(result_val.lock().unwrap().take().unwrap_or(FinkResult::None))
}

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
  use std::io::Write;

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

  // Call _run_main — handles everything internally.
  let run_main = instance.get_func(&mut store, "_run_main")
    .ok_or("no '_run_main' export")?;
  run_main.call(&mut store, &[], &mut [])
    .map_err(|e| format!("_run_main failed: {}", e))?;

  Ok(*exit_code.lock().unwrap())
}

