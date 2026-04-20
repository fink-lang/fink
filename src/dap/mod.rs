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

use crate::passes::wasm::compile::{self, CompileOptions};

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
/// suffices. Returns the `(line, col)` 1-indexed for DAP. Falls back
/// to nearest-preceding mark when there's no exact match — guards
/// against any small drift introduced by `rewrite_body`'s LEB128
/// changes during link-time PC shifting.
fn pc_to_mark_source(
  pc: u32,
  marks: &[crate::passes::debug_marks::MarkRecord],
) -> Option<(i64, i64)> {
  if let Some(m) = marks.iter().find(|m| m.wasm_pc == pc) {
    return Some((m.source.start.line as i64, m.source.start.col as i64));
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
  best.map(|m| (m.source.start.line as i64, m.source.start.col as i64))
}

// ── Runner bootstrap ────────────────────────────────────────────────────────

/// Drive a compiled fink module through the same bootstrap sequence used by
/// `src/runner/wasmtime_runner.rs`, but via `.call_async` so it can run
/// under `guest_debug`. Duplicated from the sync runner on purpose — the
/// plan is to unify once both work (see
/// `.brain/.scratch/dap-runner-bootstrap-plan.md`).
async fn run_module(
  store: &mut wasmtime::Store<DebugState>,
  linker: &wasmtime::Linker<DebugState>,
  module: &wasmtime::Module,
  program_arg: String,
) -> Result<(), String> {
  // Collect dep-init exports before instantiating; order matches linker's
  // topological sort (providers before consumers).
  let dep_init_names: Vec<String> = module
    .exports()
    .filter_map(|e| {
      let n = e.name();
      if n.ends_with(":fink_module") && n != "fink_module" {
        Some(n.to_string())
      } else {
        None
      }
    })
    .collect();

  let instance = linker.instantiate_async(&mut *store, module).await
    .map_err(|e| format!("instantiation error: {e}"))?;

  let box_func = instance.get_func(&mut *store, "_box_func")
    .ok_or("no '_box_func' export")?;
  let apply = instance.get_func(&mut *store, "_apply")
    .ok_or("no '_apply' export")?;
  let list_nil = instance.get_func(&mut *store, "_list_nil")
    .ok_or("no '_list_nil' export")?;
  let list_prepend = instance.get_func(&mut *store, "_list_prepend")
    .ok_or("no '_list_prepend' export")?;
  let fn2_stub = instance.get_func(&mut *store, "_fn2_stub")
    .ok_or("no '_fn2_stub' export")?;
  let done_ty = fn2_stub.ty(&*store);

  // Dep init — box each dep's fink_module, apply with a no-op done.
  for name in &dep_init_names {
    let dep = instance.get_func(&mut *store, name)
      .ok_or_else(|| format!("no '{}' export", name))?;

    let mut boxed_dep = [wasmtime::Val::AnyRef(None)];
    box_func.call_async(&mut *store, &[wasmtime::Val::FuncRef(Some(dep))], &mut boxed_dep).await
      .map_err(|e| format!("_box_func({name}) failed: {e}"))?;

    let noop = wasmtime::Func::new(&mut *store, done_ty.clone(), |_c, _p, _r| Ok(()));
    let mut boxed_noop = [wasmtime::Val::AnyRef(None)];
    box_func.call_async(&mut *store, &[wasmtime::Val::FuncRef(Some(noop))], &mut boxed_noop).await
      .map_err(|e| format!("_box_func(done) for {name} failed: {e}"))?;

    let mut nil = [wasmtime::Val::AnyRef(None)];
    list_nil.call_async(&mut *store, &[], &mut nil).await
      .map_err(|e| format!("_list_nil failed: {e}"))?;
    let mut init_args = [wasmtime::Val::AnyRef(None)];
    list_prepend.call_async(&mut *store, &[boxed_noop[0], nil[0]], &mut init_args).await
      .map_err(|e| format!("_list_prepend failed: {e}"))?;

    apply.call_async(&mut *store, &[init_args[0], boxed_dep[0]], &mut []).await
      .map_err(|e| format!("{name} init failed: {e}"))?;
  }

  // Entry bootstrap — populate export-slot globals (notably `main`).
  let fink_module = instance.get_func(&mut *store, "fink_module")
    .ok_or("no 'fink_module' export")?;

  let mut boxed_module = [wasmtime::Val::AnyRef(None)];
  box_func.call_async(&mut *store, &[wasmtime::Val::FuncRef(Some(fink_module))], &mut boxed_module).await
    .map_err(|e| format!("_box_func(fink_module) failed: {e}"))?;

  let done = wasmtime::Func::new(&mut *store, done_ty, |_c, _p, _r| Ok(()));
  let mut boxed_done = [wasmtime::Val::AnyRef(None)];
  box_func.call_async(&mut *store, &[wasmtime::Val::FuncRef(Some(done))], &mut boxed_done).await
    .map_err(|e| format!("_box_func(done) failed: {e}"))?;

  let mut nil = [wasmtime::Val::AnyRef(None)];
  list_nil.call_async(&mut *store, &[], &mut nil).await
    .map_err(|e| format!("_list_nil failed: {e}"))?;
  let mut init_args = [wasmtime::Val::AnyRef(None)];
  list_prepend.call_async(&mut *store, &[boxed_done[0], nil[0]], &mut init_args).await
    .map_err(|e| format!("_list_prepend failed: {e}"))?;

  apply.call_async(&mut *store, &[init_args[0], boxed_module[0]], &mut []).await
    .map_err(|e| format!("fink_module init failed: {e}"))?;

  // Read `main` from the export global — it's a boxed $Closure now.
  let main_global = instance.get_global(&mut *store, "main")
    .ok_or("no 'main' global export")?;
  let boxed_main = main_global.get(&mut *store);

  // Build argv = [program_name] as $List<$Str>. DAP doesn't yet support
  // user-supplied CLI args — follow-up when we need them.
  let args_list = build_args_list_async(&mut *store, &instance, &[program_arg.into_bytes()]).await?;

  let run_main = instance.get_func(&mut *store, "_run_main")
    .ok_or("no '_run_main' export")?;
  run_main.call_async(&mut *store, &[boxed_main, args_list], &mut []).await
    .map_err(|e| format!("_run_main failed: {e}"))?;

  Ok(())
}

/// Build a fink $List<$Str> from raw byte-string args (async variant).
async fn build_args_list_async(
  store: &mut wasmtime::Store<DebugState>,
  instance: &wasmtime::Instance,
  args: &[Vec<u8>],
) -> Result<wasmtime::Val, String> {
  let list_nil = instance.get_func(&mut *store, "_list_nil")
    .ok_or("no '_list_nil' export")?;
  let list_prepend = instance.get_func(&mut *store, "_list_prepend")
    .ok_or("no '_list_prepend' export")?;

  let mut acc = [wasmtime::Val::AnyRef(None)];
  list_nil.call_async(&mut *store, &[], &mut acc).await
    .map_err(|e| format!("_list_nil failed: {e}"))?;

  for arg in args.iter().rev() {
    let s = bytes_to_str_async(&mut *store, instance, arg).await?;
    let mut next = [wasmtime::Val::AnyRef(None)];
    list_prepend.call_async(&mut *store, &[s, acc[0]], &mut next).await
      .map_err(|e| format!("_list_prepend failed: {e}"))?;
    acc = next;
  }
  Ok(acc[0])
}

/// Allocate a $Str on the GC heap from raw bytes via `_str_wrap_bytes`.
async fn bytes_to_str_async(
  store: &mut wasmtime::Store<DebugState>,
  instance: &wasmtime::Instance,
  data: &[u8],
) -> Result<wasmtime::Val, String> {
  let array_ty = wasmtime::ArrayType::new(
    store.engine(),
    wasmtime::FieldType::new(wasmtime::Mutability::Var, wasmtime::StorageType::I8),
  );
  let alloc = wasmtime::ArrayRefPre::new(&mut *store, array_ty);
  let elems: Vec<wasmtime::Val> = data.iter().map(|&b| wasmtime::Val::I32(b as i32)).collect();
  let array = wasmtime::ArrayRef::new_fixed(&mut *store, &alloc, &elems)
    .map_err(|e| format!("byte array alloc failed: {e}"))?;

  let wrap_fn = instance.get_func(&mut *store, "_str_wrap_bytes")
    .ok_or("no '_str_wrap_bytes' export")?;
  let array_any = array.to_anyref();
  let mut result = [wasmtime::Val::AnyRef(None)];
  wrap_fn.call_async(&mut *store, &[wasmtime::Val::AnyRef(Some(array_any))], &mut result).await
    .map_err(|e| format!("_str_wrap_bytes failed: {e}"))?;
  Ok(result[0])
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

/// Bridges Wasmtime debug events to the DAP server via channels.
#[derive(Clone)]
struct FinkDebugHandler {
  /// Send stopped frame info to the DAP server.
  stopped_tx: mpsc::SyncSender<StoppedFrame>,
  /// Receive resume commands from the DAP server.
  resume_rx: Arc<Mutex<mpsc::Receiver<ResumeAction>>>,
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
        pc = wasm_pc;
        func_name = format!("func[{}]", func_idx.as_u32());
      }
      Some(StoppedFrame { func_name, pc })
    } else {
      None
    };

    // Send frame info and wait for resume — must happen synchronously
    // while we still have store access.
    if let Some(ref frame) = frame_info {
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

    // Return a no-op future (all work done synchronously above).
    async {}
  }
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
  let (wasm, source_file, mappings, marks) = if program.ends_with(".fnk") {
    // Fink source: compile through the full pipeline (returns WASM binary directly).
    let src = std::fs::read_to_string(program).map_err(|e| e.to_string())?;
    let wasm = crate::to_wasm(&src, program)?;
    (wasm.binary, program.to_string(), wasm.mappings, wasm.marks)
  } else {
    let bytes = std::fs::read(program).map_err(|e| e.to_string())?;
    let wasm = if bytes.starts_with(b"\0asm") {
      bytes
    } else {
      let src = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
      compile::wat_to_wasm(src, &CompileOptions::default())?
    };
    let fnk_path = find_fnk_source(program);
    let source_file = fnk_path.as_deref().unwrap_or(program).to_string();
    (wasm, source_file, vec![], vec![])
  };

  // Set up Wasmtime with debug support.
  let mut config = wasmtime::Config::new();
  config.wasm_gc(true);
  config.wasm_tail_call(true);
  config.wasm_function_references(true);
  config.guest_debug(true);
  config.cranelift_opt_level(wasmtime::OptLevel::None);

  let engine = wasmtime::Engine::new(&config).map_err(|e| e.to_string())?;
  let module = wasmtime::Module::new(&engine, &wasm).map_err(|e| e.to_string())?;

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
        edit.add_breakpoint(&module, m.wasm_pc).ok();
      }
    }
  }

  store.set_debug_handler(FinkDebugHandler {
    stopped_tx: stopped_tx.clone(),
    resume_rx: Arc::new(Mutex::new(resume_rx)),
  });

  // Wire env imports. Mirrors `src/runner/wasmtime_runner.rs` — routes the
  // full CPS runtime (host_exit/panic/channel_send/read/resume) rather than
  // trapping everything. DAP inherits the parent process's stdout/stderr so
  // program output flows through the normal Fink IO channels (tag=1→stdout,
  // tag=2→stderr) without DAP needing a result-printer of its own.
  let mut linker = wasmtime::Linker::new(&engine);
  let exit_code: Arc<Mutex<i64>> = Arc::new(Mutex::new(0));
  for import in module.imports() {
    if import.module() == "env"
      && let wasmtime::ExternType::Func(ft) = import.ty()
    {
      let name = import.name().to_string();
      match name.as_str() {
        "host_exit" => {
          let code = exit_code.clone();
          linker.func_new("env", &name, ft.clone(), move |_caller, params, _results| {
            *code.lock().unwrap() = params[0].unwrap_i32() as i64;
            Ok(())
          }).map_err(|e| e.to_string())?;
        }
        "host_panic" => {
          linker.func_new("env", &name, ft.clone(), move |_caller, _params, _results| {
            Err(wasmtime::Error::msg("fink panic: irrefutable pattern failed"))
          }).map_err(|e| e.to_string())?;
        }
        "host_channel_send" => {
          // Route debuggee stdout/stderr into DAP `Output` events so the
          // bytes surface in VSCode's Debug Console instead of being
          // lost. Writing to the process's real stdout would corrupt the
          // DAP JSON stream (VSCode reads DAP framing from our stdout).
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
        "host_resume" => {
          // DAP doesn't yet drive real stdin, so there are never pending
          // reads to settle. Make host_resume a no-op — "host has nothing
          // to add to the task queue." The scheduler checks the queue
          // again after this returns; if still empty, the program ends
          // cleanly. Trapping here (the previous behaviour) caused the
          // program to abort on the very first scheduler tick past the
          // last user mark.
          linker.func_new("env", &name, ft.clone(), move |_caller, _params, _results| {
            Ok(())
          }).map_err(|e| e.to_string())?;
        }
        _ => {
          // host_read + any other unknown env imports — trap for now.
          // DAP sessions don't yet plumb stdin reads through the debug
          // loop; that's a follow-up.
          let err_name = name.clone();
          linker.func_new("env", &name, ft.clone(), move |_caller, _params, _results| {
            Err(wasmtime::Error::msg(format!("builtin '{}' not yet implemented in DAP", err_name)))
          }).map_err(|e| e.to_string())?;
        }
      }
    }
  }

  // Clone program path for argv (argv[0] = program name, C-style).
  let program_arg = program.to_string();

  // Spawn WASM execution thread with async runtime (required by guest_debug).
  // Mirrors the runner bootstrap: dep init loop → fink_module init → read
  // `main` global → _run_main(boxed_main, argv_list). All via `.call_async`.
  let terminated_tx = stopped_tx;
  let wasm_thread = std::thread::spawn(move || {
    let rt = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .expect("failed to build tokio runtime");
    rt.block_on(async {
      if let Err(e) = run_module(&mut store, &linker, &module, program_arg).await {
        eprintln!("[fink dap] {e}");
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
            // WASM thread is running with single_step enabled.
            // Wait for the first breakpoint event.
            if stop_on_entry
              && let Ok(frame) = stopped_rx.recv()
            {
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
          }

          Command::Threads => {
            server.respond(req.success(ResponseBody::Threads(ThreadsResponse {
              threads: vec![Thread { id: 1, name: "main".to_string() }],
            }))).ok();
          }

          Command::StackTrace(_) => {
            let (line, col, name) = if let Some(ref frame) = last_frame {
              // Prefer mark-based source resolution: every breakpoint
              // we install corresponds to a MarkRecord with an exact
              // source `Loc`. Fall back to the legacy DWARF-derived
              // mapping for non-mark stops (e.g. legacy single_step
              // path when marks is empty).
              let (l, c) = pc_to_mark_source(frame.pc, &marks)
                .or_else(|| pc_to_source_location(frame.pc, &mappings))
                .unwrap_or((1, 1));
              (l, c, frame.func_name.clone())
            } else {
              (1, 1, "?".to_string())
            };
            let frames = vec![StackFrame {
              id: 1,
              name,
              source: Some(Source {
                name: Some(file_name.clone()),
                path: Some(abs_path.clone()),
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
            let bps: Vec<Breakpoint> = args.breakpoints.as_ref()
              .map(|bps| {
                bps.iter().map(|bp| Breakpoint {
                  verified: true,
                  line: Some(bp.line),
                  ..Default::default()
                }).collect()
              })
              .unwrap_or_default();
            server.respond(req.success(ResponseBody::SetBreakpoints(
              SetBreakpointsResponse { breakpoints: bps },
            ))).ok();
          }

          Command::Continue(_) => {
            server.respond(req.success(ResponseBody::Continue(ContinueResponse {
              all_threads_continued: Some(true),
            }))).ok();
            if running {
              // Resume without single-step — run until next breakpoint or end.
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
  /// Writes the source to a tempfile, builds a framed input buffer with the
  /// full launch→continue→disconnect sequence, then runs the DAP server to
  /// completion on a thread with a timeout so a broken bootstrap can't hang
  /// the test suite forever.
  fn drive_session(src: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.fnk");
    std::fs::write(&path, src).unwrap();
    let path_str = path.to_string_lossy().into_owned();

    let mut input = Vec::new();
    input.extend_from_slice(&frame(r#"{"seq":1,"type":"request","command":"initialize","arguments":{"adapterID":"fink"}}"#));
    input.extend_from_slice(&frame(r#"{"seq":2,"type":"request","command":"launch","arguments":{"stopOnEntry":true}}"#));
    input.extend_from_slice(&frame(r#"{"seq":3,"type":"request","command":"configurationDone"}"#));
    input.extend_from_slice(&frame(r#"{"seq":4,"type":"request","command":"continue","arguments":{"threadId":1}}"#));
    input.extend_from_slice(&frame(r#"{"seq":5,"type":"request","command":"disconnect"}"#));

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
    let out = drive_session("main = fn done: done 42\n");

    assert!(
      out.contains(r#""event":"stopped""#),
      "expected a 'stopped' event in DAP output, got:\n{out}"
    );
    assert!(
      out.contains(r#""event":"terminated""#),
      "expected a 'terminated' event in DAP output, got:\n{out}"
    );
  }
}
