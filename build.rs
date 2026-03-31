// Build script — compiles runtime WAT files to WASM at build time.
//
// This avoids a runtime dependency on the `wat` crate, keeping the
// compiler wasm32-safe. The compiled WASM bytes are embedded via
// `include_bytes!` in the emitter.

fn main() {
    println!("cargo::rerun-if-changed=src/runtime/types.wat");

    let types_wat = std::fs::read_to_string("src/runtime/types.wat")
        .expect("failed to read src/runtime/types.wat");

    let types_wasm = wat_crate::parse_str(&types_wat)
        .expect("failed to compile types.wat");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{out_dir}/types.wasm"), &types_wasm)
        .expect("failed to write types.wasm");
}
