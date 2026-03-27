// Fink compiler API — source → WASM binary.
// Safe for wasm32 targets (no native-only deps).

use crate::passes::wasm::sourcemap::WasmMapping;

pub struct CompileResult {
  pub wasm: Vec<u8>,
  pub mappings: Vec<WasmMapping>,
}

/// Compile Fink source → WASM binary through the full pipeline.
///
/// TODO: wire up WAT codegen (replaces deleted wasm::codegen).
pub fn compile_fnk(src: &str) -> Result<CompileResult, String> {
  use crate::ast::build_index;
  use crate::parser::parse;
  use crate::passes::cps::transform::lower_expr;
  use crate::passes::lifting::lift;
  use crate::passes::name_res;

  let r = parse(src).map_err(|e| e.message)?;
  let ast_index = build_index(&r);
  let scope = crate::passes::scopes::analyse(&r.root, r.node_count as usize, &[]);
  let cps = lower_expr(&r.root, &scope);
  let lifted = lift(cps, &ast_index);
  let node_count = lifted.origin.len();
  let _resolved = name_res::resolve(&lifted.root, &lifted.origin, &ast_index, node_count, &lifted.synth_alias);

  // TODO: WAT codegen pass goes here.
  Ok(CompileResult { wasm: Vec::new(), mappings: Vec::new() })
}
