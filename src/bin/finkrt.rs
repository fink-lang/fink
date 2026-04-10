// finkrt — standalone Fink runtime binary.
//
// When built and run directly, prints "no embedded module".
// When WASM is appended with the magic trailer, extracts and runs it.
//
// Trailer format (last 16 bytes of the binary):
//   [u64 LE offset to payload start] [b"f1nkw4sm" magic]
//
// Layout:
//   [finkrt executable bytes]
//   [WASM module bytes]        ← offset points here
//   [u64 LE offset]            ← 8 bytes
//   [b"f1nkw4sm"]              ← 8 bytes

use std::fs;
use std::process;

const MAGIC: &[u8; 8] = b"f1nkw4sm";

fn main() {
  let wasm = match extract_payload() {
    Ok(payload) => payload,
    Err(msg) => {
      eprintln!("{msg}");
      process::exit(1);
    }
  };

  use std::sync::{Arc, Mutex};
  let opts = fink::runner::RunOptions::default();
  let stdin: fink::runner::IoReadStream = Arc::new(Mutex::new(std::io::stdin()));
  let stdout: fink::runner::IoStream = Arc::new(Mutex::new(std::io::stdout()));
  let stderr: fink::runner::IoStream = Arc::new(Mutex::new(std::io::stderr()));

  // Pass the full CLI argv to the embedded module — argv[0] is this
  // executable's name, rest are user-supplied args. OsString → lossless
  // bytes via into_encoded_bytes().
  let cli_args: Vec<Vec<u8>> = std::env::args_os()
    .map(|a| a.into_encoded_bytes())
    .collect();

  match fink::runner::wasmtime_runner::run(&opts, &wasm, cli_args, stdin, stdout, stderr) {
    Ok(exit_code) => process::exit(exit_code as i32),
    Err(e) => {
      eprintln!("error: {e}");
      process::exit(1);
    }
  }
}

fn extract_payload() -> Result<Vec<u8>, String> {
  let exe = std::env::current_exe()
    .map_err(|e| format!("cannot locate own executable: {e}"))?;

  let data = fs::read(&exe)
    .map_err(|e| format!("cannot read {}: {e}", exe.display()))?;

  if data.len() < 16 {
    return Err("no embedded module".into());
  }

  let trailer = &data[data.len() - 16..];
  let magic = &trailer[8..16];
  if magic != MAGIC {
    return Err("no embedded module".into());
  }

  let offset = u64::from_le_bytes(trailer[..8].try_into().unwrap()) as usize;
  if offset >= data.len() - 16 {
    return Err("invalid payload offset".into());
  }

  let payload = &data[offset..data.len() - 16];
  Ok(payload.to_vec())
}
