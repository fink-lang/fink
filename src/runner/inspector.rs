// CDP inspector bridge between V8's built-in inspector and a WebSocket client
// (VSCode / Chrome DevTools).
//
// Architecture:
//
//   Main thread (V8 isolate):
//     - Runs JS via script.run()
//     - When V8 pauses: run_message_loop_on_pause() spins a JS pump loop
//     - Each JS iteration is a V8-safe execution point where request_interrupt
//       callbacks fire — the callback drains MSG_QUEUE and dispatches to V8
//     - When V8 resumes: quit_message_loop_on_pause() terminates the pump loop
//
//   Reader thread:
//     - Reads incoming WebSocket messages (blocking I/O with 5ms timeout)
//     - Pushes messages into Arc<Mutex<VecDeque>> queue
//     - Calls IsolateHandle::request_interrupt to schedule dispatch on the
//       main thread at the next V8-safe bytecode execution point
//
// Why request_interrupt:
//   dispatch_protocol_message must NOT be called directly from
//   run_message_loop_on_pause (it crashes V8 — the pause callback stack is
//   not a safe dispatch point). request_interrupt ensures dispatch happens
//   between bytecodes, where V8's heap is in a consistent state.
//
// Pump loop:
//   run_message_loop_on_pause runs `while (__fink_paused__) { __fink_pump__() }`
//   using the stored isolate pointer. Each iteration gives V8 a safe execution
//   point. The interrupt callback fires, drains MSG_QUEUE, and calls
//   dispatch_protocol_message. When the debugger resumes, quit_message_loop
//   sets __fink_paused__ = false and the pump JS exits.
//
// Setup order:
//   1. V8Inspector::create(isolate, client)   -- before any HandleScope
//   2. inspector.context_created(ctx, ...)    -- after entering ContextScope
//   3. accept_connection(port)                -- blocks until VSCode connects
//   4. inspector.connect(...) -> session      -- creates the CDP session
//   5. handshake pumped synchronously on main thread (no JS, no interrupts)
//   6. reader thread spawned
//   7. session.schedule_pause_on_next_statement()
//   8. run the JS/WASM — V8 pauses, pump loop runs, messages dispatched via interrupt
//
// VSCode launch.json:
//   { "type": "node", "request": "attach",
//     "websocketAddress": "ws://localhost:9229" }

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use tungstenite::{Message, WebSocket};

use v8::inspector::{
  Channel, ChannelImpl, StringBuffer, StringView, V8Inspector,
  V8InspectorClient, V8InspectorClientImpl, V8InspectorClientTrustLevel,
  V8InspectorSession,
};

// ── Thread-local state (main/V8 thread only) ─────────────────────────────────

thread_local! {
  /// Depth of nested run_message_loop_on_pause calls.
  static PAUSE_DEPTH: Cell<u32> = const { Cell::new(0) };

  /// Set when the client finishes its handshake (Runtime.runIfWaitingForDebugger).
  static DEBUGGER_READY: Cell<bool> = const { Cell::new(false) };

  /// The debuggerId returned by V8 during Debugger.enable in the handshake.
  /// Replayed in synthetic Debugger.enable responses while paused so VSCode
  /// keeps a consistent session ID and doesn't block step commands.
  static DEBUGGER_ID: RefCell<Option<String>> = const { RefCell::new(None) };

  /// The last Debugger.paused notification sent by V8.
  /// Replayed after a synthetic Debugger.enable response so VSCode has a
  /// valid call frame and enables step/resume commands.
  static LAST_PAUSED: RefCell<Option<String>> = const { RefCell::new(None) };

  /// Raw isolate pointer used by ensure_default_context_in_group and the
  /// pause pump loop.
  static ISOLATE_PTR: RefCell<Option<v8::UnsafeRawIsolatePtr>> =
    const { RefCell::new(None) };

  static CONTEXT_GLOBAL: RefCell<Option<v8::Global<v8::Context>>> =
    const { RefCell::new(None) };
}

/// Raw pointer to the active V8InspectorSession.
/// Safety: written/read on the V8 thread only.
static mut SESSION_PTR: *const V8InspectorSession = std::ptr::null();

// ── Cross-thread state ────────────────────────────────────────────────────────

/// Incoming CDP messages queued by the reader thread; drained by the interrupt
/// callback on the main thread.
static MSG_QUEUE: std::sync::LazyLock<Arc<Mutex<std::collections::VecDeque<String>>>> =
  std::sync::LazyLock::new(|| Arc::new(Mutex::new(std::collections::VecDeque::new())));

/// Outgoing CDP message channel (main thread → reader/writer thread).
static WS_TX: std::sync::LazyLock<Arc<Mutex<Option<mpsc::SyncSender<String>>>>> =
  std::sync::LazyLock::new(|| Arc::new(Mutex::new(None)));

static WS_RX: std::sync::LazyLock<Arc<Mutex<Option<mpsc::Receiver<String>>>>> =
  std::sync::LazyLock::new(|| Arc::new(Mutex::new(None)));

/// WebSocket owned exclusively by the reader thread after attach().
/// During pump_until_ready() the main thread owns it directly.
static WS: std::sync::LazyLock<Arc<Mutex<Option<WebSocket<TcpStream>>>>> =
  std::sync::LazyLock::new(|| Arc::new(Mutex::new(None)));

/// Thread-safe isolate handle used by the reader thread to request interrupts.
static ISOLATE_HANDLE: std::sync::LazyLock<Arc<Mutex<Option<v8::IsolateHandle>>>> =
  std::sync::LazyLock::new(|| Arc::new(Mutex::new(None)));

/// Handle to unpark the main thread from the reader thread.
static MAIN_THREAD: std::sync::LazyLock<Arc<Mutex<Option<std::thread::Thread>>>> =
  std::sync::LazyLock::new(|| Arc::new(Mutex::new(None)));

/// Script source texts keyed by V8 scriptId string.
/// Populated when Debugger.scriptParsed is sent (scriptId extracted from notification);
/// used to answer Debugger.getScriptSource while paused.
static SCRIPT_SOURCES: std::sync::LazyLock<Arc<Mutex<HashMap<String, String>>>> =
  std::sync::LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

/// Pending source registrations: URL → source text.
/// Call register_script_source(url, src) before running JS; when scriptParsed fires
/// with a matching URL, the scriptId→source mapping is stored in SCRIPT_SOURCES.
static PENDING_SOURCES: std::sync::LazyLock<Arc<Mutex<HashMap<String, String>>>> =
  std::sync::LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

/// Register a JS source for a given URL so that Debugger.getScriptSource can return
/// it once V8 assigns the scriptId via the Debugger.scriptParsed notification.
pub fn register_script_source(url: &str, source: &str) {
  if let Ok(mut map) = PENDING_SOURCES.lock() {
    map.insert(url.to_string(), source.to_string());
  }
}

// ── Channel — forwards outgoing CDP messages over the WebSocket ───────────────

struct WsChannel;

impl ChannelImpl for WsChannel {
  fn send_response(&self, _call_id: i32, message: v8::UniquePtr<StringBuffer>) {
    send_to_ws(message);
  }
  fn send_notification(&self, message: v8::UniquePtr<StringBuffer>) {
    send_to_ws(message);
  }
  fn flush_protocol_notifications(&self) {}
}

fn send_to_ws(mut msg: v8::UniquePtr<StringBuffer>) {
  let text = string_view_to_string(msg.as_mut().unwrap().string());
  eprintln!("[fink] -> {text}");
  // Capture debuggerId from V8's Debugger.enable response.
  if text.contains("\"debuggerId\"")
    && let Some(id) = extract_json_str(&text, "debuggerId")
  {
    DEBUGGER_ID.with(|d| *d.borrow_mut() = Some(id.to_string()));
  }
  // Capture the last Debugger.paused notification so we can replay it after
  // a synthetic Debugger.enable response while paused.
  if text.contains("\"method\":\"Debugger.paused\"") {
    LAST_PAUSED.with(|p| *p.borrow_mut() = Some(text.clone()));
  }
  // When V8 emits scriptParsed, correlate with any pending source registration
  // so Debugger.getScriptSource can return the source text later.
  if text.contains("\"method\":\"Debugger.scriptParsed\"")
    && let (Some(script_id), Some(url)) = (
      extract_json_str(&text, "scriptId"),
      extract_json_str(&text, "url"),
    )
    && let Ok(mut pending) = PENDING_SOURCES.lock()
    && let Some(source) = pending.remove(url)
    && let Ok(mut sources) = SCRIPT_SOURCES.lock()
  {
    sources.insert(script_id.to_string(), source);
  }
  if let Ok(guard) = WS_TX.lock()
    && let Some(tx) = guard.as_ref()
  {
    let _ = tx.send(text);
  }
}

fn send_str_to_ws(text: String) {
  if let Ok(guard) = WS_TX.lock()
    && let Some(tx) = guard.as_ref()
  {
    let _ = tx.send(text);
  }
}

// ── Interrupt callback — dispatches queued CDP messages at a V8-safe point ───

/// Called by V8 on the main thread between bytecodes (safe dispatch point).
/// Drains MSG_QUEUE and dispatches each message to the inspector session.
unsafe extern "C" fn dispatch_interrupt(
  _isolate: v8::UnsafeRawIsolatePtr,
  _data: *mut c_void,
) {
  loop {
    let msg = MSG_QUEUE.lock().ok().and_then(|mut q| q.pop_front());
    let Some(text) = msg else { break };
    dispatch_message_inner(text.as_bytes());
  }
}

/// Dispatch a single CDP message to V8 unconditionally.
/// Must only be called at a V8-safe point (interrupt callback or outside pause).
fn dispatch_message_inner(bytes: &[u8]) {
  let view = StringView::from(bytes);
  eprintln!("[fink] dispatching to V8...");
  unsafe {
    if !SESSION_PTR.is_null() {
      (*SESSION_PTR).dispatch_protocol_message(view);
    }
  }
  eprintln!("[fink] dispatch returned");
}

/// Dispatch a CDP message from within run_message_loop_on_pause.
///
/// Only execution-control methods (resume/step/pause) are forwarded to V8 —
/// they cause V8 to call quit_message_loop_on_pause and exit the paused state.
/// A CallbackScope is created so V8 has an active HandleScope during dispatch.
///
/// All other methods get synthetic responses. V8 crashes when most CDP methods
/// are dispatched while on the pause C++ call stack (even with a HandleScope).
fn dispatch_pause_message(bytes: &[u8]) {
  let Ok(text) = std::str::from_utf8(bytes) else { return };
  let id = extract_json_id(text);
  let method = extract_json_str(text, "method").unwrap_or("");

  let execution_control = [
    "Debugger.resume",
    "Debugger.stepOver",
    "Debugger.stepInto",
    "Debugger.stepOut",
    "Debugger.pause",
  ];

  if execution_control.contains(&method) {
    dispatch_message_inner(bytes);
    return;
  }

  // Everything else: return a synthetic response so VSCode doesn't hang.
  let Some(id) = id else { return };

  let domain = method.split('.').next().unwrap_or("");
  let v8_domains = ["Debugger", "Runtime", "Profiler", "HeapProfiler"];
  if !domain.is_empty() && !v8_domains.contains(&domain) {
    let resp = format!(
      "{{\"id\":{id},\"error\":{{\"code\":-32601,\"message\":\"'{method}' wasn't found\"}}}}"
    );
    eprintln!("[fink] -> {resp}  (synthetic methodNotFound)");
    send_str_to_ws(resp);
    return;
  }

  let result_json = if method == "Debugger.getScriptSource" {
    // Return the registered source text for the requested scriptId, if available.
    let script_id = extract_json_str(text, "scriptId").unwrap_or("");
    let source = SCRIPT_SOURCES
      .lock()
      .ok()
      .and_then(|m| m.get(script_id).cloned())
      .unwrap_or_default();
    eprintln!("[fink] -> (Debugger.getScriptSource scriptId={script_id} len={})  id={id}", source.len());
    // CDP spec: result = { scriptSource: string }
    let escaped = source.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\r', "\\r");
    format!("{{\"scriptSource\":\"{escaped}\"}}")
  } else if method == "Runtime.evaluate" || method == "Debugger.evaluateOnCallFrame" {
    eprintln!("[fink] -> (synthetic {method})  id={id}");
    r#"{"result":{"type":"undefined"}}"#.to_string()
  } else if method == "Debugger.enable" {
    let dbg_id = DEBUGGER_ID.with(|d| d.borrow().clone()).unwrap_or_default();
    eprintln!("[fink] -> (synthetic Debugger.enable debuggerId={dbg_id})  id={id}");
    // Also replay the last Debugger.paused notification so VSCode has a valid
    // call frame and enables step/resume commands after re-enabling.
    if let Some(paused) = LAST_PAUSED.with(|p| p.borrow().clone()) {
      eprintln!("[fink] -> (replaying Debugger.paused)");
      send_str_to_ws(paused);
    }
    format!("{{\"debuggerId\":\"{dbg_id}\"}}")
  } else {
    eprintln!("[fink] -> (synthetic {method})  id={id}");
    "{}".to_string()
  };

  let resp = format!("{{\"id\":{id},\"result\":{result_json}}}");
  send_str_to_ws(resp);
}

// ── Inspector client ──────────────────────────────────────────────────────────

struct InspectorClient;

impl V8InspectorClientImpl for InspectorClient {
  /// Called by V8 when execution is paused (breakpoint / pause-on-next).
  ///
  /// Runs a JS `while (__fink_paused__) {}` pump loop. Each iteration is a
  /// V8-safe execution point where request_interrupt callbacks fire. The
  /// interrupt callback (dispatch_interrupt) drains MSG_QUEUE and calls
  /// dispatch_protocol_message safely.
  ///
  /// quit_message_loop_on_pause sets __fink_paused__ = false via
  /// terminate_execution to break the loop.
  fn run_message_loop_on_pause(&self, _context_group_id: i32) {
    let depth = PAUSE_DEPTH.with(|d| { let v = d.get(); d.set(v + 1); v + 1 });
    eprintln!("[fink] paused (depth={depth})");

    // Dispatch CDP messages from the pause callback.
    // Only execution-control methods (resume/step) are dispatched to V8 — they
    // cause V8 to call quit_message_loop_on_pause and exit the paused state.
    // All other methods get synthetic responses so VSCode doesn't hang.
    // A CallbackScope is created before each dispatch so V8 has an active
    // HandleScope, which is required for dispatch_protocol_message.
    loop {
      if PAUSE_DEPTH.with(|d| d.get()) < depth {
        break;
      }
      let mut dispatched = false;
      loop {
        let msg = MSG_QUEUE.lock().ok().and_then(|mut q| q.pop_front());
        let Some(text) = msg else { break };
        dispatch_pause_message(text.as_bytes());
        dispatched = true;
      }
      if !dispatched {
        std::thread::park();
      }
    }

    eprintln!("[fink] resuming (depth={depth})");
  }

  fn quit_message_loop_on_pause(&self) {
    PAUSE_DEPTH.with(|d| {
      let v = d.get();
      eprintln!("[fink] quit_message_loop_on_pause depth={v}");
      if v > 0 { d.set(v - 1); }
    });
    // Unpark the main thread so the park loop in run_message_loop_on_pause
    // re-checks PAUSE_DEPTH and exits.
    if let Ok(guard) = MAIN_THREAD.lock()
      && let Some(t) = guard.as_ref()
    {
      t.unpark();
    }
  }

  fn run_if_waiting_for_debugger(&self, _context_group_id: i32) {
    DEBUGGER_READY.with(|r| r.set(true));
  }

  /// Called by V8 when it needs the default context for a context group.
  /// Pattern follows Deno's JsRuntimeInspectorState implementation.
  fn ensure_default_context_in_group(
    &self,
    context_group_id: i32,
  ) -> Option<v8::Local<'_, v8::Context>> {
    eprintln!("[fink] ensure_default_context_in_group(group={context_group_id})");
    let mut isolate_ptr = ISOLATE_PTR.with(|p| *p.borrow())?;
    CONTEXT_GLOBAL.with(|g| {
      let guard = g.borrow();
      let global = guard.as_ref()?;
      // SAFETY: inside a V8 callback on the main thread.
      let isolate = unsafe { v8::Isolate::ref_from_raw_isolate_ptr_mut(&mut isolate_ptr) };
      let mut scope_storage = unsafe { v8::CallbackScope::new(isolate) };
      let scope = unsafe { std::pin::Pin::new_unchecked(&mut scope_storage).init() };
      let local = v8::Local::new(&scope, global);
      // SAFETY: the Local is valid for the lifetime of the enclosing V8 callback.
      Some(unsafe { local.extend_lifetime_unchecked() })
    })
  }
}

// ── Public API ────────────────────────────────────────────────────────────────

pub struct DebugSession {
  _inspector: V8Inspector,
  // Boxed so the session has a stable heap address.  SESSION_PTR points into
  // this Box; moving DebugSession (e.g. returning it from attach()) does not
  // move the underlying V8InspectorSession, keeping SESSION_PTR valid.
  session: Box<V8InspectorSession>,
}

impl DebugSession {
  pub fn schedule_pause(&self) {
    self.session.schedule_pause_on_next_statement(
      StringView::from(b"debugCommand" as &[u8]),
      StringView::from(b"" as &[u8]),
    );
  }
}

impl Drop for DebugSession {
  fn drop(&mut self) {
    unsafe { SESSION_PTR = std::ptr::null() };
    ISOLATE_PTR.with(|p| { *p.borrow_mut() = None; });
    CONTEXT_GLOBAL.with(|g| g.borrow_mut().take());
    DEBUGGER_ID.with(|d| d.borrow_mut().take());
    LAST_PAUSED.with(|p| p.borrow_mut().take());
    if let Ok(mut guard) = ISOLATE_HANDLE.lock() {
      guard.take();
    }
    if let Ok(mut guard) = WS_TX.lock() {
      guard.take();
    }
    if let Ok(mut guard) = WS_RX.lock() {
      guard.take();
    }
    if let Ok(mut guard) = WS.lock() {
      guard.take();
    }
    if let Ok(mut guard) = MAIN_THREAD.lock() {
      guard.take();
    }
  }
}

pub fn create_inspector(isolate: &mut v8::Isolate) -> V8Inspector {
  let client = V8InspectorClient::new(Box::new(InspectorClient));
  V8Inspector::create(isolate, client)
}

pub fn attach(
  inspector: V8Inspector,
  scope: &mut v8::PinScope,
  context: v8::Local<v8::Context>,
  port: u16,
) -> Result<DebugSession, String> {
  // Store isolate ptr and Global<Context>.
  let isolate_ptr = unsafe {
    <v8::PinScope as AsRef<v8::Isolate>>::as_ref(scope).as_raw_isolate_ptr()
  };
  ISOLATE_PTR.with(|p| { *p.borrow_mut() = Some(isolate_ptr); });
  CONTEXT_GLOBAL.with(|g| {
    g.borrow_mut().replace(v8::Global::new(scope, context));
  });

  // Store the thread-safe isolate handle for the reader thread.
  let handle = scope.thread_safe_handle();
  *ISOLATE_HANDLE.lock().unwrap() = Some(handle);

  // Register context with the inspector (group 1 = "main world").
  inspector.context_created(
    context,
    1,
    StringView::from(b"fink" as &[u8]),
    StringView::from(b"{\"isDefault\":true,\"type\":\"default\"}" as &[u8]),
  );

  // Block until the debugger connects.
  let ws = wait_for_ws(port)?;
  ws.get_ref().set_read_timeout(Some(std::time::Duration::from_millis(5))).ok();
  *WS.lock().unwrap() = Some(ws);

  let (tx, rx) = mpsc::sync_channel::<String>(256);
  *WS_TX.lock().unwrap() = Some(tx);
  *WS_RX.lock().unwrap() = Some(rx);

  // Create the CDP session.
  let channel = Channel::new(Box::new(WsChannel));
  let session = Box::new(inspector.connect(
    1,
    channel,
    StringView::from(b"{}" as &[u8]),
    V8InspectorClientTrustLevel::FullyTrusted,
  ));

  // SESSION_PTR points into the Box, which has a stable heap address for the
  // lifetime of DebugSession regardless of how many times DebugSession itself
  // is moved (e.g. returned from this function via Ok(ds)).
  unsafe { SESSION_PTR = &*session as *const _ };

  // Pump handshake messages synchronously on the main thread until VSCode
  // sends Runtime.runIfWaitingForDebugger. Safe: no JS executes, no interrupts.
  eprintln!("[fink] Debugger connected — waiting for client ready signal...");
  pump_until_ready();
  eprintln!("[fink] Debugger ready");

  // Store main thread handle for park/unpark from reader thread and quit callback.
  *MAIN_THREAD.lock().unwrap() = Some(std::thread::current());

  // Spawn the reader thread.
  std::thread::spawn(reader_thread);

  Ok(DebugSession { _inspector: inspector, session })
}

/// Reader/writer thread: handles all WebSocket I/O and schedules interrupts.
fn reader_thread() {
  loop {
    let read_result = WS.lock().ok().and_then(|mut guard| {
      let ws = guard.as_mut()?;
      // Drain outgoing queue.
      if let Ok(rx_guard) = WS_RX.lock()
        && let Some(rx) = rx_guard.as_ref()
      {
        while let Ok(text) = rx.try_recv() {
          if let Err(e) = ws.send(Message::Text(text.into())) {
            eprintln!("[fink] send error: {e}");
          }
        }
      }
      Some(ws.read())
    });

    match read_result {
      Some(Ok(Message::Text(text))) => {
        eprintln!("[fink] <- {text}");
        MSG_QUEUE.lock().unwrap().push_back(text.to_string());
        // Request interrupt so V8 dispatches at the next safe point inside
        // its internal pause event loop. Also unpark in case the main thread
        // is parked in run_message_loop_on_pause waiting for the depth check.
        if let Ok(guard) = ISOLATE_HANDLE.lock()
          && let Some(handle) = guard.as_ref()
        {
          handle.request_interrupt(dispatch_interrupt, std::ptr::null_mut());
        }
        if let Ok(guard) = MAIN_THREAD.lock()
          && let Some(t) = guard.as_ref()
        {
          t.unpark();
        }
      }
      Some(Ok(Message::Close(_))) | None => {
        eprintln!("[fink] debugger disconnected");
        // Decrement pause depth and unpark so the pause loop exits cleanly.
        PAUSE_DEPTH.with(|d| { if d.get() > 0 { d.set(0); } });
        if let Ok(guard) = MAIN_THREAD.lock()
          && let Some(t) = guard.as_ref()
        {
          t.unpark();
        }
        break;
      }
      Some(Ok(_)) => {}
      Some(Err(tungstenite::Error::Io(ref e)))
        if e.kind() == std::io::ErrorKind::WouldBlock
          || e.kind() == std::io::ErrorKind::TimedOut => {}
      Some(Err(e)) => {
        eprintln!("[fink] ws read error: {e}");
        PAUSE_DEPTH.with(|d| { if d.get() > 0 { d.set(0); } });
        if let Ok(guard) = MAIN_THREAD.lock()
          && let Some(t) = guard.as_ref()
        {
          t.unpark();
        }
        break;
      }
    }
  }
}

/// Pump CDP handshake messages synchronously on the main thread.
/// Called before the reader thread is spawned; dispatches directly to V8
/// (safe: no JS executes during handshake, no interrupt needed).
fn pump_until_ready() {
  while !DEBUGGER_READY.with(|r| r.get()) {
    let msg = WS.lock().ok().and_then(|mut guard| {
      guard.as_mut().and_then(|ws| ws.read().ok())
    });
    match msg {
      Some(Message::Text(text)) => {
        eprintln!("[fink] <- {text}");
        // NodeWorker and other unknown domains get synthetic methodNotFound.
        // Everything else goes directly to V8 (safe during handshake).
        let method = extract_json_str(&text, "method").unwrap_or("");
        let domain = method.split('.').next().unwrap_or("");
        let v8_domains = ["Debugger", "Runtime", "Profiler", "HeapProfiler"];
        if !domain.is_empty() && !v8_domains.contains(&domain) {
          if let Some(id) = extract_json_id(&text) {
            let resp = format!(
              "{{\"id\":{id},\"error\":{{\"code\":-32601,\"message\":\"'{method}' wasn't found\"}}}}"
            );
            eprintln!("[fink] -> {resp}  (synthetic methodNotFound)");
            send_str_to_ws(resp);
          }
        } else {
          dispatch_message_inner(text.as_bytes());
        }
        // Flush responses synchronously (reader thread not yet spawned).
        if let Ok(mut guard) = WS.lock()
          && let Some(ws) = guard.as_mut()
          && let Ok(rx_guard) = WS_RX.lock()
          && let Some(rx) = rx_guard.as_ref()
        {
          while let Ok(resp) = rx.try_recv() {
            if let Err(e) = ws.send(Message::Text(resp.into())) {
              eprintln!("[fink] send error: {e}");
            }
          }
        }
      }
      Some(Message::Close(_)) | None => break,
      _ => {}
    }
  }
}

/// Listen on `port`, handle HTTP /json probes, return the first WebSocket.
fn wait_for_ws(port: u16) -> Result<WebSocket<TcpStream>, String> {
  let listener =
    TcpListener::bind(("127.0.0.1", port)).map_err(|e| e.to_string())?;

  eprintln!("[fink] starting");
  eprintln!("[fink] waiting for debugger");
  eprintln!(
    "[fink]   {{\"type\":\"node\",\"request\":\"attach\",\
     \"websocketAddress\":\"ws://localhost:{port}\"}}"
  );

  loop {
    let (stream, _addr) = listener.accept().map_err(|e| e.to_string())?;
    match try_ws_or_json(stream, port) {
      Ok(Some(ws)) => return Ok(ws),
      Ok(None) => continue,
      Err(e) => return Err(e),
    }
  }
}

#[allow(clippy::result_large_err)]
fn try_ws_or_json(
  stream: TcpStream,
  port: u16,
) -> Result<Option<WebSocket<TcpStream>>, String> {
  use tungstenite::handshake::server::{Request, Response};
  use std::sync::atomic::{AtomicBool, Ordering};
  let is_json = AtomicBool::new(false);

  let result = tungstenite::accept_hdr(stream, |req: &Request, resp: Response| {
    let path = req.uri().path();
    if path.starts_with("/json") {
      is_json.store(true, Ordering::Relaxed);
      let target_id = "fink-debugger";
      let ws_url = format!("ws://localhost:{port}");
      let body = format!(
        "[{{\"id\":\"{target_id}\",\"title\":\"fink\",\"type\":\"node\",\
         \"webSocketDebuggerUrl\":\"{ws_url}\"}}]"
      );
      Err(
        tungstenite::http::Response::builder()
          .status(200)
          .header("Content-Type", "application/json")
          .header("Content-Length", body.len().to_string())
          .body(Some(body))
          .unwrap(),
      )
    } else {
      Ok(resp)
    }
  });

  match result {
    Ok(ws) => Ok(Some(ws)),
    Err(tungstenite::HandshakeError::Failure(_)) if is_json.load(Ordering::Relaxed) => Ok(None),
    Err(e) => Err(format!("WebSocket handshake failed: {e}")),
  }
}

fn extract_json_id(s: &str) -> Option<i64> {
  let key = "\"id\":";
  let start = s.find(key)? + key.len();
  let rest = s[start..].trim_start();
  let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
  rest[..end].parse().ok()
}

fn extract_json_str<'a>(s: &'a str, key: &str) -> Option<&'a str> {
  let needle = format!("\"{key}\":\"");
  let start = s.find(&needle)? + needle.len();
  let end = s[start..].find('"')? + start;
  Some(&s[start..end])
}

fn string_view_to_string(view: StringView) -> String {
  if view.is_8bit() {
    String::from_utf8_lossy(view.characters8().unwrap()).into_owned()
  } else {
    String::from_utf16_lossy(view.characters16().unwrap())
  }
}
