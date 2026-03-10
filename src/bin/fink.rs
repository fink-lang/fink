use std::{env, fs, process};

fn main() {
  let args: Vec<String> = env::args().collect();

  let (switch, path) = match args.as_slice() {
    [_, path] => (None, path),
    [_, switch, path] if switch.starts_with('-') => (Some(switch.as_str()), path),
    _ => {
      eprintln!("usage: fink [-tokens|-ast] <file>");
      process::exit(1);
    }
  };

  let src = fs::read_to_string(path).unwrap_or_else(|e| {
    eprintln!("error: {path}: {e}");
    process::exit(1);
  });

  match switch {
    Some("-tokens") => {
      println!("{}", fink::lexer::tokenize_debug(&src));
    }
    Some("-ast") => {
      match fink::parser::parse(&src) {
        Ok(node) => println!("{}", node.print()),
        Err(e) => parse_error(&src, e),
      }
    }
    None => {
      match fink::parser::parse(&src) {
        Ok(node) => print!("{}", fink::ast::fmt::fmt(&node)),
        Err(e) => parse_error(&src, e),
      }
    }
    Some(s) => {
      eprintln!("unknown switch: {s}");
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
