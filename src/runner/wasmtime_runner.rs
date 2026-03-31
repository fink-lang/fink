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

  // The done func must match (func (param (ref null any))) — main's param type.
  let main_ty = main_fn.ty(&store);
  let done = Func::new(&mut store, main_ty, move |mut caller, params, _results| {
    if let Some(Val::AnyRef(Some(any_ref))) = params.first() {
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

  // Box the done funcref via _box_func → $Closure0 (a struct subtype of any).
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
    FinkResult::Str(s) => println!("{}", s),
    FinkResult::None => {}
  }
  Ok(())
}

