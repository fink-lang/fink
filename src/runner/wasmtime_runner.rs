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

use std::io::Read;
use std::sync::{Arc, Condvar, Mutex};

use wasmtime::*;

use super::RunOptions;

/// A completed IO read — produced by a reader thread, consumed by host_resume.
struct CompletedRead {
  /// The WASM $Future ref to settle (owned, thread-safe).
  future_ref: OwnedRooted<AnyRef>,
  /// The data that was read (raw bytes).
  data: Vec<u8>,
}

/// Shared state between host_read (producers) and host_resume (consumer).
struct PendingIo {
  completed: Vec<CompletedRead>,
  /// Number of reads in flight (spawned but not yet completed).
  in_flight: usize,
}

/// Execute a compiled Fink module with IO channels.
///
/// Calls _run_main which handles everything internally:
/// channel setup, scheduler, IO bridging, and exit.
/// The host provides host_exit, host_write_stdout, host_write_stderr,
/// host_read, host_resume.
///
/// stdout/stderr are injected so callers control where output goes
/// (real stdio for the CLI, buffers for tests).
pub fn run(
  opts: &RunOptions,
  wasm: &[u8],
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

  let engine = Engine::new(&config).map_err(|e| e.to_string())?;
  let module = Module::new(&engine, wasm).map_err(|e| e.to_string())?;

  // TODO: move exit code handling into _run_main (return i32 directly),
  // removing the need for host_exit import and this shared state.
  let exit_code: Arc<Mutex<i64>> = Arc::new(Mutex::new(0));
  let exit_code_clone = exit_code.clone();
  let mut store = Store::new(&engine, ());

  // Shared pending IO state — reader threads push, host_resume pops.
  let pending = Arc::new((Mutex::new(PendingIo { completed: Vec::new(), in_flight: 0 }), Condvar::new()));

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
        "host_channel_send" => {
          // host_channel_send(tag, offset, length)
          // Dispatches by tag: 1=stdout, 2=stderr.
          let out = stdout.clone();
          let err = stderr.clone();
          linker.func_new("env", &name, ft.clone(), move |mut caller, params, _results| {
            let tag = params[0].unwrap_i32();
            let offset = params[1].unwrap_i32() as usize;
            let length = params[2].unwrap_i32() as usize;
            if let Some(memory) = caller.get_export("memory")
              && let Some(mem) = memory.into_memory()
            {
              let data = mem.data(&caller);
              if offset + length <= data.len() {
                let writer: &super::IoStream = if tag == 1 { &out } else { &err };
                let mut w = writer.lock().unwrap();
                w.write_all(&data[offset..offset + length]).ok();
                w.write_all(b"\n").ok();
              }
            }
            Ok(())
          }).map_err(|e| e.to_string())?;
        }
        "host_read" => {
          // host_read(stream_ref, size_ref, future_ref)
          // Spawns a thread to read from stdin, stores result for host_resume.
          let io = pending.clone();
          let input = stdin.clone();
          linker.func_new("env", &name, ft.clone(), move |mut caller, params, _results| {
            // Extract size from i31ref or $Num.
            let size = extract_i32(&mut caller, &params[1])?;

            // Root the future ref so it survives across the read thread.
            let future_any = params[2].unwrap_anyref()
              .ok_or_else(|| Error::msg("host_read: null future ref"))?;
            let future_ref = future_any.to_owned_rooted(&mut caller)
              .map_err(|e| Error::msg(format!("host_read: failed to root future: {e}")))?;

            // Track in-flight.
            let (lock, _) = &*io;
            lock.lock().unwrap().in_flight += 1;

            // Spawn reader thread.
            let io_clone = io.clone();
            let reader = input.clone();
            std::thread::spawn(move || {
              let mut buf = vec![0u8; size as usize];
              let n = {
                let mut r = reader.lock().unwrap();
                r.read(&mut buf).unwrap_or(0)
              };
              buf.truncate(n);

              let (lock, cvar) = &*io_clone;
              let mut state = lock.lock().unwrap();
              state.in_flight -= 1;
              state.completed.push(CompletedRead {
                future_ref,
                data: buf,
              });
              cvar.notify_one();
            });

            Ok(())
          }).map_err(|e| e.to_string())?;
        }
        "host_resume" => {
          // Block until at least one IO completes, then settle all ready futures.
          let io = pending.clone();
          linker.func_new("env", &name, ft.clone(), move |mut caller, _params, _results| {
            let completed = {
              let (lock, cvar) = &*io;
              let mut state = lock.lock().unwrap();

              // Nothing pending at all — return immediately.
              if state.completed.is_empty() && state.in_flight == 0 {
                return Ok(());
              }

              // Wait until at least one read completes.
              while state.completed.is_empty() {
                state = cvar.wait(state).unwrap();
              }

              // Drain all completed reads.
              std::mem::take(&mut state.completed)
            };

            // Settle each future via the WASM _settle_future export.
            let settle = caller.get_export("_settle_future")
              .and_then(|e| e.into_func())
              .ok_or_else(|| Error::msg("no _settle_future export"))?;

            for cr in completed {
              // Create a $Str from the read bytes via GC allocation.
              let value = bytes_to_str(&mut caller, &cr.data)?;
              let future_rooted = cr.future_ref.to_rooted(&mut caller);
              settle.call(&mut caller, &[Val::AnyRef(Some(future_rooted)), value], &mut [])?;
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

/// Extract an i32 from a WASM value (i31ref or $Num).
fn extract_i32(caller: &mut Caller<'_, ()>, val: &Val) -> Result<i32, Error> {
  let any = val.unwrap_anyref()
    .ok_or_else(|| Error::msg("expected non-null value"))?;
  if let Ok(Some(i31)) = any.as_i31(&*caller) {
    return Ok(i31.get_i32());
  }
  if let Ok(Some(s)) = any.as_struct(&*caller)
    && let Ok(Val::F64(bits)) = s.field(&mut *caller, 0)
  {
    return Ok(f64::from_bits(bits) as i32);
  }
  Err(Error::msg("cannot extract i32 from value"))
}

/// Create a $Str value from raw bytes via the GC API.
/// Allocates a $ByteArray on the GC heap, fills it with data,
/// then wraps it in $StrBytesImpl via the _str_wrap_bytes export.
/// No linear memory involved.
fn bytes_to_str(caller: &mut Caller<'_, ()>, data: &[u8]) -> Result<Val, Error> {
  // Create $ByteArray type: (array (mut i8)).
  let array_ty = ArrayType::new(
    caller.engine(),
    FieldType::new(Mutability::Var, StorageType::I8),
  );
  let alloc = ArrayRefPre::new(&mut *caller, array_ty);

  // Build elements as Val::I32 (i8 storage type uses i32 values).
  let elems: Vec<Val> = data.iter().map(|&b| Val::I32(b as i32)).collect();
  let array = ArrayRef::new_fixed(&mut *caller, &alloc, &elems)?;

  // Wrap in $StrBytesImpl via the runtime export.
  let wrap_fn = caller.get_export("_str_wrap_bytes")
    .and_then(|e| e.into_func())
    .ok_or_else(|| Error::msg("no _str_wrap_bytes export"))?;

  let array_any = array.to_anyref();
  let mut result = [Val::AnyRef(None)];
  wrap_fn.call(&mut *caller, &[Val::AnyRef(Some(array_any))], &mut result)?;
  Ok(result[0])
}
