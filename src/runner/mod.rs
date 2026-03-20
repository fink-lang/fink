// Runner: compiles WAT or loads WASM, runs it in Wasmtime.

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

pub fn run_file(mut opts: RunOptions, path: &str) -> Result<(), String> {
  if opts.source_label == "fink" {
    opts.source_label = path.to_string();
  }

  // .fnk files: compile through the full pipeline, then run.
  if path.ends_with(".fnk") {
    let src = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let wat = compile_fnk(&src)?;
    return wasmtime_runner::run_wat(&opts, Some(path), &wat);
  }

  let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
  // WASM binaries start with magic bytes \0asm; everything else is WAT text.
  if bytes.starts_with(b"\0asm") {
    wasmtime_runner::run(&opts, &bytes)
  } else {
    let src = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
    wasmtime_runner::run_wat(&opts, Some(path), src)
  }
}

/// Compile Fink source → WAT text through the full pipeline.
pub fn compile_fnk(src: &str) -> Result<String, String> {
  use crate::ast::build_index;
  use crate::parser::parse;
  use crate::passes::closure_lifting::lift_all;
  use crate::passes::cont_lifting::lift;
  use crate::passes::cps::transform::lower_expr;
  use crate::passes::wasm::codegen::codegen;

  let r = parse(src).map_err(|e| e.message)?;
  let ast_index = build_index(&r);
  let cps = lower_expr(&r.root);
  let cps = lift(cps);
  let (lifted, resolved) = lift_all(cps, &ast_index);
  let lifted = lift(lifted);
  Ok(codegen(&lifted, &resolved, &ast_index).wat)
}
