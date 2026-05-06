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
  let output = StdCommand::new("node")
    .args(["--test", test_js.to_str().unwrap()])
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
