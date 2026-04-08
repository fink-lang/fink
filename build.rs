// Build script — compiles runtime WAT files to WASM at build time.
//
// The runtime modules are merged into a single WASM module so that
// inter-runtime calls (e.g. operators → str_eq) are plain function
// calls within one module — no import/export resolution needed.
//
// This avoids a runtime dependency on the `wat` crate, keeping the
// compiler wasm32-safe. The compiled WASM bytes are embedded via
// `include_bytes!` in the emitter.

fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();

    // types.wat is standalone — compiled separately for the emitter to
    // inject canonical type definitions into user modules.
    println!("cargo::rerun-if-changed=src/runtime/types.wat");
    let types_wat = std::fs::read_to_string("src/runtime/types.wat")
        .expect("failed to read types.wat");
    let types_wasm = wat_crate::parse_str(&types_wat)
        .expect("failed to compile types.wat");
    std::fs::write(format!("{out_dir}/types.wasm"), &types_wasm)
        .expect("failed to write types.wasm");

    // Extract the rec group from types.wat for injection into the merged module.
    let type_defs = extract_rec_group(&types_wat);

    // Runtime modules — merged into a single WASM module.
    // Order doesn't matter for function bodies (WAT allows forward refs),
    // but types (rec group) must come first.
    // Modules wired into the compiler pipeline.
    // set is not yet used — added when integrated.
    let runtime_modules = [
        "src/runtime/str.wat",
        "src/runtime/hashing.wat",
        "src/runtime/operators.wat",
        "src/runtime/list.wat",
        "src/runtime/rec.wat",
        "src/runtime/int.wat",
        "src/runtime/range.wat",
        "src/runtime/scheduler.wat",
        "src/runtime/channel.wat",
        "src/runtime/dispatch.wat",
        "src/runtime/interop-rust.wat",
    ];

    let mut imports = Vec::new();
    let mut merged_body = String::new();
    for path in &runtime_modules {
        println!("cargo::rerun-if-changed={path}");
        let wat = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {path}: {e}"));
        let (module_imports, body) = extract_module_parts(&wat);
        imports.extend(module_imports);
        merged_body.push_str(&format!("\n  ;; --- {} ---\n", path));
        merged_body.push_str(&body);
    }
    // Deduplicate imports (e.g. multiple modules importing _apply).
    imports.sort();
    imports.dedup();

    let imports_str = imports.join("\n");
    let merged_wat = format!("(module\n{type_defs}\n{imports_str}\n{merged_body})\n");
    let runtime_wasm = wat_crate::parse_str(&merged_wat)
        .unwrap_or_else(|e| panic!("failed to compile merged runtime: {e}"));
    std::fs::write(format!("{out_dir}/runtime.wasm"), &runtime_wasm)
        .expect("failed to write runtime.wasm");
}

/// Extract module parts: returns (@fink/user imports, body without imports).
/// Strips @fink/runtime/ imports (internal to merged module).
/// Hoists imports for the caller to deduplicate and place first.
/// Strips @fink/runtime/ imports (resolved internally in merged module).
/// Hoists @fink/user and "env" imports to appear before function bodies.
fn extract_module_parts(wat: &str) -> (Vec<String>, String) {
    let mut imports: Vec<String> = Vec::new();
    let mut body_lines: Vec<&str> = Vec::new();
    let mut inside_module = false;

    for line in wat.lines() {
        if !inside_module {
            if line.trim_start().starts_with("(module") {
                inside_module = true;
            }
            continue;
        }
        let trimmed = line.trim_start();
        // Strip @fink/runtime/ imports — resolved internally.
        if trimmed.starts_with("(import ") && line.contains("@fink/runtime/") {
            continue;
        }
        // Hoist @fink/user and "env" imports — the caller places them before body.
        if trimmed.starts_with("(import ") && (line.contains("@fink/user") || line.contains("\"env\"")) {
            imports.push(line.to_string());
            continue;
        }
        // Skip module-level closing paren.
        if line == ")" {
            continue;
        }
        body_lines.push(line);
    }

    (imports, body_lines.join("\n"))
}

/// Extract the rec group block from types.wat (everything from `(rec` to its
/// matching `)`, inclusive).
fn extract_rec_group(types_wat: &str) -> String {
    let start = types_wat.find("  (rec").expect("types.wat: no rec group found");
    let bytes = types_wat.as_bytes();
    let mut depth = 0;
    let mut end = start;
    for (i, &b) in bytes[start..].iter().enumerate() {
        if b == b'(' { depth += 1; }
        if b == b')' { depth -= 1; if depth == 0 { end = start + i + 1; break; } }
    }
    types_wat[start..end].to_string()
}
