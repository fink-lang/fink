// Wasmtime-based WASM runner.
//
// Runs compiled Fink modules in wasmtime with WasmGC support. Each
// fragment exports a host-facing wrapper under its canonical URL
// (`./<basename>` for the entry). The wrapper composes the module
// body with `init_module`, taking `(key, cont_id)` and tail-calling
// the cont with `(last_expr, val)` where `val = registry[mod_url][key]`.
//
// CLI flow:
//   1. Call entry wrapper with key=b"main", cont_id=1.
//   2. host_invoke_cont(cont_id=1) receives (last_expr, main_clo).
//      - If main_clo is a real $Closure, build cli args list and
//        apply main_clo with cont_id=2 from inside the callback.
//      - Otherwise treat last_expr as the program result.
//   3. host_invoke_cont(cont_id=2) receives main's result; the head
//      is the exit code.

use std::io::Read;
use std::sync::{Arc, Mutex};

use wasmtime::*;

use super::RunOptions;

/// Cont id used for the wrapper's done continuation. The wrapper
/// fires its cont with `(last_expr, main_clo)` once the module body
/// has finished evaluating.
const CONT_WRAPPER_DONE: i32 = 1;

/// Cont id used for the main-result continuation. Fired with
/// `(exit_value)` when `main` finishes.
const CONT_MAIN_DONE: i32 = 2;

/// State shared between the `host_invoke_cont` callback and the
/// outer driver. The callback writes the exit code; the driver reads
/// it once the wrapper call returns.
#[derive(Default)]
struct ExitState {
  /// Set when one of the cont callbacks captures a final exit code.
  /// Defaults to 0 if no main / no numeric result.
  exit_code: i64,
  /// Closures handed to `host_yield` from wasm. The outer driver pops
  /// them after the entry call returns and feeds each into
  /// `_invoke_resume` to re-enter the scheduler. Rooted across the
  /// callback boundary via `ManuallyRooted`.
  pending_resumes: Vec<(wasmtime::OwnedRooted<wasmtime::AnyRef>, Option<wasmtime::OwnedRooted<wasmtime::AnyRef>>)>,
}

/// Execute a compiled Fink module via its host-facing wrapper export.
///
/// `args` is the CLI argv passed to `main` — `argv[0]` is the program
/// name. Returns the exit code from `main` (or 0 if the program has
/// no `main`). `wasm` carries the binary plus optional debug
/// metadata (`marks`, `id_to_url`); annotation routes errors through
/// that metadata when present.
pub fn run(
  opts: &RunOptions,
  wasm: &crate::passes::Wasm,
  args: Vec<Vec<u8>>,
  stdin: Arc<Mutex<dyn Read + Send>>,
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

  let bytes: &[u8] = &wasm.binary;
  let engine = Engine::new(&config).map_err(|e| e.to_string())?;
  let module = Module::new(&engine, bytes)
    .map_err(|e| crate::passes::wasm::annotate_func_indices(&e.to_string(), bytes))?;
  let mut store = Store::new(&engine, ());

  let exit_state: Arc<Mutex<ExitState>> = Arc::new(Mutex::new(ExitState::default()));
  let cli_args = Arc::new(args);

  let mut linker = Linker::new(&engine);
  for import in module.imports() {
    if import.module() == "env"
      && let ExternType::Func(ft) = import.ty()
    {
      let name = import.name().to_string();
      match name.as_str() {
        "host_panic" => {
          linker.func_new("env", &name, ft, move |_caller, _params, _results| {
            Err(Error::msg("fink panic: irrefutable pattern failed"))
          }).map_err(|e| e.to_string())?;
        }
        "host_read_sync" => {
          let input = stdin.clone();
          linker.func_new("env", &name, ft, move |mut caller, params, results| {
            let size = extract_i32(&mut caller, &params[1])?;
            let mut buf = vec![0u8; size as usize];
            let n = {
              let mut r = input.lock().unwrap();
              r.read(&mut buf).unwrap_or(0)
            };
            buf.truncate(n);
            // Build a ByteArray (i8 array) to return.
            let array_ty = wasmtime::ArrayType::new(
              caller.engine(),
              wasmtime::FieldType::new(wasmtime::Mutability::Var, wasmtime::StorageType::I8),
            );
            let alloc = wasmtime::ArrayRefPre::new(&mut caller, array_ty);
            let elems: Vec<Val> = buf.iter().map(|&b| Val::I32(b as i32)).collect();
            let array = wasmtime::ArrayRef::new_fixed(&mut caller, &alloc, &elems)
              .map_err(|e| Error::msg(format!("host_read_sync byte array: {e}")))?;
            results[0] = Val::AnyRef(Some(array.to_anyref()));
            Ok(())
          }).map_err(|e| e.to_string())?;
        }
        "host_write" => {
          let out = stdout.clone();
          let err = stderr.clone();
          linker.func_new("env", &name, ft, move |mut caller, params, _results| {
            let fd = match params[0].unwrap_anyref() {
              Some(a) => {
                if let Ok(Some(i)) = a.as_i31(&caller) {
                  i.get_i32()
                } else if let Ok(Some(s)) = a.as_struct(&caller) {
                  match s.field(&mut caller, 0) {
                    Ok(Val::I64(v)) => v as i32,
                    _ => return Err(Error::msg("host_write: fd struct field0 unreadable")),
                  }
                } else {
                  return Err(Error::msg("host_write: fd not i31 or numeric struct"));
                }
              }
              None => return Err(Error::msg("host_write: null fd")),
            };
            let bytes_any = params[1].unwrap_anyref();
            if let Some(r) = bytes_any
              && let Some(arr) = r.as_array(&caller).ok().flatten() {
              let len = arr.len(&caller).unwrap_or(0);
              let mut buf = Vec::with_capacity(len as usize);
              for i in 0..len {
                if let Ok(Val::I32(b)) = arr.get(&mut caller, i) {
                  buf.push(b as u8);
                }
              }
              let writer: &super::IoStream = if fd == 2 { &err } else { &out };
              writer.lock().unwrap().write_all(&buf).ok();
            }
            Ok(())
          }).map_err(|e| e.to_string())?;
        }
        "host_yield" => {
          let state = exit_state.clone();
          linker.func_new("env", &name, ft, move |mut caller, params, _results| {
            let resume = params[0].unwrap_anyref()
              .ok_or_else(|| Error::msg("host_yield: null resume"))?;
            let resume_owned = resume.to_owned_rooted(&mut caller)?;
            let ctx_owned = match params[1].unwrap_anyref() {
              Some(c) => Some(c.to_owned_rooted(&mut caller)?),
              None => None,
            };
            state.lock().unwrap().pending_resumes.push((resume_owned, ctx_owned));
            Ok(())
          }).map_err(|e| e.to_string())?;
        }
        "host_invoke_cont" => {
          let exit = exit_state.clone();
          let argv = cli_args.clone();
          linker.func_new("env", &name, ft, move |mut caller, params, _results| {
            let cont_id = params[0].unwrap_i32();
            let args_any = params[1].unwrap_anyref()
              .ok_or_else(|| Error::msg("host_invoke_cont: null args"))?;
            let cons = args_any.unwrap_struct(&caller)?;

            // CONT_WRAPPER_DONE: args = [last_expr, main_clo].
            //   - capture last_expr as a provisional exit code, then
            //     if main_clo is a usable $Closure, apply it with the
            //     cli args + cont id 2 from inside this callback.
            // CONT_MAIN_DONE: args = [main_result].
            //   - main's actual result; capture as final exit code.

            let head = cons.field(&mut caller, 0).ok();
            capture_exit_code(&mut caller, head.as_ref(), &exit);

            if cont_id != CONT_WRAPPER_DONE {
              return Ok(());
            }

            // Walk to args[1] = exports_rec via the Cons tail.
            let exports_rec = match cons.field(&mut caller, 1).ok() {
              Some(Val::AnyRef(Some(tail_ref))) => {
                match tail_ref.as_struct(&caller) {
                  Ok(Some(tail_st)) => match tail_st.field(&mut caller, 0).ok() {
                    Some(Val::AnyRef(Some(r))) => r,
                    _ => return Ok(()),
                  },
                  _ => return Ok(()),
                }
              }
              _ => return Ok(()),
            };

            // Look up `main` in the exports rec via the interop helper.
            let main_clo = match lookup_export_by_bytes(
              &mut caller, exports_rec, b"main")? {
              Some(r) => r,
              None => return Ok(()),
            };

            // Reset the provisional exit code — main will overwrite
            // via cont id 2.
            exit.lock().unwrap().exit_code = 0;

            apply_main(&mut caller, main_clo, &argv)?;
            Ok(())
          }).map_err(|e| e.to_string())?;
        }
        _ => {
          let err_name = name.clone();
          linker.func_new("env", &name, ft, move |_caller, _params, _results| {
            Err(Error::msg(format!("builtin '{}' not yet implemented", err_name)))
          }).map_err(|e| e.to_string())?;
        }
      }
    }
  }

  let instance = linker.instantiate(&mut store, &module).map_err(|e| e.to_string())?;

  // Find the entry wrapper. compile_package always exports the entry
  // under its canonical URL `./<basename>`. We scan exports rather
  // than reconstructing the URL so finkrt (which has no source path
  // at run-time) and the CLI share one code path.
  let entry_wrapper_name = find_entry_wrapper(&module)?;
  let entry_wrapper = instance.get_func(&mut store, &entry_wrapper_name)
    .ok_or_else(|| format!("no '{entry_wrapper_name}' export"))?;

  // Host-side: turn the wrapper-done cont id into a fink anyref via
  // `wrap_host_cont_3`. The per-module wrapper signature is host-
  // neutral (`(ref null any) -> ()`); host-bridge mechanics
  // (i32 -> anyref) live on the host side of the boundary. Fn3
  // adapter is required because the apply shim now dispatches all
  // closures as Fn3.
  let wrap_host_cont = instance.get_func(&mut store, "wrap_host_cont_3")
    .ok_or_else(|| "no wrap_host_cont_3 export".to_string())?;
  let mut entry_cont_out = [Val::AnyRef(None)];
  wrap_host_cont.call(&mut store, &[Val::I32(CONT_WRAPPER_DONE)], &mut entry_cont_out)
    .map_err(|e| e.to_string())?;
  let entry_cont = entry_cont_out[0];

  if let Err(e) = entry_wrapper.call(&mut store, &[entry_cont], &mut []) {
    // Translate the wasmtime trap into a Diagnostic via the linked
    // binary's debug marks. Render with the pretty caret+context form
    // if we have source for the failing module; one-line otherwise.
    let entry_url = wasm.id_to_url.values().next().cloned().unwrap_or_default();
    let diag = super::trap::diagnose(&e, wasm, &entry_url);
    let provider = super::trap::PackageSourceProvider::new(wasm);
    let pretty = crate::errors::format_diagnostic(
      &provider, &diag, &crate::errors::FormatOptions::default(),
    );
    return Err(pretty);
  }

  // Drive any resumes the wasm scheduler handed off via host_yield.
  // Each call may itself yield further resumes, so loop until the
  // queue is empty.
  let invoke_resume = instance.get_func(&mut store, "invoke_resume");
  if let Some(invoke) = invoke_resume {
    loop {
      let pending = std::mem::take(&mut exit_state.lock().unwrap().pending_resumes);
      if pending.is_empty() {
        break;
      }
      for (resume_owned, ctx_owned) in pending {
        let resume_rooted = resume_owned.to_rooted(&mut store);
        let ctx_val = match ctx_owned {
          Some(c) => Val::AnyRef(Some(c.to_rooted(&mut store))),
          None => Val::AnyRef(None),
        };
        // Placeholder value for yield-style resume (unit). For io,
        // host would substitute the actual io result.
        let unit_val = Val::AnyRef(Some(AnyRef::from_i31(&mut store, I31::wrapping_i32(0))));
        invoke
          .call(&mut store, &[Val::AnyRef(Some(resume_rooted)), unit_val, ctx_val], &mut [])
          .map_err(|e| crate::passes::wasm::annotate_func_indices(
            &format!("invoke_resume: {e}"), bytes))?;
      }
    }
  }

  Ok(exit_state.lock().unwrap().exit_code)
}

/// Scan module exports for the entry wrapper. compile_package emits
/// each module's wrapper under its canonical URL, and the entry's
/// canonical URL is always `./<basename>`. Picks the first such export.
fn find_entry_wrapper(module: &Module) -> Result<String, String> {
  for export in module.exports() {
    let name = export.name();
    if name.starts_with("./")
      && let ExternType::Func(_) = export.ty()
    {
      return Ok(name.to_string());
    }
  }
  Err("no entry wrapper export (expected one starting with './')".into())
}

/// Look up `key` in `rec` by raw bytes. Calls the interop helper
/// `rec_get_by_bytes` on the running instance — host wraps key bytes
/// into a `$ByteArray`, hands to the helper, gets a fink anyref back
/// (or null if the key is absent / value is itself null).
fn lookup_export_by_bytes(
  caller: &mut Caller<'_, ()>,
  rec: Rooted<AnyRef>,
  key: &[u8],
) -> Result<Option<Rooted<AnyRef>>, Error> {
  let rec_get_by_bytes = caller.get_export("rec_get_by_bytes")
    .and_then(|e| e.into_func())
    .ok_or_else(|| Error::msg("no rec_get_by_bytes export"))?;
  let array_ty = ArrayType::new(
    caller.engine(),
    FieldType::new(Mutability::Var, StorageType::I8),
  );
  let alloc = ArrayRefPre::new(&mut *caller, array_ty);
  let elems: Vec<Val> = key.iter().map(|&b| Val::I32(b as i32)).collect();
  let array = ArrayRef::new_fixed(&mut *caller, &alloc, &elems)
    .map_err(|e| Error::msg(format!("key bytes alloc: {e}")))?;
  let mut out = [Val::AnyRef(None)];
  rec_get_by_bytes.call(&mut *caller,
    &[Val::AnyRef(Some(rec)), Val::AnyRef(Some(array.to_anyref()))],
    &mut out)?;
  Ok(match out[0] {
    Val::AnyRef(Some(r)) => Some(r),
    _ => None,
  })
}


/// Apply `main_clo` with the program's cli args and a fresh done
/// continuation (cont id 2). Called from inside `host_invoke_cont` so
/// `main_clo` stays rooted on the wasm stack.
fn apply_main(
  caller: &mut Caller<'_, ()>,
  main_clo: Rooted<AnyRef>,
  argv: &[Vec<u8>],
) -> Result<(), Error> {
  // Fn3 pipeline: wrap_host_cont_3 + apply_3 with an empty ctx minted
  // by the wasm-side `env:empty_ctx` — same shape as the per-module
  // wrapper's entry call.
  let wrap_host_cont = caller.get_export("wrap_host_cont_3")
    .and_then(|e| e.into_func())
    .ok_or_else(|| Error::msg("no wrap_host_cont_3 export"))?;
  let args_empty = caller.get_export("args_empty")
    .and_then(|e| e.into_func())
    .ok_or_else(|| Error::msg("no args_empty export"))?;
  let args_prepend = caller.get_export("args_prepend")
    .and_then(|e| e.into_func())
    .ok_or_else(|| Error::msg("no args_prepend export"))?;
  let str_wrap = caller.get_export("str_wrap_bytes")
    .and_then(|e| e.into_func())
    .ok_or_else(|| Error::msg("no str_wrap_bytes export"))?;
  let apply_fn = caller.get_export("apply_3")
    .and_then(|e| e.into_func())
    .ok_or_else(|| Error::msg("no apply_3 export"))?;
  let empty_ctx_fn = caller.get_export("empty_ctx")
    .and_then(|e| e.into_func())
    .ok_or_else(|| Error::msg("no empty_ctx export"))?;

  // done_cont = wrap_host_cont(CONT_MAIN_DONE).
  let mut done_out = [Val::AnyRef(None)];
  wrap_host_cont.call(&mut *caller, &[Val::I32(CONT_MAIN_DONE)], &mut done_out)?;
  let done_cont = done_out[0];

  // Materialise each cli arg as a $Str.
  let array_ty = ArrayType::new(
    caller.engine(),
    FieldType::new(Mutability::Var, StorageType::I8),
  );
  let alloc = ArrayRefPre::new(&mut *caller, array_ty);
  let mut main_args_vals: Vec<Val> = vec![done_cont];
  for bytes in argv {
    let elems: Vec<Val> = bytes.iter().map(|&b| Val::I32(b as i32)).collect();
    let array = ArrayRef::new_fixed(&mut *caller, &alloc, &elems)
      .map_err(|e| Error::msg(format!("byte array alloc: {e}")))?;
    let mut wrapped = [Val::AnyRef(None)];
    str_wrap.call(&mut *caller, &[Val::AnyRef(Some(array.to_anyref()))], &mut wrapped)?;
    main_args_vals.push(wrapped[0]);
  }

  // Cons-chain the args (cont at head, then argv in order).
  let mut acc_out = [Val::AnyRef(None)];
  args_empty.call(&mut *caller, &[], &mut acc_out)?;
  let mut acc = acc_out[0];
  for v in main_args_vals.iter().rev() {
    let mut next = [Val::AnyRef(None)];
    args_prepend.call(&mut *caller, &[*v, acc], &mut next)?;
    acc = next[0];
  }

  let mut ctx_out = [Val::AnyRef(None)];
  empty_ctx_fn.call(&mut *caller, &[], &mut ctx_out)?;
  apply_fn.call(&mut *caller,
    &[acc, ctx_out[0], Val::AnyRef(Some(main_clo))],
    &mut [])?;
  Ok(())
}

/// Inspect an anyref and store its numeric form into the exit code
/// slot. Bools/i31s and `$Num` structs map to integers; anything else
/// leaves the slot untouched.
fn capture_exit_code(
  caller: &mut Caller<'_, ()>,
  val: Option<&Val>,
  exit: &Arc<Mutex<ExitState>>,
) {
  let Some(Val::AnyRef(Some(r))) = val else { return; };
  if let Ok(Some(i31)) = r.as_i31(&*caller) {
    exit.lock().unwrap().exit_code = i31.get_i32() as i64;
    return;
  }
  if let Ok(Some(st)) = r.as_struct(&*caller) {
    match st.field(&mut *caller, 0) {
      Ok(Val::I64(v)) => {
        // $Int subtype: field 0 is i64.
        exit.lock().unwrap().exit_code = v;
      }
      Ok(Val::F64(bits)) => {
        // $F64 / $Decimal: field 0 is f64.
        exit.lock().unwrap().exit_code = f64::from_bits(bits) as i64;
      }
      _ => {}
    }
  }
}

/// Extract an i32 from a wasm value (i31ref, `$F64`/`$Decimal` struct, or `$Int` struct).
fn extract_i32(caller: &mut Caller<'_, ()>, val: &Val) -> Result<i32, Error> {
  let any = val.unwrap_anyref()
    .ok_or_else(|| Error::msg("expected non-null value"))?;
  if let Ok(Some(i31)) = any.as_i31(&*caller) {
    return Ok(i31.get_i32());
  }
  if let Ok(Some(s)) = any.as_struct(&*caller) {
    match s.field(&mut *caller, 0) {
      Ok(Val::I64(v)) => return Ok(v as i32),
      Ok(Val::F64(bits)) => return Ok(f64::from_bits(bits) as i32),
      _ => {}
    }
  }
  Err(Error::msg("cannot extract i32 from value"))
}

