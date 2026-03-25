use std::{env, fs, process};

fn main() {
  let args: Vec<String> = env::args().collect();

  let sourcemap = args.iter().any(|a| a == "--sourcemap");
  let embed_source = args.iter().any(|a| a == "--embed-source");
  let pass: Option<u32> = args.iter().find_map(|a| a.strip_prefix("--pass=").and_then(|v| v.parse().ok()));
  let positional: Vec<&str> = args.iter().skip(1).filter(|a| !a.starts_with("--")).map(|s| s.as_str()).collect();

  let (cmd, path) = match positional.as_slice() {
    [cmd, path] => (*cmd, *path),
    _ => {
      eprintln!("usage: fink <tokens|ast|fmt|fmt2|cps|wat|run|dap> [--sourcemap] [--embed-source] [--pass=N] <file>");
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
        Ok(r) => println!("{}", r.root.print()),
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
          let cps = fink::passes::cps::transform::lower_expr(&r.root);

          // --pass=N selects pipeline stage:
          //   0 (default): raw CPS after lower_expr
          //   1: after lifting (fully lifted)
          let result = match pass.unwrap_or(0) {
            1 => fink::passes::lifting::lift(cps, &ast_index),
            _ => cps,
          };

          let ctx = fink::passes::cps::fmt::Ctx {
            origin: &result.origin,
            ast_index: &ast_index,
            captures: None,
          };
          if sourcemap {
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
            println!("{}", fink::passes::cps::fmt::fmt_with(&result.root, &ctx));
          }
        }
        Err(e) => parse_error(&src, e, path),
      }
    }
    "wat" => {
      let result = fink::runner::compile_fnk(&src).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        process::exit(1);
      });
      let wat = wasmprinter::print_bytes(&result.wasm).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        process::exit(1);
      });
      println!("{wat}");
    }
    "run" => {
      if let Err(e) = fink::runner::run_file(Default::default(), path) {
        eprintln!("error: {e}");
        process::exit(1);
      }
    }
    "dap" => {
      if let Err(e) = fink::dap::run(std::io::stdin(), std::io::stdout(), path) {
        eprintln!("error: {e}");
        process::exit(1);
      }
    }
    _ => {
      eprintln!("unknown command: {cmd}");
      eprintln!("usage: fink <tokens|ast|fmt|cps|wat|run|dap> <file>");
      process::exit(1);
    }
  }
}

fn parse_error(src: &str, e: fink::parser::ParseError, path: &str) -> ! {
  let diag = fink::errors::Diagnostic { message: e.message, loc: e.loc, hint: None };
  let opts = fink::errors::FormatOptions { path: Some(path), ..Default::default() };
  eprintln!("{}", fink::errors::format_diagnostic(src, &diag, &opts));
  process::exit(1);
}
