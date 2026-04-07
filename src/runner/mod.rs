// Runner: executes compiled WASM in Wasmtime.

pub mod wasmtime_runner;

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

/// Compile source and run it.
pub fn run_source(mut opts: RunOptions, src: &str, path: &str) -> Result<(), String> {
  if opts.source_label == "fink" {
    opts.source_label = path.to_string();
  }
  let wasm = crate::to_wasm(src, path)?;
  wasmtime_runner::run(&opts, &wasm.binary)
}

/// Read a file and run it. Supports .fnk source and .wasm binaries.
pub fn run_file(mut opts: RunOptions, path: &str) -> Result<(), String> {
  if opts.source_label == "fink" {
    opts.source_label = path.to_string();
  }

  if path.ends_with(".fnk") {
    let src = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    return run_source(opts, &src, path);
  }

  let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
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
    let wasm = crate::to_wasm(src, "test").expect("compilation failed");
    match wasmtime_runner::exec(&RunOptions::default(), &wasm.binary) {
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

  test_macros::include_fink_tests!("src/runner/test_literals.fnk");
  test_macros::include_fink_tests!("src/runner/test_bindings.fnk");
  test_macros::include_fink_tests!("src/runner/test_operators.fnk");
  test_macros::include_fink_tests!("src/runner/test_functions.fnk");
  test_macros::include_fink_tests!("src/runner/test_strings.fnk");
  test_macros::include_fink_tests!("src/runner/test_records.fnk");
  test_macros::include_fink_tests!("src/runner/test_formatting.fnk");
  test_macros::include_fink_tests!("src/runner/test_patterns.fnk");
  test_macros::include_fink_tests!("src/runner/test_ranges.fnk");
  test_macros::include_fink_tests!("src/runner/test_errors.fnk");
  test_macros::include_fink_tests!("src/runner/test_fn_match.fnk");
}
