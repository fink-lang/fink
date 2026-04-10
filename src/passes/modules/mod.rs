// Host-neutral source loading for multi-module compilation.
//
// `SourceLoader` is the only abstraction the compiler core uses to read
// module sources. Hosts provide concrete implementations:
//
//   - `FileSourceLoader`: wraps `std::fs`, used by the native CLI and by
//     tests that point at real files on disk.
//   - `InMemorySourceLoader`: in-memory map from path to source string,
//     used by inline-source entry points (`to_wasm(src, path)`, REPL,
//     ad-hoc test sources).
//   - A future wasm32/browser host would provide its own callback-backed
//     impl without pulling `std::fs` into the compiler core.
//
// The loader's job is strictly source loading. It does NOT compile, does
// NOT understand URL schemes, and does NOT know about fink at all. The
// fink-specific compile orchestration lives in `src/passes/wasm-link/`
// where the WASM-target import resolver consumes a `SourceLoader` and
// runs the per-unit compile pipeline over the loaded sources.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Abstract source loader. Hosts provide concrete implementations.
///
/// `load` takes a host-specific path (absolute filesystem path for
/// `FileSourceLoader`, an opaque key for in-memory loaders) and returns
/// the source text or a diagnostic.
pub trait SourceLoader {
  fn load(&mut self, path: &Path) -> Result<String, String>;
}

/// Filesystem-backed source loader. Used by the native CLI and by tests
/// that point at real files on disk.
///
/// Holds no root directory — callers pass absolute paths. Path resolution
/// (turning a relative URL into an absolute path) is the concern of the
/// import resolver in `wasm-link`, not this loader.
pub struct FileSourceLoader;

impl FileSourceLoader {
  pub fn new() -> Self {
    Self
  }
}

impl Default for FileSourceLoader {
  fn default() -> Self {
    Self::new()
  }
}

impl SourceLoader for FileSourceLoader {
  fn load(&mut self, path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
      .map_err(|e| format!("cannot read {}: {e}", path.display()))
  }
}

/// In-memory source loader. Used by single-source entry points
/// (`to_wasm(src, path)`, REPL, tests with inline sources).
///
/// A request for a path not in the map returns a "no such file" error.
/// This is the expected behaviour for single-source compiles that happen
/// to contain imports: they get a clean error rather than silently
/// producing broken WASM.
pub struct InMemorySourceLoader {
  files: BTreeMap<PathBuf, String>,
}

impl InMemorySourceLoader {
  pub fn new() -> Self {
    Self { files: BTreeMap::new() }
  }

  /// Convenience constructor: loader with a single pre-loaded source.
  pub fn single(path: impl Into<PathBuf>, source: impl Into<String>) -> Self {
    let mut loader = Self::new();
    loader.add(path, source);
    loader
  }

  pub fn add(&mut self, path: impl Into<PathBuf>, source: impl Into<String>) {
    self.files.insert(path.into(), source.into());
  }
}

impl Default for InMemorySourceLoader {
  fn default() -> Self {
    Self::new()
  }
}

impl SourceLoader for InMemorySourceLoader {
  fn load(&mut self, path: &Path) -> Result<String, String> {
    self
      .files
      .get(path)
      .cloned()
      .ok_or_else(|| format!("no such source in loader: {}", path.display()))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn in_memory_single_loads_the_only_entry() {
    let mut loader = InMemorySourceLoader::single("main.fnk", "main = fn: 42");
    assert_eq!(
      loader.load(Path::new("main.fnk")).unwrap(),
      "main = fn: 42"
    );
  }

  #[test]
  fn in_memory_missing_entry_errors() {
    let mut loader = InMemorySourceLoader::new();
    let err = loader.load(Path::new("missing.fnk")).unwrap_err();
    assert!(err.contains("no such source"));
  }
}
