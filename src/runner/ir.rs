//! IR-pipeline runner test harness.
//!
//! Parallel to the main runner's `run` / `run_main` harnesses in
//! `src/runner/mod.rs`, but drives the **new** pipeline end-to-end:
//!
//!   source → to_lifted → ir_lower → ir_link → ir_emit → wasmtime
//!
//! `run(src) -> String` mirrors the existing `run` semantics:
//! compile a bare expression (no `main = fn:` wrapper), invoke
//! `fink_module` with a host-wrapped done continuation, capture the
//! value the done receives, stringify it matching the existing
//! convention (integer-valued floats rendered without `.0`).
//!
//! Used by `include_fink_tests!("src/runner/test_ir.fnk")` below.
//! The fixture set grows by demand as `ir_lower` gains coverage;
//! once it covers what the main runner tests exercise, we swap
//! `test_ir.fnk` for shared `test_literals.fnk` / `test_operators.fnk`
//! etc. imports.

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

  /// Run a bare-expression Fink source through the new IR pipeline
  /// and return the value the done continuation receives, stringified.
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

  /// Hybrid loader for IR runner tests — entry source is registered at
  /// a synthetic disk path inside `src/runner/`, dep imports resolve
  /// against the real fixture tree (`src/runner/test_modules/...`).
  /// Mirrors the OLD `runner::run`'s loader shape so test fixtures
  /// can be shared across pipelines.
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
    let pkg = crate::passes::wasm::ir_compile_package::compile_package(
      &entry_abs_path, &mut loader,
    ).map_err(|e| format!("compile_package: {e}"))?;
    let bytes = crate::passes::wasm::ir_emit::emit(&pkg.fragment);


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
            // done_cont fires with (i32 id, args). Pull args[0] and
            // inspect its shape to recover the result value.
            let captured_clone = captured.clone();
            linker.func_new("env", &name, ft, move |mut caller, params, _results| {
              let args_any = params[1].unwrap_anyref()
                .ok_or_else(|| Error::msg("host_invoke_cont: null args"))?;
              let cons = args_any.unwrap_struct(&caller)?;
              // Args list may be empty (`$Nil` — 0 fields). Treat as
              // "done called with no result" — capture None and return
              // cleanly instead of trapping.
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
              // Bools: i31ref (0 = false, 1 = true).
              if let Ok(Some(i31)) = head_any.as_i31(&caller) {
                *captured_clone.lock().unwrap() =
                  Some(TestResult::Bool(i31.get_i32() != 0));
                return Ok(());
              }
              // String types: $Str subtypes are GC structs.
              //   $StrEmpty:    no fields           — empty string.
              //   $StrDataImpl: (i32 offset, i32 len) — read from memory 0.
              //   $StrBytesImpl: (ref $ByteArray)    — read array elements.
              // Detect by struct field shape.
              if let Ok(Some(st)) = head_any.as_struct(&caller) {
                let field0 = st.field(&mut caller, 0);
                match field0 {
                  // $Num: f64.
                  Ok(Val::F64(bits)) => {
                    *captured_clone.lock().unwrap() =
                      Some(TestResult::Num(f64::from_bits(bits)));
                    return Ok(());
                  }
                  // $StrDataImpl: i32 offset + i32 length.
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
                  // $StrBytesImpl: a (ref $ByteArray) — read element-wise.
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
                  // $StrEmpty: no field 0 — index errors. Treat as empty string.
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
          // host_read(stream, size, future) — fired by the runtime
          // when ƒink code does `read stream, N`. The runtime parks
          // the cont on the future, then asks us to fulfil it. We:
          //   1. Pop up to `size` bytes off TEST_STDIN.
          //   2. Allocate a $ByteArray, wrap as $Str via _str_wrap_bytes.
          //   3. Settle the future via _settle_future, which wakes the
          //      parked cont synchronously inside this callback.
          //
          // Synchronous in-place is fine here — tests have a fixed
          // input buffer; no need for the producer-thread + condvar
          // model the production runner uses.
          "host_read" => {
            let cap = io_capture.clone();
            linker.func_new("env", &name, ft, move |mut caller, params, _results| {
              // Extract size from i31 or $Num field 0.
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

              // Wrap bytes as $Str via the qualified runtime export.
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

              // Settle the future the runtime gave us.
              let settle = caller.get_export("std/async.wat:_settle_future")
                .and_then(|e| e.into_func())
                .ok_or_else(|| Error::msg("host_read: no _settle_future export"))?;
              let future_ref = params[2].clone();
              settle.call(&mut caller, &[future_ref, wrapped[0].clone()], &mut [])?;
              Ok(())
            }).map_err(|e| e.to_string())?;
          }

          // host_channel_send(tag: i32, bytes: (ref null any)) — fired
          // by the runtime when ƒink code does `'msg' >> stdout` (or
          // `<<`) against a host channel. We read the $ByteArray
          // contents and append into the IO capture keyed by tag.
          // 1=stdout, 2=stderr.
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

    // Run the module body to settle top-level bindings (including `main`
    // if defined). Done cont fires once with whatever the body evaluates
    // to; we discard that value if `main` is also defined.
    let body_args = build_args_list(&args_empty, &args_prepend, &mut store, &[done_cont.clone()])?;
    fink_module.call(&mut store, &[Val::AnyRef(None), body_args], &mut [])
      .map_err(|e| format!("fink_module: {e}"))?;

    // If the source defined a top-level `main`, invoke it with a fresh
    // done cont (reuses `done_cont`) and the test cli args. Result of
    // `main` overrides the module body's result.
    // Globals are FQN-prefixed by ir_compile_package. Entry compiles
    // under `./test.fnk:`, so `main` lives at `./test.fnk:main`.
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

  /// Call a WASM func returning a single anyref. Wraps the boilerplate
  /// of allocating a result buffer and surfacing errors with a label.
  fn call1(
    func: &wasmtime::Func, store: &mut Store<()>, params: &[Val], label: &str,
  ) -> Result<Val, String> {
    let mut out = [Val::AnyRef(None)];
    func.call(store, params, &mut out).map_err(|e| format!("{label}: {e}"))?;
    Ok(out[0].clone())
  }

  /// Build a fink args list from `vals` by repeated `args_prepend`.
  /// `vals[0]` ends up at the head (i.e. `args_head` returns it).
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

  /// Wrap raw host bytes as a `$Str` via the runtime's `_str_wrap_bytes`.
  /// Allocates a `$ByteArray` (i8 elements) through the GC API and hands
  /// it to the wrap function.
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

  // Shared fixtures — same .fnk files the main runner uses. Tests
  // tagged `skip-ir` are the ones the new pipeline can't handle yet;
  // they emit `#[ignore = "skip-ir"]` and stay visible in the test
  // count as a coverage-gap indicator.
  test_macros::include_fink_tests!("src/runner/test_literals.fnk",  skip-ir);
  test_macros::include_fink_tests!("src/runner/test_operators.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_bindings.fnk",  skip-ir);
  test_macros::include_fink_tests!("src/runner/test_functions.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_ranges.fnk",    skip-ir);
  test_macros::include_fink_tests!("src/runner/test_records.fnk",   skip-ir);
  test_macros::include_fink_tests!("src/runner/test_strings.fnk",   skip-ir);
  test_macros::include_fink_tests!("src/runner/test_patterns.fnk",  skip-ir);
  test_macros::include_fink_tests!("src/runner/test_formatting.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_tasks.fnk",     skip-ir);
  test_macros::include_fink_tests!("src/runner/test_ir_main.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_ir_io.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_ir_link.fnk", skip-ir);
  test_macros::include_fink_tests!("src/runner/test_ir_sets.fnk", skip-ir);
}
