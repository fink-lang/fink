// Compile command — produces WASM or standalone native executables.
//
// Native executables are created by appending WASM bytes + a magic trailer
// to a copy of the finkrt binary (Deno-style binary append).
//
// Trailer format (last 16 bytes):
//   [u64 LE offset to payload start] [b"f1nkw4sm" magic]

use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"f1nkw4sm";

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
///   1. targets_dir/<triple>/finkrt (env var override)
///   2. fink_dir/targets/<triple>/finkrt
///   3. fink_dir/finkrt (current-arch fallback)
fn find_finkrt(target: &str, search: &FinkrtSearch) -> Result<PathBuf, String> {
  if let Some(dir) = &search.targets_dir {
    let p = dir.join(target).join("finkrt");
    if p.exists() { return Ok(p); }
  }

  let p = search.fink_dir.join("targets").join(target).join("finkrt");
  if p.exists() { return Ok(p); }

  let p = search.fink_dir.join("finkrt");
  if p.exists() { return Ok(p); }

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
