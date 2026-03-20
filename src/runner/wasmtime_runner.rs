// Wasmtime-based WASM runner.
//
// Lighter alternative to V8: pure Rust, ~2MB, supports WasmGC.
// No debug inspector (yet) — use V8 backend for CDP debugging.

use wasmtime::*;

use super::RunOptions;

pub fn run(opts: &RunOptions, wasm: &[u8]) -> Result<(), String> {
  let mut config = Config::new();
  config.wasm_gc(true);
  if opts.debug {
    config.debug_info(true);
    config.cranelift_opt_level(OptLevel::None);
  }

  let engine = Engine::new(&config).map_err(|e| e.to_string())?;
  let module = Module::new(&engine, wasm).map_err(|e| e.to_string())?;
  let mut store = Store::new(&engine, PrintState::default());

  let mut linker = Linker::new(&engine);
  linker
    .func_wrap("env", "print", |mut caller: Caller<'_, PrintState>, val: i32| {
      caller.data_mut().output.push(val.to_string());
    })
    .map_err(|e| e.to_string())?;

  let instance = linker.instantiate(&mut store, &module).map_err(|e| e.to_string())?;

  // Call fink_main if exported (runtime-controlled entry point).
  if let Ok(main) = instance.get_typed_func::<(), ()>(&mut store, "fink_main") {
    main.call(&mut store, ()).map_err(|e| e.to_string())?;
  }

  let output = &store.data().output;
  if !output.is_empty() {
    for line in output {
      println!("[wasm] {line}");
    }
  }

  Ok(())
}

pub fn run_wat(opts: &RunOptions, wat_src: &str) -> Result<(), String> {
  // Wasmtime can load WAT directly via Module::new, but we pre-parse here
  // so we can embed DWARF when debugging (needed for LLDB source mapping).
  let wasm = if opts.debug {
    wat::Parser::new()
      .generate_dwarf(wat::GenerateDwarf::Full)
      .parse_str(None, wat_src)
      .map_err(|e| e.to_string())?
  } else {
    wat::parse_str(wat_src).map_err(|e| e.to_string())?
  };
  run(opts, &wasm)
}

#[derive(Default)]
struct PrintState {
  output: Vec<String>,
}
