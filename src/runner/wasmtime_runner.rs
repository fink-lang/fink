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

/// Result of executing a Fink module with IO channels.
#[derive(Debug, Clone, PartialEq)]
pub struct MainResult {
  pub exit_code: i64,
  pub stdout_lines: Vec<String>,
  pub stderr_lines: Vec<String>,
}

/// Execute a compiled WASM module with stdin/stdout/stderr channel injection.
///
/// Main receives `(stdin, stdout, stderr)` as channels.
/// After main exits, stdout and stderr channels are drained.
pub fn exec_main(opts: &RunOptions, wasm: &[u8]) -> Result<MainResult, String> {
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

  // Wire up "env" imports.
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

  let main_fn = instance.get_func(&mut store, "main")
    .ok_or("no 'main' export")?;
  let box_func = instance.get_func(&mut store, "_box_func")
    .ok_or("no '_box_func' export")?;
  let list_nil = instance.get_func(&mut store, "_list_nil")
    .ok_or("no '_list_nil' export")?;
  let list_prepend = instance.get_func(&mut store, "_list_prepend")
    .ok_or("no '_list_prepend' export")?;
  let channel_new = instance.get_func(&mut store, "_channel_new")
    .ok_or("no '_channel_new' export")?;

  // Create stdin/stdout/stderr channels with i31ref tags (0, 1, 2).
  let tag_0 = Val::AnyRef(Some(AnyRef::from_i31(&mut store, I31::wrapping_u32(0))));
  let tag_1 = Val::AnyRef(Some(AnyRef::from_i31(&mut store, I31::wrapping_u32(1))));
  let tag_2 = Val::AnyRef(Some(AnyRef::from_i31(&mut store, I31::wrapping_u32(2))));
  let mut stdin_ch = [Val::AnyRef(None)];
  let mut stdout_ch = [Val::AnyRef(None)];
  let mut stderr_ch = [Val::AnyRef(None)];
  channel_new.call(&mut store, &[tag_0], &mut stdin_ch)
    .map_err(|e| format!("_channel_new(stdin) failed: {}", e))?;
  channel_new.call(&mut store, &[tag_1], &mut stdout_ch)
    .map_err(|e| format!("_channel_new(stdout) failed: {}", e))?;
  channel_new.call(&mut store, &[tag_2], &mut stderr_ch)
    .map_err(|e| format!("_channel_new(stderr) failed: {}", e))?;

  // Pre-load stdin with a test value (i31ref 42).
  let test_data = Val::AnyRef(Some(AnyRef::from_i31(&mut store, I31::wrapping_u32(42))));
  let mut msg_list = [Val::AnyRef(None)];
  list_nil.call(&mut store, &[], &mut msg_list)
    .map_err(|e| format!("_list_nil failed: {}", e))?;
  list_prepend.call(
    &mut store,
    &[test_data, msg_list[0]],
    &mut msg_list,
  ).map_err(|e| format!("_list_prepend failed: {}", e))?;

  // Set stdin.$messages (field 0) to the pre-loaded list.
  if let Val::AnyRef(Some(stdin_ref)) = &stdin_ch[0]
    && let Ok(Some(stdin_struct)) = stdin_ref.as_struct(&store)
    && let Val::AnyRef(Some(msg_ref)) = &msg_list[0]
  {
    stdin_struct.set_field(&mut store, 0, Val::AnyRef(Some(*msg_ref)))
      .map_err(|e| format!("stdin set_field failed: {}", e))?;
  }

  // Create done continuation.
  let result_val: Arc<Mutex<Option<FinkResult>>> = Arc::new(Mutex::new(None));
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
        *result_clone.lock().unwrap() = Some(FinkResult::Num(i31.get_i32() as f64));
      } else if let Ok(Some(struct_ref)) = any_ref.as_struct(&caller)
        && let Ok(Val::F64(bits)) = struct_ref.field(&mut caller, 0)
      {
        *result_clone.lock().unwrap() = Some(FinkResult::Num(f64::from_bits(bits)));
      }
    }
    Ok(())
  });

  // Box continuation.
  let mut box_result = [Val::AnyRef(None)];
  box_func.call(&mut store, &[Val::FuncRef(Some(done))], &mut box_result)
    .map_err(|e| format!("_box_func failed: {}", e))?;

  // Build args list: [cont, stdin, stdout, stderr]
  let mut args = [Val::AnyRef(None)];
  list_nil.call(&mut store, &[], &mut args)
    .map_err(|e| format!("_list_nil failed: {}", e))?;
  // Prepend in reverse order: stderr, stdout, stdin, cont
  list_prepend.call(&mut store, &[stderr_ch[0], args[0]], &mut args)
    .map_err(|e| format!("prepend stderr failed: {}", e))?;
  list_prepend.call(&mut store, &[stdout_ch[0], args[0]], &mut args)
    .map_err(|e| format!("prepend stdout failed: {}", e))?;
  list_prepend.call(&mut store, &[stdin_ch[0], args[0]], &mut args)
    .map_err(|e| format!("prepend stdin failed: {}", e))?;
  list_prepend.call(&mut store, &[box_result[0], args[0]], &mut args)
    .map_err(|e| format!("prepend cont failed: {}", e))?;

  // Call main.
  main_fn.call(
    &mut store,
    &[Val::AnyRef(None), args[0]],
    &mut [],
  ).map_err(|e| format!("main failed: {}", e))?;

  // Extract exit code.
  let exit_code = match result_val.lock().unwrap().take() {
    Some(FinkResult::Num(v)) => v as i64,
    _ => 0,
  };

  // Drain channel messages: read $Channel.$messages (field 0), walk $Cons list.
  let stdout_lines = drain_channel(&mut store, &instance, &stdout_ch[0])?;
  let stderr_lines = drain_channel(&mut store, &instance, &stderr_ch[0])?;

  Ok(MainResult { exit_code, stdout_lines, stderr_lines })
}

/// Drain all messages from a channel's $messages list.
/// Each message is decoded as a string ($StrDataImpl: offset + length).
fn drain_channel(
  store: &mut Store<()>,
  instance: &Instance,
  channel_val: &Val,
) -> Result<Vec<String>, String> {
  let mut lines = Vec::new();

  let memory = instance.get_export(&mut *store, "memory")
    .and_then(|e| e.into_memory())
    .ok_or("no 'memory' export")?;

  // Get $Channel struct, read $messages (field 0).
  let ch_ref = match channel_val {
    Val::AnyRef(Some(r)) => *r,
    _ => return Ok(lines),
  };
  let ch_struct = ch_ref.as_struct(&*store)
    .map_err(|e| format!("channel as_struct: {}", e))?;
  let ch_struct = match ch_struct {
    Some(s) => s,
    None => return Ok(lines),
  };

  // Walk the $Cons list in $messages (field 0).
  let mut list_val = ch_struct.field(&mut *store, 0)
    .map_err(|e| format!("channel field 0: {}", e))?;

  while let Val::AnyRef(Some(r)) = &list_val {
    // Try to read as $Cons (head + tail). $Nil has 0 fields — field(0) will fail.
    let cons = match r.as_struct(&*store) {
      Ok(Some(s)) => s,
      _ => break,
    };

    // Head = message value. If field(0) fails, this is $Nil — done.
    let head = match cons.field(&mut *store, 0) {
      Ok(v) => v,
      Err(_) => break,
    };

    // Decode message as string.
    if let Val::AnyRef(Some(msg_ref)) = &head {
      if let Ok(Some(i31)) = msg_ref.as_i31(&*store) {
        // i31ref — format as integer.
        lines.push(format!("{}", i31.get_i32()));
      } else if let Ok(Some(msg_struct)) = msg_ref.as_struct(&*store) {
        if let Ok(Val::I32(offset)) = msg_struct.field(&mut *store, 0)
          && let Ok(Val::I32(length)) = msg_struct.field(&mut *store, 1)
        {
          let data = memory.data(&*store);
          let start = offset as usize;
          let end = start + length as usize;
          if end <= data.len() {
            lines.push(String::from_utf8_lossy(&data[start..end]).into_owned());
          }
        } else if let Ok(Val::F64(bits)) = msg_struct.field(&mut *store, 0) {
          let v = f64::from_bits(bits);
          if v == v.floor() && v.abs() < 1e15 {
            lines.push(format!("{}", v as i64));
          } else {
            lines.push(format!("{}", v));
          }
        }
      }
    }

    // Tail = next cons or nil.
    list_val = cons.field(&mut *store, 1)
      .map_err(|e| format!("cons tail: {}", e))?;
  };

  Ok(lines)
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

