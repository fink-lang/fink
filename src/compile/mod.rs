//! `fink compile` command — produces WASM or standalone native executables.
//!
//! Native executables are created by appending WASM bytes + a magic
//! trailer to a copy of the `finkrt` binary (Deno-style binary append).
//! Trailer format (last 16 bytes): `[u64 LE offset to payload start]`
//! followed by `b"f1nkw4sm"` magic.
//!
//! # finkrt layout
//!
//! In packaged releases, `finkrt` binaries live under
//! `<fink_dir>/targets/<triple>/finkrt` — one per supported target, so
//! `fink compile --target=<any>` works offline for every supported
//! triple.
//!
//! In the cargo-build dev workflow, `cargo build` puts `fink` and
//! `finkrt` as siblings in `target/<profile>/` with no `targets/`
//! subdirectory. The sibling `finkrt` is valid only for the host target
//! — cross-target compilation in the dev workflow requires running
//! `cargo build --target=<triple>` and pointing `FINK_TARGETS_DIR` at
//! the staged output.

use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"f1nkw4sm";

/// Host triple this fink binary was compiled for. Set by build.rs from
/// cargo's $TARGET env var.
const HOST_TARGET: &str = env!("TARGET");

/// Compile source to a WASM file.
pub fn compile_to_wasm(src: &str, path: &str, out_path: &str) -> Result<(), String> {
  let wasm = crate::to_wasm(src, path)?;
  std::fs::write(out_path, &wasm.binary).map_err(|e| e.to_string())
}

/// Compile source to a standalone native executable for the given target.
pub fn compile_to_native(
  src: &str,
  path: &str,
  target: &str,
  out_path: &str,
  finkrt_search: &FinkrtSearch,
) -> Result<(), String> {
  let wasm = crate::to_wasm(src, path)?;
  let finkrt_path = find_finkrt(target, finkrt_search)?;

  let mut binary = std::fs::read(&finkrt_path)
    .map_err(|e| format!("cannot read {}: {e}", finkrt_path.display()))?;

  let offset = binary.len() as u64;
  binary.extend_from_slice(&wasm.binary);
  binary.extend_from_slice(&offset.to_le_bytes());
  binary.extend_from_slice(MAGIC);

  std::fs::write(out_path, &binary).map_err(|e| e.to_string())?;

  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(out_path, std::fs::Permissions::from_mode(0o755))
      .map_err(|e| e.to_string())?;
  }

  Ok(())
}

/// Where to search for finkrt binaries.
pub struct FinkrtSearch {
  /// Directory containing the fink binary (for sibling/targets/ lookup).
  pub fink_dir: PathBuf,
  /// Optional override via FINK_TARGETS_DIR env var.
  pub targets_dir: Option<PathBuf>,
}

/// Locate the finkrt binary for a given target triple.
///
/// Search order:
///   1. `targets_dir/<triple>/finkrt` (env var override)
///   2. `fink_dir/targets/<triple>/finkrt` (packaged release layout)
///   3. `fink_dir/finkrt` (dev workflow: sibling of cargo-built fink) —
///      only valid when `target == HOST_TARGET`, otherwise we'd be handing
///      back a host-arch binary for a cross-target request.
fn find_finkrt(target: &str, search: &FinkrtSearch) -> Result<PathBuf, String> {
  if let Some(dir) = &search.targets_dir {
    let p = dir.join(target).join("finkrt");
    if p.exists() { return Ok(p); }
  }

  let p = search.fink_dir.join("targets").join(target).join("finkrt");
  if p.exists() { return Ok(p); }

  // Sibling finkrt only satisfies host-target lookups — see module docs.
  if target == HOST_TARGET {
    let p = search.fink_dir.join("finkrt");
    if p.exists() { return Ok(p); }
  }

  Err(format!("finkrt not found for target {target}"))
}

/// Derive the default output path from the input path and target.
pub fn default_output(path: &str, target: &str) -> String {
  let stem = Path::new(path).file_stem()
    .and_then(|s| s.to_str()).unwrap_or("out");
  if target == "wasm" {
    format!("{stem}.wasm")
  } else {
    stem.to_string()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn write_stub(path: &Path) {
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, b"").unwrap();
  }

  // A made-up triple guaranteed not to equal the host.
  const OTHER_TARGET: &str = "riscv64-unknown-elf";

  #[test]
  fn find_finkrt_prefers_targets_subdir() {
    let tmp = tempfile::tempdir().unwrap();
    let fink_dir = tmp.path().to_path_buf();
    write_stub(&fink_dir.join("targets").join(HOST_TARGET).join("finkrt"));

    let search = FinkrtSearch { fink_dir: fink_dir.clone(), targets_dir: None };
    let found = find_finkrt(HOST_TARGET, &search).unwrap();
    assert_eq!(found, fink_dir.join("targets").join(HOST_TARGET).join("finkrt"));
  }

  #[test]
  fn find_finkrt_dev_fallback_only_for_host() {
    // Dev workflow: cargo build puts fink and finkrt side-by-side with no
    // targets/ subdir. The sibling finkrt is valid for the host target.
    let tmp = tempfile::tempdir().unwrap();
    let fink_dir = tmp.path().to_path_buf();
    write_stub(&fink_dir.join("finkrt"));

    let search = FinkrtSearch { fink_dir: fink_dir.clone(), targets_dir: None };
    let found = find_finkrt(HOST_TARGET, &search).unwrap();
    assert_eq!(found, fink_dir.join("finkrt"));
  }

  #[test]
  fn find_finkrt_sibling_fallback_rejects_cross_target() {
    // BUG REPRO: before the fix, this test fails — the sibling finkrt was
    // returned for *any* target, silently handing back a host-arch binary
    // when the user asked for a cross-target.
    let tmp = tempfile::tempdir().unwrap();
    let fink_dir = tmp.path().to_path_buf();
    write_stub(&fink_dir.join("finkrt"));

    let search = FinkrtSearch { fink_dir, targets_dir: None };
    let result = find_finkrt(OTHER_TARGET, &search);
    assert!(
      result.is_err(),
      "sibling-finkrt fallback must not satisfy cross-target lookups; got: {result:?}"
    );
  }

  #[test]
  fn find_finkrt_env_override_wins() {
    let tmp = tempfile::tempdir().unwrap();
    let fink_dir = tmp.path().join("fink-dir");
    let override_dir = tmp.path().join("override");

    // Stub under both the override path and the packaged layout.
    write_stub(&fink_dir.join("targets").join(OTHER_TARGET).join("finkrt"));
    write_stub(&override_dir.join(OTHER_TARGET).join("finkrt"));

    let search = FinkrtSearch {
      fink_dir,
      targets_dir: Some(override_dir.clone()),
    };
    let found = find_finkrt(OTHER_TARGET, &search).unwrap();
    assert_eq!(found, override_dir.join(OTHER_TARGET).join("finkrt"));
  }
}
