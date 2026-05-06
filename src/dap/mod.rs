//! Debug Adapter Protocol server for ƒink.
//!
//! Speaks DAP on stdin/stdout, controls WASM execution via Wasmtime's
//! guest debug API, and maps WASM byte offsets back to ƒink source
//! locations using the compiler-generated source map.
//!
//! ```text
//!   VSCode ←DAP stdin/stdout→ fink dap ←Wasmtime debug API→ WASM
//! ```
//!
//! The WASM thread runs in Wasmtime with `guest_debug` enabled. When a
//! breakpoint fires, the `DebugHandler` sends frame info to the DAP
//! server via a channel, then blocks waiting for a resume command. The
//! DAP server translates the WASM PC to a source location and reports it
//! to the editor.

use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::{Arc, Mutex, mpsc};

use dap::events::{Event, ExitedEventBody, StoppedEventBody};
use dap::requests::Command;
use dap::responses::{
  ContinueResponse, ResponseBody, ScopesResponse, SetBreakpointsResponse,
  StackTraceResponse, ThreadsResponse, VariablesResponse,
};
use dap::server::Server;
use dap::types::*;


/// Map a WASM PC offset to a (line, col) in the Fink source.
/// Returns 1-indexed line and column for DAP.
fn pc_to_source_location(
  pc: u32,
  mappings: &[crate::passes::wasm::sourcemap::WasmMapping],
) -> Option<(i64, i64)> {
  // Find the closest mapping at or before the PC offset.
  // Mappings are in emission order, which is roughly ascending by offset.
  let mut best: Option<&crate::passes::wasm::sourcemap::WasmMapping> = None;
  for m in mappings {
    if m.wasm_offset <= pc {
      match best {
        Some(b) if b.wasm_offset > m.wasm_offset => {}
        _ => best = Some(m),
      }
    }
  }
  best.map(|m| (m.src_line as i64, m.src_col as i64))
}

/// Look up a `MarkRecord` by linked-binary PC. Wasmtime fires
/// breakpoints at the exact PC we registered, so an exact-match scan
/// suffices. Falls back to nearest-preceding mark when there's no
/// exact match — guards against any small drift introduced by
/// `rewrite_body`'s LEB128 changes during link-time PC shifting.
/// Returning the full record lets callers read `source` *and*
/// `module_id` to resolve which file the stop belongs to.
fn pc_to_mark(
  pc: u32,
  marks: &[crate::passes::debug_marks::MarkRecord],
) -> Option<&crate::passes::debug_marks::MarkRecord> {
  if let Some(m) = marks.iter().find(|m| m.wasm_pc == pc) {
    return Some(m);
  }
  // Nearest preceding — same logic as pc_to_source_location.
  let mut best: Option<&crate::passes::debug_marks::MarkRecord> = None;
  for m in marks {
    if m.wasm_pc <= pc {
      match best {
        Some(b) if b.wasm_pc > m.wasm_pc => {}
        _ => best = Some(m),
      }
    }
  }
  best
}

// ── Runner bootstrap ────────────────────────────────────────────────────────

/// Drive a compiled fink module through the host-wrapper protocol via
/// `.call_async` so it can run under `guest_debug`. Same shape as
/// `src/runner/wasmtime_runner.rs::run` (find entry wrapper, call with
/// key=b"main" + cont id 1, dispatch main from inside `host_invoke_cont`),
/// just async. The host_invoke_cont callback that drives main lives in
/// the linker setup at the call site — here we only need to call the
/// wrapper.
async fn run_module(
  store: &mut wasmtime::Store<DebugState>,
  linker: &wasmtime::Linker<DebugState>,
  module: &wasmtime::Module,
) -> Result<(), String> {
  let instance = linker.instantiate_async(&mut *store, module).await
    .map_err(|e| format!("instantiation error: {e}"))?;

  let entry_wrapper_name = find_entry_wrapper(module)?;
  let entry_wrapper = instance.get_func(&mut *store, &entry_wrapper_name)
    .ok_or_else(|| format!("no '{entry_wrapper_name}' export"))?;

  // Host-side i32 -> anyref wrap (host-bridge bookkeeping, not part
  // of the per-module wrapper ABI).
  let wrap_host_cont = instance.get_func(&mut *store, "wrap_host_cont")
    .ok_or_else(|| "no wrap_host_cont export".to_string())?;
  let mut entry_cont_out = [wasmtime::Val::AnyRef(None)];
  wrap_host_cont.call_async(&mut *store,
    &[wasmtime::Val::I32(CONT_WRAPPER_DONE)], &mut entry_cont_out).await
    .map_err(|e| format!("wrap_host_cont: {e}"))?;
  let entry_cont = entry_cont_out[0];

  entry_wrapper
    .call_async(&mut *store, &[entry_cont], &mut [])
    .await
    .map_err(|e| {
      // Wasmtime wraps host-trap errors with an "error while
      // executing at wasm backtrace: ..." outer Display, stuffing
      // the friendly trap message into the cause chain. Surface
      // the whole chain so DAP consumers see the real reason.
      let mut msg = format!("entry wrapper: {e}");
      let mut cause: Option<&dyn std::error::Error> = e.source();
      while let Some(c) = cause {
        msg.push_str(&format!("\n  caused by: {c}"));
        cause = c.source();
      }
      msg
    })?;
  Ok(())
}

/// Cont id used for the wrapper's done continuation in DAP runs.
/// Matches the constant in `wasmtime_runner.rs`.
const CONT_WRAPPER_DONE: i32 = 1;

/// Cont id used for `main`'s done continuation in DAP runs.
const CONT_MAIN_DONE: i32 = 2;

/// Scan module exports for the entry wrapper, exported under the
/// canonical URL `./<basename>`.
fn find_entry_wrapper(module: &wasmtime::Module) -> Result<String, String> {
  for export in module.exports() {
    let name = export.name();
    if name.starts_with("./")
      && let wasmtime::ExternType::Func(_) = export.ty()
    {
      return Ok(name.to_string());
    }
  }
  Err("no entry wrapper export (expected one starting with './')".into())
}


/// Look up `key` in `rec` by raw bytes via the interop helper.
/// Async variant for DAP.
async fn lookup_export_by_bytes_dap(
  caller: &mut wasmtime::Caller<'_, DebugState>,
  rec: wasmtime::Rooted<wasmtime::AnyRef>,
  key: &[u8],
) -> Result<Option<wasmtime::Rooted<wasmtime::AnyRef>>, wasmtime::Error> {
  let rec_get_by_bytes = caller.get_export("rec_get_by_bytes")
    .and_then(|e| e.into_func())
    .ok_or_else(|| wasmtime::Error::msg("no rec_get_by_bytes export"))?;
  let array_ty = wasmtime::ArrayType::new(
    caller.engine(),
    wasmtime::FieldType::new(wasmtime::Mutability::Var, wasmtime::StorageType::I8),
  );
  let alloc = wasmtime::ArrayRefPre::new(&mut *caller, array_ty);
  let elems: Vec<wasmtime::Val> =
    key.iter().map(|&b| wasmtime::Val::I32(b as i32)).collect();
  let array = wasmtime::ArrayRef::new_fixed(&mut *caller, &alloc, &elems)
    .map_err(|e| wasmtime::Error::msg(format!("key bytes alloc: {e}")))?;
  let mut out = [wasmtime::Val::AnyRef(None)];
  rec_get_by_bytes.call_async(&mut *caller,
    &[wasmtime::Val::AnyRef(Some(rec)), wasmtime::Val::AnyRef(Some(array.to_anyref()))],
    &mut out).await?;
  Ok(match out[0] {
    wasmtime::Val::AnyRef(Some(r)) => Some(r),
    _ => None,
  })
}

/// Capture an exit code into the DAP exit-code slot. Mirrors
/// `capture_exit_code` in `wasmtime_runner.rs`.
fn capture_dap_exit_code(
  caller: &mut wasmtime::Caller<'_, DebugState>,
  val: Option<&wasmtime::Val>,
  exit: &Arc<Mutex<i64>>,
) {
  let Some(wasmtime::Val::AnyRef(Some(r))) = val else { return; };
  if let Ok(Some(i31)) = r.as_i31(&*caller) {
    *exit.lock().unwrap() = i31.get_i32() as i64;
    return;
  }
  if let Ok(Some(st)) = r.as_struct(&*caller)
    && let Ok(wasmtime::Val::F64(bits)) = st.field(&mut *caller, 0)
  {
    *exit.lock().unwrap() = f64::from_bits(bits) as i64;
  }
}

/// Apply `main_clo` with cli args + cont id 2 from inside `host_invoke_cont`.
/// Same shape as `apply_main` in the sync runner, but uses `call_async`
/// because the DAP store is async-configured (required by `guest_debug`).
async fn apply_main_dap(
  caller: &mut wasmtime::Caller<'_, DebugState>,
  main_clo: wasmtime::Rooted<wasmtime::AnyRef>,
  argv: &[Vec<u8>],
) -> Result<(), wasmtime::Error> {
  let wrap_host_cont = caller.get_export("wrap_host_cont")
    .and_then(|e| e.into_func())
    .ok_or_else(|| wasmtime::Error::msg("no wrap_host_cont export"))?;
  let args_empty = caller.get_export("args_empty")
    .and_then(|e| e.into_func())
    .ok_or_else(|| wasmtime::Error::msg("no args_empty export"))?;
  let args_prepend = caller.get_export("args_prepend")
    .and_then(|e| e.into_func())
    .ok_or_else(|| wasmtime::Error::msg("no args_prepend export"))?;
  let str_wrap = caller.get_export("str_wrap_bytes")
    .and_then(|e| e.into_func())
    .ok_or_else(|| wasmtime::Error::msg("no str_wrap_bytes export"))?;
  let apply_fn = caller.get_export("apply")
    .and_then(|e| e.into_func())
    .ok_or_else(|| wasmtime::Error::msg("no apply export"))?;

  let mut done_out = [wasmtime::Val::AnyRef(None)];
  wrap_host_cont
    .call_async(&mut *caller, &[wasmtime::Val::I32(CONT_MAIN_DONE)], &mut done_out)
    .await?;
  let done_cont = done_out[0];

  let array_ty = wasmtime::ArrayType::new(
    caller.engine(),
    wasmtime::FieldType::new(wasmtime::Mutability::Var, wasmtime::StorageType::I8),
  );
  let alloc = wasmtime::ArrayRefPre::new(&mut *caller, array_ty);
  let mut main_args_vals: Vec<wasmtime::Val> = vec![done_cont];
  for bytes in argv {
    let elems: Vec<wasmtime::Val> =
      bytes.iter().map(|&b| wasmtime::Val::I32(b as i32)).collect();
    let array = wasmtime::ArrayRef::new_fixed(&mut *caller, &alloc, &elems)
      .map_err(|e| wasmtime::Error::msg(format!("byte array alloc: {e}")))?;
    let mut wrapped = [wasmtime::Val::AnyRef(None)];
    str_wrap
      .call_async(&mut *caller,
        &[wasmtime::Val::AnyRef(Some(array.to_anyref()))], &mut wrapped)
      .await?;
    main_args_vals.push(wrapped[0]);
  }

  let mut acc_out = [wasmtime::Val::AnyRef(None)];
  args_empty.call_async(&mut *caller, &[], &mut acc_out).await?;
  let mut acc = acc_out[0];
  for v in main_args_vals.iter().rev() {
    let mut next = [wasmtime::Val::AnyRef(None)];
    args_prepend
      .call_async(&mut *caller, &[*v, acc], &mut next)
      .await?;
    acc = next[0];
  }

  apply_fn
    .call_async(&mut *caller, &[acc, wasmtime::Val::AnyRef(Some(main_clo))], &mut [])
    .await?;
  Ok(())
}

// ── Types ───────────────────────────────────────────────────────────────────

/// Info about a stopped frame, sent from WASM thread → DAP server.
struct StoppedFrame {
  /// Function name (from WASM export or debug name).
  func_name: String,
  /// WASM PC offset within the module.
  pc: u32,
}

/// Commands sent from DAP server → WASM thread.
///
/// Currently only `Continue` — both plain "continue" and DAP step commands
/// resume until the next mark breakpoint. Per-WASM-instruction stepping
/// (the old `Step` variant) was removed because marks already give
/// source-meaningful step granularity; see the Next/StepIn/StepOut
/// handler for the reasoning.
enum ResumeAction {
  /// Continue execution until the next breakpoint.
  Continue,
}

/// State stored in the Wasmtime Store.
#[derive(Default)]
struct DebugState {}

// ── Debug handler ───────────────────────────────────────────────────────────

/// Filter mode controlling which breakpoint fires are exposed to the DAP
/// loop (and hence to VSCode). Set by the DAP loop before dispatching a
/// resume command.
#[derive(Clone, Copy)]
enum FilterMode {
  /// Stop at any mark — used by step commands. Every mark fire surfaces
  /// as a Stopped event.
  StepAny,
  /// Run until a mark whose source line is in `user_bps`, or program
  /// termination. Intermediate marks are silently re-resumed inside the
  /// handler — the DAP loop (and VSCode) never sees them.
  ContinueUntilUserBp,
}

/// State shared between the DAP loop and the wasmtime debug handler.
/// The handler reads it on every breakpoint fire to decide whether the
/// stop is user-visible.
struct HandlerState {
  /// Current filter mode. Mutated by the DAP loop immediately before
  /// sending a resume command.
  mode: FilterMode,
  /// User-placed breakpoints keyed by (source path, 1-indexed line).
  /// Path is as received from `setBreakpoints` — VSCode sends its own
  /// canonicalised path.
  user_bps: std::collections::HashSet<(String, i64)>,
}

/// Bridges Wasmtime debug events to the DAP server via channels.
#[derive(Clone)]
struct FinkDebugHandler {
  /// Send stopped frame info to the DAP server.
  stopped_tx: mpsc::SyncSender<StoppedFrame>,
  /// Receive resume commands from the DAP server.
  resume_rx: Arc<Mutex<mpsc::Receiver<ResumeAction>>>,
  /// Shared filter state (see `HandlerState`).
  state: Arc<Mutex<HandlerState>>,
  /// All debug-marks in the linked binary, indexed by the order produced
  /// by the emitter. Used to look up a firing PC's source line for the
  /// user-breakpoint filter.
  marks: Arc<Vec<crate::passes::debug_marks::MarkRecord>>,
  /// Per-module canonicalised absolute path, keyed by `ModuleId`. Lets the
  /// matcher resolve which file each mark belongs to so user breakpoints
  /// set in imported modules — not just the entry — can match.
  module_paths: Arc<std::collections::BTreeMap<
    crate::passes::wasm::ir::ModuleId, String,
  >>,
}

impl wasmtime::DebugHandler for FinkDebugHandler {
  type Data = DebugState;

  fn handle(
    &self,
    mut store: wasmtime::StoreContextMut<'_, Self::Data>,
    event: wasmtime::DebugEvent<'_>,
  ) -> impl std::future::Future<Output = ()> + Send {
    // Collect frame info while we have access to the store.
    let should_stop = matches!(event, wasmtime::DebugEvent::Breakpoint);
    let frame_info = if should_stop {
      let mut func_name = String::from("<unknown>");
      let mut pc = 0u32;
      let frames: Vec<_> = store.debug_exit_frames().collect();
      if let Some(frame) = frames.first()
        && let Ok(Some((func_idx, wasm_pc))) = frame.wasm_function_index_and_pc(&mut store)
      {
        pc = wasm_pc.raw();
        func_name = format!("func[{}]", func_idx.as_u32());
      }
      Some(StoppedFrame { func_name, pc })
    } else {
      None
    };

    if let Some(ref frame) = frame_info {
      // Consult the filter. In ContinueUntilUserBp mode, silently re-
      // resume stops that don't hit a user breakpoint. The wasm thread
      // returns from this handler and wasmtime immediately continues
      // execution to the next installed breakpoint.
      let expose = {
        let st = self.state.lock().unwrap();
        match st.mode {
          FilterMode::StepAny => true,
          FilterMode::ContinueUntilUserBp => mark_matches_user_bp(
            frame.pc, &self.marks, &self.module_paths, &st.user_bps,
          ),
        }
      };
      if expose {
        let _ = self.stopped_tx.send(StoppedFrame {
          func_name: frame.func_name.clone(),
          pc: frame.pc,
        });
        // Block until DAP server tells us to resume. We don't re-toggle
        // single_step — stepping is mark-granular, not instruction-
        // granular, so pre-installed mark breakpoints do all the gating.
        if let Ok(guard) = self.resume_rx.lock() {
          let _ = guard.recv();
        }
      }
    }

    // Return a no-op future (all work done synchronously above).
    async {}
  }
}

/// True if the mark at PC `pc` belongs to a line the user has placed a
/// breakpoint on. The mark's `module_id` looks up its source path in
/// `module_paths`; the resulting `(path, line)` is compared to the user
/// breakpoint set. Path comparison is literal — VSCode and the recorded
/// per-module path should agree after `fs::canonicalize`.
fn mark_matches_user_bp(
  pc: u32,
  marks: &[crate::passes::debug_marks::MarkRecord],
  module_paths: &std::collections::BTreeMap<
    crate::passes::wasm::ir::ModuleId, String,
  >,
  user_bps: &std::collections::HashSet<(String, i64)>,
) -> bool {
  let Some(m) = marks.iter().find(|m| m.wasm_pc == pc) else {
    return false;
  };
  let Some(path) = module_paths.get(&m.module_id) else {
    return false;
  };
  let line = m.source.start.line as i64;
  user_bps.contains(&(path.clone(), line))
}

// ── DAP server ──────────────────────────────────────────────────────────────

pub fn run<R: Read, W: Write + Send + 'static>(
  input: R,
  output: W,
  program: &str,
) -> Result<(), String> {
  eprintln!("[fink dap] starting for: {program}");
  let mut server = Server::new(BufReader::new(input), BufWriter::new(output));

  // Load or compile the program.
  let (wasm, source_file, mappings, marks, id_to_url) = if program.ends_with(".fnk") {
    // Fink source: compile the package via the multi-module path so
    // imported `.fnk` files are loaded from disk (matches what
    // `fink run` does). `to_wasm` would only register the entry file
    // in an in-memory loader and fail on any `import './foo.fnk'`.
    let mut loader = crate::passes::modules::FileSourceLoader::new();
    let wasm = crate::compile_package(std::path::Path::new(program), &mut loader)?;
    (wasm.binary, program.to_string(), wasm.mappings, wasm.marks, wasm.id_to_url)
  } else {
    let bytes = std::fs::read(program).map_err(|e| e.to_string())?;
    if !bytes.starts_with(b"\0asm") {
      return Err("only .fnk source and .wasm binaries are supported".into());
    }
    let fnk_path = find_fnk_source(program);
    let source_file = fnk_path.as_deref().unwrap_or(program).to_string();
    (bytes, source_file, vec![], vec![], std::collections::BTreeMap::new())
  };

  // Set up Wasmtime with debug support.
  let mut config = wasmtime::Config::new();
  config.wasm_gc(true);
  config.wasm_tail_call(true);
  config.wasm_function_references(true);
  config.guest_debug(true);
  config.cranelift_opt_level(wasmtime::OptLevel::None);

  let engine = wasmtime::Engine::new(&config).map_err(|e| e.to_string())?;
  let module = wasmtime::Module::new(&engine, &wasm)
    .map_err(|e| crate::passes::wasm::annotate_func_indices(&e.to_string(), &wasm))?;

  // Channels between DAP server (main thread) and WASM execution thread.
  let (stopped_tx, stopped_rx) = mpsc::sync_channel::<StoppedFrame>(1);
  let (resume_tx, resume_rx) = mpsc::sync_channel::<ResumeAction>(1);

  let mut store = wasmtime::Store::new(&engine, DebugState::default());

  // Install a breakpoint at every step-stop the debug_marks pass
  // identified. Replaces the prior single_step(true) bootstrap, which
  // fired on every WASM instruction. With marks in place the debugger
  // stops only at user-meaningful CPS nodes.
  //
  // If the marks vector is empty (no debug_marks available, e.g. WAT
  // input or compile failure) fall back to the legacy single_step
  // behaviour so the debugger at least stops *somewhere*.
  if let Some(mut edit) = store.edit_breakpoints() {
    if marks.is_empty() {
      edit.single_step(true).ok();
    } else {
      for m in &marks {
        edit.add_breakpoint(&module, wasmtime::ModulePC::new(m.wasm_pc)).ok();
      }
    }
  }

  // Shared filter state between DAP loop and debug handler. Default
  // mode is ContinueUntilUserBp so the program runs without stopping at
  // every intermediate mark when stopOnEntry is false — but the first
  // stop we produce is the synthetic entry stop, which is handled by the
  // ConfigurationDone path below *before* mode filtering applies.
  let handler_state = Arc::new(Mutex::new(HandlerState {
    mode: FilterMode::StepAny,
    user_bps: std::collections::HashSet::new(),
  }));
  let marks_arc = Arc::new(marks.clone());
  // Per-module canonicalised absolute path map for comparing marks
  // against `setBreakpoints.source.path`. Built from the package
  // compiler's `id_to_url` plus the entry's directory: each module's
  // canonical URL is resolved to a disk path under the entry dir, then
  // canonicalised so it agrees with what VSCode sends.
  let entry_dir = std::path::Path::new(program)
    .parent()
    .map(|p| p.to_path_buf())
    .unwrap_or_else(|| std::path::PathBuf::from("."));
  let mut module_paths: std::collections::BTreeMap<
    crate::passes::wasm::ir::ModuleId, String,
  > = std::collections::BTreeMap::new();
  for (id, url) in &id_to_url {
    let disk = crate::passes::wasm::compile_package::resolve_canonical_to_disk(
      &entry_dir, url,
    );
    let canonical = std::fs::canonicalize(&disk)
      .map(|p| p.to_string_lossy().to_string())
      .unwrap_or_else(|_| disk.to_string_lossy().to_string());
    module_paths.insert(*id, canonical);
  }
  // Fallback for the no-package path (.wasm input): one entry keyed
  // under ModuleId(0) so single-module flows still work.
  if module_paths.is_empty() {
    let canonical = std::fs::canonicalize(&source_file)
      .map(|p| p.to_string_lossy().to_string())
      .unwrap_or_else(|_| source_file.clone());
    module_paths.insert(crate::passes::wasm::ir::ModuleId(0), canonical);
  }
  let module_paths_arc = Arc::new(module_paths);

  store.set_debug_handler(FinkDebugHandler {
    stopped_tx: stopped_tx.clone(),
    resume_rx: Arc::new(Mutex::new(resume_rx)),
    state: handler_state.clone(),
    marks: marks_arc.clone(),
    module_paths: module_paths_arc.clone(),
  });

  // Wire env imports for the host-wrapper API. The wrapper protocol uses:
  //   - `host_invoke_cont(cont_id, args)` — fired by the wrapper with cont
  //     id 1 (`(last_expr, main_clo)`) and by main's done with cont id 2
  //     (`(main_result)`).
  //   - `host_panic`, `host_channel_send`. `host_read` traps with a
  //     clear error — proper stdin under DAP needs runInTerminal + an
  //     adapter/debuggee process split (mirroring Node / cppdbg /
  //     CodeLLDB), tracked separately.
  //
  // host_channel_send routes debuggee stdout/stderr into DAP `Output`
  // events so the bytes surface in VSCode's Debug Console rather than
  // corrupting the DAP JSON stream on real stdout.
  let mut linker = wasmtime::Linker::new(&engine);
  let exit_code: Arc<Mutex<i64>> = Arc::new(Mutex::new(0));
  // `argv[0]` for the user's `main`. DAP runs are parameterised only by
  // the program path today; user-supplied CLI args are a follow-up.
  let cli_args: Arc<Vec<Vec<u8>>> = Arc::new(vec![program.to_string().into_bytes()]);
  for import in module.imports() {
    if import.module() == "env"
      && let wasmtime::ExternType::Func(ft) = import.ty()
    {
      let name = import.name().to_string();
      match name.as_str() {
        "host_panic" => {
          linker.func_new("env", &name, ft.clone(), move |_caller, _params, _results| {
            Err(wasmtime::Error::msg("fink panic: irrefutable pattern failed"))
          }).map_err(|e| e.to_string())?;
        }
        "host_channel_send" => {
          let out = server.output.clone();
          linker.func_new("env", &name, ft.clone(), move |mut caller, params, _results| {
            let tag = params[0].unwrap_i32();
            let bytes_any = params[1].unwrap_anyref()
              .ok_or_else(|| wasmtime::Error::msg("host_channel_send: null bytes ref"))?;
            let arr = bytes_any.unwrap_array(&mut caller)?;
            let len = arr.len(&caller)? as usize;
            let mut buf = Vec::with_capacity(len);
            for v in arr.elems(&mut caller)? {
              buf.push(v.unwrap_i32() as u8);
            }
            let text = String::from_utf8_lossy(&buf).into_owned();
            let category = if tag == 1 {
              OutputEventCategory::Stdout
            } else {
              OutputEventCategory::Stderr
            };
            let event = Event::Output(dap::events::OutputEventBody {
              category: Some(category),
              output: text,
              group: None,
              variables_reference: None,
              source: None,
              line: None,
              column: None,
              data: None,
            });
            if let Ok(mut o) = out.lock() {
              let _ = o.send_event(event);
            }
            Ok(())
          }).map_err(|e| e.to_string())?;
        }
        "host_invoke_cont" => {
          let exit = exit_code.clone();
          let argv = cli_args.clone();
          linker.func_new_async("env", &name, ft.clone(), move |mut caller, params, _results| {
            let exit = exit.clone();
            let argv = argv.clone();
            Box::new(async move {
              let cont_id = params[0].unwrap_i32();
              let args_any = params[1].unwrap_anyref()
                .ok_or_else(|| wasmtime::Error::msg("host_invoke_cont: null args"))?;
              let cons = args_any.unwrap_struct(&caller)?;

              let head = cons.field(&mut caller, 0).ok();
              capture_dap_exit_code(&mut caller, head.as_ref(), &exit);

              if cont_id != CONT_WRAPPER_DONE {
                return Ok(());
              }

              // args[1] is the exports rec; pull `main` host-side
              // via the interop rec_get_by_bytes helper.
              let exports_rec = match cons.field(&mut caller, 1).ok() {
                Some(wasmtime::Val::AnyRef(Some(tail_ref))) => {
                  match tail_ref.as_struct(&caller) {
                    Ok(Some(tail_st)) => match tail_st.field(&mut caller, 0).ok() {
                      Some(wasmtime::Val::AnyRef(Some(r))) => r,
                      _ => return Ok(()),
                    },
                    _ => return Ok(()),
                  }
                }
                _ => return Ok(()),
              };
              let main_clo = match lookup_export_by_bytes_dap(
                &mut caller, exports_rec, b"main").await? {
                Some(r) => r,
                None => return Ok(()),
              };

              *exit.lock().unwrap() = 0;
              apply_main_dap(&mut caller, main_clo, &argv).await?;
              Ok(())
            })
          }).map_err(|e| e.to_string())?;
        }
        "host_read" => {
          // `read stdin` cannot work in the current DAP topology: `fink
          // dap` is *both* DAP adapter (talks DAP on its own stdin/
          // stdout to VSCode) and debuggee (runs the WASM program in
          // process). Reading stdin would compete with the DAP frame
          // reader.
          //
          // The conventional fix used by Node / cppdbg / CodeLLDB is
          // DAP's `runInTerminal` reverse request — VSCode launches the
          // debuggee in an integrated terminal with a real stdin, while
          // the adapter stays on its own channel. That requires an
          // adapter/debuggee process split here plus a launch-config
          // bit (`console: integratedTerminal`) in vscode-fink, and
          // is tracked as a follow-up.
          //
          // Until then, trap with a friendly, actionable error so the
          // user sees what to do instead of a generic
          // "builtin '...' not yet implemented".
          linker.func_new("env", &name, ft.clone(), move |_caller, _params, _results| {
            Err(wasmtime::Error::msg(
              "read stdin is not supported under DAP. \
               Run the program with `fink <file>` for stdin-using \
               programs. Real stdin under the debugger needs \
               `runInTerminal` support, which is not yet wired."
            ))
          }).map_err(|e| e.to_string())?;
        }
        _ => {
          // Any other unknown env imports — trap with a generic
          // "not implemented" so missing host functions don't fail
          // silently.
          let err_name = name.clone();
          linker.func_new("env", &name, ft.clone(), move |_caller, _params, _results| {
            Err(wasmtime::Error::msg(format!("builtin '{}' not yet implemented in DAP", err_name)))
          }).map_err(|e| e.to_string())?;
        }
      }
    }
  }

  // Spawn WASM execution thread with async runtime (required by guest_debug).
  let terminated_tx = stopped_tx;
  // Trap errors from the wasm execution surface here. Forward them as
  // a stderr `Output` DAP event so the user sees them in the Debug
  // Console — without this, trap messages (e.g. the friendly
  // host_read error) only show up on the adapter's host stderr,
  // which VSCode never displays.
  let trap_output = server.output.clone();
  let wasm_thread = std::thread::spawn(move || {
    let rt = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .expect("failed to build tokio runtime");
    rt.block_on(async {
      if let Err(e) = run_module(&mut store, &linker, &module).await {
        let msg = format!("error: {e}\n");
        eprintln!("[fink dap] {msg}");
        if let Ok(mut o) = trap_output.lock() {
          let _ = o.send_event(Event::Output(dap::events::OutputEventBody {
            category: Some(OutputEventCategory::Stderr),
            output: msg,
            group: None,
            variables_reference: None,
            source: None,
            line: None,
            column: None,
            data: None,
          }));
        }
      }
    });
    let _ = terminated_tx.send(StoppedFrame { func_name: String::new(), pc: u32::MAX });
  });

  // Track the last stopped frame for stackTrace requests.
  let mut last_frame: Option<StoppedFrame> = None;
  let mut stop_on_entry = false;
  let mut running = false;

  let abs_path = std::fs::canonicalize(&source_file)
    .map(|p| p.to_string_lossy().to_string())
    .unwrap_or_else(|_| source_file.clone());
  let file_name = std::path::Path::new(&source_file)
    .file_name()
    .map(|f| f.to_string_lossy().to_string())
    .unwrap_or_default();

  // True if the WASM program has finished and we've announced it to
  // VSCode — see the terminate-and-break branches below.
  let mut done = false;

  loop {
    if done {
      break;
    }
    match server.poll_request() {
      Ok(Some(req)) => {
        match &req.command {
          Command::Initialize { .. } => {
            server.respond(req.success(ResponseBody::Initialize(Capabilities {
              supports_configuration_done_request: Some(true),
              ..Default::default()
            }))).ok();
            server.send_event(Event::Initialized).ok();
          }

          Command::Launch(args) => {
            if let Some(data) = &args.additional_data
              && let Some(soe) = data.get("stopOnEntry")
            {
              stop_on_entry = soe.as_bool().unwrap_or(false);
            }
            server.respond(req.success(ResponseBody::Launch)).ok();
          }

          Command::ConfigurationDone => {
            server.respond(req.success(ResponseBody::ConfigurationDone)).ok();
            // Configure the filter before the program runs. If
            // stopOnEntry is set we want the FIRST mark to surface (as
            // the entry stop), so StepAny. Otherwise we want to skip
            // every intermediate mark and only stop at user-placed
            // breakpoints, so ContinueUntilUserBp.
            handler_state.lock().unwrap().mode = if stop_on_entry {
              FilterMode::StepAny
            } else {
              FilterMode::ContinueUntilUserBp
            };
            if stop_on_entry {
              // Wait for the first breakpoint event (the entry).
              if let Ok(frame) = stopped_rx.recv() {
                if frame.pc == u32::MAX {
                  server.send_event(Event::Exited(ExitedEventBody { exit_code: *exit_code.lock().unwrap() })).ok();
                  server.send_event(Event::Terminated(None)).ok();
                  done = true;
                } else {
                  last_frame = Some(frame);
                  running = true;
                  server.send_event(Event::Stopped(StoppedEventBody {
                    reason: StoppedEventReason::Entry,
                    description: None,
                    thread_id: Some(1),
                    preserve_focus_hint: None,
                    text: None,
                    all_threads_stopped: Some(true),
                    hit_breakpoint_ids: None,
                  })).ok();
                }
              }
            } else {
              // Run-from-start. The handler is currently blocked on the
              // very first breakpoint it hit (during bootstrap, before
              // we had a chance to set the mode to ContinueUntilUserBp).
              // Drain it, silently resume, and then wait for the next
              // stop — which will be either a user breakpoint or
              // termination. Subsequent stops the handler auto-filters.
              running = true;
              loop {
                match stopped_rx.recv() {
                  Ok(frame) if frame.pc == u32::MAX => {
                    running = false;
                    server.send_event(Event::Exited(ExitedEventBody { exit_code: *exit_code.lock().unwrap() })).ok();
                    server.send_event(Event::Terminated(None)).ok();
                    done = true;
                    break;
                  }
                  Ok(frame) => {
                    // In ContinueUntilUserBp mode the handler only
                    // forwards user-breakpoint stops; but the very
                    // first stop was captured under the default mode
                    // before ConfigurationDone could flip it. Check
                    // the mark's line against user_bps here and skip
                    // silently if it doesn't match.
                    let st = handler_state.lock().unwrap();
                    let is_user_bp = mark_matches_user_bp(
                      frame.pc, &marks_arc, &module_paths_arc, &st.user_bps,
                    );
                    drop(st);
                    if is_user_bp {
                      last_frame = Some(frame);
                      server.send_event(Event::Stopped(StoppedEventBody {
                        reason: StoppedEventReason::Breakpoint,
                        description: None,
                        thread_id: Some(1),
                        preserve_focus_hint: None,
                        text: None,
                        all_threads_stopped: Some(true),
                        hit_breakpoint_ids: None,
                      })).ok();
                      break;
                    } else {
                      // Silently resume — don't bother VSCode.
                      let _ = resume_tx.send(ResumeAction::Continue);
                    }
                  }
                  Err(_) => {
                    running = false;
                    server.send_event(Event::Exited(ExitedEventBody { exit_code: *exit_code.lock().unwrap() })).ok();
                    server.send_event(Event::Terminated(None)).ok();
                    done = true;
                    break;
                  }
                }
              }
            }
          }

          Command::Threads => {
            server.respond(req.success(ResponseBody::Threads(ThreadsResponse {
              threads: vec![Thread { id: 1, name: "main".to_string() }],
            }))).ok();
          }

          Command::StackTrace(_) => {
            // Resolve the firing PC to a (line, col, source path). Prefer
            // the mark-based path: every installed breakpoint has a
            // MarkRecord with an exact `Loc` *and* the `module_id` of
            // the source file, so multi-module stops jump to the right
            // file. Fall back to the legacy DWARF-derived mapping (line
            // only) + the entry path for non-mark stops (legacy
            // single_step, .wasm input).
            let (line, col, name, src_path, src_name) = if let Some(ref frame) = last_frame {
              if let Some(m) = pc_to_mark(frame.pc, &marks) {
                let path = module_paths_arc.get(&m.module_id)
                  .cloned()
                  .unwrap_or_else(|| abs_path.clone());
                let name_for_source = std::path::Path::new(&path)
                  .file_name()
                  .map(|f| f.to_string_lossy().to_string())
                  .unwrap_or_else(|| file_name.clone());
                (
                  m.source.start.line as i64,
                  m.source.start.col as i64,
                  frame.func_name.clone(),
                  path,
                  name_for_source,
                )
              } else {
                let (l, c) = pc_to_source_location(frame.pc, &mappings)
                  .unwrap_or((1, 1));
                (l, c, frame.func_name.clone(), abs_path.clone(), file_name.clone())
              }
            } else {
              (1, 1, "?".to_string(), abs_path.clone(), file_name.clone())
            };
            let frames = vec![StackFrame {
              id: 1,
              name,
              source: Some(Source {
                name: Some(src_name),
                path: Some(src_path),
                ..Default::default()
              }),
              line,
              column: col,
              ..Default::default()
            }];
            server.respond(req.success(ResponseBody::StackTrace(StackTraceResponse {
              stack_frames: frames,
              total_frames: Some(1),
            }))).ok();
          }

          Command::Scopes(_) => {
            server.respond(req.success(ResponseBody::Scopes(ScopesResponse {
              scopes: vec![Scope {
                name: "Locals".to_string(),
                variables_reference: 1,
                expensive: false,
                ..Default::default()
              }],
            }))).ok();
          }

          Command::Variables(_) => {
            server.respond(req.success(ResponseBody::Variables(VariablesResponse {
              variables: vec![],
            }))).ok();
          }

          Command::SetBreakpoints(args) => {
            // VSCode re-sends the full breakpoint set for a source on
            // every change, so we replace rather than merge the subset
            // keyed on this file. Mark bps `verified: true` whenever we
            // can resolve the requested line to an existing debug-mark
            // line — otherwise leave them unverified so VSCode greys
            // them out.
            let file_path = args.source.path.clone().unwrap_or_default();
            let mark_lines: std::collections::HashSet<i64> = marks
              .iter()
              .map(|m| m.source.start.line as i64)
              .collect();
            let mut state = handler_state.lock().unwrap();
            // Drop any previous bps for this file, then reinsert.
            state.user_bps.retain(|(p, _)| p != &file_path);
            let bps: Vec<Breakpoint> = args.breakpoints.as_ref()
              .map(|bps| {
                bps.iter().map(|bp| {
                  let verified = mark_lines.contains(&bp.line);
                  if verified {
                    state.user_bps.insert((file_path.clone(), bp.line));
                  }
                  Breakpoint {
                    verified,
                    line: Some(bp.line),
                    ..Default::default()
                  }
                }).collect()
              })
              .unwrap_or_default();
            drop(state);
            server.respond(req.success(ResponseBody::SetBreakpoints(
              SetBreakpointsResponse { breakpoints: bps },
            ))).ok();
          }

          Command::Continue(_) => {
            server.respond(req.success(ResponseBody::Continue(ContinueResponse {
              all_threads_continued: Some(true),
            }))).ok();
            if running {
              // Filter out intermediate marks — only stop at user-
              // placed breakpoints. Reset mode to StepAny at the end so
              // subsequent Step commands behave correctly.
              handler_state.lock().unwrap().mode = FilterMode::ContinueUntilUserBp;
              let _ = resume_tx.send(ResumeAction::Continue);
              // Wait for next stop or termination.
              match stopped_rx.recv() {
                Ok(frame) if frame.pc == u32::MAX => {
                  running = false;
                  server.send_event(Event::Exited(ExitedEventBody { exit_code: *exit_code.lock().unwrap() })).ok();
                  server.send_event(Event::Terminated(None)).ok();
                  done = true;
                }
                Ok(frame) => {
                  last_frame = Some(frame);
                  server.send_event(Event::Stopped(StoppedEventBody {
                    reason: StoppedEventReason::Breakpoint,
                    description: None,
                    thread_id: Some(1),
                    preserve_focus_hint: None,
                    text: None,
                    all_threads_stopped: Some(true),
                    hit_breakpoint_ids: None,
                  })).ok();
                }
                Err(_) => {
                  running = false;
                  server.send_event(Event::Exited(ExitedEventBody { exit_code: *exit_code.lock().unwrap() })).ok();
                  server.send_event(Event::Terminated(None)).ok();
                  done = true;
                }
              }
            }
          }

          Command::Next(_) | Command::StepIn(_) | Command::StepOut(_) => {
            let resp = match &req.command {
              Command::Next(_) => ResponseBody::Next,
              Command::StepIn(_) => ResponseBody::StepIn,
              _ => ResponseBody::StepOut,
            };
            server.respond(req.success(resp)).ok();
            if running {
              // Step always stops at the next mark — ignore user_bps.
              handler_state.lock().unwrap().mode = FilterMode::StepAny;
              // Step at *mark* granularity — resume until the next mark
              // breakpoint fires. Per-WASM-instruction single_step isn't
              // useful to a user: one source expression expands to
              // hundreds of ops (closure dispatch, CPS glue, scheduler
              // yields), so single_step hops through runtime internals
              // rather than source lines. Treating step as "run to next
              // mark" makes every step land on a user-meaningful
              // location. True Next/StepIn/StepOut semantics (call-depth
              // aware) is a follow-up — today all three do the same
              // thing.
              let _ = resume_tx.send(ResumeAction::Continue);
              // Wait for next stop or termination.
              match stopped_rx.recv() {
                Ok(frame) if frame.pc == u32::MAX => {
                  running = false;
                  server.send_event(Event::Exited(ExitedEventBody { exit_code: *exit_code.lock().unwrap() })).ok();
                  server.send_event(Event::Terminated(None)).ok();
                  done = true;
                }
                Ok(frame) => {
                  last_frame = Some(frame);
                  server.send_event(Event::Stopped(StoppedEventBody {
                    reason: StoppedEventReason::Step,
                    description: None,
                    thread_id: Some(1),
                    preserve_focus_hint: None,
                    text: None,
                    all_threads_stopped: Some(true),
                    hit_breakpoint_ids: None,
                  })).ok();
                }
                Err(_) => {
                  running = false;
                  server.send_event(Event::Exited(ExitedEventBody { exit_code: *exit_code.lock().unwrap() })).ok();
                  server.send_event(Event::Terminated(None)).ok();
                  done = true;
                }
              }
            }
          }

          Command::Disconnect(_) => {
            server.respond(req.success(ResponseBody::Disconnect)).ok();
            break;
          }

          _ => {
            server.respond(req.success(ResponseBody::Disconnect)).ok();
          }
        }
      }
      Ok(None) => break,
      Err(e) => {
        eprintln!("[fink dap] error: {e:?}");
        break;
      }
    }
  }

  let _ = wasm_thread.join();
  Ok(())
}

/// Test scaffolding: look for a .fnk source file corresponding to a .wat file.
fn find_fnk_source(wat_path: &str) -> Option<String> {
  let p = std::path::Path::new(wat_path);
  let stem = p.file_stem()?.to_str()?;
  let fnk_path = p.parent()?.parent()?.join("fnk").join(format!("{stem}.fnk"));
  if fnk_path.exists() {
    Some(fnk_path.to_string_lossy().into_owned())
  } else {
    None
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Build a DAP request as `Content-Length: N\r\n\r\n{json}`.
  fn frame(json: &str) -> Vec<u8> {
    use std::io::Write as _;
    let mut out = Vec::new();
    write!(out, "Content-Length: {}\r\n\r\n{}", json.len(), json).unwrap();
    out
  }

  /// Drive a scripted DAP session through `dap::run` and return the raw output.
  ///
  /// Writes the source to a tempfile, builds a framed input buffer from the
  /// given JSON request bodies, then runs the DAP server to completion on a
  /// thread with a timeout so a broken bootstrap can't hang the test suite
  /// forever. Each entry in `requests` is the bare JSON object — framing is
  /// added here.
  fn drive_session(src: &str, requests: &[String]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.fnk");
    std::fs::write(&path, src).unwrap();
    let path_str = path.to_string_lossy().into_owned();

    let mut input = Vec::new();
    for req in requests {
      input.extend_from_slice(&frame(req));
    }

    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    let output_clone = output.clone();

    let handle = std::thread::spawn(move || {
      struct SharedWrite(Arc<Mutex<Vec<u8>>>);
      impl std::io::Write for SharedWrite {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
          self.0.lock().unwrap().extend_from_slice(buf);
          Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
      }
      let writer = SharedWrite(output_clone);
      let reader = std::io::Cursor::new(input);
      super::run(reader, writer, &path_str).ok();
    });

    // Wait up to 10s for the DAP session to finish. If it hangs past that,
    // the test fails loudly rather than hanging CI.
    let start = std::time::Instant::now();
    while !handle.is_finished() {
      if start.elapsed() > std::time::Duration::from_secs(10) {
        panic!("DAP session did not terminate within 10s");
      }
      std::thread::sleep(std::time::Duration::from_millis(50));
    }
    handle.join().unwrap();

    String::from_utf8_lossy(&output.lock().unwrap()).into_owned()
  }

  #[test]
  fn stop_on_entry_then_continue_terminates_cleanly() {
    // The simplest CPS program: main calls its done continuation with 42.
    // The compiler produces at least one debug-marks breakpoint for the
    // call site, so a correctly-bootstrapped DAP session must:
    //   1) emit a Stopped event at entry (driven by stopOnEntry),
    //   2) emit a Terminated event after continue,
    //   3) not hang.
    let out = drive_session(include_str!("test_fixtures/hello_world.fnk"), &[
      r#"{"seq":1,"type":"request","command":"initialize","arguments":{"adapterID":"fink"}}"#.to_string(),
      r#"{"seq":2,"type":"request","command":"launch","arguments":{"stopOnEntry":true}}"#.to_string(),
      r#"{"seq":3,"type":"request","command":"configurationDone"}"#.to_string(),
      r#"{"seq":4,"type":"request","command":"continue","arguments":{"threadId":1}}"#.to_string(),
      r#"{"seq":5,"type":"request","command":"disconnect"}"#.to_string(),
    ]);

    assert!(
      out.contains(r#""event":"stopped""#),
      "expected a 'stopped' event in DAP output, got:\n{out}"
    );
    assert!(
      out.contains(r#""event":"terminated""#),
      "expected a 'terminated' event in DAP output, got:\n{out}"
    );
  }

  /// Count how many `"event":"stopped"` events appear in the DAP output.
  fn count_stops(out: &str) -> usize {
    out.matches(r#""event":"stopped""#).count()
  }

  #[test]
  fn continue_stops_only_at_user_breakpoints() {
    // Multi-line `main` — without any user breakpoint, stepping / a
    // blind Continue would stop at each mark in turn. With one user-
    // placed breakpoint on line 6 (the `x` return), a plain Continue
    // from entry should reach exactly ONE user stop (entry itself is
    // skipped, since stopOnEntry = false) and then stop on line 6,
    // skipping any intermediate marks on lines 4-5. After a second
    // continue the program terminates.
    let src = include_str!("test_fixtures/let_write_return.fnk");

    // setBreakpoints must use the canonicalised path VSCode would send.
    // We don't know it ahead of time — drive_session writes to a
    // tempfile whose path we can pass in via the setBreakpoints source.
    // For the test we wildcard the path: the DAP server should match on
    // line number alone when the source path matches the program path.
    // (See user_bps resolution in Command::SetBreakpoints.)
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.fnk");
    std::fs::write(&path, src).unwrap();
    let path_str = path.canonicalize().unwrap().to_string_lossy().into_owned();
    // Minimal JSON-string escaping — tempdir paths on macOS/Linux only
    // need backslash and double-quote escaping.
    let path_json = format!(
      "\"{}\"",
      path_str.replace('\\', "\\\\").replace('"', "\\\"")
    );

    let mut input = Vec::new();
    input.extend_from_slice(&frame(r#"{"seq":1,"type":"request","command":"initialize","arguments":{"adapterID":"fink"}}"#));
    input.extend_from_slice(&frame(r#"{"seq":2,"type":"request","command":"launch","arguments":{"stopOnEntry":false}}"#));
    input.extend_from_slice(&frame(&format!(
      r#"{{"seq":3,"type":"request","command":"setBreakpoints","arguments":{{"source":{{"path":{path_json}}},"breakpoints":[{{"line":6}}]}}}}"#
    )));
    input.extend_from_slice(&frame(r#"{"seq":4,"type":"request","command":"configurationDone"}"#));
    // After configurationDone with stopOnEntry=false, the program should
    // run and hit the line-3 user breakpoint.
    // Then we continue to terminate.
    input.extend_from_slice(&frame(r#"{"seq":5,"type":"request","command":"continue","arguments":{"threadId":1}}"#));
    input.extend_from_slice(&frame(r#"{"seq":6,"type":"request","command":"disconnect","arguments":{}}"#));

    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    let output_clone = output.clone();
    let path_for_run = path.to_string_lossy().into_owned();

    let handle = std::thread::spawn(move || {
      struct SharedWrite(Arc<Mutex<Vec<u8>>>);
      impl std::io::Write for SharedWrite {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
          self.0.lock().unwrap().extend_from_slice(buf);
          Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
      }
      let writer = SharedWrite(output_clone);
      let reader = std::io::Cursor::new(input);
      super::run(reader, writer, &path_for_run).ok();
    });

    let start = std::time::Instant::now();
    while !handle.is_finished() {
      if start.elapsed() > std::time::Duration::from_secs(10) {
        panic!("DAP session did not terminate within 10s");
      }
      std::thread::sleep(std::time::Duration::from_millis(50));
    }
    handle.join().unwrap();

    let out = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();

    // Exactly one stopped event: the line-3 user breakpoint. Without
    // user-bp filtering the program would stop at every mark on the way.
    assert_eq!(
      count_stops(&out), 1,
      "expected exactly 1 stopped event (at user breakpoint), got {}:\n{out}",
      count_stops(&out),
    );
    assert!(
      out.contains(r#""event":"terminated""#),
      "expected a 'terminated' event, got:\n{out}"
    );
  }

  #[test]
  fn breakpoint_in_imported_module_fires() {
    // Two-file program. Entry imports `helper.fnk` and calls
    // `double 21`. Helper has a multi-line `double` body. The user
    // sets a breakpoint on line 2 of helper.fnk (the `doubled = n * 2`
    // line) and expects the debugger to stop there exactly once when
    // entry runs.
    let entry_src = include_str!("test_fixtures/multi_module/entry.fnk");
    let helper_src = include_str!("test_fixtures/multi_module/helper.fnk");

    let dir = tempfile::tempdir().unwrap();
    let entry_path = dir.path().join("entry.fnk");
    let helper_path = dir.path().join("helper.fnk");
    std::fs::write(&entry_path, entry_src).unwrap();
    std::fs::write(&helper_path, helper_src).unwrap();
    let helper_path_canonical = helper_path
      .canonicalize().unwrap().to_string_lossy().into_owned();
    let helper_path_json = format!(
      "\"{}\"",
      helper_path_canonical.replace('\\', "\\\\").replace('"', "\\\""),
    );

    // Send a generous number of `continue`s (more than the program will
    // ever produce stops) so the session always reaches `disconnect`.
    // A user breakpoint can fire multiple times on the same source line
    // because debug-marks are CPS-node-granular and a single line may
    // contain multiple meaningful expressions.
    let mut input = Vec::new();
    input.extend_from_slice(&frame(r#"{"seq":1,"type":"request","command":"initialize","arguments":{"adapterID":"fink"}}"#));
    input.extend_from_slice(&frame(r#"{"seq":2,"type":"request","command":"launch","arguments":{"stopOnEntry":false}}"#));
    input.extend_from_slice(&frame(&format!(
      r#"{{"seq":3,"type":"request","command":"setBreakpoints","arguments":{{"source":{{"path":{helper_path_json}}},"breakpoints":[{{"line":2}}]}}}}"#
    )));
    input.extend_from_slice(&frame(r#"{"seq":4,"type":"request","command":"configurationDone"}"#));
    // Ask for a stackTrace once so the response carries the source path
    // VSCode would jump to. After that, drain any further stops.
    input.extend_from_slice(&frame(r#"{"seq":5,"type":"request","command":"stackTrace","arguments":{"threadId":1}}"#));
    for seq in 6..16 {
      input.extend_from_slice(&frame(&format!(
        r#"{{"seq":{seq},"type":"request","command":"continue","arguments":{{"threadId":1}}}}"#
      )));
    }
    input.extend_from_slice(&frame(r#"{"seq":16,"type":"request","command":"disconnect","arguments":{}}"#));

    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    let output_clone = output.clone();
    let entry_path_for_run = entry_path.to_string_lossy().into_owned();

    let handle = std::thread::spawn(move || {
      struct SharedWrite(Arc<Mutex<Vec<u8>>>);
      impl std::io::Write for SharedWrite {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
          self.0.lock().unwrap().extend_from_slice(buf);
          Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
      }
      let writer = SharedWrite(output_clone);
      let reader = std::io::Cursor::new(input);
      super::run(reader, writer, &entry_path_for_run).ok();
    });

    let start = std::time::Instant::now();
    while !handle.is_finished() {
      if start.elapsed() > std::time::Duration::from_secs(10) {
        panic!("DAP session did not terminate within 10s");
      }
      std::thread::sleep(std::time::Duration::from_millis(50));
    }
    handle.join().unwrap();

    let out = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();

    // At least one stopped event: the helper-line-2 user breakpoint.
    // Could be more than one if line 2 has multiple debug-mark CPS
    // nodes; what matters is that breakpoints in imported modules
    // fire at all.
    assert!(
      count_stops(&out) >= 1,
      "expected at least 1 stopped event (at imported-module user breakpoint), got {}:\n{out}",
      count_stops(&out),
    );
    // The stackTrace response must carry the helper.fnk path, not the
    // entry's. Without the per-mark module identity wired into the
    // StackFrame source, VSCode would highlight the wrong file.
    assert!(
      out.contains(&helper_path_canonical),
      "expected stackTrace to report helper.fnk path ({helper_path_canonical}), got:\n{out}"
    );
    assert!(
      out.contains(r#""event":"terminated""#),
      "expected a 'terminated' event, got:\n{out}"
    );
  }

  #[test]
  fn read_stdin_under_dap_traps_with_clear_error() {
    // `read stdin` cannot work under DAP today (single-process adapter
    // + debuggee, stdin contended). The host stub must trap with a
    // friendly, actionable error mentioning `runInTerminal` rather
    // than the generic "builtin '...' not yet implemented".
    let out = drive_session(include_str!("test_fixtures/reads_stdin.fnk"), &[
      r#"{"seq":1,"type":"request","command":"initialize","arguments":{"adapterID":"fink"}}"#.to_string(),
      r#"{"seq":2,"type":"request","command":"launch","arguments":{"stopOnEntry":false}}"#.to_string(),
      r#"{"seq":3,"type":"request","command":"configurationDone"}"#.to_string(),
      r#"{"seq":4,"type":"request","command":"continue","arguments":{"threadId":1}}"#.to_string(),
      r#"{"seq":5,"type":"request","command":"disconnect"}"#.to_string(),
    ]);

    assert!(
      out.contains("read stdin is not supported"),
      "expected friendly host_read error, got:\n{out}"
    );
    assert!(
      out.contains(r#""event":"terminated""#),
      "expected a 'terminated' event, got:\n{out}"
    );
  }
}
