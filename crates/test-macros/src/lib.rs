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
