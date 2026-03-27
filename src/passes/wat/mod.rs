// WAT text generator.
//
// Produces WAT text from fully-lifted CPS IR. Replaces wasmprinter.
//
// ## Requirements
//
// 1. Input: fully-lifted CPS IR (CpsResult after lifting pass).
//    No dependency on name_res — Ref::Synth(CpsId) is the resolved identity.
//
// 2. Output: WAT text (s-expression format) that assembles to valid WASM.
//    - Module with types, globals, functions, exports
//    - Same calling convention as current codegen: every fn takes (anyref * N),
//      last param is cont (ref $Cont), all calls are return_call/return_call_ref
//    - WasmGC types: $Int (i32 boxed in struct), $Cont (funcref), $Any (anyref)
//
// 3. Source map support: track WAT text positions → CPS nodes → AST nodes
//    → source locations. Uses MappedWriter pattern (like AST/CPS formatters).
//    CpsId → AstId via origin map, AstId → source Loc via ast_index.
//
// 4. Snapshot-testable: deterministic output, suitable for .fnk test expectations.
//    Tests compare gen_wat(src) output against expected WAT strings.
//
// 5. CLI integration: `fink wat <file>` uses this instead of wasmprinter.
//    `fink wat --sourcemap <file>` emits WAT + inline source map.
//
// 6. Playground integration: WAT text with source map for visualization of
//    compiler internals (source ↔ WAT mapping).
//
// ## Non-goals (handled elsewhere)
//
// - WASM binary emission: stays in wasm/codegen.rs (wasm-encoder).
// - Debugging: DAP adapter uses WasmMapping from codegen, not WAT text.
// - name_res: not needed — Ref::Synth carries bind identity directly.

pub mod writer;

#[cfg(test)]
mod tests {
  use crate::ast::build_index;
  use crate::parser::parse;
  use crate::passes::cps::transform::lower_module;
  use crate::passes::lifting::lift;

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

    // WAT with source map
    let (wat_output, wat_srcmap) = super::writer::emit_mapped_with_content(&lifted, &ast_index, "test", src);
    let wat_json = wat_srcmap.to_json();
    let wat_b64 = crate::sourcemap::base64_encode(wat_json.as_bytes());

    // Dump WAT + CPS files for source map review (DUMP_WAT=1)
    if std::env::var("DUMP_WAT").is_ok() {
      let name = crate::test_context::name();
      let slug: String = name.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
      let dir = ".claude.local/scratch/wat";
      let _ = std::fs::create_dir_all(dir);
      // WAT file (// prefix for viewer compatibility)
      let wat_content = format!("{}\n//# sourceMappingURL=data:application/json;base64,{wat_b64}", wat_output.trim());
      let _ = std::fs::write(format!("{dir}/{slug}.wat.js"), &wat_content);
      // Pre-lift CPS file
      let pre_lift_cps = lower_module(exprs, &scope);
      let pre_lift_ctx = crate::passes::cps::fmt::Ctx {
        origin: &pre_lift_cps.origin,
        ast_index: &ast_index,
        captures: None,
      };
      let (pre_cps_output, pre_cps_srcmap) = crate::passes::cps::fmt::fmt_with_mapped_content(&pre_lift_cps.root, &pre_lift_ctx, "test", src);
      let pre_cps_json = pre_cps_srcmap.to_json();
      let pre_cps_b64 = crate::sourcemap::base64_encode(pre_cps_json.as_bytes());
      let pre_cps_content = format!("{pre_cps_output}\n//# sourceMappingURL=data:application/json;base64,{pre_cps_b64}");
      let _ = std::fs::write(format!("{dir}/{slug}.cps.js"), &pre_cps_content);
      // Lifted CPS file
      let lifted_ctx = crate::passes::cps::fmt::Ctx {
        origin: &lifted.origin,
        ast_index: &ast_index,
        captures: None,
      };
      let (cps_output, cps_srcmap) = crate::passes::cps::fmt::fmt_with_mapped_content(&lifted.root, &lifted_ctx, "test", src);
      let cps_json = cps_srcmap.to_json();
      let cps_b64 = crate::sourcemap::base64_encode(cps_json.as_bytes());
      let cps_content = format!("{cps_output}\n//# sourceMappingURL=data:application/json;base64,{cps_b64}");
      let _ = std::fs::write(format!("{dir}/{slug}.lft.js"), &cps_content);
    }

    format!("{}\n#sourcemaps:{wat_b64}", wat_output.trim())
  }

  test_macros::include_fink_tests!("src/passes/wat/test_wat.fnk");
}
