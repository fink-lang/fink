use std::{env, fs, process};

fn main() {
  let args: Vec<String> = env::args().collect();

  if args.iter().any(|a| a == "--version") {
    println!("fink {}", env!("CARGO_PKG_VERSION"));
    return;
  }

  let source_map = args.iter().any(|a| a == "--source-map");
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
      eprintln!("usage: fink <tokens|ast|fmt|fmt2|cps|marks|wat|wasm|compile|run|dap> [options] <file>");
      eprintln!("       fink --version");
      eprintln!("  ast [--desugar]              parse (optionally desugar)");
      eprintln!("  cps [--lifted[=plain]]       CPS transform (optionally lifted)");
      eprintln!("  marks                        debugger step-stops (per-CpsId markers + source map)");
      eprintln!("  ast/fmt/fmt2/cps [--source-map]  append embedded source map comment");
      eprintln!("  wat                          emit WAT text from IR fragment to stdout");
      eprintln!("  wasm                         emit WASM binary to stdout");
      eprintln!("  wasm [-O|-O1..4|-Os|-Oz]     run wasm-opt (default -O)");
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
        let desugared = fink::to_desugared(&src, path).unwrap_or_else(|e| die(&e));
        println!("{}", desugared.ast.print());
      } else {
        let ast = fink::to_ast(&src, path).unwrap_or_else(|e| die(&e));
        println!("{}", ast.print());
      }
    }

    "fmt" => {
      let ast = fink::to_ast(&src, path).unwrap_or_else(|e| die(&e));
      if source_map {
        let (output, srcmap) = fink::ast::fmt::fmt_mapped_native(&ast);
        println!("{output}\n# sm:{}", srcmap.encode_base64url());
      } else {
        println!("{}", fink::ast::fmt::fmt(&ast));
      }
    }

    "fmt2" => {
      let ast = fink::to_ast(&src, path).unwrap_or_else(|e| die(&e));
      let cfg = fink::fmt::FmtConfig::default();
      let laid_out = fink::fmt::layout::layout(&ast, &cfg);
      if source_map {
        let (output, srcmap) = fink::fmt::print::print_mapped_native(&laid_out);
        println!("{output}\n# sm:{}", srcmap.encode_base64url());
      } else {
        println!("{}", fink::fmt::print::print(&laid_out));
      }
    }

    "cps" => {
      let desugared = fink::to_desugared(&src, path).unwrap_or_else(|e| die(&e));
      let cps = fink::passes::lower(&desugared);

      let result = if lifted.is_some() {
        fink::passes::lift(cps, &desugared).result
      } else {
        cps.result
      };

      let bk = fink::passes::cps::ir::collect_bind_kinds(&result.root);
      let ctx = fink::passes::cps::fmt::Ctx {
        origin: &result.origin,
        ast: &desugared.ast,
        captures: None,
        param_info: Some(&result.param_info),
        bind_kinds: Some(&bk),
      };
      let lifted_flat = lifted.as_ref().is_some_and(|v| v.is_none());
      if lifted_flat && source_map {
        let (output, srcmap) = fink::passes::lifting::fmt::fmt_flat_mapped_native(&result.root, &ctx);
        println!("{output}\n# sm:{}", srcmap.encode_base64url());
      } else if lifted_flat {
        println!("{}", fink::passes::lifting::fmt::fmt_flat(&result.root, &ctx));
      } else if source_map {
        let (output, srcmap) = fink::passes::cps::fmt::fmt_with_mapped_native(&result.root, &ctx);
        println!("{output}\n# sm:{}", srcmap.encode_base64url());
      } else {
        println!("{}", fink::passes::cps::fmt::fmt_with(&result.root, &ctx));
      }
    }

    "marks" => {
      // Debug-marks pass output: one `s_<kind>#<id>` token per CpsId
      // the pass deems a step-stop, optionally followed by `# sm:<b64>`.
      // Skeleton commit: policy marks nothing, so output is just the
      // (empty) sm line. Used by the vscode-fink extension to decorate
      // source ranges that will become stops.
      let (lifted, desugared) = fink::to_lifted(&src, path).unwrap_or_else(|e| die(&e));
      let debug_marks = fink::passes::debug_marks::analyse(&lifted, &desugared);
      let (output, srcmap) = fink::passes::debug_marks::fmt::render_mapped_native(
        &debug_marks, &lifted, &desugared,
      );
      if output.is_empty() {
        println!("# sm:{}", srcmap.encode_base64url());
      } else {
        println!("{output}\n# sm:{}", srcmap.encode_base64url());
      }
    }

    "wat" => {
      #[cfg(not(feature = "compile"))]
      { eprintln!("error: 'wat' command requires the 'compile' feature"); process::exit(1); }
      #[cfg(feature = "compile")]
      {
        let entry_abs = std::path::Path::new(path).canonicalize()
          .unwrap_or_else(|e| die(&format!("canonicalize {path}: {e}")));
        let mut loader = fink::passes::modules::FileSourceLoader::new();
        let pkg = fink::passes::wasm::compile_package::compile_package(
          &entry_abs, &mut loader,
        ).unwrap_or_else(|e| die(&e));
        if source_map {
          let (wat, srcmap) = fink::passes::wasm::fmt::fmt_fragment_with_sm(&pkg.fragment);
          println!("{wat}\n;; sm:{}", srcmap.encode_base64url());
        } else {
          let wat = fink::passes::wasm::fmt::fmt_fragment(&pkg.fragment);
          println!("{wat}");
        }
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

    "decode-sm" => {
      // `src` is the input text (from file or stdin). Find the last
      // `# sm:<b64>` or `;; sm:<b64>` line, decode, and print one row
      // per mapping: index, output byte offset, a short preview of the
      // output slice that mapping covers, and the source byte range.
      //
      // If a `--source <path>` was supplied (positional: second file after
      // the main input), also print the source slice. For ad-hoc use on a
      // blessed test file, pipe `fink cps --embed-sm foo.fnk | fink decode-sm - --source=foo.fnk`.
      let source_ref = args.iter().find_map(|a| a.strip_prefix("--source=").map(|v| v.to_string()));
      let source_text: Option<String> = source_ref.as_deref().map(|p| {
        fs::read_to_string(p).unwrap_or_else(|e| die(&format!("{p}: {e}")))
      });

      let blob = src.lines().rev().find_map(|l| {
        let t = l.trim_start();
        t.strip_prefix("# sm:").or_else(|| t.strip_prefix(";; sm:"))
      }).unwrap_or_else(|| die("error: no '# sm:' or ';; sm:' line found in input"));

      let sm = fink::sourcemap::native::SourceMap::decode_base64url(blob.trim())
        .unwrap_or_else(|e| die(&format!("decode: {e}")));

      // Strip the blob line from the input so our `out` offsets line up with
      // the generated output only.
      let generated = strip_sm_line(&src);

      for (i, m) in sm.mappings.iter().enumerate() {
        let next_out = sm.mappings.get(i + 1).map(|n| n.out).unwrap_or(generated.len() as u32);
        let out_preview = preview(&generated, m.out, next_out);
        match m.src {
          None => {
            println!("{:4}: out@{:>5} {:<30}  | <no src>", i, m.out, out_preview);
          }
          Some(src_r) => {
            let src_preview = source_text.as_deref()
              .map(|s| preview(s, src_r.start, src_r.end))
              .unwrap_or_default();
            println!(
              "{:4}: out@{:>5} {:<30}  | src[{}..{}] {}",
              i, m.out, out_preview, src_r.start, src_r.end, src_preview
            );
          }
        }
      }
    }

    _ => {
      eprintln!("unknown command: {cmd}");
      eprintln!("usage: fink <tokens|ast|fmt|fmt2|cps|wat|wasm|compile|run|dap> [options] <file>");
      process::exit(1);
    }
  }
}

fn die(msg: &str) -> ! {
  eprintln!("error: {msg}");
  process::exit(1);
}



/// Remove the trailing `# sm:<b64>` or `;; sm:<b64>` line from `s`, so
/// byte offsets in the sourcemap align with the non-SM part of the
/// output.
fn strip_sm_line(s: &str) -> String {
  let mut lines: Vec<&str> = s.lines().collect();
  while let Some(last) = lines.last() {
    let t = last.trim_start();
    if t.starts_with("# sm:") || t.starts_with(";; sm:") || t.is_empty() {
      lines.pop();
    } else {
      break;
    }
  }
  lines.join("\n")
}

/// One-line preview of `text[start..end]`, quoted, max ~25 chars.
fn preview(text: &str, start: u32, end: u32) -> String {
  let s = start as usize;
  let e = (end as usize).min(text.len()).max(s);
  if s > text.len() { return format!("\"<bad range {start}..{end}>\""); }
  let slice = &text[s..e];
  let shortened: String = slice.chars().take(25).map(|c| if c == '\n' { '↵' } else { c }).collect();
  let suffix = if slice.chars().count() > 25 { "…" } else { "" };
  format!("\"{shortened}{suffix}\"")
}
