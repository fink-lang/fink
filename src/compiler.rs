// Fink compiler API — source → WASM binary.
// Safe for wasm32 targets (no native-only deps).

use crate::passes::wasm::sourcemap::WasmMapping;

pub struct CompileResult {
  pub wasm: Vec<u8>,
  pub mappings: Vec<WasmMapping>,
}

/// Compile Fink source → WASM binary through the full pipeline.
pub fn compile_fnk(src: &str) -> Result<CompileResult, String> {
  use crate::ast::build_index;
  use crate::parser::parse;
  use crate::passes::closure_lifting::lift_all;
  use crate::passes::cps::transform::lower_expr;
  use crate::passes::wasm::codegen::codegen;

  let r = parse(src).map_err(|e| e.message)?;
  let ast_index = build_index(&r);
  let cps = lower_expr(&r.root);
  let (lifted, resolved) = lift_all(cps, &ast_index);
  let result = codegen(&lifted, &resolved, &ast_index);
  Ok(CompileResult { wasm: result.wasm, mappings: result.mappings })
}
