// Runner: executes compiled WASM in Wasmtime.

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
/// `args` is the CLI argv passed to `main` — argv[0] is the program name.
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
/// `args` is the CLI argv passed to `main` — argv[0] is the program name.
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
  use super::*;
  use std::sync::{Arc, Mutex};
  use wasmtime::*;

  /// Result of calling an exported CPS function directly.
  enum TestResult {
    Num(f64),
    Bool(bool),
    Str(String),
    None,
  }

  /// Bootstrap a fink module and return the init result.
  ///
  /// The module's `fink_module` export is a CPS function that takes [ƒret]
  /// as its args. We create a done continuation that captures the result,
  /// build [done] args, and call `_apply([done], fink_module_closure)`.
  fn exec_module_init(wasm: &[u8]) -> Result<TestResult, String> {
    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_tail_call(true);
    config.wasm_function_references(true);

    let engine = Engine::new(&config).map_err(|e| e.to_string())?;
    let module = Module::new(&engine, wasm).map_err(|e| e.to_string())?;
    let mut store = Store::new(&engine, ());

    // Wire up "env" imports — trap with "not yet implemented".
    let mut linker = Linker::new(&engine);
    for import in module.imports() {
      if import.module() == "env"
        && let ExternType::Func(ft) = import.ty()
      {
        let name = import.name().to_string();
        let err_name = name.clone();
        linker.func_new("env", &name, ft.clone(), move |_caller, _params, _results| {
          Err(Error::msg(format!("builtin '{}' not yet implemented", err_name)))
        }).map_err(|e| e.to_string())?;
      }
    }

    let instance = linker.instantiate(&mut store, &module).map_err(|e| e.to_string())?;

    let fink_module = instance.get_func(&mut store, "fink_module")
      .ok_or("no 'fink_module' export")?;
    let box_func = instance.get_func(&mut store, "_box_func")
      .ok_or("no '_box_func' export")?;
    let apply = instance.get_func(&mut store, "_apply")
      .ok_or("no '_apply' export")?;

    // Box fink_module as a $Closure.
    let mut boxed_module = [Val::AnyRef(None)];
    box_func.call(&mut store, &[Val::FuncRef(Some(fink_module))], &mut boxed_module)
      .map_err(|e| format!("_box_func(fink_module) failed: {}", e))?;

    // Create the "done" continuation — receives the module init result.
    let result_val: Arc<Mutex<Option<TestResult>>> = Arc::new(Mutex::new(None));
    let result_clone = result_val.clone();

    let fn2_stub = instance.get_func(&mut store, "_fn2_stub")
      .ok_or("no '_fn2_stub' export")?;
    let done_ty = fn2_stub.ty(&store);
    let done = Func::new(&mut store, done_ty, move |mut caller, params, _results| {
      if let Some(Val::AnyRef(Some(args_list))) = params.get(1)
        && let Ok(Some(cons)) = args_list.as_struct(&caller)
        && let Ok(Val::AnyRef(Some(any_ref))) = cons.field(&mut caller, 0)
      {
        if let Ok(Some(i31)) = any_ref.as_i31(&caller) {
          *result_clone.lock().unwrap() = Some(TestResult::Bool(i31.get_i32() != 0));
        } else if let Ok(Some(struct_ref)) = any_ref.as_struct(&caller) {
          if let Ok(Val::F64(bits)) = struct_ref.field(&mut caller, 0) {
            *result_clone.lock().unwrap() = Some(TestResult::Num(f64::from_bits(bits)));
          } else if let Ok(Val::I32(offset)) = struct_ref.field(&mut caller, 0)
            && let Ok(Val::I32(length)) = struct_ref.field(&mut caller, 1)
          {
            // $StrDataImpl — read directly from linear memory.
            if let Some(memory) = caller.get_export("memory")
              && let Some(mem) = memory.into_memory()
            {
              let data = mem.data(&caller);
              let start = offset as usize;
              let end = start + length as usize;
              if end <= data.len() {
                let s = String::from_utf8_lossy(&data[start..end]).into_owned();
                *result_clone.lock().unwrap() = Some(TestResult::Str(s));
              }
            }
          } else if let Ok(Val::AnyRef(Some(arr_any))) = struct_ref.field(&mut caller, 0)
            && let Ok(Some(arr)) = arr_any.as_array(&caller)
            && let Ok(len) = arr.len(&caller)
          {
            // $StrBytesImpl — read bytes from GC $ByteArray.
            let mut bytes = Vec::with_capacity(len as usize);
            for i in 0..len {
              if let Ok(Val::I32(b)) = arr.get(&mut caller, i) {
                bytes.push(b as u8);
              }
            }
            let s = String::from_utf8_lossy(&bytes).into_owned();
            *result_clone.lock().unwrap() = Some(TestResult::Str(s));
          }
        }
      }
      Ok(())
    });

    // Box done as a $Closure and build args [done].
    let mut boxed_done = [Val::AnyRef(None)];
    box_func.call(&mut store, &[Val::FuncRef(Some(done))], &mut boxed_done)
      .map_err(|e| format!("_box_func(done) failed: {}", e))?;

    let list_nil = instance.get_func(&mut store, "_list_nil")
      .ok_or("no '_list_nil' export")?;
    let mut nil = [Val::AnyRef(None)];
    list_nil.call(&mut store, &[], &mut nil)
      .map_err(|e| format!("_list_nil failed: {}", e))?;

    let list_prepend = instance.get_func(&mut store, "_list_prepend")
      .ok_or("no '_list_prepend' export")?;
    let mut args = [Val::AnyRef(None)];
    list_prepend.call(&mut store, &[boxed_done[0], nil[0]], &mut args)
      .map_err(|e| format!("_list_prepend failed: {}", e))?;

    // _apply([done], fink_module_closure) — runs the module body.
    apply.call(&mut store, &[args[0], boxed_module[0]], &mut [])
      .map_err(|e| format!("module init failed: {}", e))?;

    Ok(result_val.lock().unwrap().take().unwrap_or(TestResult::None))
  }

  /// Bootstrap a multi-module package and return the entry module's init result.
  ///
  /// Discovers all `*:fink_module` dep init exports, calls them first (in
  /// export order, which is BFS dependency order from compile_package), then
  /// calls the entry `fink_module` with a done continuation that captures
  /// the result.
  ///
  /// `dep_urls` lists the canonical URLs of dep modules in init order
  /// (dependencies before their consumers).
  fn exec_package(wasm: &[u8], dep_urls: &[&str]) -> Result<TestResult, String> {
    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_tail_call(true);
    config.wasm_function_references(true);

    let engine = Engine::new(&config).map_err(|e| e.to_string())?;
    let module = Module::new(&engine, wasm).map_err(|e| e.to_string())?;
    let mut store = Store::new(&engine, ());

    // Wire up host imports ("env" module) — trap with "not implemented".
    let mut linker = Linker::new(&engine);
    for import in module.imports() {
      if import.module() == "env"
        && let ExternType::Func(ft) = import.ty()
      {
        let name = import.name().to_string();
        let err_name = name.clone();
        linker.func_new("env", &name, ft.clone(), move |_caller, _params, _results| {
          Err(Error::msg(format!("host builtin '{}' not yet implemented", err_name)))
        }).map_err(|e| e.to_string())?;
      }
    }
    let instance = linker.instantiate(&mut store, &module).map_err(|e| e.to_string())?;

    let box_func = instance.get_func(&mut store, "_box_func")
      .ok_or("no '_box_func' export")?;
    let apply = instance.get_func(&mut store, "_apply")
      .ok_or("no '_apply' export")?;
    let list_nil = instance.get_func(&mut store, "_list_nil")
      .ok_or("no '_list_nil' export")?;
    let list_prepend = instance.get_func(&mut store, "_list_prepend")
      .ok_or("no '_list_prepend' export")?;
    let fn2_stub = instance.get_func(&mut store, "_fn2_stub")
      .ok_or("no '_fn2_stub' export")?;
    let fn2_ty = fn2_stub.ty(&store);

    // Initialize each dep module in order (deps before consumers).
    for dep_url in dep_urls {
      let export_name = format!("{}:fink_module", dep_url);
      let dep_fink_module = instance.get_func(&mut store, &export_name)
        .ok_or_else(|| format!("no '{}' export", export_name))?;

      // Box the dep fink_module.
      let mut boxed = [Val::AnyRef(None)];
      box_func.call(&mut store, &[Val::FuncRef(Some(dep_fink_module))], &mut boxed)
        .map_err(|e| format!("_box_func({}) failed: {}", export_name, e))?;

      // No-op done continuation for dep init — we don't capture the result.
      let noop = Func::new(&mut store, fn2_ty.clone(), |_caller, _params, _results| Ok(()));

      let mut boxed_noop = [Val::AnyRef(None)];
      box_func.call(&mut store, &[Val::FuncRef(Some(noop))], &mut boxed_noop)
        .map_err(|e| format!("_box_func(noop) for {} failed: {}", dep_url, e))?;

      let mut nil = [Val::AnyRef(None)];
      list_nil.call(&mut store, &[], &mut nil)
        .map_err(|e| format!("_list_nil failed: {}", e))?;

      let mut args = [Val::AnyRef(None)];
      list_prepend.call(&mut store, &[boxed_noop[0], nil[0]], &mut args)
        .map_err(|e| format!("_list_prepend failed: {}", e))?;

      apply.call(&mut store, &[args[0], boxed[0]], &mut [])
        .map_err(|e| format!("{}:fink_module init failed: {}", dep_url, e))?;
    }

    // Now run the entry module and capture its result.
    let fink_module = instance.get_func(&mut store, "fink_module")
      .ok_or("no 'fink_module' export")?;
    let mut boxed_module = [Val::AnyRef(None)];
    box_func.call(&mut store, &[Val::FuncRef(Some(fink_module))], &mut boxed_module)
      .map_err(|e| format!("_box_func(fink_module) failed: {}", e))?;

    let result_val: Arc<Mutex<Option<TestResult>>> = Arc::new(Mutex::new(None));
    let result_clone = result_val.clone();

    let done = Func::new(&mut store, fn2_ty.clone(), move |mut caller, params, _results| {
      if let Some(Val::AnyRef(Some(args_list))) = params.get(1)
        && let Ok(Some(cons)) = args_list.as_struct(&caller)
        && let Ok(Val::AnyRef(Some(any_ref))) = cons.field(&mut caller, 0)
      {
        if let Ok(Some(i31)) = any_ref.as_i31(&caller) {
          *result_clone.lock().unwrap() = Some(TestResult::Bool(i31.get_i32() != 0));
        } else if let Ok(Some(struct_ref)) = any_ref.as_struct(&caller)
          && let Ok(Val::F64(bits)) = struct_ref.field(&mut caller, 0) {
            *result_clone.lock().unwrap() = Some(TestResult::Num(f64::from_bits(bits)));
          }
      }
      Ok(())
    });

    let mut boxed_done = [Val::AnyRef(None)];
    box_func.call(&mut store, &[Val::FuncRef(Some(done))], &mut boxed_done)
      .map_err(|e| format!("_box_func(done) failed: {}", e))?;

    let mut nil = [Val::AnyRef(None)];
    list_nil.call(&mut store, &[], &mut nil)
      .map_err(|e| format!("_list_nil failed: {}", e))?;

    let mut args = [Val::AnyRef(None)];
    list_prepend.call(&mut store, &[boxed_done[0], nil[0]], &mut args)
      .map_err(|e| format!("_list_prepend failed: {}", e))?;

    apply.call(&mut store, &[args[0], boxed_module[0]], &mut [])
      .map_err(|e| format!("module init failed: {}", e))?;

    Ok(result_val.lock().unwrap().take().unwrap_or(TestResult::None))
  }

  #[test]
  fn multi_module_two_files_inline() {
    // lib.fnk exports `double` (fn x: x * 2).
    // entry.fnk imports double and returns double 21.
    let lib_src = "double = fn x: x * 2";
    let entry_src = "{double} = import './lib.fnk'\ndouble 21";

    let mut loader = crate::passes::modules::InMemorySourceLoader::new();
    loader.add("./lib.fnk", lib_src);
    loader.add("./entry.fnk", entry_src);

    let wasm = crate::compile_package(std::path::Path::new("./entry.fnk"), &mut loader)
      .expect("compile_package failed");

    match exec_package(&wasm.binary, &["./lib.fnk"]) {
      Ok(TestResult::Num(v)) => {
        assert_eq!(v, 42.0, "expected double(21) = 42, got {}", v);
      }
      Ok(_other) => panic!("expected Num(42), got non-numeric result"),
      Err(e) => panic!("exec_package failed: {}", e),
    }
  }

  #[test]
  fn multi_module_diamond_shared_dep() {
    // Diamond-shaped dep graph — verifies the linker handles a shared
    // dependency correctly (common.fnk is compiled once and linked once
    // even though both consumers import it). Specifically exercises the
    // "two different relative URLs reach the same file" case: `left.fnk`
    // lives in ./sub/ and imports common as `./common.fnk`, while
    // `right.fnk` lives at the top and imports it as `./sub/common.fnk`.
    // Both must canonicalise to the same entry-relative form and share
    // a single fragment in the linked output.
    //
    //         entry
    //        /     \
    //  sub/left    right
    //        \     /
    //     sub/common
    //
    // sub/common.fnk : base = 10
    // sub/left.fnk   : {base} = import './common.fnk' ; left_val = base + 1   (=11)
    // right.fnk      : {base} = import './sub/common.fnk' ; right_val = base + 2  (=12)
    // entry.fnk      : imports left_val + right_val, returns sum (=23)
    let common_src = "base = 10";
    let left_src = "{base} = import './common.fnk'\nleft_val = base + 1";
    let right_src = "{base} = import './sub/common.fnk'\nright_val = base + 2";
    let entry_src =
      "{left_val} = import './sub/left.fnk'\n\
       {right_val} = import './right.fnk'\n\
       left_val + right_val";

    let mut loader = crate::passes::modules::InMemorySourceLoader::new();
    loader.add("./sub/common.fnk", common_src);
    loader.add("./sub/left.fnk", left_src);
    loader.add("./right.fnk", right_src);
    loader.add("./entry.fnk", entry_src);

    let wasm = crate::compile_package(std::path::Path::new("./entry.fnk"), &mut loader)
      .expect("compile_package failed");

    // Sanity: common.fnk must be exported exactly once — not duplicated,
    // even though the two consumers refer to it via different relative URLs.
    let wat = wasmprinter::print_bytes(&wasm.binary).expect("wasmprinter failed");
    let common_export_count = wat.matches("\"./sub/common.fnk:fink_module\"").count();
    assert_eq!(
      common_export_count, 1,
      "sub/common.fnk should be linked exactly once, found {} fink_module exports",
      common_export_count,
    );

    // Init order: common before its consumers (left/right), then entry.
    // Names here are entry-relative canonical URLs — what the linker knows them as.
    let dep_urls = ["./sub/common.fnk", "./sub/left.fnk", "./right.fnk"];
    match exec_package(&wasm.binary, &dep_urls) {
      Ok(TestResult::Num(v)) => {
        assert_eq!(v, 23.0, "expected left_val(11) + right_val(12) = 23, got {}", v);
      }
      Ok(_other) => panic!("expected Num(23), got non-numeric result"),
      Err(e) => panic!("exec_package failed: {}", e),
    }
  }

  #[test]
  fn multi_module_file_loader() {
    // Test the FileSourceLoader path with the test_modules directory.
    // entry.fnk: {foo} = import './foobar/spam.fnk' \n shrub = fn ham: foo ham
    // spam.fnk:  foo = fn ni: ni * 2
    let entry_path = std::path::Path::new(
      concat!(env!("CARGO_MANIFEST_DIR"), "/src/runner/test_modules/entry.fnk"),
    );
    let mut loader = crate::passes::modules::FileSourceLoader::new();
    let wasm = crate::compile_package(entry_path, &mut loader)
      .expect("compile_package from filesystem failed");

    // The binary should be valid WASM.
    assert!(wasm.binary.starts_with(b"\0asm"), "not a valid WASM binary");

    // Both fink_module exports should be present.
    let wat = wasmprinter::print_bytes(&wasm.binary)
      .expect("wasmprinter failed");
    assert!(wat.contains("\"fink_module\""), "missing entry fink_module export");
    assert!(
      wat.contains("\"./foobar/spam.fnk:fink_module\""),
      "missing dep fink_module export — got wat:\n{}",
      wat,
    );
  }

  #[allow(unused)]
  fn run(src: &str) -> String {
    let wasm = crate::to_wasm(src, "test").expect("compilation failed");
    match exec_module_init(&wasm.binary) {
      Ok(TestResult::Num(v)) => {
        if v == v.floor() && v.abs() < 1e15 {
          format!("{}", v as i64)
        } else {
          format!("{}", v)
        }
      }
      Ok(TestResult::Bool(b)) => format!("{}", b),
      Ok(TestResult::Str(s)) => s,
      Ok(TestResult::None) => String::new(),
      Err(e) => format!("ERROR: {}", e),
    }
  }

  /// Hybrid SourceLoader for `run_main`:
  ///
  /// - The inline test source lives at a synthetic path
  ///   `<CARGO_MANIFEST_DIR>/src/runner/__test_entry.fnk`.
  /// - Any imports (e.g. `import './test_modules/entry.fnk'`) resolve
  ///   relative to that synthetic path's parent — i.e. `src/runner/` —
  ///   via `FileSourceLoader`, which picks up real files from disk.
  ///
  /// This lets `.fnk` runner tests exercise the full multi-module
  /// compile pipeline from inline source without needing to write the
  /// entry module out to a real file first.
  struct RunMainLoader {
    entry_abs_path: std::path::PathBuf,
    entry_source: String,
    disk: crate::passes::modules::FileSourceLoader,
  }

  impl crate::passes::modules::SourceLoader for RunMainLoader {
    fn load(&mut self, path: &std::path::Path) -> Result<String, String> {
      if path == self.entry_abs_path {
        Ok(self.entry_source.clone())
      } else {
        self.disk.load(path)
      }
    }
  }

  #[allow(unused)]
  fn run_main(src: &str) -> String {
    let entry_abs_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("src/runner/__test_entry.fnk");
    let mut loader = RunMainLoader {
      entry_abs_path: entry_abs_path.clone(),
      entry_source: src.to_string(),
      disk: crate::passes::modules::FileSourceLoader::new(),
    };
    let wasm = match crate::compile_package(&entry_abs_path, &mut loader) {
      Ok(w) => w,
      Err(e) => return format!("ERROR: compile: {e}"),
    };
    let stdin_buf: IoReadStream = Arc::new(Mutex::new(std::io::Cursor::new(b"hello from stdin".to_vec())));
    let stdout_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));

    // Fixed fake argv for tests: program name + two dummy args.
    let args = vec![b"test".to_vec(), b"alpha".to_vec(), b"beta".to_vec()];

    match wasmtime_runner::run(
      &RunOptions::default(),
      &wasm.binary,
      args,
      stdin_buf,
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
  test_macros::include_fink_tests!("src/runner/test_modules.fnk");
}
