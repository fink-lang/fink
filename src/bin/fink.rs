use std::{env, fs, process};

fn main() {
  let args: Vec<String> = env::args().collect();

  let sourcemap = args.iter().any(|a| a == "--sourcemap");
  let embed_source = args.iter().any(|a| a == "--embed-source");
  let desugar = args.iter().any(|a| a == "--desugar");
  let optimize = args.iter().find_map(|a| {
    if a == "--optimize" || a == "-O" { return Some("-O") }
    for flag in ["-O1", "-O2", "-O3", "-O4", "-Os", "-Oz"] {
      if a == flag { return Some(flag) }
    }
    if let Some(v) = a.strip_prefix("--optimize=") {
      match v {
        "1" => return Some("-O1"),
        "2" => return Some("-O2"),
        "3" => return Some("-O3"),
        "4" => return Some("-O4"),
        "s" => return Some("-Os"),
        "z" => return Some("-Oz"),
        _ => {
          eprintln!("error: unknown optimization level: {v} (expected 1, 2, 3, 4, s, z)");
          process::exit(1);
        }
      }
    }
    None
  });
  let lifted = args.iter().find_map(|a| {
    if a == "--lifted" {
      Some(None)
    } else {
      a.strip_prefix("--lifted=").map(|v| Some(v.to_string()))
    }
  });
  let positional: Vec<&str> = args.iter().skip(1).filter(|a| !a.starts_with("-")).map(|s| s.as_str()).collect();

  let (cmd, path) = match positional.as_slice() {
    [cmd, path] => (*cmd, *path),
    _ => {
      eprintln!("usage: fink <tokens|ast|fmt|fmt2|cps|wat|wasm|run|dap> [options] <file>");
      eprintln!("  ast [--desugar]              parse (optionally desugar)");
      eprintln!("  cps [--lifted[=plain]]       CPS transform (optionally lifted)");
      eprintln!("  fmt/cps [--sourcemap]        emit source map");
      eprintln!("  fmt/cps [--embed-source]     embed source in source map");
      eprintln!("  wasm                         emit WASM binary to stdout");
      eprintln!("  wat/wasm [-O|-O1..4|-Os|-Oz]  run wasm-opt (default -O)");
      process::exit(1);
    }
  };

  let src = fs::read_to_string(path).unwrap_or_else(|e| {
    eprintln!("error: {path}: {e}");
    process::exit(1);
  });

  match cmd {
    "tokens" => {
      println!("{}", fink::lexer::tokenize_debug(&src));
    }
    "ast" => {
      match fink::parser::parse(&src) {
        Ok(r) => {
          if desugar {
            let (desugared, _) = fink::passes::partial::apply(r.root, r.node_count).unwrap_or_else(|e| {
              eprintln!("error: {e:?}");
              process::exit(1);
            });
            println!("{}", desugared.print());
          } else {
            println!("{}", r.root.print());
          }
        }
        Err(e) => parse_error(&src, e, path),
      }
    }
    "fmt" => {
      match fink::parser::parse(&src) {
        Ok(r) => {
          if sourcemap {
            let (output, srcmap) = if embed_source {
              fink::ast::fmt::fmt_mapped_with_content(&r.root, path, &src)
            } else {
              fink::ast::fmt::fmt_mapped(&r.root, path)
            };
            let json = srcmap.to_json();
            let b64 = fink::sourcemap::base64_encode(json.as_bytes());
            println!("{output}");
            println!("//# sourceMappingURL=data:application/json;base64,{b64}");
          } else {
            println!("{}", fink::ast::fmt::fmt(&r.root));
          }
        }
        Err(e) => parse_error(&src, e, path),
      }
    }
    "fmt2" => {
      match fink::parser::parse(&src) {
        Ok(r) => {
          let cfg = fink::fmt::FmtConfig::default();
          let laid_out = fink::fmt::layout::layout(&r.root, &cfg);
          if sourcemap {
            let (output, srcmap) = if embed_source {
              fink::fmt::print::print_mapped_with_content(&laid_out, path, &src)
            } else {
              fink::fmt::print::print_mapped(&laid_out, path)
            };
            let json = srcmap.to_json();
            let b64 = fink::sourcemap::base64_encode(json.as_bytes());
            println!("{output}");
            println!("//# sourceMappingURL=data:application/json;base64,{b64}");
          } else {
            println!("{}", fink::fmt::print::print(&laid_out));
          }
        }
        Err(e) => parse_error(&src, e, path),
      }
    }
    "cps" => {
      match fink::parser::parse(&src) {
        Ok(r) => {
          let ast_index = fink::ast::build_index(&r);
          let scope = fink::passes::scopes::analyse(&r.root, r.node_count as usize, &[]);
          let cps = match &r.root.kind {
            fink::ast::NodeKind::Module(exprs) => {
              fink::passes::cps::transform::lower_module(&exprs.items, &scope)
            }
            _ => fink::passes::cps::transform::lower_expr(&r.root, &scope),
          };

          let result = if lifted.is_some() {
            fink::passes::lifting::lift(cps, &ast_index)
          } else {
            cps
          };

          let ctx = fink::passes::cps::fmt::Ctx {
            origin: &result.origin,
            ast_index: &ast_index,
            captures: None,
          };
          if lifted.as_ref().is_some_and(|v| v.is_none()) {
            // --lifted (no value): pretty flat formatter
            println!("{}", fink::passes::lifting::fmt::fmt_flat(&result.root, &ctx));
          } else if sourcemap {
            let (output, srcmap) = if embed_source {
              fink::passes::cps::fmt::fmt_with_mapped_content(&result.root, &ctx, path, &src)
            } else {
              fink::passes::cps::fmt::fmt_with_mapped(&result.root, &ctx, path)
            };
            let json = srcmap.to_json();
            let b64 = fink::sourcemap::base64_encode(json.as_bytes());
            println!("{output}");
            println!("//# sourceMappingURL=data:application/json;base64,{b64}");
          } else {
            // raw CPS or --lifted=plain: nested formatter
            println!("{}", fink::passes::cps::fmt::fmt_with(&result.root, &ctx));
          }
        }
        Err(e) => parse_error(&src, e, path),
      }
    }
    "wat" => {
      #[cfg(not(feature = "wat"))]
      { eprintln!("error: 'wat' command requires the 'wat' feature"); process::exit(1); }
      #[cfg(feature = "wat")]
      {
        let result = fink::compiler::compile_fnk(&src).unwrap_or_else(|e| {
          eprintln!("error: {e}");
          process::exit(1);
        });
        let wasm = if let Some(level) = optimize { wasm_opt(&result.wasm, level) } else { result.wasm };
        let wat = wasmprinter::print_bytes(&wasm).unwrap_or_else(|e| {
          eprintln!("error: {e}");
          process::exit(1);
        });
        println!("{wat}");
      }
    }
    "wasm" => {
      #[cfg(not(feature = "wat"))]
      { eprintln!("error: 'wasm' command requires the 'wat' feature"); process::exit(1); }
      #[cfg(feature = "wat")]
      {
        use std::io::Write;
        let result = fink::compiler::compile_fnk(&src).unwrap_or_else(|e| {
          eprintln!("error: {e}");
          process::exit(1);
        });
        let wasm = if let Some(level) = optimize { wasm_opt(&result.wasm, level) } else { result.wasm };
        std::io::stdout().write_all(&wasm).unwrap_or_else(|e| {
          eprintln!("error: {e}");
          process::exit(1);
        });
      }
    }
    "run" => {
      #[cfg(not(feature = "runner"))]
      { eprintln!("error: 'run' command requires the 'runner' feature"); process::exit(1); }
      #[cfg(feature = "runner")]
      if let Err(e) = fink::runner::run_file(Default::default(), path) {
        eprintln!("error: {e}");
        process::exit(1);
      }
    }
    "dap" => {
      #[cfg(not(feature = "runner"))]
      { eprintln!("error: 'dap' command requires the 'runner' feature"); process::exit(1); }
      #[cfg(feature = "runner")]
      if let Err(e) = fink::dap::run(std::io::stdin(), std::io::stdout(), path) {
        eprintln!("error: {e}");
        process::exit(1);
      }
    }
    _ => {
      eprintln!("unknown command: {cmd}");
      eprintln!("usage: fink <tokens|ast|fmt|fmt2|cps|wat|wasm|run|dap> [options] <file>");
      process::exit(1);
    }
  }
}

fn wasm_opt(wasm: &[u8], level: &str) -> Vec<u8> {
  use std::io::Write;
  use std::process::Command;

  let mut child = Command::new("wasm-opt")
    .args([level, "--enable-gc", "--enable-reference-types", "--enable-tail-call", "-o", "-", "-"])
    .stdin(process::Stdio::piped())
    .stdout(process::Stdio::piped())
    .stderr(process::Stdio::piped())
    .spawn()
    .unwrap_or_else(|e| {
      eprintln!("error: failed to run wasm-opt: {e}");
      process::exit(1);
    });

  child.stdin.take().unwrap().write_all(wasm).unwrap_or_else(|e| {
    eprintln!("error: wasm-opt stdin: {e}");
    process::exit(1);
  });

  let output = child.wait_with_output().unwrap_or_else(|e| {
    eprintln!("error: wasm-opt: {e}");
    process::exit(1);
  });

  if !output.status.success() {
    eprintln!("error: wasm-opt failed:\n{}", String::from_utf8_lossy(&output.stderr));
    process::exit(1);
  }

  output.stdout
}

fn parse_error(src: &str, e: fink::parser::ParseError, path: &str) -> ! {
  let diag = fink::errors::Diagnostic { message: e.message, loc: e.loc, hint: None };
  let opts = fink::errors::FormatOptions { path: Some(path), ..Default::default() };
  eprintln!("{}", fink::errors::format_diagnostic(src, &diag, &opts));
  process::exit(1);
}
