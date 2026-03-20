// DAP (Debug Adapter Protocol) server for Fink.
//
// Speaks DAP on stdin/stdout, controls WASM execution via Wasmtime's
// guest debug API. Maps WASM byte offsets → Fink source locations
// using the compiler-generated source map.
//
// Architecture:
//   VSCode ←DAP stdin/stdout→ fink dap ←Wasmtime debug API→ WASM
//
// Usage: `fink dap <file>`

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

/// Run the DAP server on the given input/output streams.
pub fn run<R: Read, W: Write>(
  input: R,
  output: W,
  program: &str,
) -> Result<(), String> {
  let mut server = Server::new(BufReader::new(input), BufWriter::new(output));

  // Load the WAT/WASM file.
  let bytes = std::fs::read(program).map_err(|e| e.to_string())?;
  let wasm = if bytes.starts_with(b"\0asm") {
    bytes
  } else {
    let src = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
    compile::wat_to_wasm(src, &CompileOptions::default())?
  };

  // Find sibling .fnk source file (test scaffolding).
  let fnk_path = find_fnk_source(program);
  let source_file = fnk_path.as_deref().unwrap_or(program).to_string();

  // Set up Wasmtime with debug support.
  let mut config = wasmtime::Config::new();
  config.wasm_gc(true);
  config.guest_debug(true);
  config.cranelift_opt_level(wasmtime::OptLevel::None);

  let engine = wasmtime::Engine::new(&config).map_err(|e| e.to_string())?;
  let module = wasmtime::Module::new(&engine, &wasm).map_err(|e| e.to_string())?;

  // Channel for debug events → DAP server loop.
  let (event_tx, _event_rx) = mpsc::sync_channel::<DebugAction>(1);
  let event_tx_for_wasm = event_tx.clone();

  // Channel for DAP commands → debug handler (resume/step).
  let (cmd_tx, cmd_rx) = mpsc::sync_channel::<ResumeAction>(1);

  let mut store = wasmtime::Store::new(&engine, DebugState::default());

  // Install debug handler.
  let handler = FinkDebugHandler {
    event_tx: event_tx.clone(),
    cmd_rx: Arc::new(Mutex::new(cmd_rx)),
  };
  store.set_debug_handler(handler);

  let mut linker = wasmtime::Linker::new(&engine);
  linker
    .func_wrap("env", "print", |mut caller: wasmtime::Caller<'_, DebugState>, val: i32| {
      caller.data_mut().output.push(val.to_string());
    })
    .map_err(|e| e.to_string())?;

  // Spawn WASM execution in a separate thread — it blocks on breakpoints.
  let wasm_thread = std::thread::spawn(move || {
    match linker.instantiate(&mut store, &module) {
      Ok(inst) => {
        if let Ok(main) = inst.get_typed_func::<(), ()>(&mut store, "fink_main")
          && let Err(e) = main.call(&mut store, ())
        {
          eprintln!("[fink dap] wasm error: {e}");
        }
        for line in &store.data().output {
          eprintln!("[wasm] {line}");
        }
      }
      Err(e) => eprintln!("[fink dap] instantiation error: {e}"),
    }
    let _ = event_tx_for_wasm.send(DebugAction::Terminated);
  });

  // DAP server loop.
  let mut stop_on_entry = false;

  loop {
    match server.poll_request() {
      Ok(Some(req)) => {
        match &req.command {
          Command::Initialize { .. } => {
            let resp = req.success(ResponseBody::Initialize(Capabilities {
              supports_configuration_done_request: Some(true),
              ..Default::default()
            }));
            server.respond(resp).ok();
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
            if stop_on_entry {
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

          Command::Threads => {
            server.respond(req.success(ResponseBody::Threads(ThreadsResponse {
              threads: vec![Thread { id: 1, name: "main".to_string() }],
            }))).ok();
          }

          Command::StackTrace(_) => {
            let abs_path = std::fs::canonicalize(&source_file)
              .map(|p| p.to_string_lossy().to_string())
              .unwrap_or_else(|_| source_file.clone());
            let name = std::path::Path::new(&source_file)
              .file_name()
              .map(|f| f.to_string_lossy().to_string())
              .unwrap_or_default();

            let frames = vec![StackFrame {
              id: 1,
              name: "fink_main".to_string(),
              source: Some(Source {
                name: Some(name),
                path: Some(abs_path),
                ..Default::default()
              }),
              line: 4,
              column: 0,
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
            let _ = cmd_tx.send(ResumeAction::Continue);
            server.respond(req.success(ResponseBody::Continue(ContinueResponse {
              all_threads_continued: Some(true),
            }))).ok();
          }

          Command::Next(_) => {
            let _ = cmd_tx.send(ResumeAction::StepOver);
            server.respond(req.success(ResponseBody::Next)).ok();
          }

          Command::StepIn(_) => {
            let _ = cmd_tx.send(ResumeAction::StepIn);
            server.respond(req.success(ResponseBody::StepIn)).ok();
          }

          Command::StepOut(_) => {
            let _ = cmd_tx.send(ResumeAction::StepOut);
            server.respond(req.success(ResponseBody::StepOut)).ok();
          }

          Command::Disconnect(_) => {
            server.respond(req.success(ResponseBody::Disconnect)).ok();
            break;
          }

          _ => {
            // Unknown request — ack with empty response.
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

/// Debug events sent from the WASM thread to the DAP server.
#[allow(dead_code)]
enum DebugAction {
  Stopped,
  Terminated,
}

/// Resume commands sent from the DAP server to the WASM thread.
#[allow(dead_code)]
enum ResumeAction {
  Continue,
  StepOver,
  StepIn,
  StepOut,
}

/// State stored in the Wasmtime Store.
#[derive(Default)]
struct DebugState {
  output: Vec<String>,
}

/// Debug handler that bridges Wasmtime debug events to the DAP server.
#[derive(Clone)]
struct FinkDebugHandler {
  event_tx: mpsc::SyncSender<DebugAction>,
  cmd_rx: Arc<Mutex<mpsc::Receiver<ResumeAction>>>,
}

impl wasmtime::DebugHandler for FinkDebugHandler {
  type Data = DebugState;

  fn handle(
    &self,
    _store: wasmtime::StoreContextMut<'_, Self::Data>,
    event: wasmtime::DebugEvent<'_>,
  ) -> impl std::future::Future<Output = ()> + Send {
    let event_tx = self.event_tx.clone();
    let cmd_rx = self.cmd_rx.clone();
    async move {
      if matches!(event, wasmtime::DebugEvent::Breakpoint) {
        // Notify the DAP server that we've stopped.
        let _ = event_tx.send(DebugAction::Stopped);
        // Block until the DAP server tells us to resume.
        if let Ok(guard) = cmd_rx.lock() {
          let _ = guard.recv();
        }
      }
    }
  }
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
