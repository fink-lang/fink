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

use dap::events::{Event, StoppedEventBody};
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

// ── Types ───────────────────────────────────────────────────────────────────

/// Info about a stopped frame, sent from WASM thread → DAP server.
struct StoppedFrame {
  /// Function name (from WASM export or debug name).
  func_name: String,
  /// WASM PC offset within the module.
  pc: u32,
}

/// Commands sent from DAP server → WASM thread.
enum ResumeAction {
  /// Continue execution (disable single-step).
  Continue,
  /// Step to next instruction (enable single-step).
  Step,
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
    // while we still have store access (for single_step toggling).
    if let Some(ref frame) = frame_info {
      let _ = self.stopped_tx.send(StoppedFrame {
        func_name: frame.func_name.clone(),
        pc: frame.pc,
      });
      // Block until DAP server tells us to resume.
      if let Ok(guard) = self.resume_rx.lock()
        && let Ok(action) = guard.recv()
      {
        // Toggle single-step based on action.
        let enable_step = matches!(action, ResumeAction::Step);
        if let Some(mut edit) = store.edit_breakpoints() {
          edit.single_step(enable_step).ok();
        }
      }
    }

    // Return a no-op future (all work done synchronously above).
    async {}
  }
}

// ── DAP server ──────────────────────────────────────────────────────────────

pub fn run<R: Read, W: Write>(
  input: R,
  output: W,
  program: &str,
) -> Result<(), String> {
  eprintln!("[fink dap] starting for: {program}");
  let mut server = Server::new(BufReader::new(input), BufWriter::new(output));

  // Load or compile the program.
  let (wasm, source_file, mappings) = if program.ends_with(".fnk") {
    // Fink source: compile through the full pipeline (returns WASM binary directly).
    let src = std::fs::read_to_string(program).map_err(|e| e.to_string())?;
    let wasm = crate::to_wasm(&src, program)?;
    (wasm.binary, program.to_string(), wasm.mappings)
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
    (wasm, source_file, vec![])
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

  // Enable single-step so we break on the first instruction.
  if let Some(mut edit) = store.edit_breakpoints() {
    edit.single_step(true).ok();
  }

  store.set_debug_handler(FinkDebugHandler {
    stopped_tx: stopped_tx.clone(),
    resume_rx: Arc::new(Mutex::new(resume_rx)),
  });

  // Wire up all "env" imports as stubs that trap (builtins not yet implemented).
  let mut linker = wasmtime::Linker::new(&engine);
  for import in module.imports() {
    if import.module() == "env"
      && let wasmtime::ExternType::Func(ft) = import.ty()
    {
      let name = import.name().to_string();
      let err_name = name.clone();
      linker.func_new("env", &name, ft.clone(), move |_caller, _params, _results| {
        Err(wasmtime::Error::msg(format!("builtin '{}' not yet implemented", err_name)))
      }).map_err(|e| e.to_string())?;
    }
  }

  // Spawn WASM execution thread with async runtime (required by guest_debug).
  let terminated_tx = stopped_tx;
  let wasm_thread = std::thread::spawn(move || {
    let rt = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .expect("failed to build tokio runtime");
    rt.block_on(async {
      let inst = match linker.instantiate_async(&mut store, &module).await {
        Ok(inst) => inst,
        Err(e) => {
          eprintln!("[fink dap] instantiation error: {e}");
          let _ = terminated_tx.send(StoppedFrame { func_name: String::new(), pc: u32::MAX });
          return;
        }
      };

      // CPS execution: box a host continuation, call main(boxed_cont).
      let main_fn = match inst.get_func(&mut store, "main") {
        Some(f) => f,
        None => {
          eprintln!("[fink dap] no 'main' export");
          let _ = terminated_tx.send(StoppedFrame { func_name: String::new(), pc: u32::MAX });
          return;
        }
      };
      let box_func = match inst.get_func(&mut store, "_box_func") {
        Some(f) => f,
        None => {
          eprintln!("[fink dap] no '_box_func' export");
          let _ = terminated_tx.send(StoppedFrame { func_name: String::new(), pc: u32::MAX });
          return;
        }
      };

      // Create a host "done" continuation that logs the result to stderr.
      let main_ty = main_fn.ty(&store);
      let done = wasmtime::Func::new(&mut store, main_ty, |mut caller, params, _results| {
        if let Some(wasmtime::Val::AnyRef(Some(any_ref))) = params.first()
          && let Ok(Some(struct_ref)) = any_ref.as_struct(&caller)
          && let Ok(wasmtime::Val::F64(bits)) = struct_ref.field(&mut caller, 0)
        {
          let v = f64::from_bits(bits);
          if v == v.floor() && v.abs() < 1e15 {
            eprintln!("[fink dap] result: {}", v as i64);
          } else {
            eprintln!("[fink dap] result: {}", v);
          }
        }
        Ok(())
      });

      // Box it via _box_func.
      let mut box_result = [wasmtime::Val::AnyRef(None)];
      if let Err(e) = box_func.call_async(&mut store, &[wasmtime::Val::FuncRef(Some(done))], &mut box_result).await {
        eprintln!("[fink dap] _box_func error: {e}");
        let _ = terminated_tx.send(StoppedFrame { func_name: String::new(), pc: u32::MAX });
        return;
      }

      // Call main with the boxed continuation.
      if let Err(e) = main_fn.call_async(&mut store, &box_result, &mut []).await {
        eprintln!("[fink dap] wasm error: {e}");
      }
    });
    // Signal termination with a sentinel.
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

  loop {
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
                server.send_event(Event::Terminated(None)).ok();
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
              let (l, c) = pc_to_source_location(frame.pc, &mappings).unwrap_or((1, 1));
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
                  server.send_event(Event::Terminated(None)).ok();
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
                  server.send_event(Event::Terminated(None)).ok();
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
              // Resume with single-step — break at next instruction.
              let _ = resume_tx.send(ResumeAction::Step);
              // Wait for next stop or termination.
              match stopped_rx.recv() {
                Ok(frame) if frame.pc == u32::MAX => {
                  running = false;
                  server.send_event(Event::Terminated(None)).ok();
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
                  server.send_event(Event::Terminated(None)).ok();
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
