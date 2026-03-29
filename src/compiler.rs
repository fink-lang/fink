// Fink compiler API — source → WASM binary.
// Safe for wasm32 targets (no native-only deps).

use crate::passes::wasm::sourcemap::WasmMapping;

pub struct CompileResult {
  pub wasm: Vec<u8>,
  pub mappings: Vec<WasmMapping>,
}

/// Compile Fink source → WASM binary through the full pipeline.
///
/// Pipeline: parse → AST → partial → scopes → CPS → lift → emit → DWARF.
pub fn compile_fnk(src: &str) -> Result<CompileResult, String> {
  use crate::ast::{build_index, NodeKind};
  use crate::parser::parse;
  use crate::passes::cps::transform::lower_module;
  use crate::passes::lifting::lift;
  use crate::passes::wasm::{collect, dwarf, emit};

  let r = parse(src).map_err(|e| e.message)?;
  let (root, node_count) = crate::passes::partial::apply(r.root, r.node_count)
    .map_err(|e| format!("{:?}", e))?;
  let r = crate::ast::ParseResult { root, node_count };
  let ast_index = build_index(&r);
  let scope = crate::passes::scopes::analyse(&r.root, r.node_count as usize, &[]);

  let exprs = match &r.root.kind {
    NodeKind::Module(exprs) => &exprs.items,
    _ => return Err("expected module".into()),
  };
  let cps = lower_module(exprs, &scope);
  let lifted = lift(cps, &ast_index);

  // Collect module structure from lifted CPS.
  let ir_ctx = collect::IrCtx::new(&lifted.origin, &ast_index);
  let module = collect::collect(&lifted.root, &ir_ctx);
  let ir_ctx = ir_ctx.with_globals(module.globals.clone());

  // Emit WASM binary with byte offset tracking.
  let mut result = emit::emit(&module, &ir_ctx);

  // Emit DWARF and append to binary.
  let dwarf_sections = dwarf::emit_dwarf("input.fnk", Some(src), &result.offset_mappings);
  dwarf::append_dwarf_sections(&mut result.wasm, &dwarf_sections);

  // Convert OffsetMapping → WasmMapping for DAP compatibility.
  let mappings = result.offset_mappings.iter().map(|m| WasmMapping {
    wasm_offset: m.wasm_offset,
    src_line: m.loc.start.line.saturating_sub(1), // 0-indexed for source map
    src_col: m.loc.start.col,
  }).collect();

  Ok(CompileResult { wasm: result.wasm, mappings })
}
