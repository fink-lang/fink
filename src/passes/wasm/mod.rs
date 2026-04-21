//! WASM codegen — lifted CPS IR → self-contained, debuggable WASM binary.
//!
//! Stage order:
//!
//! ```text
//!   collect → emit → dwarf → link → fmt
//! ```
//!
//! `collect` walks the lifted CPS and builds `Module` / `CollectedFn`;
//! `emit` produces the binary via `wasm-encoder` plus byte-offset
//! mappings; `dwarf` appends line tables; `link` merges the runtime and
//! rewrites `@fink/` imports; `fmt` renders back to WAT with the native
//! source map for the playground and `fink wat`.
//!
//! The unified `$Fn2(captures, args)` calling convention, single-
//! `_apply` dispatch, closure layout, and how spread / varargs fit in
//! are all specified in `calling-convention.md` next to this module.
//!
//! Structural source locations (func headers, params, globals, exports)
//! are carried alongside the binary as `StructuralLoc` — they don't
//! correspond to code-section byte offsets and can't live in DWARF.
//!
//! Compiler-generated helpers use the `$_` prefix; the WAT formatter
//! hides them from test output.

pub mod builtins;
pub mod collect;
pub mod dwarf;
pub mod emit;
pub mod fmt;
pub mod ir;
pub mod ir_fmt;
pub mod ir_lower;
pub mod link;
pub mod runtime_contract;
pub mod sourcemap;

#[cfg(feature = "run")]
pub mod compile;

#[cfg(test)]
mod tests {
  /// Round-trip gen_wat: CPS → emit (WASM binary) → format (WAT text + source map).
  fn gen_wat(src: &str) -> String {
    // Catch panics from emit/link/format so failing tests can still produce a
    // blessable string showing the panic message.
    let src_owned = src.to_string();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || gen_wat_inner(&src_owned))) {
      Ok(s) => s,
      Err(e) => {
        let msg = if let Some(s) = e.downcast_ref::<&str>() {
          (*s).to_string()
        } else if let Some(s) = e.downcast_ref::<String>() {
          s.clone()
        } else {
          "<unknown panic>".to_string()
        };
        format!("PANIC: {msg}")
      }
    }
  }

  fn gen_wat_inner(src: &str) -> String {
    let (lifted, desugared) = crate::to_lifted(src, "test").unwrap_or_else(|e| panic!("{e}"));

    // Collect + emit WASM binary.
    let ir_ctx = super::collect::IrCtx::new(&lifted.result.origin, &desugared.ast);
    let module = super::collect::collect(&lifted.result.root, &ir_ctx, &lifted.result.module_locals, lifted.result.module_imports.clone());
    let ir_ctx = ir_ctx.with_globals(module.globals.clone());
    let mut result = super::emit::emit(&module, &ir_ctx, None);

    // Emit DWARF and append to binary.
    let dwarf_sections = super::dwarf::emit_dwarf("test", Some(src), &result.offset_mappings);
    super::dwarf::append_dwarf_sections(&mut result.wasm, &dwarf_sections);

    // Link: merge core runtime + user code into a standalone binary.
    static RUNTIME_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime.wasm"));

    let link_inputs = vec![
      super::link::LinkInput { module_name: "@fink/runtime".into(), wasm: RUNTIME_WASM.to_vec() },
      super::link::LinkInput { module_name: "@fink/user".into(), wasm: result.wasm },
    ];
    let linked = super::link::link(&link_inputs);

    // Format WASM → WAT with native source map (including structural locs).
    let (wat_output, wat_srcmap) = super::fmt::format_mapped_native(
      &linked.wasm, &result.structural_locs,
    );
    let wat_b64 = wat_srcmap.encode_base64url();

    format!("{}\n;; sm:{wat_b64}", wat_output.trim())
  }

  /// CPS → IR `Fragment` → WAT. No wasm-encoder, no linker, no runtime
  /// filtering. Drives the tracer-phase tests.
  fn ir_wat(src: &str) -> String {
    let src_owned = src.to_string();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || ir_wat_inner(&src_owned))) {
      Ok(s) => s,
      Err(e) => {
        let msg = if let Some(s) = e.downcast_ref::<&str>() {
          (*s).to_string()
        } else if let Some(s) = e.downcast_ref::<String>() {
          s.clone()
        } else {
          "<unknown panic>".to_string()
        };
        format!("PANIC: {msg}")
      }
    }
  }

  fn ir_wat_inner(src: &str) -> String {
    let (lifted, desugared) = crate::to_lifted(src, "test").unwrap_or_else(|e| panic!("{e}"));
    let frag = super::ir_lower::lower(&lifted.result, &desugared.ast);
    let (wat, sm) = super::ir_fmt::fmt_fragment_with_sm(&frag);
    let b64 = sm.encode_base64url();
    format!("{}\n;; sm:{b64}", wat.trim())
  }

  test_macros::include_fink_tests!("src/passes/wasm/test_bindings.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_literals.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_operators.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_functions.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_strings.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_records.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_fink_module.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_ir.fnk");
}
