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

  // Wire up "env" imports — match builtins get real implementations,
  // everything else traps with "not yet implemented".
  let mut linker = Linker::new(&engine);
  for import in module.imports() {
    if import.module() == "env"
      && let ExternType::Func(ft) = import.ty()
    {
      let name = import.name().to_string();
      match name.as_str() {
        "match_value" => {
          linker.func_new("env", &name, ft.clone(), host_match_value)
            .map_err(|e| e.to_string())?;
        }
        "match_arm" => {
          linker.func_new("env", &name, ft.clone(), host_match_arm)
            .map_err(|e| e.to_string())?;
        }
        "match_block" => {
          linker.func_new("env", &name, ft.clone(), host_match_block)
            .map_err(|e| e.to_string())?;
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

// ---------------------------------------------------------------------------
// Host-implemented match builtins
// ---------------------------------------------------------------------------

/// Helper: unbox a $Num struct to f64.
fn unbox_num(caller: &mut Caller<'_, ()>, val: &Val) -> Result<f64> {
  if let Val::AnyRef(Some(any_ref)) = val
    && let Ok(Some(struct_ref)) = any_ref.as_struct(&mut *caller)
    && let Ok(Val::F64(bits)) = struct_ref.field(&mut *caller, 0)
  {
    Ok(f64::from_bits(bits))
  } else {
    Err(Error::msg("match_value: expected $Num"))
  }
}

/// Detect the call arity of a body function from its anyref representation.
/// For $FuncBox: extract funcref, check its param count.
/// For $ClosureN: arity = funcref_params - N_captures.
/// Falls back to 1 (body takes just cont) if detection fails.
fn detect_body_arity(caller: &mut Caller<'_, ()>, body_ref: &Val) -> usize {
  if let Val::AnyRef(Some(any_ref)) = body_ref
    && let Ok(Some(struct_ref)) = any_ref.as_struct(&mut *caller)
  {
    // Try to read field 0 as a funcref.
    if let Ok(Val::FuncRef(Some(func_ref))) = struct_ref.field(&mut *caller, 0) {
      let ft = func_ref.ty(&*caller);
      let n_fields = struct_ref.ty(&*caller).map(|t| t.fields().len()).unwrap_or(1);
      // $FuncBox has 1 field (funcref). $ClosureN has N+1 fields (funcref + N captures).
      let n_captures = n_fields.saturating_sub(1);
      // funcref param count = call_arity + n_captures.
      return ft.params().len().saturating_sub(n_captures);
    }
  }
  1 // default: body(cont)
}

/// Helper: call a WASM export by name with the given args.
fn call_export(
  caller: &mut Caller<'_, ()>,
  name: &str,
  args: &[Val],
  results: &mut [Val],
) -> Result<()> {
  let func = caller.get_export(name)
    .and_then(|e| e.into_func())
    .ok_or_else(|| Error::msg(format!("missing export '{}'", name)))?;
  func.call(caller, args, results)
}

/// match_value(val, lit, fail, cont) — compare val == lit, call cont or fail.
fn host_match_value(mut caller: Caller<'_, ()>, params: &[Val], _results: &mut [Val]) -> Result<()> {
  let val_f64 = unbox_num(&mut caller, &params[0])?;
  let lit_f64 = unbox_num(&mut caller, &params[1])?;
  let callee = if val_f64 == lit_f64 { params[3] } else { params[2] };
  call_export(&mut caller, "_croc_0", &[callee], &mut [])
}

/// match_arm(matcher, body, arm_cont) — package matcher+body, call arm_cont.
fn host_match_arm(mut caller: Caller<'_, ()>, params: &[Val], _results: &mut [Val]) -> Result<()> {
  let matcher = params[0];
  let body = params[1];
  let arm_cont = params[2];
  let mut arm_ref = [Val::AnyRef(None)];
  call_export(&mut caller, "_make_match_arm", &[matcher, body], &mut arm_ref)?;
  call_export(&mut caller, "_croc_1", &[arm_ref[0], arm_cont], &mut [])
}

/// match_block(subject, arm_0, ..., arm_N, cont) — try arms in order.
/// Hard-coded: 1 subject.
fn host_match_block(mut caller: Caller<'_, ()>, params: &[Val], _results: &mut [Val]) -> Result<()> {
  let n_params = params.len();
  let subject = params[0];
  let cont = params[n_params - 1];
  let arms: Vec<Val> = params[1..n_params - 1].to_vec();
  try_arm(&mut caller, subject, &arms, cont)
}

/// Try the first arm; on failure, try remaining arms.
fn try_arm(caller: &mut Caller<'_, ()>, subject: Val, arms: &[Val], cont: Val) -> Result<()> {
  if arms.is_empty() {
    return Err(Error::msg("match_block: no matching arm"));
  }

  let arm = arms[0];
  let remaining: Vec<Val> = arms[1..].to_vec();

  // Extract matcher and body from $MatchArm.
  let mut matcher = [Val::AnyRef(None)];
  call_export(caller, "_match_arm_get_matcher", &[arm], &mut matcher)?;
  let mut body = [Val::AnyRef(None)];
  call_export(caller, "_match_arm_get_body", &[arm], &mut body)?;

  // Fail continuation — tries the next arm.
  let engine = caller.engine().clone();
  let fail_fn = Func::new(
    caller.as_context_mut(),
    FuncType::new(&engine, vec![], vec![]),
    move |mut caller, _params, _results| {
      try_arm(&mut caller, subject, &remaining, cont)
    },
  );
  let mut boxed_fail = [Val::AnyRef(None)];
  call_export(caller, "_box_func", &[Val::FuncRef(Some(fail_fn))], &mut boxed_fail)?;

  // Succ continuation — receives bindings from matcher, calls body(bindings..., cont).
  // B = body_arity - 1 (body takes bindings + block_cont).
  let body_arity = detect_body_arity(caller, &body[0]);
  let bindings = body_arity.saturating_sub(1);
  let body_val = body[0];
  let main_fn_for_ty = caller.get_export("main")
    .and_then(|e| e.into_func())
    .expect("main export");
  let anyref = main_fn_for_ty.ty(&*caller).param(0).unwrap();
  let succ_params: Vec<ValType> = vec![anyref; bindings];
  let succ_fn = Func::new(
    caller.as_context_mut(),
    FuncType::new(&engine, succ_params, vec![]),
    move |mut caller, params, _results| {
      let dispatch_name = format!("_croc_{}", params.len() + 1);
      let mut args: Vec<Val> = params.to_vec();
      args.push(cont);      // block_cont
      args.push(body_val);   // callee (body)
      call_export(&mut caller, &dispatch_name, &args, &mut [])
    },
  );
  let mut boxed_succ = [Val::AnyRef(None)];
  call_export(caller, "_box_func", &[Val::FuncRef(Some(succ_fn))], &mut boxed_succ)?;

  // Call matcher(subject, fail, succ) via _croc_3.
  call_export(
    caller,
    "_croc_3",
    &[subject, boxed_fail[0], boxed_succ[0], matcher[0]],
    &mut [],
  )
}

