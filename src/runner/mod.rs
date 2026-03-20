// Runner: compiles WAT or loads WASM, runs it in an embedded runtime.
//
// Two backends:
//   - V8:       full CDP debugging (--dbg), heavier (~30MB)
//   - Wasmtime: lightweight (~2MB), WasmGC support, no debug inspector yet
//
// Selected via `--runtime=v8|wasmtime` (default: wasmtime).

pub mod inspector;
pub mod v8_runner;
pub mod wasmtime_runner;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runtime {
  V8,
  Wasmtime,
}

pub struct RunOptions {
  pub runtime: Runtime,
  pub debug: bool,
  /// Pause before WASM runs (--dbg=brk). When false, only user breakpoints stop execution.
  pub break_on_start: bool,
  pub inspect_port: u16,
  /// Source label shown in the debugger (e.g. the input file path).
  pub source_label: String,
}

impl Default for RunOptions {
  fn default() -> Self {
    Self { runtime: Runtime::Wasmtime, debug: false, break_on_start: false, inspect_port: 9229, source_label: "fink".into() }
  }
}

pub fn run_file(mut opts: RunOptions, path: &str) -> Result<(), String> {
  if opts.source_label == "fink" {
    opts.source_label = path.to_string();
  }
  // CDP inspector (break_on_start, WebSocket attach) requires V8.
  // Wasmtime supports LLDB-based debugging via DWARF — no auto-switch needed.
  if opts.break_on_start && opts.runtime != Runtime::V8 {
    eprintln!("[fink] --dbg=brk requires V8 runtime, switching to --runtime=v8");
    opts.runtime = Runtime::V8;
  }
  let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
  // WASM binaries start with magic bytes \0asm; everything else is WAT text.
  if bytes.starts_with(b"\0asm") {
    match opts.runtime {
      Runtime::Wasmtime => wasmtime_runner::run(&opts, &bytes),
      Runtime::V8 => v8_runner::run(opts, &bytes),
    }
  } else {
    let src = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
    match opts.runtime {
      Runtime::Wasmtime => wasmtime_runner::run_wat(&opts, Some(path), src),
      Runtime::V8 => v8_runner::run_wat(opts, path, src),
    }
  }
}
