// Compiler passes — each sub-module is one stage of the pipeline.
//
// Passes that take and produce CpsResult must uphold the CPS transform
// contract. See docs/cps-transform-contract.md.

pub mod ast;
pub mod cps;
#[cfg(not(feature = "flat-ast-wip"))]
pub mod lifting;
pub mod modules;
pub mod partial;
pub mod scopes;
#[cfg(not(feature = "flat-ast-wip"))]
pub mod wasm;
#[cfg(all(feature = "compile", not(feature = "flat-ast-wip")))]
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
//   desugar(Ast) → DesugaredAst  (partial application + index + scopes)
//   lower(DesugaredAst) → Cps
//   lift(Cps, DesugaredAst) → LiftedCps
//   compile_package(entry, loader) → Wasm  (see wasm_link::compile_package)
// ---------------------------------------------------------------------------

/// Raw parsed AST — for display/formatting only.
/// To proceed to CPS or codegen, call `desugar()`.
#[cfg(not(feature = "flat-ast-wip"))]
pub struct Ast<'src> {
  pub result: ast::ParseResult<'src>,
}

/// Under the flat-ast-wip refactor, `parse()` returns `ast::Ast<'src>`
/// directly — the stage-type wrapper collapses because `ast::Ast`
/// already is "a parsed AST value". The `Ast` alias here exists only
/// so downstream code that still wants `passes::Ast<'src>` compiles.
#[cfg(feature = "flat-ast-wip")]
pub type Ast<'src> = ast::Ast<'src>;

/// Desugared AST with index and scope analysis — the gateway to CPS.
/// `result` is boxed so the AST index can hold stable references into it.
#[cfg(not(feature = "flat-ast-wip"))]
pub struct DesugaredAst<'src> {
  pub result: Box<ast::ParseResult<'src>>,
  pub ast_index: crate::propgraph::PropGraph<ast::AstId, Option<&'src ast::Node<'src>>>,
  pub scope: scopes::ScopeResult,
}

/// Desugared AST under the flat-ast arena. The owning `ast::Ast<'src>`
/// carries every node at its own AstId slot — no external index needed,
/// no self-referential Box, no unsafe.
#[cfg(feature = "flat-ast-wip")]
pub struct DesugaredAst<'src> {
  pub ast: ast::Ast<'src>,
  pub scope: scopes::ScopeResult,
}

/// CPS intermediate representation (not yet closure-lifted).
#[cfg(not(feature = "flat-ast-wip"))]
pub struct Cps {
  pub result: cps::ir::CpsResult,
}

/// Closure-lifted CPS — ready for codegen.
#[cfg(not(feature = "flat-ast-wip"))]
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
#[cfg(not(feature = "flat-ast-wip"))]
pub fn parse<'src>(src: &'src str, url: &str) -> Result<Ast<'src>, ast::parser::ParseError> {
  let result = ast::parser::parse(src, url)?;
  Ok(Ast { result })
}

/// In the flat-ast-wip mode `parse()` returns `ast::Ast<'src>` directly
/// — the type alias above makes `passes::Ast<'src>` resolve to it.
#[cfg(feature = "flat-ast-wip")]
pub fn parse<'src>(src: &'src str, url: &str) -> Result<Ast<'src>, ast::parser::ParseError> {
  ast::parser::parse(src, url)
}

/// Desugar partial applications and run scope analysis.
/// Produces the typed result needed by `lower()`.
#[cfg(not(feature = "flat-ast-wip"))]
pub fn desugar<'src>(parsed: Ast<'src>) -> Result<DesugaredAst<'src>, ast::transform::TransformError> {
  let (root, node_count) = partial::apply(parsed.result.root, parsed.result.node_count)?;
  let result = Box::new(ast::ParseResult { root, node_count });
  // SAFETY: result is heap-allocated (Box) so the address is stable after move.
  // The ast_index holds references into result's nodes, which borrow from 'src (the
  // source string), not from the Box itself. Moving the Box into the struct does not
  // relocate the heap data.
  let result_ref: &ast::ParseResult<'_> = unsafe { &*(&*result as *const _) };
  let ast_index = ast::build_index(result_ref);
  let scope = scopes::analyse(&result.root, result.node_count as usize, &[]);
  Ok(DesugaredAst { result, ast_index, scope })
}

/// Desugar under the flat-ast-wip refactor: run partial, then scope analysis.
/// **This is the Gate 3 payoff — the `unsafe` block that the legacy path
/// carries above disappears entirely. `ast::Ast` is its own index; no Box
/// self-reference, no `&'src` reborrow hack.**
#[cfg(feature = "flat-ast-wip")]
pub fn desugar<'src>(parsed: Ast<'src>) -> Result<DesugaredAst<'src>, ast::transform::TransformError> {
  let ast = partial::apply(parsed)?;
  let scope = scopes::analyse(&ast, &[]);
  Ok(DesugaredAst { ast, scope })
}

/// Lower desugared AST to CPS IR.
#[cfg(not(feature = "flat-ast-wip"))]
pub fn lower<'src>(
  desugared: &'src DesugaredAst<'src>,
) -> Cps {
  let exprs = match &desugared.result.root.kind {
    ast::NodeKind::Module { exprs, .. } => &exprs.items,
    _ => panic!("lower: expected Module root"),
  };
  let result = cps::transform::lower_module(exprs, &desugared.scope);
  Cps { result }
}

/// Lower desugared AST to CPS IR (flat-ast-wip variant).
#[cfg(feature = "flat-ast-wip")]
pub struct Cps {
  pub result: cps::ir::CpsResult,
}

#[cfg(feature = "flat-ast-wip")]
pub fn lower<'src>(
  desugared: &'src DesugaredAst<'src>,
) -> Cps {
  let root_node = desugared.ast.nodes.get(desugared.ast.root);
  let exprs: Vec<ast::AstId> = match &root_node.kind {
    ast::NodeKind::Module { exprs, .. } => exprs.items.iter().copied().collect(),
    _ => panic!("lower: expected Module root"),
  };
  let result = cps::transform::lower_module(&desugared.ast, &exprs, &desugared.scope);
  Cps { result }
}

/// Lift closures in CPS IR — produces the result needed by codegen.
#[cfg(not(feature = "flat-ast-wip"))]
pub fn lift<'src>(
  cps: Cps,
  desugared: &'src DesugaredAst<'src>,
) -> LiftedCps {
  let result = lifting::lift(cps.result, &desugared.ast_index);
  LiftedCps { result }
}

/// WASM binary output.
#[cfg(not(feature = "flat-ast-wip"))]
pub struct Wasm {
  pub binary: Vec<u8>,
  pub mappings: Vec<wasm::sourcemap::WasmMapping>,
}

/// Emit WAT text from a WASM binary.
#[cfg(all(feature = "compile", not(feature = "flat-ast-wip")))]
pub fn emit_wat(wasm: &Wasm) -> Result<String, String> {
  wasmprinter::print_bytes(&wasm.binary).map_err(|e| e.to_string())
}

/// Run wasm-opt on a WASM binary. Requires the `wasm-opt` tool on PATH.
/// `level` is the optimization flag (e.g. "-O", "-O2", "-Os", "-Oz").
/// Native only — shells out to an external process.
#[cfg(all(feature = "run", not(feature = "flat-ast-wip")))]
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

