// Runner: compiles WAT or loads WASM, runs it in an embedded V8 isolate.
//
// WASM bytes are exposed as `__wasm_bytes` (ArrayBuffer) on the JS global.
// Instantiation and export enumeration happens via normal JS WebAssembly API
// because the v8 Rust bindings don't expose WasmInstanceObject::new directly.
//
// Debug mode (`--dbg`): starts a CDP inspector WebSocket on `inspect_port`
// (default 9229) — VSCode / Chrome DevTools can attach using:
//   {"type":"node","request":"attach","websocketAddress":"ws://localhost:9229"}

pub mod inspector;
use std::sync::OnceLock;

pub struct RunOptions {
  pub debug: bool,
  pub inspect_port: u16,
  /// Source label shown in the debugger (e.g. the input file path).
  pub source_label: String,
}

impl Default for RunOptions {
  fn default() -> Self {
    Self { debug: false, inspect_port: 9229, source_label: "fink".into() }
  }
}

/// V8 platform must be initialised exactly once per process.
static V8_INIT: OnceLock<()> = OnceLock::new();

fn init_v8() {
  V8_INIT.get_or_init(|| {
    let platform = v8::new_default_platform(0, false).make_shared();
    v8::V8::initialize_platform(platform);
    v8::V8::initialize();
  });
}

pub fn run_file(mut opts: RunOptions, path: &str) -> Result<(), String> {
  if opts.source_label == "fink" {
    opts.source_label = path.to_string();
  }
  let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
  if path.ends_with(".wat") {
    let src = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
    run_wat(opts, path, src)
  } else {
    run(opts, &bytes)
  }
}

pub fn run_wat(opts: RunOptions, path: &str, wat_src: &str) -> Result<(), String> {
  // In debug mode, embed DWARF so V8 can map WASM bytecode offsets → WAT source lines.
  let wasm = if opts.debug {
    wat::Parser::new()
      .generate_dwarf(wat::GenerateDwarf::Full)
      .parse_str(Some(std::path::Path::new(path)), wat_src)
      .map_err(|e| e.to_string())?
  } else {
    wat::parse_str(wat_src).map_err(|e| e.to_string())?
  };
  run(opts, &wasm)
}

pub fn run(opts: RunOptions, wasm: &[u8]) -> Result<(), String> {
  init_v8();

  let isolate = &mut v8::Isolate::new(Default::default());

  // Inspector must be created before any HandleScope borrows the isolate.
  let insp_opt = if opts.debug {
    Some(inspector::create_inspector(isolate))
  } else {
    None
  };

  v8::scope!(let scope, isolate);
  let context = v8::Context::new(scope, Default::default());
  let ctx_copy = context; // Local<Context> is Copy
  let scope = &mut v8::ContextScope::new(scope, context);

  // If debugging: register context, wait for the debugger, then pause on the
  // first JS statement (--inspect-brk style) so the user can set breakpoints
  // or step before any code runs.
  let _session = if let Some(insp) = insp_opt {
    let session = inspector::attach(insp, scope, ctx_copy, opts.inspect_port)?;
    session.schedule_pause();
    Some(session)
  } else {
    None
  };

  // Expose WASM bytes as `__wasm_bytes` (ArrayBuffer) on the JS global.
  let bs = v8::ArrayBuffer::new_backing_store_from_vec(wasm.to_vec());
  let buf = v8::ArrayBuffer::with_backing_store(scope, &bs.make_shared());
  let key = v8::String::new(scope, "__wasm_bytes").unwrap();
  context.global(scope).set(scope, key.into(), buf.into());

  // Instantiate the module via the JS WebAssembly API.
  // Provides env.print(i32) as a host import so WASM modules can call back.
  //
  // `//# sourceURL=` at the end sets callFrame.url so VSCode can show line
  // highlighting in the correct file.  ScriptOrigin::resource_name only sets
  // embedderName; the actual URL used by the debugger comes from this comment.
  let script_url = format!("file://{}", opts.source_label);
  let js = format!(r#"
      const output = [];
      const imports = {{
        env: {{
          print: (n) => output.push(String(n)),
        }},
      }};
      const mod = new WebAssembly.Module(__wasm_bytes);
      const inst = new WebAssembly.Instance(mod, imports);
      const exportList = WebAssembly.Module.exports(mod)
        .map(e => `  ${{e.name}} (${{e.kind}})`)
        .join('\n');
      const printed = output.map(s => `[wasm] ${{s}}`).join('\n');
      [exportList, printed].filter(Boolean).join('\n')
    //# sourceURL={script_url}
    "#);

  // Register the JS source so Debugger.getScriptSource can return it while paused.
  if _session.is_some() {
    inspector::register_script_source(&script_url, &js);
  }

  let exports = run_js(scope, &js, &script_url)?;
  if !exports.trim().is_empty() {
    println!("module exports:\n{exports}");
  }

  Ok(())
}

/// Evaluate a JS snippet and return the result as a String.
/// `url` is set as the ScriptOrigin resource name so the debugger can populate
/// callFrame.url and correlate frames back to the source file.
///
/// Compile and run use separate TryCatch scopes. The compile phase produces a
/// Global<UnboundScript> that outlives the compile TryCatch. The run phase then
/// binds and runs it with its own TryCatch as the innermost scope.
///
/// This matters for debugging: V8 calls run_message_loop_on_pause from within
/// script.run(). Inspector messages like Runtime.evaluate try to execute JS,
/// and V8 crashes if an outer TryCatch is active at that point. By closing the
/// compile TryCatch before calling run(), the run TryCatch is the innermost
/// scope and V8's re-entrant JS execution works correctly.
fn run_js(scope: &mut v8::PinScope, code: &str, url: &str) -> Result<String, String> {
  // ── compile phase ────────────────────────────────────────────────────────
  // Compile to UnboundScript so it can be stored as a Global<UnboundScript>
  // that outlives the compile TryCatch scope.
  let unbound_global = {
    v8::tc_scope!(let tc, scope);
    let source_str = v8::String::new(tc, code).ok_or("failed to create source string")?;
    let url_str = v8::String::new(tc, url)
      .ok_or("failed to create url string")?
      .into();
    let origin = v8::ScriptOrigin::new(
      tc,
      url_str,
      0,
      0,
      false,
      -1,
      None,
      false,
      false,
      false,
      None,
    );
    let mut src = v8::script_compiler::Source::new(source_str, Some(&origin));
    match v8::script_compiler::compile_unbound_script(
      tc,
      &mut src,
      v8::script_compiler::CompileOptions::NoCompileOptions,
      v8::script_compiler::NoCacheReason::NoReason,
    ) {
      Some(unbound) => Ok(v8::Global::new(tc, unbound)),
      None => {
        let msg = tc
          .exception()
          .map_or_else(|| "compile error".into(), |e| e.to_rust_string_lossy(tc));
        Err(msg)
      }
    }
  }?;  // compile TryCatch dropped here; Global<UnboundScript> survives

  // ── run phase ────────────────────────────────────────────────────────────
  // Fresh TryCatch as the innermost scope; V8 may call run_message_loop_on_pause
  // from within script.run() — inspector's Runtime.evaluate re-enters JS and
  // requires no outer TryCatch to be active at that point.
  v8::tc_scope!(let tc, scope);
  let unbound = v8::Local::new(tc, &unbound_global);
  let script = unbound.bind_to_current_context(tc);
  match script.run(tc) {
    Some(result) => Ok(result.to_rust_string_lossy(tc)),
    None => {
      let msg = tc
        .exception()
        .map_or_else(|| "runtime error".into(), |e| e.to_rust_string_lossy(tc));
      Err(msg)
    }
  }
}
