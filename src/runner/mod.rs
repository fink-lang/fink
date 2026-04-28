//! Runner — executes compiled WASM binaries under Wasmtime.
//!
//! Wires the user program's IO channels (stdin/stdout/stderr) to host
//! streams, sets up the scheduler, and returns the exit code from `main`.

use std::sync::{Arc, Mutex};

pub mod wasmtime_runner;

/// Shared, thread-safe write stream (stdout or stderr).
pub type IoStream = Arc<Mutex<dyn std::io::Write + Send>>;

/// Shared, thread-safe read stream (stdin).
pub type IoReadStream = Arc<Mutex<dyn std::io::Read + Send>>;

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
///
/// `args` is the CLI argv passed to `main` — `argv[0]` is the program name.
#[cfg(feature = "compile")]
pub fn run_source(
  mut opts: RunOptions,
  src: &str,
  path: &str,
  args: Vec<Vec<u8>>,
  stdin: IoReadStream,
  stdout: IoStream,
  stderr: IoStream,
) -> Result<i64, String> {
  if opts.source_label == "fink" {
    opts.source_label = path.to_string();
  }
  let wasm = crate::to_wasm(src, path)?;
  wasmtime_runner::run(&opts, &wasm.binary, args, stdin, stdout, stderr)
}

/// Read a file and run it. Supports .fnk source and .wasm binaries.
/// Returns the exit code from main.
///
/// For `.fnk` entries, constructs a `FileSourceLoader` and calls
/// `compile_package` — this is the multi-module path, used by both
/// `fink run` and any other filesystem-backed invocation.
///
/// `args` is the CLI argv passed to `main` — `argv[0]` is the program name.
#[cfg(feature = "compile")]
pub fn run_file(
  mut opts: RunOptions,
  path: &str,
  args: Vec<Vec<u8>>,
  stdin: IoReadStream,
  stdout: IoStream,
  stderr: IoStream,
) -> Result<i64, String> {
  if opts.source_label == "fink" {
    opts.source_label = path.to_string();
  }

  if path.ends_with(".fnk") {
    let mut loader = crate::passes::modules::FileSourceLoader::new();
    let wasm = crate::compile_package(std::path::Path::new(path), &mut loader)?;
    return wasmtime_runner::run(&opts, &wasm.binary, args, stdin, stdout, stderr);
  }

  let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
  if bytes.starts_with(b"\0asm") {
    wasmtime_runner::run(&opts, &bytes, args, stdin, stdout, stderr)
  } else {
    Err("only .fnk and .wasm files are supported".into())
  }
}

#[cfg(test)]
mod tests {
  use std::sync::{Arc, Mutex};
  use wasmtime::{
    ArrayRef, ArrayRefPre, ArrayType, Config, Engine, Error, ExternType,
    FieldType, Linker, Module, Mutability, Store, StorageType, Val,
  };

  /// Hardcoded cli args passed to `main` when the source defines it.
  /// Tests that exercise cli arg behaviour (e.g. pattern-matching on
  /// `..args`) must match this fixture exactly.
  const TEST_CLI_ARGS: &[&[u8]] = &[b"test", b"alpha", b"beta"];

  /// Hardcoded stdin contents delivered to the program when it does
  /// `read stdin, N`. Matches the convention of the existing
  /// `run_main` helper for parity.
  const TEST_STDIN: &[u8] = b"hello from stdin";

  /// Test-side capture of host-channel writes during a run. Keyed by
  /// channel tag (0=stdin, 1=stdout, 2=stderr). Each write appends a
  /// chunk of bytes to the corresponding stream.
  ///
  /// Set up before instantiation, drained when formatting results.
  #[derive(Default)]
  struct IoCapture {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Cursor into TEST_STDIN — advances as `host_read` is called.
    stdin_cursor: usize,
  }

  impl IoCapture {
    fn append(&mut self, tag: i32, bytes: &[u8]) {
      match tag {
        1 => self.stdout.extend_from_slice(bytes),
        2 => self.stderr.extend_from_slice(bytes),
        _ => {} // Unknown tags ignored for now.
      }
    }

    /// Pop up to `size` bytes off the stdin buffer, advancing the
    /// cursor. Returns whatever's available (possibly empty).
    fn read_stdin(&mut self, size: usize) -> &'static [u8] {
      let start = self.stdin_cursor;
      let end = (start + size).min(TEST_STDIN.len());
      self.stdin_cursor = end;
      &TEST_STDIN[start..end]
    }
  }

  enum TestResult {
    Num(f64),
    Bool(bool),
    Str(Vec<u8>),
    None,
  }

  /// Run a bare-expression Fink source through the IR pipeline and
  /// return the value the done continuation receives, stringified.
  ///
  /// If the program performed any IO writes (to stdout/stderr via
  /// `>>` against `import 'std/io.fnk'` channels), the result is
  /// rendered as a multi-line block:
  ///
  /// ```text
  /// <exit value>
  /// stdout == ":
  ///   <line>
  ///   <line>
  /// stderr == ":
  ///   <line>
  /// ```
  ///
  /// Pure-value programs render as a single line (the value's textual
  /// representation), matching the existing test conventions.
  #[allow(unused)]
  fn run(src: &str) -> String {
    let io_capture: Arc<Mutex<IoCapture>> = Arc::new(Mutex::new(IoCapture::default()));
    let result = exec_ir_module(src, io_capture.clone());

    // Format the headline value. Unit-shaped results (the channel-send
    // success is rendered as Bool(false) by the runtime; module bodies
    // that end with a side-effect propagate that) are suppressed when
    // IO blocks follow — the test fixtures expect just the IO content
    // when there's no meaningful return value.
    let cap_has_io = {
      let cap = io_capture.lock().unwrap();
      !cap.stdout.is_empty() || !cap.stderr.is_empty()
    };
    let headline = match result {
      Ok(TestResult::Num(v)) => {
        if v == v.floor() && v.abs() < 1e15 { format!("{}", v as i64) }
        else { format!("{}", v) }
      }
      // Bool(false) when followed by IO is treated as "unit / void" —
      // the channel-send completion fires the cont with what the
      // runtime considers a unit value. Tests with explicit boolean
      // results never have IO blocks following, so this is safe.
      Ok(TestResult::Bool(b)) => {
        if cap_has_io && !b { String::new() } else { format!("{}", b) }
      }
      Ok(TestResult::Str(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
      Ok(TestResult::None) => String::new(),
      Err(e) => format!("ERROR: {}", e),
    };

    // If IO occurred, emit the multi-stream block format.
    let cap = io_capture.lock().unwrap();
    if cap.stdout.is_empty() && cap.stderr.is_empty() {
      return headline;
    }

    let mut out = headline;
    if !cap.stdout.is_empty() {
      if !out.is_empty() { out.push('\n'); }
      out.push_str("stdout == \":");
      let s = String::from_utf8_lossy(&cap.stdout);
      for line in s.split('\n').filter(|l| !l.is_empty()) {
        out.push_str(&format!("\n  {line}"));
      }
    }
    if !cap.stderr.is_empty() {
      out.push('\n');
      out.push_str("stderr == \":");
      let s = String::from_utf8_lossy(&cap.stderr);
      for line in s.split('\n').filter(|l| !l.is_empty()) {
        out.push_str(&format!("\n  {line}"));
      }
    }
    out
  }

  /// Hybrid loader for runner tests — entry source is registered at a
  /// synthetic disk path inside `src/runner/`, dep imports resolve
  /// against the real fixture tree (`src/runner/test_modules/...`).
  struct PkgRunnerLoader {
    entry_abs_path: std::path::PathBuf,
    entry_source: String,
    disk: crate::passes::modules::FileSourceLoader,
  }

  impl crate::passes::modules::SourceLoader for PkgRunnerLoader {
    fn load(&mut self, path: &std::path::Path) -> Result<String, String> {
      if path == self.entry_abs_path {
        Ok(self.entry_source.clone())
      } else {
        crate::passes::modules::SourceLoader::load(&mut self.disk, path)
      }
    }
  }

  fn exec_ir_module(src: &str, io_capture: Arc<Mutex<IoCapture>>) -> Result<TestResult, String> {
    // Anchor the entry at `src/runner/test.fnk` so relative imports
    // like `./test_modules/entry.fnk` reach the real fixture tree on
    // disk.
    let entry_abs_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("src/runner/test.fnk");
    let mut loader = PkgRunnerLoader {
      entry_abs_path: entry_abs_path.clone(),
      entry_source: src.to_string(),
      disk: crate::passes::modules::FileSourceLoader::new(),
    };
    let pkg = crate::passes::wasm::compile_package::compile_package(
      &entry_abs_path, &mut loader,
    ).map_err(|e| format!("compile_package: {e}"))?;
    let bytes = crate::passes::wasm::emit::emit(&pkg.fragment);

    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_function_references(true);
    config.wasm_tail_call(true);
    let engine = Engine::new(&config).map_err(|e| e.to_string())?;
    let module = Module::new(&engine, &bytes).map_err(|e| format!("{e:#}"))?;
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);

    let captured: Arc<Mutex<Option<TestResult>>> = Arc::new(Mutex::new(None));

    for imp in module.imports() {
      if imp.module() == "env"
        && let ExternType::Func(ft) = imp.ty()
      {
        let name = imp.name().to_string();
        match name.as_str() {
          "host_invoke_cont" => {
            let captured_clone = captured.clone();
            linker.func_new("env", &name, ft, move |mut caller, params, _results| {
              let args_any = params[1].unwrap_anyref()
                .ok_or_else(|| Error::msg("host_invoke_cont: null args"))?;
              let cons = args_any.unwrap_struct(&caller)?;
              let head = match cons.field(&mut caller, 0) {
                Ok(h) => h,
                Err(_) => {
                  *captured_clone.lock().unwrap() = Some(TestResult::None);
                  return Ok(());
                }
              };
              let head_any = match head {
                Val::AnyRef(Some(r)) => r,
                _ => return Ok(()),
              };
              if let Ok(Some(i31)) = head_any.as_i31(&caller) {
                *captured_clone.lock().unwrap() =
                  Some(TestResult::Bool(i31.get_i32() != 0));
                return Ok(());
              }
              if let Ok(Some(st)) = head_any.as_struct(&caller) {
                let field0 = st.field(&mut caller, 0);
                match field0 {
                  Ok(Val::F64(bits)) => {
                    *captured_clone.lock().unwrap() =
                      Some(TestResult::Num(f64::from_bits(bits)));
                    return Ok(());
                  }
                  Ok(Val::I32(offset)) => {
                    if let Ok(Val::I32(length)) = st.field(&mut caller, 1) {
                      let mem = caller.get_export("memory")
                        .and_then(|e| e.into_memory());
                      if let Some(mem) = mem {
                        let data = mem.data(&caller);
                        let off = offset as usize;
                        let len = length as usize;
                        if off + len <= data.len() {
                          let bytes = data[off..off + len].to_vec();
                          *captured_clone.lock().unwrap() =
                            Some(TestResult::Str(bytes));
                          return Ok(());
                        }
                      }
                    }
                  }
                  Ok(Val::AnyRef(Some(_))) => {
                    if let Ok(Val::AnyRef(Some(ar))) = st.field(&mut caller, 0)
                      && let Ok(Some(arr)) = ar.as_array(&caller)
                    {
                      let len = arr.len(&caller).unwrap_or(0);
                      let mut bytes = Vec::with_capacity(len as usize);
                      for i in 0..len {
                        if let Ok(Val::I32(b)) = arr.get(&mut caller, i) {
                          bytes.push(b as u8);
                        }
                      }
                      *captured_clone.lock().unwrap() =
                        Some(TestResult::Str(bytes));
                      return Ok(());
                    }
                  }
                  Err(_) => {
                    *captured_clone.lock().unwrap() =
                      Some(TestResult::Str(Vec::new()));
                    return Ok(());
                  }
                  _ => {}
                }
              }
              Ok(())
            }).map_err(|e| e.to_string())?;
          }
          "host_read" => {
            let cap = io_capture.clone();
            linker.func_new("env", &name, ft, move |mut caller, params, _results| {
              let size = {
                let any = params[1].unwrap_anyref()
                  .ok_or_else(|| Error::msg("host_read: null size"))?;
                if let Ok(Some(i31)) = any.as_i31(&caller) {
                  i31.get_i32()
                } else if let Ok(Some(s)) = any.as_struct(&caller)
                  && let Ok(Val::F64(bits)) = s.field(&mut caller, 0)
                {
                  f64::from_bits(bits) as i32
                } else {
                  return Err(Error::msg("host_read: size is neither i31 nor $Num"));
                }
              };
              let bytes: Vec<u8> = cap.lock().unwrap().read_stdin(size as usize).to_vec();

              let str_wrap = caller.get_export("std/str.wat:_str_wrap_bytes")
                .and_then(|e| e.into_func())
                .ok_or_else(|| Error::msg("host_read: no _str_wrap_bytes export"))?;
              let array_ty = ArrayType::new(
                caller.engine(),
                FieldType::new(Mutability::Var, StorageType::I8),
              );
              let alloc = ArrayRefPre::new(&mut caller, array_ty);
              let elems: Vec<Val> = bytes.iter().map(|&b| Val::I32(b as i32)).collect();
              let array = ArrayRef::new_fixed(&mut caller, &alloc, &elems)
                .map_err(|e| Error::msg(format!("host_read byte array: {e}")))?;
              let mut wrapped = [Val::AnyRef(None)];
              str_wrap.call(&mut caller, &[Val::AnyRef(Some(array.to_anyref()))], &mut wrapped)?;

              let settle = caller.get_export("std/async.wat:_settle_future")
                .and_then(|e| e.into_func())
                .ok_or_else(|| Error::msg("host_read: no _settle_future export"))?;
              let future_ref = params[2].clone();
              settle.call(&mut caller, &[future_ref, wrapped[0].clone()], &mut [])?;
              Ok(())
            }).map_err(|e| e.to_string())?;
          }
          "host_channel_send" => {
            let cap = io_capture.clone();
            linker.func_new("env", &name, ft, move |mut caller, params, _results| {
              let tag = params[0].unwrap_i32();
              let bytes_any = match &params[1] {
                Val::AnyRef(Some(r)) => *r,
                _ => return Ok(()),
              };
              let arr = bytes_any.as_array(&caller)
                .map_err(|e| Error::msg(format!("host_channel_send: bytes not an array: {e}")))?
                .ok_or_else(|| Error::msg("host_channel_send: bytes array null"))?;
              let len = arr.len(&caller).unwrap_or(0);
              let mut buf = Vec::with_capacity(len as usize);
              for i in 0..len {
                if let Ok(Val::I32(b)) = arr.get(&mut caller, i) {
                  buf.push(b as u8);
                }
              }
              cap.lock().unwrap().append(tag, &buf);
              Ok(())
            }).map_err(|e| e.to_string())?;
          }
          _ => {
            let name_for_msg = name.clone();
            linker.func_new("env", &name, ft, move |_c, _p, _r| {
              Err(Error::msg(format!("host stub `{name_for_msg}` fired unexpectedly")))
            }).map_err(|e| e.to_string())?;
          }
        }
      }
    }

    let instance = linker.instantiate(&mut store, &module)
      .map_err(|e| e.to_string())?;

    let wrap_host_cont = get_func(&instance, &mut store, "wrap_host_cont")?;
    let args_empty     = get_func(&instance, &mut store, "std/fn.fnk:args_empty")?;
    let args_prepend   = get_func(&instance, &mut store, "std/fn.fnk:args_prepend")?;
    let fink_module    = get_func(&instance, &mut store, "fink_module")?;

    let done_cont = call1(&wrap_host_cont, &mut store, &[Val::I32(1)], "wrap_host_cont")?;

    let body_args = build_args_list(&args_empty, &args_prepend, &mut store, &[done_cont.clone()])?;
    fink_module.call(&mut store, &[Val::AnyRef(None), body_args], &mut [])
      .map_err(|e| format!("fink_module: {e}"))?;

    // If the source defined a top-level `main`, invoke it with a fresh
    // done cont and the test cli args. Result of `main` overrides the
    // module body's result. Globals are FQN-prefixed by compile_package.
    // Entry compiles under `./test.fnk:`, so `main` lives at `./test.fnk:main`.
    if let Some(main_global) = instance.get_global(&mut store, "./test.fnk:main") {
      *captured.lock().unwrap() = None;

      let main_clo  = main_global.get(&mut store);
      let apply_fn  = get_func(&instance, &mut store, "rt/apply.wat:apply")?;
      let str_wrap  = get_func(&instance, &mut store, "std/str.wat:_str_wrap_bytes")?;

      let mut main_args_vals = vec![done_cont];
      for bytes in TEST_CLI_ARGS {
        main_args_vals.push(wrap_bytes_as_str(&str_wrap, &mut store, bytes)?);
      }
      let main_args = build_args_list(&args_empty, &args_prepend, &mut store, &main_args_vals)?;

      apply_fn.call(&mut store, &[main_args, main_clo], &mut [])
        .map_err(|e| format!("_apply(main): {e}"))?;
    }

    Ok(captured.lock().unwrap().take().unwrap_or(TestResult::None))
  }

  fn get_func(
    instance: &wasmtime::Instance, store: &mut Store<()>, name: &str,
  ) -> Result<wasmtime::Func, String> {
    instance.get_func(store, name).ok_or_else(|| format!("no '{name}' export"))
  }

  fn call1(
    func: &wasmtime::Func, store: &mut Store<()>, params: &[Val], label: &str,
  ) -> Result<Val, String> {
    let mut out = [Val::AnyRef(None)];
    func.call(store, params, &mut out).map_err(|e| format!("{label}: {e}"))?;
    Ok(out[0].clone())
  }

  fn build_args_list(
    args_empty: &wasmtime::Func,
    args_prepend: &wasmtime::Func,
    store: &mut Store<()>,
    vals: &[Val],
  ) -> Result<Val, String> {
    let mut acc = call1(args_empty, store, &[], "args_empty")?;
    for v in vals.iter().rev() {
      acc = call1(args_prepend, store, &[v.clone(), acc], "args_prepend")?;
    }
    Ok(acc)
  }

  fn wrap_bytes_as_str(
    str_wrap: &wasmtime::Func, store: &mut Store<()>, bytes: &[u8],
  ) -> Result<Val, String> {
    let array_ty = ArrayType::new(
      store.engine(),
      FieldType::new(Mutability::Var, StorageType::I8),
    );
    let alloc = ArrayRefPre::new(&mut *store, array_ty);
    let elems: Vec<Val> = bytes.iter().map(|&b| Val::I32(b as i32)).collect();
    let array = ArrayRef::new_fixed(&mut *store, &alloc, &elems)
      .map_err(|e| format!("byte array alloc: {e}"))?;
    call1(str_wrap, store, &[Val::AnyRef(Some(array.to_anyref()))], "_str_wrap_bytes")
  }

  test_macros::include_fink_tests!("src/runner/test_literals.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_operators.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_bindings.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_functions.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_ranges.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_records.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_strings.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_patterns.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_formatting.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_tasks.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_main.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_io.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_linking.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_sets.fnk", skip-ir);
}
