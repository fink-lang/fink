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
  let target = args.iter().find_map(|a| a.strip_prefix("--target=").map(|v| v.to_string()));
  let output = args.iter().zip(args.iter().skip(1)).find_map(|(a, v)| {
    if a == "-o" { Some(v.to_string()) } else { None }
  });

  let positional: Vec<&str> = args.iter().skip(1)
    .filter(|a| *a == "-" || !a.starts_with("-"))
    // Skip the value after -o (already captured above).
    .filter(|a| !args.iter().zip(args.iter().skip(1)).any(|(f, v)| f == "-o" && v.as_str() == a.as_str()))
    .map(|s| s.as_str()).collect();

  let (cmd, path) = match positional.as_slice() {
    // For `run`, extra positionals after the source file are forwarded to
    // the user's main as CLI args — so accept [cmd, path, ..].
    [cmd, path, ..] => (*cmd, *path),
    _ => {
      eprintln!("usage: fink <tokens|ast|fmt|fmt2|cps|wat|wasm|compile|run|dap> [options] <file>");
      eprintln!("  ast [--desugar]              parse (optionally desugar)");
      eprintln!("  cps [--lifted[=plain]]       CPS transform (optionally lifted)");
      eprintln!("  fmt/cps [--sourcemap]        emit source map");
      eprintln!("  fmt/cps [--embed-source]     embed source in source map");
      eprintln!("  wasm                         emit WASM binary to stdout");
      eprintln!("  wat/wasm [-O|-O1..4|-Os|-Oz]  run wasm-opt (default -O)");
      eprintln!("  compile --target=<wasm|triple> [-o output] <file>");
      process::exit(1);
    }
  };

  let src = if path == "-" {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).unwrap_or_else(|e| {
      eprintln!("error: stdin: {e}");
      process::exit(1);
    });
    buf
  } else {
    fs::read_to_string(path).unwrap_or_else(|e| {
      eprintln!("error: {path}: {e}");
      process::exit(1);
    })
  };

  match cmd {
    "tokens" => {
      println!("{}", fink::lexer::tokenize_debug(&src));
    }

    "ast" => {
      if desugar {
        let desugared = fink::to_desugared(&src).unwrap_or_else(|e| die(&e));
        println!("{}", desugared.result.root.print());
      } else {
        let ast = fink::to_ast(&src).unwrap_or_else(|e| die(&e));
        println!("{}", ast.result.root.print());
      }
    }

    "fmt" => {
      let ast = fink::to_ast(&src).unwrap_or_else(|e| die(&e));
      if sourcemap {
        let (output, srcmap) = if embed_source {
          fink::ast::fmt::fmt_mapped_with_content(&ast.result.root, path, &src)
        } else {
          fink::ast::fmt::fmt_mapped(&ast.result.root, path)
        };
        print_with_sourcemap(&output, &srcmap);
      } else {
        println!("{}", fink::ast::fmt::fmt(&ast.result.root));
      }
    }

    "fmt2" => {
      let ast = fink::to_ast(&src).unwrap_or_else(|e| die(&e));
      let cfg = fink::fmt::FmtConfig::default();
      let laid_out = fink::fmt::layout::layout(&ast.result.root, &cfg);
      if sourcemap {
        let (output, srcmap) = if embed_source {
          fink::fmt::print::print_mapped_with_content(&laid_out, path, &src)
        } else {
          fink::fmt::print::print_mapped(&laid_out, path)
        };
        print_with_sourcemap(&output, &srcmap);
      } else {
        println!("{}", fink::fmt::print::print(&laid_out));
      }
    }

    "cps" => {
      let desugared = fink::to_desugared(&src).unwrap_or_else(|e| die(&e));
      let cps = fink::passes::lower(&desugared);

      let result = if lifted.is_some() {
        fink::passes::lift(cps, &desugared).result
      } else {
        cps.result
      };

      let bk = fink::passes::cps::ir::collect_bind_kinds(&result.root);
      let ctx = fink::passes::cps::fmt::Ctx {
        origin: &result.origin,
        ast_index: &desugared.ast_index,
        captures: None,
        param_info: Some(&result.param_info),
        bind_kinds: Some(&bk),
      };
      if lifted.as_ref().is_some_and(|v| v.is_none()) {
        println!("{}", fink::passes::lifting::fmt::fmt_flat(&result.root, &ctx));
      } else if sourcemap {
        let (output, srcmap) = if embed_source {
          fink::passes::cps::fmt::fmt_with_mapped_content(&result.root, &ctx, path, &src)
        } else {
          fink::passes::cps::fmt::fmt_with_mapped(&result.root, &ctx, path)
        };
        print_with_sourcemap(&output, &srcmap);
      } else {
        println!("{}", fink::passes::cps::fmt::fmt_with(&result.root, &ctx));
      }
    }

    "wat" => {
      #[cfg(not(feature = "compile"))]
      { eprintln!("error: 'wat' command requires the 'compile' feature"); process::exit(1); }
      #[cfg(feature = "compile")]
      {
        let mut wasm = fink::to_wasm(&src, path).unwrap_or_else(|e| die(&e));
        if let Some(level) = optimize {
          fink::passes::optimize_wasm(&mut wasm, level).unwrap_or_else(|e| die(&e));
        }
        let wat = fink::passes::emit_wat(&wasm).unwrap_or_else(|e| die(&e));
        println!("{wat}");
      }
    }

    "wasm" => {
      #[cfg(not(feature = "compile"))]
      { eprintln!("error: 'wasm' command requires the 'compile' feature"); process::exit(1); }
      #[cfg(feature = "compile")]
      {
        use std::io::Write;
        let mut wasm = fink::to_wasm(&src, path).unwrap_or_else(|e| die(&e));
        if let Some(level) = optimize {
          fink::passes::optimize_wasm(&mut wasm, level).unwrap_or_else(|e| die(&e));
        }
        std::io::stdout().write_all(&wasm.binary).unwrap_or_else(|e| die(&e.to_string()));
      }
    }

    "compile" => {
      #[cfg(not(feature = "compile"))]
      { eprintln!("error: 'compile' command requires the 'compile' feature"); process::exit(1); }
      #[cfg(feature = "compile")]
      {
        let target = target.as_deref().unwrap_or("wasm");
        let target = if target == "native" { env!("TARGET") } else { target };
        let out_path = output.unwrap_or_else(|| fink::compile::default_output(path, target));

        let fink_dir = env::current_exe().unwrap_or_else(|e| die(&e.to_string()))
          .parent().unwrap().to_path_buf();
        let search = fink::compile::FinkrtSearch {
          fink_dir,
          targets_dir: env::var("FINK_TARGETS_DIR").ok().map(Into::into),
        };

        if target == "wasm" {
          fink::compile::compile_to_wasm(&src, path, &out_path)
            .unwrap_or_else(|e| die(&e));
        } else {
          fink::compile::compile_to_native(&src, path, target, &out_path, &search)
            .unwrap_or_else(|e| die(&e));
        }

        eprintln!("wrote {out_path} (target: {target})");
      }
    }

    "run" => {
      #[cfg(not(feature = "run"))]
      { eprintln!("error: 'run' command requires the 'run' feature"); process::exit(1); }
      #[cfg(feature = "run")]
      {
        use std::sync::{Arc, Mutex};
        let stdin: fink::runner::IoReadStream = Arc::new(Mutex::new(std::io::stdin()));
        let stdout: fink::runner::IoStream = Arc::new(Mutex::new(std::io::stdout()));
        let stderr: fink::runner::IoStream = Arc::new(Mutex::new(std::io::stderr()));

        // Build argv for the user program: argv[0] is the source file path,
        // followed by everything on the CLI after the source file. OsString
        // round-trips as lossless bytes on both Unix and Windows via
        // into_encoded_bytes() (fink strings are byte strings).
        let mut cli_args: Vec<Vec<u8>> = vec![path.as_bytes().to_vec()];
        let mut os_args = env::args_os().skip(1);
        let mut after_file = false;
        for a in &mut os_args {
          if after_file {
            cli_args.push(a.into_encoded_bytes());
          } else if a.to_str() == Some(path) {
            after_file = true;
          }
        }

        let exit_code = fink::run(&src, path, cli_args, stdin, stdout, stderr).unwrap_or_else(|e| die(&e));
        process::exit(exit_code as i32);
      }
    }

    "dap" => {
      #[cfg(not(feature = "run"))]
      { eprintln!("error: 'dap' command requires the 'run' feature"); process::exit(1); }
      #[cfg(feature = "run")]
      fink::debug(path).unwrap_or_else(|e| die(&e));
    }

    _ => {
      eprintln!("unknown command: {cmd}");
      eprintln!("usage: fink <tokens|ast|fmt|fmt2|cps|wat|wasm|run|dap> [options] <file>");
      process::exit(1);
    }
  }
}

fn die(msg: &str) -> ! {
  eprintln!("error: {msg}");
  process::exit(1);
}



fn print_with_sourcemap(output: &str, srcmap: &fink::sourcemap::SourceMap) {
  let json = srcmap.to_json();
  let b64 = fink::sourcemap::base64_encode(json.as_bytes());
  println!("{output}");
  println!("//# sourceMappingURL=data:application/json;base64,{b64}");
}
