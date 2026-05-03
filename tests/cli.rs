//! Integration tests for the `fink` and `finkrt` CLI binaries.
//!
//! These tests exercise the actual built binary via `assert_cmd`, and
//! treat the CLI as a black box: arguments in, stdout/stderr/exit code
//! out. The `.fnk` fixtures the tests feed live next to the binary
//! source under `src/bin/fixtures/`.
//!
//! Tests for feature-gated subcommands (`wat`, `wasm`, `compile`,
//! `run`, `dap`) are themselves gated on the same features so
//! `cargo test --no-default-features` still passes.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::prelude::*;

fn fink() -> Command {
  Command::cargo_bin("fink").expect("fink binary")
}

fn fixture(name: &str) -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("src/bin/fixtures")
    .join(name)
}

fn fixture_str(name: &str) -> String {
  fixture(name).to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// Top-level: --version, no-args usage, missing file.
// ---------------------------------------------------------------------------

#[test]
fn version_flag_prints_version() {
  fink().arg("--version").assert().success()
    .stdout(predicate::str::starts_with("fink "));
}

#[test]
fn version_flag_short_circuits_other_args() {
  // `--version` is checked before any positional, so spurious args are ignored.
  fink().args(["--version", "garbage", "args"]).assert().success()
    .stdout(predicate::str::starts_with("fink "));
}

#[test]
fn no_args_prints_usage_to_stderr() {
  fink().assert().failure().code(1)
    .stderr(predicate::str::contains("usage: fink"));
}

#[test]
fn missing_file_errors_with_path() {
  fink().args(["ast", "does-not-exist.fnk"]).assert().failure().code(1)
    .stderr(predicate::str::contains("does-not-exist.fnk"));
}

// ---------------------------------------------------------------------------
// Read-only commands: tokens, ast, fmt, fmt2, cps, marks.
// ---------------------------------------------------------------------------

#[test]
fn tokens_emits_token_stream() {
  fink().args(["tokens", &fixture_str("hello.fnk")]).assert().success()
    .stdout(predicate::str::contains("BlockStart"))
    .stdout(predicate::str::contains("Ident 'stdout'"));
}

#[test]
fn ast_emits_module_tree() {
  fink().args(["ast", &fixture_str("hello.fnk")]).assert().success()
    .stdout(predicate::str::starts_with("Module"))
    .stdout(predicate::str::contains("LitStr 'hello'"));
}

#[test]
fn ast_desugar_runs_desugar_pass() {
  // hello.fnk has nothing to desugar visibly; assert it parses + prints
  // a Module without erroring.
  fink().args(["ast", "--desugar", &fixture_str("hello.fnk")]).assert().success()
    .stdout(predicate::str::starts_with("Module"));
}

// Usage advertises `ast [--source-map]` but the `ast` arm ignores the
// flag — only `fmt`, `fmt2`, `cps`, `marks`, and `wat` actually emit a
// source-map line. Documented as a wiring gap, not fixed here.
#[test]
#[ignore = "BUG: `ast --source-map` is silently ignored; usage advertises it"]
fn ast_source_map_appends_sm_line() {
  fink().args(["ast", "--source-map", &fixture_str("hello.fnk")]).assert().success()
    .stdout(predicate::str::contains("# sm:"));
}

#[test]
fn ast_on_parse_error_exits_nonzero() {
  fink().args(["ast", &fixture_str("parse_error.fnk")]).assert().failure().code(1)
    .stderr(predicate::str::contains("error: "));
}

#[test]
fn fmt_round_trips_source() {
  fink().args(["fmt", &fixture_str("hello.fnk")]).assert().success()
    .stdout(predicate::str::contains("main = fn"))
    .stdout(predicate::str::contains("'hello'"));
}

#[test]
fn fmt_source_map_appends_sm_line() {
  fink().args(["fmt", "--source-map", &fixture_str("hello.fnk")]).assert().success()
    .stdout(predicate::str::contains("# sm:"));
}

#[test]
fn fmt2_round_trips_source() {
  fink().args(["fmt2", &fixture_str("hello.fnk")]).assert().success()
    .stdout(predicate::str::contains("main = fn"))
    .stdout(predicate::str::contains("'hello'"));
}

#[test]
fn fmt2_source_map_appends_sm_line() {
  fink().args(["fmt2", "--source-map", &fixture_str("hello.fnk")]).assert().success()
    .stdout(predicate::str::contains("# sm:"));
}

#[test]
fn cps_emits_ir() {
  fink().args(["cps", &fixture_str("hello.fnk")]).assert().success()
    .stdout(predicate::str::contains("ƒink_module"));
}

#[test]
fn cps_lifted_runs_lifting() {
  fink().args(["cps", "--lifted", &fixture_str("hello.fnk")]).assert().success()
    .stdout(predicate::str::contains("ƒink_module"));
}

#[test]
fn cps_lifted_plain_renders_plain_form() {
  fink().args(["cps", "--lifted=plain", &fixture_str("hello.fnk")]).assert().success()
    .stdout(predicate::str::contains("ƒink_module"));
}

#[test]
fn cps_source_map_appends_sm_line() {
  fink().args(["cps", "--source-map", &fixture_str("hello.fnk")]).assert().success()
    .stdout(predicate::str::contains("# sm:"));
}

#[test]
fn marks_emits_sm_line() {
  fink().args(["marks", &fixture_str("hello.fnk")]).assert().success()
    .stdout(predicate::str::contains("# sm:"));
}

// ---------------------------------------------------------------------------
// Stdin (-): read source from stdin instead of a file.
// ---------------------------------------------------------------------------

#[test]
fn ast_reads_stdin_when_path_is_dash() {
  let src = std::fs::read_to_string(fixture("hello.fnk")).unwrap();
  fink().args(["ast", "-"]).write_stdin(src).assert().success()
    .stdout(predicate::str::starts_with("Module"));
}

#[test]
fn empty_stdin_does_not_panic() {
  // Empty stdin must reach the parser cleanly — no IO panic, no abort.
  // Whatever exit code the parser settles on is fine; we only care that
  // the CLI's IO layer survives.
  let output = fink().args(["ast", "-"]).write_stdin("").output().unwrap();
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    !stderr.contains("panicked"),
    "fink panicked on empty stdin; stderr={stderr}"
  );
}

// ---------------------------------------------------------------------------
// decode-sm: decodes a `# sm:` / `;; sm:` blob from input.
// ---------------------------------------------------------------------------

#[test]
fn decode_sm_decodes_blob_from_stdin() {
  // Pipe `fmt --source-map` through `decode-sm` and assert it produces
  // mapping rows. Drives the CLI via two invocations.
  let fmt_out = fink().args(["fmt", "--source-map", &fixture_str("hello.fnk")])
    .assert().success().get_output().stdout.clone();
  let fmt_str = String::from_utf8(fmt_out).unwrap();

  fink().args(["decode-sm", "-"]).write_stdin(fmt_str).assert().success()
    .stdout(predicate::str::contains("out@"));
}

#[test]
fn decode_sm_with_source_arg_shows_source_slices() {
  let fmt_out = fink().args(["fmt", "--source-map", &fixture_str("hello.fnk")])
    .assert().success().get_output().stdout.clone();
  let fmt_str = String::from_utf8(fmt_out).unwrap();

  let source_arg = format!("--source={}", fixture_str("hello.fnk"));
  fink().args(["decode-sm", "-", &source_arg]).write_stdin(fmt_str)
    .assert().success()
    .stdout(predicate::str::contains("src["));
}

#[test]
fn decode_sm_errors_when_no_sm_line() {
  fink().args(["decode-sm", "-"]).write_stdin("no sm line here\n")
    .assert().failure().code(1)
    .stderr(predicate::str::contains("no '# sm:'"));
}

// ---------------------------------------------------------------------------
// `compile`-feature commands: wat, wasm, compile.
// ---------------------------------------------------------------------------

#[cfg(feature = "compile")]
mod compile_feature {
  use super::*;

  #[test]
  fn wat_emits_module_text() {
    fink().args(["wat", &fixture_str("hello.fnk")]).assert().success()
      .stdout(predicate::str::starts_with("(module"));
  }

  #[test]
  fn wat_source_map_appends_sm_line() {
    fink().args(["wat", "--source-map", &fixture_str("hello.fnk")]).assert().success()
      .stdout(predicate::str::contains(";; sm:"));
  }

  #[test]
  fn wasm_emits_wasm_binary_with_magic() {
    let out = fink().args(["wasm", &fixture_str("hello.fnk")])
      .assert().success().get_output().stdout.clone();
    assert!(out.len() > 8, "wasm output too small: {} bytes", out.len());
    assert_eq!(&out[0..4], b"\0asm", "missing wasm magic");
    assert_eq!(&out[4..8], &[0x01, 0x00, 0x00, 0x00], "missing wasm version");
  }

  #[test]
  fn wasm_unknown_optimize_level_errors() {
    fink().args(["wasm", "--optimize=bad", &fixture_str("hello.fnk")])
      .assert().failure().code(1)
      .stderr(predicate::str::contains("unknown optimization level: bad"));
  }

  // The runtime emits multivalue results and saturating-truncation
  // instructions; wasm-opt's default validator rejects both. `wasm -O`
  // (and the explicit `-O1..-Oz` levels) currently fail end-to-end on
  // the hello fixture. Tracked as a real bug to surface, not fix here.
  #[test]
  #[ignore = "BUG: wasm-opt rejects runtime's multivalue/sat-conv usage; see session note"]
  fn wasm_optimize_default_succeeds() {
    let out = fink().args(["wasm", "-O", &fixture_str("hello.fnk")])
      .assert().success().get_output().stdout.clone();
    assert_eq!(&out[0..4], b"\0asm");
  }

  #[test]
  fn compile_writes_wasm_to_o_output() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("hello.wasm");
    fink().args([
      "compile",
      &fixture_str("hello.fnk"),
      "-o", out.to_str().unwrap(),
    ]).assert().success()
      .stderr(predicate::str::contains("wrote "))
      .stderr(predicate::str::contains("target: wasm"));

    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(&bytes[0..4], b"\0asm");
  }

  #[test]
  fn compile_default_output_uses_stem_dot_wasm() {
    let dir = tempfile::tempdir().unwrap();
    // Copy the fixture into the tempdir so `default_output` writes there
    // and the test cleans up after itself.
    let src = dir.path().join("hello.fnk");
    std::fs::copy(fixture("hello.fnk"), &src).unwrap();

    fink().current_dir(dir.path())
      .args(["compile", "hello.fnk"])
      .assert().success();

    let out = dir.path().join("hello.wasm");
    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(&bytes[0..4], b"\0asm");
  }

  #[test]
  fn compile_unknown_native_target_errors() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("x");
    fink().args([
      "compile", "--target=bogus-triple",
      &fixture_str("hello.fnk"),
      "-o", out.to_str().unwrap(),
    ]).assert().failure().code(1)
      .stderr(predicate::str::contains("finkrt not found for target"));
  }
}

// ---------------------------------------------------------------------------
// `run`-feature commands: run (explicit + implicit), dap.
// ---------------------------------------------------------------------------

#[cfg(feature = "run")]
mod run_feature {
  use super::*;

  #[test]
  fn run_executes_and_writes_to_stdout() {
    fink().args(["run", &fixture_str("hello.fnk")]).assert().success()
      .stdout(predicate::str::contains("hello"));
  }

  #[test]
  fn implicit_run_executes_when_first_arg_is_path() {
    fink().arg(fixture_str("hello.fnk")).assert().success()
      .stdout(predicate::str::contains("hello"));
  }

  #[test]
  fn run_propagates_main_exit_code() {
    fink().args(["run", &fixture_str("exit_code.fnk")]).assert().code(42);
  }

  #[test]
  fn implicit_run_propagates_main_exit_code() {
    fink().arg(fixture_str("exit_code.fnk")).assert().code(42);
  }

  #[test]
  fn run_forwards_argv_to_main() {
    // echo_args.fnk returns 7 iff its trailing argv is exactly `one two`.
    fink().args(["run", &fixture_str("echo_args.fnk"), "one", "two"])
      .assert().code(7);
  }

  #[test]
  fn implicit_run_forwards_argv_to_main() {
    fink().args([&fixture_str("echo_args.fnk"), "one", "two"])
      .assert().code(7);
  }

  #[test]
  fn run_reads_stdin_via_dash() {
    let src = std::fs::read_to_string(fixture("exit_code.fnk")).unwrap();
    fink().args(["run", "-"]).write_stdin(src).assert().code(42);
  }

  #[test]
  fn run_reads_user_stdin_through_program() {
    // echo_stdin.fnk reads <=64 bytes from stdin and writes them to stdout.
    fink().args(["run", &fixture_str("echo_stdin.fnk")])
      .write_stdin("hi from stdin")
      .assert().success()
      .stdout(predicate::str::contains("hi from stdin"));
  }

  #[test]
  fn dap_missing_file_errors() {
    fink().args(["dap", "does-not-exist.fnk"]).assert().failure().code(1)
      .stderr(predicate::str::contains("does-not-exist.fnk"));
  }

  // Full DAP smoke is left for the dap-specific test suite — running
  // the server against a real fixture would block on stdin waiting for
  // DAP messages. Missing-file is the cheapest CLI-wiring check.
}

// ---------------------------------------------------------------------------
// finkrt: standalone runtime binary. Without an embedded payload it
// prints "no embedded module" and exits 1. With a payload appended by
// `fink compile --target=native`, it executes the embedded module and
// propagates the exit code.
// ---------------------------------------------------------------------------

#[cfg(feature = "runtime")]
fn finkrt() -> Command {
  Command::cargo_bin("finkrt").expect("finkrt binary")
}

#[cfg(feature = "runtime")]
#[test]
fn finkrt_without_payload_errors() {
  finkrt().assert().failure().code(1)
    .stderr(predicate::str::contains("no embedded module"));
}

#[cfg(feature = "run")]
#[test]
fn compile_native_produces_runnable_binary() {
  let dir = tempfile::tempdir().unwrap();
  let out = dir.path().join("hello-bin");
  fink().args([
    "compile", "--target=native",
    &fixture_str("hello.fnk"),
    "-o", out.to_str().unwrap(),
  ]).assert().success();

  // Sanity: the produced file should carry the `f1nkw4sm` trailer.
  let bytes = std::fs::read(&out).unwrap();
  assert!(bytes.len() > 16);
  assert_eq!(&bytes[bytes.len() - 8..], b"f1nkw4sm");

  // And it should actually execute and write "hello" to stdout.
  let output = StdCommand::new(&out).output().expect("run native binary");
  assert!(output.status.success(), "binary exited with {:?}", output.status);
  assert!(
    String::from_utf8_lossy(&output.stdout).contains("hello"),
    "stdout was: {:?}", String::from_utf8_lossy(&output.stdout),
  );
}
