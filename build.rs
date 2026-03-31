// Build script — compiles runtime WAT files to WASM at build time.
//
// This avoids a runtime dependency on the `wat` crate, keeping the
// compiler wasm32-safe. The compiled WASM bytes are embedded via
// `include_bytes!` in the emitter.

fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();

    // Compile runtime WAT files to WASM at build time.
    // Avoids a runtime dependency on the `wat` crate, keeping the
    // compiler wasm32-safe. The compiled WASM bytes are embedded via
    // `include_bytes!` in the emitter and linker.

    // types.wat is standalone — compiles directly.
    println!("cargo::rerun-if-changed=src/runtime/types.wat");
    let types_wat = std::fs::read_to_string("src/runtime/types.wat")
        .expect("failed to read types.wat");
    let types_wasm = wat_crate::parse_str(&types_wat)
        .expect("failed to compile types.wat");
    std::fs::write(format!("{out_dir}/types.wasm"), &types_wasm)
        .expect("failed to write types.wasm");

    // Extract the rec group from types.wat for injection into dependent modules.
    // This is the content between the first `(rec` and its closing `)`.
    let type_defs = extract_rec_group(&types_wat);

    // Dependent modules use @fink/runtime/types imports.
    // Build.rs strips the import and injects type definitions so the module
    // compiles standalone. The linker handles the real imports at link time.
    let dependent_modules = [
        "src/runtime/dispatch.wat",
        "src/runtime/operators.wat",
        "src/runtime/list.wat",
        "src/runtime/string.wat",
    ];

    for path in &dependent_modules {
        println!("cargo::rerun-if-changed={path}");
        let wat = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {path}: {e}"));
        let wat = prepare_wat(&wat, &type_defs);
        let wasm = wat_crate::parse_str(&wat)
            .unwrap_or_else(|e| panic!("failed to compile {path}: {e}"));
        let stem = std::path::Path::new(path).file_stem().unwrap().to_str().unwrap();
        std::fs::write(format!("{out_dir}/{stem}.wasm"), &wasm)
            .unwrap_or_else(|e| panic!("failed to write {stem}.wasm: {e}"));
    }
}

/// Prepare a WAT source that uses @fink/runtime imports for standalone
/// compilation: strip the wildcard types import and inject type definitions.
/// Specific @fink/ function imports inside the module are kept as-is —
/// they produce WASM import entries that the linker resolves at link time.
fn prepare_wat(wat: &str, type_defs: &str) -> String {
    let wat = wat.replace(
        "(import \"@fink/runtime/types\" \"*\" (func (param anyref)))",
        "",
    );
    wat.replace("(module\n", &format!("(module\n{type_defs}\n"))
}

/// Extract the rec group block from types.wat (everything from `(rec` to its
/// matching `)`, inclusive, plus indentation for injection into a module).
fn extract_rec_group(types_wat: &str) -> String {
    let start = types_wat.find("  (rec").expect("types.wat: no rec group found");
    // Find the matching closing paren — track depth.
    let bytes = types_wat.as_bytes();
    let mut depth = 0;
    let mut end = start;
    for (i, &b) in bytes[start..].iter().enumerate() {
        if b == b'(' { depth += 1; }
        if b == b')' { depth -= 1; if depth == 0 { end = start + i + 1; break; } }
    }
    types_wat[start..end].to_string()
}
