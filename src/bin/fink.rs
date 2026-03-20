use std::{env, fs, process};

fn main() {
  let args: Vec<String> = env::args().collect();

  let sourcemap = args.iter().any(|a| a == "--sourcemap");
  let embed_source = args.iter().any(|a| a == "--embed-source");
  let positional: Vec<&str> = args.iter().skip(1).filter(|a| !a.starts_with("--")).map(|s| s.as_str()).collect();

  let (cmd, path) = match positional.as_slice() {
    [cmd, path] => (*cmd, *path),
    _ => {
      eprintln!("usage: fink <tokens|ast|fmt|cps|run|dap> [--sourcemap] <file>");
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
        Err(e) => parse_error(&src, e),
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
        Err(e) => parse_error(&src, e),
      }
    }
    "cps" => {
      match fink::parser::parse(&src) {
        Ok(r) => {
          let ast_index = fink::ast::build_index(&r);
          let cps = fink::passes::cps::transform::lower_expr(&r.root);
          let ctx = fink::passes::cps::fmt::Ctx {
            origin: &cps.origin,
            ast_index: &ast_index,
            captures: None,
          };
          println!("{}", fink::passes::cps::fmt::fmt_with(&cps.root, &ctx));
        }
        Err(e) => parse_error(&src, e),
      }
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
      eprintln!("usage: fink <tokens|ast|fmt|cps|run|dap> <file>");
      process::exit(1);
    }
  }
}

fn parse_error(src: &str, e: fink::parser::ParseError) -> ! {
  let diag = fink::errors::Diagnostic { message: e.message, loc: e.loc, hint: None };
  let opts = fink::errors::FormatOptions::default();
  eprintln!("{}", fink::errors::format_diagnostic(src, &diag, &opts));
  process::exit(1);
}
