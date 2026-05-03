// Build script — exposes the host target triple to compile-time code
// via the `TARGET` env var. Used by `compile/mod.rs::HOST_TARGET` so
// `fink.rs` can resolve `--target=native`.

fn main() {
    println!("cargo::rustc-env=TARGET={}", std::env::var("TARGET").unwrap());
}
