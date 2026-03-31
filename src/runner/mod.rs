// Runner: compiles Fink source or loads WAT/WASM, runs it in Wasmtime.

pub mod wasmtime_runner;

pub use crate::compiler::{CompileResult, compile_fnk};

pub struct RunOptions {
  pub debug: bool,
  /// Source label shown in the debugger (e.g. the input file path).
  pub source_label: String,
}

impl Default for RunOptions {
  fn default() -> Self {
    Self { debug: false, source_label: "fink".into() }
  }
}

pub fn run_file(mut opts: RunOptions, path: &str) -> Result<(), String> {
  if opts.source_label == "fink" {
    opts.source_label = path.to_string();
  }

  // .fnk files: compile through the full pipeline, then run.
  if path.ends_with(".fnk") {
    let src = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let result = compile_fnk(&src)?;
    return wasmtime_runner::run(&opts, &result.wasm);
  }

  let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
  // WASM binaries start with magic bytes \0asm.
  if bytes.starts_with(b"\0asm") {
    wasmtime_runner::run(&opts, &bytes)
  } else {
    Err("only .fnk and .wasm files are supported".into())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use wasmtime_runner::FinkResult;

  #[allow(unused)]
  fn run(src: &str) -> String {
    let result = compile_fnk(src).expect("compilation failed");
    match wasmtime_runner::exec(&RunOptions::default(), &result.wasm) {
      Ok(FinkResult::Num(v)) => {
        if v == v.floor() && v.abs() < 1e15 {
          format!("{}", v as i64)
        } else {
          format!("{}", v)
        }
      }
      Ok(FinkResult::Bool(b)) => format!("{}", b),
      Ok(FinkResult::Str(s)) => s,
      Ok(FinkResult::None) => String::new(),
      Err(e) => format!("ERROR: {}", e),
    }
  }

  test_macros::include_fink_tests!("src/runner/test_runner.fnk");
}
