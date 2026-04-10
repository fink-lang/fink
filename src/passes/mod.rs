// Compiler passes — each sub-module is one stage of the pipeline.
//
// Passes that take and produce CpsResult must uphold the CPS transform
// contract. See docs/cps-transform-contract.md.

pub mod ast;
pub mod cps;
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
//   desugar(Ast) → DesugaredAst  (partial application + index + scopes)
//   lower(DesugaredAst) → Cps
//   lift(Cps, DesugaredAst) → LiftedCps
//   emit_wasm(LiftedCps, DesugaredAst) → Vec<u8>
// ---------------------------------------------------------------------------

/// Raw parsed AST — for display/formatting only.
/// To proceed to CPS or codegen, call `desugar()`.
pub struct Ast<'src> {
  pub result: ast::ParseResult<'src>,
}

/// Desugared AST with index and scope analysis — the gateway to CPS.
/// `result` is boxed so the AST index can hold stable references into it.
pub struct DesugaredAst<'src> {
  pub result: Box<ast::ParseResult<'src>>,
  pub ast_index: crate::propgraph::PropGraph<ast::AstId, Option<&'src ast::Node<'src>>>,
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
  let result = ast::parser::parse(src, url)?;
  Ok(Ast { result })
}

/// Desugar partial applications and run scope analysis.
/// Produces the typed result needed by `lower()`.
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

/// Lower desugared AST to CPS IR.
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

/// Lift closures in CPS IR — produces the result needed by codegen.
pub fn lift<'src>(
  cps: Cps,
  desugared: &'src DesugaredAst<'src>,
) -> LiftedCps {
  let result = lifting::lift(cps.result, &desugared.ast_index);
  LiftedCps { result }
}

/// WASM binary output.
pub struct Wasm {
  pub binary: Vec<u8>,
  pub mappings: Vec<wasm::sourcemap::WasmMapping>,
}

/// Emit WASM binary from lifted CPS IR.
/// Runs: collect → emit → DWARF → link.
#[cfg(feature = "compile")]
pub fn emit_wasm<'src>(
  lifted: &LiftedCps,
  desugared: &'src DesugaredAst<'src>,
  path: &str,
  src: &str,
) -> Wasm {
  use wasm::{collect, dwarf, emit, link};

  let ir_ctx = collect::IrCtx::new(&lifted.result.origin, &desugared.ast_index);
  let module = collect::collect(&lifted.result.root, &ir_ctx);
  let ir_ctx = ir_ctx.with_globals(module.globals.clone());

  let mut result = emit::emit(&module, &ir_ctx);

  let dwarf_sections = dwarf::emit_dwarf(path, Some(src), &result.offset_mappings);
  dwarf::append_dwarf_sections(&mut result.wasm, &dwarf_sections);

  static RUNTIME_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime.wasm"));
  let link_inputs = vec![
    link::LinkInput { module_name: "@fink/runtime".into(), wasm: RUNTIME_WASM.to_vec() },
    link::LinkInput { module_name: "@fink/user".into(), wasm: result.wasm },
  ];
  let linked = link::link(&link_inputs);

  let mappings = result.offset_mappings.iter().map(|m| wasm::sourcemap::WasmMapping {
    wasm_offset: m.wasm_offset,
    src_line: m.loc.start.line.saturating_sub(1),
    src_col: m.loc.start.col,
  }).collect();

  Wasm { binary: linked.wasm, mappings }
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

