// Build script — compiles runtime WAT files to WASM at build time.
//
// Two parallel merge paths, producing two separate artifacts:
//
// * `runtime.wasm`   — from `src/runtime/*.wat` (legacy tree).
//                      Consumed by emit.rs + link.rs (old pipeline).
// * `runtime-ir.wasm`— from `src/passes/wasm/{rt,std,interop}/*.wat`
//                      (new tree). Consumed by ir_emit when it starts
//                      targeting the new runtime.
//
// Both merges are textual WAT splices: strip internal imports (they
// resolve flat post-merge), concat bodies, prepend the rec group, wrap
// in a single `(module ...)`. Old and new trees coexist until the old
// pipeline is retired (Phase 4 cutover).
//
// No runtime dependency on the `wat` crate — keeps the compiler
// wasm32-safe. Compiled WASM bytes are embedded via `include_bytes!`
// in the emitter.

fn main() {
    // Expose the host target triple so fink.rs can resolve --target=native.
    println!("cargo::rustc-env=TARGET={}", std::env::var("TARGET").unwrap());

    let out_dir = std::env::var("OUT_DIR").unwrap();

    // --- Old-tree artefacts ---------------------------------------------

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

    let merged_wat = merge_runtime(&type_defs, &runtime_modules, old_tree_strip, false);
    let runtime_wasm = wat_crate::parse_str(&merged_wat)
        .unwrap_or_else(|e| panic!("failed to compile merged runtime: {e}"));
    std::fs::write(format!("{out_dir}/runtime.wasm"), &runtime_wasm)
        .expect("failed to write runtime.wasm");

    // --- New-tree artefacts (runtime-ir.wasm) ---------------------------

    println!("cargo::rerun-if-changed=src/passes/wasm/rt/types.wat");
    let types_ir_wat = std::fs::read_to_string("src/passes/wasm/rt/types.wat")
        .expect("failed to read rt/types.wat");
    let types_ir_wasm = wat_crate::parse_str(&types_ir_wat)
        .expect("failed to compile rt/types.wat");
    std::fs::write(format!("{out_dir}/types-ir.wasm"), &types_ir_wasm)
        .expect("failed to write types-ir.wasm");

    let type_defs_ir = extract_rec_group(&types_ir_wat);

    // New-tree runtime modules. Order: types first (in type_defs_ir),
    // then all fragments. Layout mirrors the design-doc rt/std/interop
    // vocabulary.
    let runtime_modules_ir = [
        "src/passes/wasm/rt/apply.wat",
        "src/passes/wasm/rt/protocols.wat",
        "src/passes/wasm/std/str.wat",
        "src/passes/wasm/std/hashing.wat",
        "src/passes/wasm/std/list.wat",
        "src/passes/wasm/std/dict.wat",
        "src/passes/wasm/std/int.wat",
        "src/passes/wasm/std/range.wat",
        "src/passes/wasm/std/async.wat",
        "src/passes/wasm/std/channel.wat",
        "src/passes/wasm/interop/rust.wat",
    ];

    let merged_ir_wat = merge_runtime(&type_defs_ir, &runtime_modules_ir, new_tree_strip, true);
    let runtime_ir_wasm = wat_crate::parse_str(&merged_ir_wat)
        .unwrap_or_else(|e| panic!("failed to compile merged runtime-ir: {e}"));
    std::fs::write(format!("{out_dir}/runtime-ir.wasm"), &runtime_ir_wasm)
        .expect("failed to write runtime-ir.wasm");
}

/// Common merge driver. For each fragment: read, strip internal imports
/// (via the `strip` predicate), hoist external imports (`@fink/user` /
/// `env`), collect bodies. Dedup hoisted imports, prepend type defs,
/// wrap in `(module ...)`.
///
/// When `qualify_exports` is true, every `(export "NAME"` in the body
/// is rewritten to `(export "<fragment-url>:NAME"`, where the
/// fragment-url is the path with the `src/passes/wasm/` prefix
/// stripped (so `src/passes/wasm/rt/protocols.wat` becomes the URL
/// `rt/protocols.wat`). This produces unique cross-fragment names in
/// the merged export table and makes `<url>:<name>` lookup from
/// ir_emit unambiguous. Host-facing interop exports are NOT qualified
/// (the host side of the ABI expects bare names).
fn merge_runtime(
    type_defs: &str,
    paths: &[&str],
    strip: fn(&str) -> bool,
    qualify_exports: bool,
) -> String {
    let mut imports: Vec<String> = Vec::new();
    let mut merged_body = String::new();
    for path in paths {
        println!("cargo::rerun-if-changed={path}");
        let wat = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {path}: {e}"));
        let (module_imports, mut body) = extract_module_parts(&wat, strip);
        imports.extend(module_imports);
        if qualify_exports {
            body = qualify_export_names(&body, path);
        }
        merged_body.push_str(&format!("\n  ;; --- {} ---\n", path));
        merged_body.push_str(&body);
    }
    imports.sort();
    imports.dedup();
    let imports_str = imports.join("\n");
    format!("(module\n{type_defs}\n{imports_str}\n{merged_body})\n")
}

/// Rewrite every `(export "NAME"` in `body` to `(export "<url>:NAME"`,
/// where `<url>` is `path` with the `src/passes/wasm/` prefix stripped.
/// Exceptions:
///  - `interop/*.wat` exports stay bare — they're host-facing and the
///    host expects unqualified names.
///  - Exports whose name already contains `:` are left as-is — used to
///    expose protocol dispatchers under virtual stdlib namespaces (e.g.
///    `rt/protocols.wat` exporting `std/io.fnk:stdout`).
fn qualify_export_names(body: &str, path: &str) -> String {
    let url = path.strip_prefix("src/passes/wasm/").unwrap_or(path);
    if url.starts_with("interop/") {
        return body.to_string();
    }
    // Mechanical pass: walk `(export "<NAME>"` occurrences, qualifying
    // each unless `<NAME>` already contains `:`.
    let mut out = String::with_capacity(body.len());
    let needle = "(export \"";
    let mut rest = body;
    while let Some(pos) = rest.find(needle) {
        out.push_str(&rest[..pos]);
        let after_needle = &rest[pos + needle.len()..];
        let close = after_needle.find('"').unwrap_or(after_needle.len());
        let name = &after_needle[..close];
        if name.contains(':') {
            // Already qualified — pass through verbatim.
            out.push_str(&rest[pos..pos + needle.len() + close + 1]);
        } else {
            out.push_str("(export \"");
            out.push_str(url);
            out.push(':');
            out.push_str(name);
            out.push('"');
        }
        rest = &after_needle[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Old-tree strip predicate: drop @fink/runtime/* imports (resolved
/// internally post-merge).
fn old_tree_strip(import_line: &str) -> bool {
    import_line.contains("@fink/runtime/")
}

/// New-tree strip predicate: drop `rt/*.wat`, `std/*.wat`,
/// `interop/*.wat` imports (all resolved internally post-merge).
fn new_tree_strip(import_line: &str) -> bool {
    import_line.contains("\"rt/") && import_line.contains(".wat\"")
        || import_line.contains("\"std/") && import_line.contains(".wat\"")
        || import_line.contains("\"interop/") && import_line.contains(".wat\"")
}

/// Extract module parts: returns (hoisted imports, body without imports).
/// `strip` decides which imports to drop (they resolve internally in the
/// merged module). Non-stripped non-dropped imports are hoisted to the
/// front of the merged module by the caller.
/// Hoists @fink/user and "env" imports to appear before function bodies.
fn extract_module_parts(wat: &str, strip: fn(&str) -> bool) -> (Vec<String>, String) {
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
        // Strip internal imports — resolved internally in the merged module.
        if trimmed.starts_with("(import ") && strip(line) {
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
