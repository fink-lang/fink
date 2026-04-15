pub mod errors;
#[cfg(feature = "compile")]
pub mod compile;
#[cfg(feature = "run")]
pub mod dap;
#[cfg(feature = "runtime")]
pub mod runner;
pub mod passes;
pub mod propgraph;
pub mod sourcemap;
pub mod strings;

#[cfg(feature = "run")]
pub mod runtime;
pub mod test_context;

// Re-exports for convenience — short paths for foundational types.
pub use passes::ast;
pub use passes::ast::lexer;
pub use passes::ast::parser;

// ---------------------------------------------------------------------------
// Pipeline convenience — run the full pass chain to a target stage.
// ---------------------------------------------------------------------------

/// Parse source → raw AST.
///
/// `url` is the module's stable identity (file path, `"<stdin>"`, `"test"`,
/// etc.). It's stored in the `Module` node so downstream passes — notably
/// the WASM emitter — can recover it without a threaded parameter.
pub fn to_ast<'src>(src: &'src str, url: &str) -> Result<passes::Ast<'src>, String> {
  passes::parse(src, url).map_err(|e| e.message)
}

/// Parse + desugar → desugared AST with scope analysis.
pub fn to_desugared<'src>(src: &'src str, url: &str) -> Result<passes::DesugaredAst<'src>, String> {
  let parsed = passes::parse(src, url).map_err(|e| e.message)?;
  passes::desugar(parsed).map_err(|e| format!("{e:?}"))
}

/// Compile source → CPS IR (+ desugared AST for context).
pub fn to_cps<'src>(src: &'src str, url: &str) -> Result<(passes::Cps, passes::DesugaredAst<'src>), String> {
  let desugared = to_desugared(src, url)?;
  let cps = passes::lower(&desugared);
  Ok((cps, desugared))
}

/// Compile source → lifted CPS IR (+ desugared AST for context).
pub fn to_lifted<'src>(src: &'src str, url: &str) -> Result<(passes::LiftedCps, passes::DesugaredAst<'src>), String> {
  let (cps, desugared) = to_cps(src, url)?;
  let lifted = passes::lift(cps, &desugared);
  Ok((lifted, desugared))
}

/// Compile source → WASM binary.
///
/// For callers that have a source string in hand and just want to compile
/// it. Internally wraps the source in a one-entry `InMemorySourceLoader`
/// and calls `compile_package`, so single-source and multi-source inputs
/// share the same code path. A single-source program with no imports
/// works identically to before; a single-source program that tries to
/// import will get a clean error from the in-memory loader.
#[cfg(feature = "compile")]
pub fn to_wasm(src: &str, path: &str) -> Result<passes::Wasm, String> {
  use passes::modules::InMemorySourceLoader;
  let mut loader = InMemorySourceLoader::single(path, src);
  passes::wasm_link::compile_package(std::path::Path::new(path), &mut loader)
}

/// Compile a package rooted at `entry_path` to a linked WASM binary.
///
/// The multi-module compile entry point. `loader` is any `SourceLoader`
/// implementation — typically `FileSourceLoader` for filesystem-backed
/// compiles or `InMemorySourceLoader` for REPL/test scenarios.
///
/// For now this is a thin wrapper over the single-source pipeline —
/// Slices 4+ extend it with the real multi-module behaviour.
#[cfg(feature = "compile")]
pub fn compile_package(
  entry_path: &std::path::Path,
  loader: &mut dyn passes::modules::SourceLoader,
) -> Result<passes::Wasm, String> {
  passes::wasm_link::compile_package(entry_path, loader)
}

/// Compile source → optimized WASM binary.
#[cfg(feature = "run")]
pub fn to_wasm_opt(src: &str, path: &str, level: &str) -> Result<passes::Wasm, String> {
  let mut wasm = to_wasm(src, path)?;
  passes::optimize_wasm(&mut wasm, level)?;
  Ok(wasm)
}

/// Compile source → WAT text.
#[cfg(feature = "compile")]
pub fn to_wat(src: &str, path: &str) -> Result<String, String> {
  let wasm = to_wasm(src, path)?;
  passes::emit_wat(&wasm)
}

/// Compile and run source. Returns the exit code from main.
///
/// `args` is the CLI argv passed to the user's `main` — argv[0] is the
/// program name, followed by user-supplied CLI arguments.
#[cfg(feature = "run")]
pub fn run(
  src: &str,
  path: &str,
  args: Vec<Vec<u8>>,
  stdin: runner::IoReadStream,
  stdout: runner::IoStream,
  stderr: runner::IoStream,
) -> Result<i64, String> {
  runner::run_source(Default::default(), src, path, args, stdin, stdout, stderr)
}

/// Start DAP debug server for a .fnk file, communicating over stdin/stdout.
#[cfg(feature = "run")]
pub fn debug(path: &str) -> Result<(), String> {
  dap::run(std::io::stdin(), std::io::stdout(), path)
}
