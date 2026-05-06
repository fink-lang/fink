//! Integration test for the Fink JS interop layer.
//!
//! Compiles `test_interop.fnk` via the `fink` CLI, then drives
//! `node --test test_interop.js` against the produced wasm. The Node
//! test asserts the wasm instantiates and (over time) exercises the JS
//! interop surface in `src/runtime/interop/js/interop.wat` + `fink.js`.
//!
//! Wired as a `[[test]]` target in `Cargo.toml` so it runs under
//! `cargo test`. Skipped if `node` is not on `PATH` — see
//! `feedback_ci_external_binary_guard.md`.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;

fn fink() -> Command {
  Command::cargo_bin("fink").expect("fink binary")
}

fn interop_dir() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("src/runtime/interop/js")
}

fn node_available() -> bool {
  StdCommand::new("node").arg("--version").output()
    .map(|o| o.status.success()).unwrap_or(false)
}

/// `node --version` prints e.g. `v20.11.0`. Node < 22 needs
/// `--experimental-wasm-gc` to enable WasmGC; Node 22+ has it stable
/// and removed the flag.
fn needs_wasm_gc_flag() -> bool {
  let out = StdCommand::new("node").arg("--version").output();
  let Ok(o) = out else { return false };
  let s = String::from_utf8_lossy(&o.stdout);
  let trimmed = s.trim().trim_start_matches('v');
  let major: u32 = trimmed.split('.').next()
    .and_then(|m| m.parse().ok())
    .unwrap_or(0);
  major < 22
}

#[test]
#[cfg(feature = "compile")]
fn js_interop_round_trip() {
  if !node_available() {
    eprintln!("skipping: `node` not on PATH");
    return;
  }

  let dir = tempfile::tempdir().unwrap();
  let wasm = dir.path().join("test_interop.wasm");

  let fnk = interop_dir().join("test_interop.fnk");
  fink().args([
    "compile",
    "--target=wasm+js",
    fnk.to_str().unwrap(),
    "-o", wasm.to_str().unwrap(),
  ]).assert().success();

  let test_js = interop_dir().join("test_interop.js");
  // Older Node (< 22) needs `--experimental-wasm-gc`. Node 22+ has
  // WasmGC stable and removed the flag entirely (passing it errors).
  // Detect via `node --version` and conditionally include.
  let mut args: Vec<String> = Vec::new();
  if needs_wasm_gc_flag() {
    args.push("--experimental-wasm-gc".to_string());
  }
  args.push("--test".to_string());
  args.push(test_js.to_str().unwrap().to_string());

  let output = StdCommand::new("node")
    .args(&args)
    .env("FINK_TEST_WASM", &wasm)
    .output()
    .expect("failed to invoke node");

  if !output.status.success() {
    panic!(
      "node --test failed (exit {:?})\n--- stdout ---\n{}\n--- stderr ---\n{}",
      output.status.code(),
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr),
    );
  }
}
