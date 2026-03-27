extern crate proc_macro;

use std::{env, fs, path::Path};

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, LitStr};

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
    NodeKind::Module(exprs) => exprs,
    NodeKind::Fn { body, .. } => body,
    _ => return vec![],
  };

  let mut out = Vec::new();

  for stmt in &stmts.items {
    // test 'name', fn: <body>
    let (name, fn_body) = match &stmt.kind {
      NodeKind::Apply { func, args } if matches!(func.kind, NodeKind::Ident("test")) => {
        let Some(name_node) = args.items.first() else {
          panic!("include_fink_tests: `test` at line {} has no name argument", stmt.loc.start.line);
        };
        let NodeKind::LitStr { content: name, .. } = &name_node.kind else {
          panic!("include_fink_tests: `test` at line {} — name is not a string literal", stmt.loc.start.line);
        };
        let Some(fn_node) = args.items.get(1) else {
          panic!("include_fink_tests: test '{}' at line {} has no fn body", name, stmt.loc.start.line);
        };
        let NodeKind::Fn { body, .. } = &fn_node.kind else {
          panic!("include_fink_tests: test '{}' at line {} — second arg is not `fn:`", name, stmt.loc.start.line);
        };
        (name.clone(), body)
      }
      _ => continue,
    };

    // fn body is a single Pipe node: [expect_call, equals_call]
    let pipe_nodes: &[fink::ast::Node<'src>] = match fn_body.items.as_slice() {
      [single] => match &single.kind {
        NodeKind::Pipe(parts) => &parts.items,
        _ => panic!(
          "include_fink_tests: test '{}' at line {} — fn body is not a pipe expression \
           (did you leave a blank line before `| equals`?)",
          name, stmt.loc.start.line
        ),
      },
      _ => panic!(
        "include_fink_tests: test '{}' at line {} — fn body must have exactly one expression",
        name, stmt.loc.start.line
      ),
    };
    if pipe_nodes.len() < 2 {
      panic!(
        "include_fink_tests: test '{}' at line {} — pipe must have at least two segments (expect | equals)",
        name, stmt.loc.start.line
      );
    }

    // expect <func> [text] fn: <src>
    // Parsed as: Apply(expect, [Apply(func, [body_node])])  (right-to-left application)
    let (func_name, src_text) = match &pipe_nodes[0].kind {
      NodeKind::Apply { func, args } if matches!(func.kind, NodeKind::Ident("expect")) => {
        let Some(inner) = args.items.first() else {
          panic!("include_fink_tests: test '{}' at line {} — `expect` has no arguments", name, stmt.loc.start.line);
        };
        // inner is Apply(func_name, [body_node])
        let NodeKind::Apply { func: fn_ident, args: fn_args } = &inner.kind else {
          panic!("include_fink_tests: test '{}' at line {} — expect argument is not a function call", name, stmt.loc.start.line);
        };
        let NodeKind::Ident(func_name) = &fn_ident.kind else {
          panic!("include_fink_tests: test '{}' at line {} — expect function name is not an identifier", name, stmt.loc.start.line);
        };
        let Some(body_node) = fn_args.items.first() else {
          panic!("include_fink_tests: test '{}' at line {} — `expect {}` has no source body", name, stmt.loc.start.line, func_name);
        };
        let text = match &body_node.kind {
          NodeKind::LitStr { content: s, .. } => {
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
              // Unescape \${ → ${ so interpolation syntax can appear in fink": src inputs
              // without triggering the parser's own interpolation. Everything else verbatim.
              text.replace("\\${", "${")
            } else {
              extract_fn_body_text(body_node, file_src).unwrap_or_else(|| {
                panic!("include_fink_tests: test '{}' at line {} — cannot extract source body from `expect {}`", name, stmt.loc.start.line, func_name)
              })
            }
          }
        };
        (*func_name, text)
      }
      _ => panic!(
        "include_fink_tests: test '{}' at line {} — first pipe segment is not `expect <func> fink\":`",
        name, stmt.loc.start.line
      ),
    };

    // equals[_fink] [text] fn: <exp>   OR   equals '...'   OR   equals fink": ...
    let equals_node = pipe_nodes.last().unwrap();
    let exp_text = match &equals_node.kind {
      NodeKind::Apply { func, args }
        if matches!(&func.kind, NodeKind::Ident(s) if s.starts_with("equals")) =>
      {
        let Some(body_node) = args.items.first() else {
          panic!("include_fink_tests: test '{}' at line {} — `equals` has no expected body", name, stmt.loc.start.line);
        };
        // Accept a string literal, raw": tagged template, or a fn/text fn body.
        let text = match &body_node.kind {
          NodeKind::LitStr { content: s, .. } => s.clone(),
          _ => {
            if let Some(text) = extract_raw_templ(body_node) {
              text
            } else {
              extract_fn_body_text(body_node, file_src).unwrap_or_else(|| {
                panic!("include_fink_tests: test '{}' at line {} — cannot extract expected body from `equals`", name, stmt.loc.start.line)
              })
            }
          }
        };
        text
      }
      _ => panic!(
        "include_fink_tests: test '{}' at line {} — last pipe segment is not `equals fink\":`",
        name, stmt.loc.start.line
      ),
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

/// Extract the verbatim string content from a tagged template node (e.g. `fink":`, `wat":`, `wat''`).
///
/// Matches `Apply { func: Ident(_), args: [LitStr { content: s } | StrRawTempl { children: [LitStr { content: s }] }] }` and
/// returns `s` verbatim — no unescaping, no trimming. Any tag name is accepted.
fn extract_raw_templ<'src>(node: &fink::ast::Node<'src>) -> Option<String> {
  use fink::ast::NodeKind;
  let NodeKind::Apply { func, args } = &node.kind else { return None };
  if !matches!(func.kind, NodeKind::Ident(_)) { return None; }
  let arg = args.items.first()?;
  match &arg.kind {
    // No interpolation: raw": collapses to Apply(raw, LitStr) — verbatim, no unescape.
    NodeKind::LitStr { content: s, .. } => Some(s.to_string()),
    // With interpolation: Apply(raw, StrRawTempl([LitStr, ...])).
    // A single plain-text child is fine (e.g. fink": with no ${}).
    // Multiple children means the fink": block contains an unescaped ${...} — use \${ instead.
    NodeKind::StrRawTempl { children, .. } => {
      if let [child] = children.as_slice() {
        if let NodeKind::LitStr { content: s, .. } = &child.kind {
          return Some(s.to_string());
        }
      }
      panic!(
        "include_fink_tests: tagged template block contains interpolation — \
         use \\${{}} to escape '${{' in test source inputs"
      );
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
      args.items.first()?
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

  let result = fink::parser::parse(&src)
    .unwrap_or_else(|e| {
      let diag = fink::errors::Diagnostic {
        message: e.message.clone(),
        loc: e.loc,
        hint: None,
      };
      let opts = fink::errors::FormatOptions {
        lines_before: 1,
        lines_after: 0,
        path: Some(&rel_path),
      };
      let pretty = fink::errors::format_diagnostic(&src, &diag, &opts);
      panic!("include_fink_tests: parse error\n\n{pretty}\n");
    });

  let tests = extract_tests(&src, &result.root);

  let mut output = proc_macro2::TokenStream::new();

  // Emit include_str! so cargo tracks the .fnk file and recompiles when it changes.
  output.extend(quote! {
    const _: &str = include_str!(#abs_path_str);
  });

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
    let test_name_str = &test.name;

    output.extend(quote! {
      #[test]
      fn #test_name() {
        crate::test_context::set(#test_name_str, #abs_path_str);
        let actual = #func(std::str::from_utf8(#src_lit).unwrap());
        let expected = std::str::from_utf8(#exp_lit).unwrap();
        if std::env::var("BLESS").is_ok() && actual != expected {
          // Find the test by name, locate `| equals` line, replace the body below it.
          let file = std::fs::read_to_string(#abs_path_str).unwrap();
          let test_marker = format!("test '{}',", #test_name_str);
          if let Some(test_pos) = file.find(&test_marker) {
            let after_test = &file[test_pos..];
            // Find "| equals" line after the test marker.
            if let Some(equals_offset) = after_test.find("| equals") {
              let equals_pos = test_pos + equals_offset;
              // Find the end of the "| equals ..." line.
              let after_equals = &file[equals_pos..];
              let line_end = after_equals.find('\n').map(|i| equals_pos + i + 1).unwrap_or(file.len());
              // Find where the body ends: the indented block below `| equals ...:`.
              // Body lines start with 4 spaces. Stop at the first non-indented line
              // (blank or otherwise).
              let body_end = {
                let mut pos = line_end;
                for line in file[line_end..].lines() {
                  if !line.starts_with("    ") { break; }
                  pos += line.len() + 1; // +1 for newline
                }
                pos.min(file.len())
              };
              // Build indented replacement.
              let indented: String = actual.lines()
                .map(|l| if l.is_empty() { String::new() } else { format!("    {l}") })
                .collect::<Vec<_>>()
                .join("\n");
              let new_file = format!("{}{}\n{}", &file[..line_end], indented, &file[body_end..]);
              std::fs::write(#abs_path_str, new_file).unwrap();
              eprintln!("BLESS: updated {}", #path_info);
            }
          }
        }
        pretty_assertions::assert_eq!(
          actual,
          expected,
          "{}",
          #path_info
        );
      }
    });
  }

  output.into()
}
