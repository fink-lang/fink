// Compiler passes — each sub-module is one stage of the pipeline.
//
// Passes that take and produce CpsResult must uphold the CPS transform
// contract. See docs/cps-transform-contract.md.

pub mod ast;
pub mod cps;
pub mod debug_marks;
pub mod lifting;
pub mod modules;
pub mod partial;
pub mod scopes;
pub mod wasm;
#[cfg(feature = "compile")]
#[path = "wasm-link/mod.rs"]
pub mod wasm_link;

// ---------------------------------------------------------------------------
// Pipeline — typed stage results enforce correct pass ordering.
//
// Each stage produces a result that can only be constructed by running that
// stage's function. Downstream stages take the previous result as input,
// so the type system prevents skipping or misordering passes.
//
//   tokenize(src) → token stream (debug only)
//   parse(src) → Ast
//   desugar(Ast) → DesugaredAst  (partial application + scopes)
//   lower(DesugaredAst) → Cps
//   lift(Cps, DesugaredAst) → LiftedCps
//   compile_package(entry, loader) → Wasm  (see wasm_link::compile_package)
// ---------------------------------------------------------------------------

/// Raw parsed AST — for display/formatting only.
/// To proceed to CPS or codegen, call `desugar()`.
///
/// `parse()` returns `ast::Ast<'src>` directly — `Ast` here is just a type
/// alias for the flat-AST value type so older `passes::Ast<'src>` paths
/// continue to resolve.
pub type Ast<'src> = ast::Ast<'src>;

/// Desugared AST — the gateway to CPS.
/// The owning `ast::Ast<'src>` carries every node at its own AstId slot;
/// no external index needed, no self-referential Box, no unsafe.
pub struct DesugaredAst<'src> {
  pub ast: ast::Ast<'src>,
  pub scope: scopes::ScopeResult,
}

/// CPS intermediate representation (not yet closure-lifted).
pub struct Cps {
  pub result: cps::ir::CpsResult,
}

/// Closure-lifted CPS — ready for codegen.
pub struct LiftedCps {
  pub result: cps::ir::CpsResult,
}

// --- Pipeline functions ---

/// Parse source into a raw AST.
///
/// `url` is the module's stable identity — file path, "@fink/*" virtual URL,
/// "<stdin>", "test", etc. It gets stored on the root `NodeKind::Module` so
/// downstream passes (emitter in particular) can recover it without threading
/// a separate parameter.
pub fn parse<'src>(src: &'src str, url: &str) -> Result<Ast<'src>, ast::parser::ParseError> {
  ast::parser::parse(src, url)
}

/// Desugar partial applications and run scope analysis.
/// Produces the typed result needed by `lower()`.
pub fn desugar<'src>(parsed: Ast<'src>) -> Result<DesugaredAst<'src>, ast::transform::TransformError> {
  let ast = partial::apply(parsed)?;
  let scope = scopes::analyse(&ast, &[]);
  Ok(DesugaredAst { ast, scope })
}

/// Lower desugared AST to CPS IR.
pub fn lower<'src>(
  desugared: &'src DesugaredAst<'src>,
) -> Cps {
  let root_node = desugared.ast.nodes.get(desugared.ast.root);
  let exprs: Vec<ast::AstId> = match &root_node.kind {
    ast::NodeKind::Module { exprs, .. } => exprs.items.to_vec(),
    _ => panic!("lower: expected Module root"),
  };
  let result = cps::transform::lower_module(&desugared.ast, &exprs, &desugared.scope);
  Cps { result }
}

/// Lift closures in CPS IR — produces the result needed by codegen.
pub fn lift<'src>(
  cps: Cps,
  desugared: &'src DesugaredAst<'src>,
) -> LiftedCps {
  let result = lifting::lift(cps.result, &desugared.ast);
  LiftedCps { result }
}

/// WASM binary output.
pub struct Wasm {
  pub binary: Vec<u8>,
  pub mappings: Vec<wasm::sourcemap::WasmMapping>,
  /// Step-stop records emitted by `debug_marks::analyse` and realised by
  /// the WASM emitter. Empty during Step 1 plumbing — `compile_package`
  /// returns `vec![]`. Populated once emit is wired up in Step 2/3.
  pub marks: Vec<debug_marks::MarkRecord>,
}

/// Emit WAT text from a WASM binary.
#[cfg(feature = "compile")]
pub fn emit_wat(wasm: &Wasm) -> Result<String, String> {
  wasmprinter::print_bytes(&wasm.binary).map_err(|e| e.to_string())
}

/// Run wasm-opt on a WASM binary. Requires the `wasm-opt` tool on PATH.
/// `level` is the optimization flag (e.g. "-O", "-O2", "-Os", "-Oz").
/// Native only — shells out to an external process.
#[cfg(feature = "run")]
pub fn optimize_wasm(wasm: &mut Wasm, level: &str) -> Result<(), String> {
  use std::io::Write;
  use std::process::Command;

  let mut child = Command::new("wasm-opt")
    .args([level, "--enable-gc", "--enable-reference-types", "--enable-tail-call", "-o", "-", "-"])
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .map_err(|e| format!("failed to run wasm-opt: {e}"))?;

  child.stdin.take().unwrap().write_all(&wasm.binary)
    .map_err(|e| format!("wasm-opt stdin: {e}"))?;

  let output = child.wait_with_output()
    .map_err(|e| format!("wasm-opt: {e}"))?;

  if !output.status.success() {
    return Err(format!("wasm-opt failed:\n{}", String::from_utf8_lossy(&output.stderr)));
  }

  wasm.binary = output.stdout;
  Ok(())
}
