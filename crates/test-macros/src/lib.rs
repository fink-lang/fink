extern crate proc_macro;

use std::{env, fs, path::Path};

use fancy_regex::Regex;
use glob::glob;
use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
  parse, parse_macro_input,
  punctuated::Punctuated,
  FnArg, ItemFn, LitStr, PatType, Stmt,
};

#[proc_macro_attribute]
pub fn test_template(attr: TokenStream, item: TokenStream) -> TokenStream {
  let attr_args: Vec<String> =
    parse_macro_input!(attr with Punctuated::<LitStr, syn::Token![,]>::parse_terminated)
      .iter()
      .map(|arg| arg.value())
      .collect();

  let [base_dir, incl_pattern, test_regex] = &attr_args[..] else {
    panic!("Expected three arguments: base_dir, source_pattern, test_regex");
  };

  let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
  let abs_base = Path::new(&manifest_dir).join(base_dir);
  let glob_pattern = abs_base.join(incl_pattern);

  let re = Regex::new(test_regex).expect("Invalid regex");

  let templ_fn = parse_macro_input!(item as ItemFn);
  let args = templ_fn.sig.inputs;
  let body = templ_fn.block.stmts;

  let mut tests: Vec<ItemFn> = vec![];
  let mut includes: proc_macro2::TokenStream = proc_macro2::TokenStream::new();

  for (idx, path) in glob(glob_pattern.to_str().unwrap())
    .expect("Failed to read glob pattern")
    .map(|r| r.expect("Glob error"))
    .enumerate()
  {
    let abs_path = path.to_str().unwrap().to_string();
    let inc_ident = format_ident!("s_{idx}");

    includes.extend(quote! {
      static #inc_ident: &str = include_str!(#abs_path);
    });

    let contents = fs::read_to_string(&path).expect("Could not read test file");

    for cap in re.captures_iter(&contents) {
      let grp = cap.unwrap();

      let raw_name = grp
        .name("name")
        .expect("Expected capture group 'name'")
        .as_str();
      let sanitized: String = raw_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
      let name = format!("t_{sanitized}");
      let test_name = format_ident!("{}", name);

      let arg_stmts: Vec<Stmt> = args
        .iter()
        .map(|arg: &FnArg| -> Stmt {
          let arg_name = if let FnArg::Typed(PatType { pat, .. }) = arg {
            format!("{}", quote! { #pat })
          } else {
            panic!("Expected typed function argument");
          };

          let stmt = match arg_name.as_str() {
            "path" => {
              let pos = grp.get(0).unwrap().start();
              let line = contents[..=pos].lines().count();
              let path_info = format!("{}:{}", path.display(), line);
              quote! { let #arg = #path_info; }
            }
            _ => {
              let cap = grp
                .name(&arg_name)
                .unwrap_or_else(|| panic!("Expected capture group '{arg_name}'"));
              let start = cap.start();
              let end = cap.end();
              quote! { let #arg = &#inc_ident[#start..#end]; }
            }
          };

          parse(stmt.into()).unwrap()
        })
        .collect();

      let test_fn: TokenStream = quote! {
        #[test]
        fn #test_name() {
          #(#arg_stmts)*
          #(#body)*
        }
      }
      .into();

      tests.push(parse_macro_input!(test_fn as ItemFn));
    }
  }

  quote! {
    #includes
    #(#tests)*
  }
  .into()
}

/// Extracted test case from a `.fnk` test file.
/// `src` and `exp` are already dedented, trimmed strings — ready to embed as literals.
struct FinkTest {
  name: String,
  func: String,
  src:  String,
  exp:  String,
  line: u32,
}

/// Walk the parsed AST of a `.fnk` test file and extract all test cases.
///
/// Each test has the shape:
///   test 'name', fn:
///     expect <func> [text] fn: <src-body>
///   | equals[_fink] [text] fn: <exp-body>
///
/// `text fn:` and bare `fn:` are treated identically — the body is extracted
/// as raw source text via Loc, then dedented. `text` is a hint for humans only.
fn extract_tests<'src>(file_src: &'src str, node: &fink::ast::Node<'src>) -> Vec<FinkTest> {
  use fink::ast::NodeKind;

  let stmts = match &node.kind {
    NodeKind::Fn { body, .. } => body,
    _ => return vec![],
  };

  let mut out = Vec::new();

  for stmt in stmts {
    // test 'name', fn: <body>
    let (name, fn_body) = match &stmt.kind {
      NodeKind::Apply { func, args } if matches!(func.kind, NodeKind::Ident("test")) => {
        let Some(name_node) = args.first() else { continue };
        let NodeKind::LitStr(name) = &name_node.kind else { continue };
        let Some(fn_node) = args.get(1) else { continue };
        let NodeKind::Fn { body, .. } = &fn_node.kind else { continue };
        (name.clone(), body)
      }
      _ => continue,
    };

    // fn body is a single Pipe node: [expect_call, equals_call]
    let pipe_nodes: &[fink::ast::Node<'src>] = match fn_body.as_slice() {
      [single] => match &single.kind {
        NodeKind::Pipe(parts) => parts,
        _ => continue,
      },
      _ => continue,
    };
    if pipe_nodes.len() < 2 { continue; }

    // expect <func> [text] fn: <src>
    // Parsed as: Apply(expect, [Apply(func, [body_node])])  (right-to-left application)
    let (func_name, src_text) = match &pipe_nodes[0].kind {
      NodeKind::Apply { func, args } if matches!(func.kind, NodeKind::Ident("expect")) => {
        let Some(inner) = args.first() else { continue };
        // inner is Apply(func_name, [body_node])
        let NodeKind::Apply { func: fn_ident, args: fn_args } = &inner.kind else { continue };
        let NodeKind::Ident(func_name) = &fn_ident.kind else { continue };
        let Some(body_node) = fn_args.first() else { continue };
        let text = match &body_node.kind {
          NodeKind::LitStr(s) => {
            // TODO: the AST doesn't distinguish 'quoted' LitStr from ":" block LitStr.
            // Peek at the byte just before the node's loc start in the source —
            // if it's `'` this came from a quoted string and needs unescape;
            // otherwise it came from a ":" block and must be used verbatim.
            let start = body_node.loc.start.idx as usize;
            let preceded_by_quote = file_src.as_bytes().get(start) == Some(&b'\'');
            if preceded_by_quote {
              fink::strings::render(s)
            } else {
              s.to_string()
            }
          }
          _ => {
            if let Some(text) = extract_raw_templ(body_node) {
              // raw": src: process escapes so e.g. \$ → $ before passing to tokenize
              fink::strings::render(&text)
            } else {
              let Some(text) = extract_fn_body_text(body_node, file_src) else { continue };
              text
            }
          }
        };
        (*func_name, text)
      }
      _ => continue,
    };

    // equals[_fink] [text] fn: <exp>   OR   equals '...'
    let exp_text = match &pipe_nodes.last().unwrap().kind {
      NodeKind::Apply { func, args }
        if matches!(&func.kind, NodeKind::Ident(s) if s.starts_with("equals")) =>
      {
        let Some(body_node) = args.first() else { continue };
        // Accept a string literal, raw": tagged template, or a fn/text fn body.
        match &body_node.kind {
          NodeKind::LitStr(s) => s.clone(),
          _ => {
            if let Some(text) = extract_raw_templ(body_node) {
              text
            } else {
              let Some(text) = extract_fn_body_text(body_node, file_src) else { continue };
              text
            }
          }
        }
      }
      _ => continue,
    };

    out.push(FinkTest {
      name,
      func: func_name.to_string(),
      src:  src_text,
      exp:  exp_text,
      line: stmt.loc.start.line,
    });
  }

  out
}

/// Extract the verbatim string content from a `raw":\n  ...` tagged template node.
///
/// Matches `Apply { func: Ident("raw"), args: [LitStr(s) | StrRawTempl([LitStr(s)])] }` and
/// returns `s` verbatim — no unescaping, no trimming. This is the `raw":` form used in tests.
fn extract_raw_templ<'src>(node: &fink::ast::Node<'src>) -> Option<String> {
  use fink::ast::NodeKind;
  let NodeKind::Apply { func, args } = &node.kind else { return None };
  if !matches!(func.kind, NodeKind::Ident("raw")) { return None; }
  let arg = args.first()?;
  match &arg.kind {
    // No interpolation: raw": collapses to Apply(raw, LitStr) — verbatim, no unescape.
    // TODO: trailing \n comes from consume_str_block_text including \n in each line's end pos.
    // The lexer should strip the trailing newline from the last line of a block string instead
    // of having the macro paper over it here. Fix in lexer, then remove this trim.
    NodeKind::LitStr(s) => Some(s.trim_end_matches('\n').to_string()),
    // With interpolation: Apply(raw, StrRawTempl([LitStr, ...])) — only plain text supported in tests.
    NodeKind::StrRawTempl(children) => {
      if let [child] = children.as_slice() {
        if let NodeKind::LitStr(s) = &child.kind {
          return Some(s.trim_end_matches('\n').to_string());
        }
      }
      None
    }
    _ => None,
  }
}

/// Extract and dedent the body text of a `fn:` or `text fn:` node.
///
/// Accepts either:
///   - `Fn { body }` — bare `fn: <body>`
///   - `Apply { func: Ident("text"), args: [Fn { body }] }` — `text fn: <body>`
///
/// Returns the raw source text of the body, dedented and trimmed.
fn extract_fn_body_text<'src>(
  node: &fink::ast::Node<'src>,
  file_src: &'src str,
) -> Option<String> {
  use fink::ast::NodeKind;

  // Unwrap optional `text` wrapper.
  let fn_node = match &node.kind {
    NodeKind::Fn { .. } => node,
    NodeKind::Apply { func, args }
      if matches!(func.kind, NodeKind::Ident("text")) =>
    {
      args.first()?
    }
    _ => return None,
  };

  if !matches!(fn_node.kind, NodeKind::Fn { .. }) { return None; }

  // Use the Fn node's loc directly — body content begins after `fn:\n` (+4).
  // End is whatever the Fn node recorded. When the LitStr loc bug is fixed
  // this will automatically include closing delimiters.
  let body_start = fn_node.loc.start.idx as usize + 4;
  let body_end   = fn_node.loc.end.idx as usize;
  if body_start >= body_end { return None; }
  let raw = &file_src[body_start..body_end];
  Some(dedent_str(raw).trim().to_string())
}

/// Strip a common leading-whitespace prefix from every line.
fn dedent_str(s: &str) -> String {
  let indent = s.lines()
    .filter(|l| !l.trim().is_empty())
    .map(|l| l.len() - l.trim_start().len())
    .min()
    .unwrap_or(0);
  s.lines()
    .map(|l| if l.len() >= indent { &l[indent..] } else { l })
    .collect::<Vec<_>>()
    .join("\n")
}

#[proc_macro]
pub fn include_fink_tests(input: TokenStream) -> TokenStream {
  let path_lit = parse_macro_input!(input as LitStr);
  let rel_path = path_lit.value();

  let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
  let abs_path = Path::new(&manifest_dir).join(&rel_path);
  let abs_path_str = abs_path.to_str().unwrap().to_string();

  let src = fs::read_to_string(&abs_path)
    .unwrap_or_else(|_| panic!("include_fink_tests: cannot read {abs_path_str}"));

  let node = fink::parser::parse(&src)
    .unwrap_or_else(|e| panic!("include_fink_tests: parse error in {abs_path_str}: {}", e.message));

  let tests = extract_tests(&src, &node);

  let mut output = proc_macro2::TokenStream::new();

  for test in tests {
    let test_name = {
      let sanitized: String = test.name.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
      format_ident!("t_{sanitized}")
    };
    let func      = format_ident!("{}", test.func);
    let src_lit   = proc_macro2::Literal::byte_string(test.src.as_bytes());
    let exp_lit   = proc_macro2::Literal::byte_string(test.exp.as_bytes());
    let path_info = format!("{}:{}", rel_path, test.line);

    output.extend(quote! {
      #[test]
      fn #test_name() {
        pretty_assertions::assert_eq!(
          #func(std::str::from_utf8(#src_lit).unwrap()),
          std::str::from_utf8(#exp_lit).unwrap(),
          "{}",
          #path_info
        );
      }
    });
  }

  output.into()
}
