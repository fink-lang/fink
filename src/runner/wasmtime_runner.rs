// Wasmtime-based WASM runner.
//
// Runs compiled Fink modules in wasmtime with WasmGC support.
// All Fink functions are CPS — the host provides the initial continuation
// that receives the result.
//
// The module exports `_box_func` to box a funcref into $FuncBox (an $Any
// subtype), so the host can create boxed continuations without needing
// direct access to GC struct types.

use std::sync::{Arc, Mutex};

use wasmtime::*;

use super::RunOptions;

/// Result of executing a Fink module.
#[derive(Debug, Clone, PartialEq)]
pub enum FinkResult {
  /// Numeric result (f64 from $Num struct).
  Num(f64),
  /// Boolean result (i32 from $Bool struct).
  Bool(bool),
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

  // Wire up all "env" imports as stubs that trap.
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

  // Find _box_func: (func (param funcref) (result (ref null $Any))).
  let box_func = instance.get_func(&mut store, "_box_func")
    .ok_or("no '_box_func' export — module may be from an older compiler")?;

  // Create the "done" continuation — receives the result.
  let result_val: Arc<Mutex<Option<FinkResult>>> = Arc::new(Mutex::new(None));
  let result_clone = result_val.clone();

  // The done func must match (func (param (ref null $Any))) — main's param type.
  let main_ty = main_fn.ty(&store);
  let done = Func::new(&mut store, main_ty, move |mut caller, params, _results| {
    if let Some(Val::AnyRef(Some(any_ref))) = params.first()
      && let Ok(Some(struct_ref)) = any_ref.as_struct(&caller)
    {
      // Try $Num (f64 field) first, then $Bool (i32 field).
      if let Ok(Val::F64(bits)) = struct_ref.field(&mut caller, 0) {
        *result_clone.lock().unwrap() = Some(FinkResult::Num(f64::from_bits(bits)));
      } else if let Ok(Val::I32(v)) = struct_ref.field(&mut caller, 0) {
        *result_clone.lock().unwrap() = Some(FinkResult::Bool(v != 0));
      }
    }
    Ok(())
  });

  // Box the done funcref via _box_func → $FuncBox (a subtype of $Any).
  let mut box_result = [Val::AnyRef(None)];
  box_func.call(&mut store, &[Val::FuncRef(Some(done))], &mut box_result)
    .map_err(|e| format!("_box_func failed: {}", e))?;

  // Call main with the boxed continuation.
  main_fn.call(&mut store, &box_result, &mut [])
    .map_err(|e| format!("main failed: {}", e))?;

  // Extract the result.
  Ok(result_val.lock().unwrap().take().unwrap_or(FinkResult::None))
}

/// Execute and print the result to stdout.
pub fn run(opts: &RunOptions, wasm: &[u8]) -> Result<(), String> {
  match exec(opts, wasm)? {
    FinkResult::Num(v) => {
      if v == v.floor() && v.abs() < 1e15 {
        println!("{}", v as i64);
      } else {
        println!("{}", v);
      }
    }
    FinkResult::Bool(b) => println!("{}", b),
    FinkResult::None => {}
  }
  Ok(())
}

