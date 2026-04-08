// Runner: executes compiled WASM in Wasmtime.

use std::sync::{Arc, Mutex};

pub mod wasmtime_runner;

/// Shared, thread-safe IO stream (stdout or stderr).
pub type IoStream = Arc<Mutex<dyn std::io::Write + Send>>;

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

/// Compile source and run it. Returns the exit code from main.
pub fn run_source(
  mut opts: RunOptions,
  src: &str,
  path: &str,
  stdout: IoStream,
  stderr: IoStream,
) -> Result<i64, String> {
  if opts.source_label == "fink" {
    opts.source_label = path.to_string();
  }
  let wasm = crate::to_wasm(src, path)?;
  wasmtime_runner::run(&opts, &wasm.binary, stdout, stderr)
}

/// Read a file and run it. Supports .fnk source and .wasm binaries.
/// Returns the exit code from main.
pub fn run_file(
  mut opts: RunOptions,
  path: &str,
  stdout: IoStream,
  stderr: IoStream,
) -> Result<i64, String> {
  if opts.source_label == "fink" {
    opts.source_label = path.to_string();
  }

  if path.ends_with(".fnk") {
    let src = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    return run_source(opts, &src, path, stdout, stderr);
  }

  let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
  if bytes.starts_with(b"\0asm") {
    wasmtime_runner::run(&opts, &bytes, stdout, stderr)
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

  // TODO: move run_main to a test helper module — it is not part of the runner.
  #[allow(unused)]
  fn run_main(src: &str) -> String {
    let wasm = crate::to_wasm(src, "test").expect("compilation failed");
    let stdout_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));

    match wasmtime_runner::run(
      &RunOptions::default(),
      &wasm.binary,
      stdout_buf.clone(),
      stderr_buf.clone(),
    ) {
      Ok(exit_code) => {
        let stdout_bytes = stdout_buf.lock().unwrap();
        let stdout = String::from_utf8_lossy(&stdout_bytes);
        let stderr_bytes = stderr_buf.lock().unwrap();
        let stderr = String::from_utf8_lossy(&stderr_bytes);

        let mut out = format!("{}", exit_code);

        let stdout_lines: Vec<&str> = stdout.split('\n').filter(|s| !s.is_empty()).collect();
        if !stdout_lines.is_empty() {
          out.push_str("\nstdout == \":");
          for line in &stdout_lines {
            out.push_str(&format!("\n  {}", line));
          }
        }

        let stderr_lines: Vec<&str> = stderr.split('\n').filter(|s| !s.is_empty()).collect();
        if !stderr_lines.is_empty() {
          out.push_str("\nstderr == \":");
          for line in &stderr_lines {
            out.push_str(&format!("\n  {}", line));
          }
        }

        out.trim_end().to_string()
      }
      Err(e) => format!("ERROR: {}", e),
    }
  }

  test_macros::include_fink_tests!("src/runner/test_io.fnk");
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
  test_macros::include_fink_tests!("src/runner/test_tasks.fnk");
}
