// WASM passes — collection, binary emission, and post-processing.
//
// ## Architecture
//
// The pipeline produces a self-contained, debuggable WASM binary from
// lifted CPS IR. WAT text is a derived view — formatted from the binary.
//
//   Lifted CPS IR
//       ↓
//   collect.rs  → Module / CollectedFn (shared with wat/writer.rs)
//       ↓
//   emit.rs     → WASM binary (wasm-encoder) + byte offset mappings
//       ↓
//   dwarf.rs    → DWARF .debug_* sections (gimli::write) appended to binary
//       ↓
//   fmt.rs      → WAT text + Source Map v3 (wasmparser + gimli::read)
//
// The WASM binary contains: WasmGC types, imported builtins, defined
// functions, globals, exports, name section, and DWARF debug info.
// The formatter reads it back to produce human-readable WAT with
// source maps for the playground and `fink wat` CLI.
//
// Structural source locations (func headers, params, globals, exports)
// are passed alongside the binary via StructuralLoc, since they don't
// correspond to code section byte offsets and can't be in DWARF.
//
// ## Module layout
//
// collect.rs    — shared collect phase (lifted CPS → Module/CollectedFn)
// emit.rs       — wasm-encoder binary emitter + byte offset tracking
// dwarf.rs      — gimli::write DWARF line table emission
// fmt.rs        — custom WASM→WAT formatter (wasmparser + gimli::read)
// sourcemap.rs  — WasmMapping type (used by DAP)
// compile.rs    — WAT text → WASM binary (wat crate wrapper, legacy)

pub mod collect;
pub mod dwarf;
pub mod emit;
pub mod fmt;
pub mod sourcemap;

#[cfg(feature = "runner")]
pub mod compile;

#[cfg(test)]
mod tests {
  use crate::ast::build_index;
  use crate::parser::parse;
  use crate::passes::cps::transform::lower_module;
  use crate::passes::lifting::lift;

  /// Round-trip gen_wat: CPS → emit (WASM binary) → format (WAT text + source map).
  fn gen_wat(src: &str) -> String {
    let r = parse(src).unwrap_or_else(|e| panic!("parse error: {}", e.message));
    let (root, node_count) = crate::passes::partial::apply(r.root, r.node_count)
      .unwrap_or_else(|e| panic!("partial error: {:?}", e));
    let r = crate::ast::ParseResult { root, node_count };
    let ast_index = build_index(&r);
    let scope = crate::passes::scopes::analyse(&r.root, r.node_count as usize, &[]);
    let exprs = match &r.root.kind {
      crate::ast::NodeKind::Module(exprs) => &exprs.items,
      _ => panic!("expected module"),
    };
    let cps = lower_module(exprs, &scope);
    let lifted = lift(cps, &ast_index);

    // Collect + emit WASM binary.
    let ir_ctx = super::collect::IrCtx::new(&lifted.origin, &ast_index);
    let module = super::collect::collect(&lifted.root, &ir_ctx);
    let ir_ctx = ir_ctx.with_globals(module.globals.clone());
    let mut result = super::emit::emit(&module, &ir_ctx);

    // Emit DWARF and append to binary.
    let dwarf_sections = super::dwarf::emit_dwarf("test", Some(src), &result.offset_mappings);
    super::dwarf::append_dwarf_sections(&mut result.wasm, &dwarf_sections);

    // Format WASM → WAT with source map (including structural locs).
    let (wat_output, wat_srcmap) = super::fmt::format_mapped_with_locs(
      &result.wasm, &result.structural_locs, "test", src,
    );
    let wat_json = wat_srcmap.to_json();
    let wat_b64 = crate::sourcemap::base64_encode(wat_json.as_bytes());

    // Dump files for source map review (DUMP_WAT=1).
    if std::env::var("DUMP_WAT").is_ok() {
      let name = crate::test_context::name();
      let slug: String = name.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
      let dir = ".claude.local/scratch/wasm";
      let _ = std::fs::create_dir_all(dir);

      // WAT file
      let wat_content = format!("{}\n//# sourceMappingURL=data:application/json;base64,{wat_b64}", wat_output.trim());
      let _ = std::fs::write(format!("{dir}/{slug}.wat.js"), &wat_content);

    }

    format!("{}\n;;sourcemaps:{wat_b64}", wat_output.trim())
  }

  test_macros::include_fink_tests!("src/passes/wasm/test_wasm.fnk");
}
