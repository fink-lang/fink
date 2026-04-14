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
//
// ## Closure representation and calling convention
//
// After lifting, every lambda with captures becomes a top-level function
// with extra leading params for the captured values, plus an `·fn_closure`
// call at the original site that packages the funcref + captures into a
// closure value.
//
// ### WasmGC types
//
// Universal value type: `(ref null any)` — WASM GC built-in. All value
// slots use this. Structs are implicitly subtypes of `any`.
//
// Plain functions use `$FnN` types (one per arity):
//
//   (type $Fn2 (func (param (ref any) (ref any))))
//
// Closures use `$ClosureN` struct types (one per capture count N):
//
//   (type $Closure0 (struct (field funcref)))           ;; bare funcref wrapper
//   (type $Closure1 (struct
//     (field funcref)         ;; funcref to lifted fn (arity = call_arity + N)
//     (field (ref any))       ;; capture 0
//   ))
//
// `$Closure0` replaces the old `$FuncBox` — wraps a raw funcref so it can
// flow through `(ref null any)` slots (funcrefs are not subtypes of `any`
// in the WASM GC spec).
//
// Numbers are `$Num` structs (f64 field). Booleans are i31ref (0/1).
//
// ### Construction: `$_closure_N` helper
//
// The `·fn_closure` builtin compiles to a call to `$_closure_N` (N = number
// of captures + 1 for the funcref). This is an emitted helper function:
//
//   (func $_closure_2 (param funcref) (param (ref any))
//     (struct.new $Closure1 (local.get 0) (local.get 1))
//   )
//
// It takes the funcref + N captures and returns the boxed struct as
// `(ref any)`.
//
// ### Dispatch: `$_apply_N` helper (call-ref-or-closure)
//
// At every `Callable::Val` call site (indirect call through an `(ref any)`
// value), we don't statically know whether the callee is a plain funcref
// or a closure struct. Instead of a static type inference pass, we use
// WasmGC's `br_on_cast` for runtime dispatch.
//
// For each call-site arity N, an emitted helper `$_apply_N`
// tries each `$ClosureK` type that exists in the module:
//
//   (func $_apply_2
//     (param $a0 (ref any)) (param $a1 (ref any)) (param $callee (ref any))
//     (block $try_clos1
//       (br_on_cast_fail $try_clos1 (ref any) (ref $Closure1) (local.get $callee))
//       ;; it's $Closure1 — extract funcref + 1 capture, call with arity 3
//       (struct.get $Closure1 1)   ;; capture 0
//       (local.get $a0)
//       (local.get $a1)
//       (struct.get $Closure1 0)   ;; funcref
//       (return_call_ref $Fn3)
//     )
//     ;; fallthrough: plain $Closure0 — unbox and call directly
//     (return_call_ref $Fn2 (local.get $a0) (local.get $a1)
//       (ref.cast (ref $Fn2) (struct.get $Closure0 0 (local.get $callee))))
//   )
//
// This is correct by construction — no static analysis needed. A future
// type inference pass can eliminate branches where the type is known.
//
// ### Internal naming convention
//
// All compiler-generated helper functions use the `$_` prefix to
// distinguish them from user-defined functions. The formatter hides
// `$_`-prefixed functions from test output.
//
// ### Arity tracking
//
// The set of `$ClosureN` types to emit is determined by scanning for
// `·fn_closure` call sites during collection. The set of
// `$_apply_N` helpers is determined by `Callable::Val` call
// site arities (already tracked by `scan_call_arities`).

pub mod builtins;
pub mod collect;
pub mod dwarf;
pub mod emit;
pub mod fmt;
pub mod link;
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
    let ir_ctx = super::collect::IrCtx::new(&lifted.result.origin, &desugared.ast_index);
    let module = super::collect::collect(&lifted.result.root, &ir_ctx, &lifted.result.module_locals, lifted.result.module_imports.clone());
    let ir_ctx = ir_ctx.with_globals(module.globals.clone());
    let mut result = super::emit::emit(&module, &ir_ctx);

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

    // Format WASM → WAT with source map (including structural locs).
    let (wat_output, wat_srcmap) = super::fmt::format_mapped_with_locs(
      &linked.wasm, &result.structural_locs, "test", src,
    );
    let wat_json = wat_srcmap.to_json();
    let wat_b64 = crate::sourcemap::base64_encode(wat_json.as_bytes());

    // Dump files for source map review — set `DUMP_WAT_DIR=<path>` to
    // enable, unset to skip. No default path: if the env var is missing
    // the block is a no-op.
    if let Some(dir) = std::env::var_os("DUMP_WAT_DIR") {
      let dir = std::path::PathBuf::from(dir);
      let name = crate::test_context::name();
      let slug: String = name.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
      let _ = std::fs::create_dir_all(&dir);
      let wat_content = format!("{}\n//# sourceMappingURL=data:application/json;base64,{wat_b64}", wat_output.trim());
      let _ = std::fs::write(dir.join(format!("{slug}.wat.js")), &wat_content);
    }

    format!("{}\n;;sourcemaps:{wat_b64}", wat_output.trim())
  }

  test_macros::include_fink_tests!("src/passes/wasm/test_bindings.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_literals.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_operators.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_functions.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_strings.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_records.fnk");
  test_macros::include_fink_tests!("src/passes/wasm/test_fink_module.fnk");
}
