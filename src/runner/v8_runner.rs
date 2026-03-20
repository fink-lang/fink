// V8-based WASM runner.
//
// Full CDP debugging support via embedded inspector (WebSocket on port 9229).
// Heavier than Wasmtime (~30MB) but provides rich debugging UX in VSCode.

use std::sync::OnceLock;

use super::v8_inspector as inspector;
use super::RunOptions;

/// V8 platform must be initialised exactly once per process.
static V8_INIT: OnceLock<()> = OnceLock::new();

fn init_v8() {
  V8_INIT.get_or_init(|| {
    let platform = v8::new_default_platform(0, false).make_shared();
    v8::V8::initialize_platform(platform);
    v8::V8::initialize();
  });
}

pub fn run_wat(opts: RunOptions, path: &str, wat_src: &str) -> Result<(), String> {
  // Register the WAT source before compiling so the debug session can serve it
  // via Debugger.getScriptSource when VSCode opens the source file.
  if opts.debug {
    let wat_url = format!("file://{path}");
    inspector::register_wasm_source(&wat_url, wat_src);
  }
  let compile_opts = crate::passes::wasm::compile::CompileOptions {
    debug: opts.debug,
    source_path: Some(path),
  };
  let wasm = crate::passes::wasm::compile::wat_to_wasm(wat_src, &compile_opts)?;
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

  // If debugging: register context and wait for the debugger to attach.
  // The JS runner contains a `debugger` statement just before WASM instantiation
  // so the first pause lands immediately before WASM runs (not in JS boilerplate).
  let _session = if let Some(insp) = insp_opt {
    let session = inspector::attach(insp, scope, ctx_copy, opts.inspect_port)?;
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
  // No //# sourceURL= comment: the JS runner scaffold is internal plumbing and
  // doesn't need a source view. The WASM scriptParsed url is patched separately
  // to point at the WAT source file.
  let script_url = "fink://runner".to_string();
  let brk = if opts.break_on_start { "debugger;" } else { "" };
  let js = format!(r#"
      const output = [];
      const imports = {{
        env: {{
          print: (n) => output.push(String(n)),
        }},
      }};
      const mod = new WebAssembly.Module(__wasm_bytes);
      {brk}
      const inst = new WebAssembly.Instance(mod, imports);
      if (inst.exports.fink_main) inst.exports.fink_main();
      const exportList = WebAssembly.Module.exports(mod)
        .map(e => `  ${{e.name}} (${{e.kind}})`)
        .join('\n');
      const printed = output.map(s => `[wasm] ${{s}}`).join('\n');
      [exportList, printed].filter(Boolean).join('\n')
    "#);

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
