// DAP (Debug Adapter Protocol) server for Fink.
//
// Speaks DAP on stdin/stdout, controls WASM execution via Wasmtime's
// guest debug API. Maps WASM byte offsets → Fink source locations
// using the compiler-generated source map.
//
// Architecture:
//   VSCode ←DAP stdin/stdout→ fink dap ←Wasmtime debug API→ WASM
//
// The WASM thread runs in Wasmtime with guest_debug enabled. When a
// breakpoint fires, the DebugHandler sends frame info to the DAP server
// via a channel, then blocks waiting for a resume command. The DAP server
// translates the WASM PC to a source location and reports it to VSCode.

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

// ── Source map (hardcoded for tests/wat/add.wat → tests/fnk/add.fnk) ────────

/// Map a WASM PC offset to a (line, col) in the Fink source. 0-indexed internally,
/// converted to 1-indexed for DAP.
fn pc_to_source_location(pc: u32) -> Option<(i64, i64)> {
  // Hardcoded for add.wat. The real compiler will produce these from origin maps.
  // PC offsets from wasm-tools dump of add.wat:
  match pc {
    0x46 => Some((2, 3)),   // local.get $a → "a" in "a + b" (line 2, col 3)
    0x48 => Some((2, 7)),   // local.get $b → "b" in "a + b"
    0x4a => Some((2, 5)),   // i32.add     → "+" in "a + b"
    0x4e => Some((5, 7)),   // i32.const 2 → "2" in "add 2, 3"
    0x50 => Some((5, 10)),  // i32.const 3 → "3" in "add 2, 3"
    0x52 => Some((5, 3)),   // call $add   → "add" in "add 2, 3"
    0x54 => Some((6, 5)),   // call $print → "print" in "| print"
    _ => None,
  }
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
struct DebugState {
  output: Vec<String>,
}

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
  let (wasm, source_file) = if program.ends_with(".fnk") {
    // Fink source: compile through the full pipeline (returns WASM binary directly).
    let src = std::fs::read_to_string(program).map_err(|e| e.to_string())?;
    let result = crate::runner::compile_fnk(&src)?;
    (result.wasm, program.to_string())
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
    (wasm, source_file)
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

  let mut linker = wasmtime::Linker::new(&engine);
  linker
    .func_wrap("env", "print", |mut caller: wasmtime::Caller<'_, DebugState>, val: i32| {
      caller.data_mut().output.push(val.to_string());
    })
    .map_err(|e| e.to_string())?;

  // Spawn WASM execution thread with async runtime (required by guest_debug).
  let terminated_tx = stopped_tx;
  let wasm_thread = std::thread::spawn(move || {
    let rt = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .expect("failed to build tokio runtime");
    rt.block_on(async {
      match linker.instantiate_async(&mut store, &module).await {
        Ok(inst) => {
          if let Ok(main) = inst.get_typed_func::<(), ()>(&mut store, "fink_main")
            && let Err(e) = main.call_async(&mut store, ()).await
          {
            eprintln!("[fink dap] wasm error: {e}");
          }
          for line in &store.data().output {
            eprintln!("[wasm] {line}");
          }
        }
        Err(e) => eprintln!("[fink dap] instantiation error: {e}"),
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
              let (l, c) = pc_to_source_location(frame.pc).unwrap_or((1, 1));
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
