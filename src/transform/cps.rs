// CPS transform pass — not yet implemented

#[cfg(test)]
mod tests {
  use test_macros::test_template;
  use pretty_assertions::assert_eq;

  fn dedent(s: &str) -> String {
    s.lines()
      .map(|line| line.strip_prefix("    ").unwrap_or(line))
      .collect::<Vec<_>>()
      .join("\n")
  }

  fn cps_debug(src: &str) -> String {
    let _ = src;
    "not implemented".to_string()
  }

  #[test_template(
    "src/transform", "./test_cps.fnk",
    r"(?ms)^test '(?P<name>[^']+)', fn:\n  expect \S+ fn:\n(?P<src>[\s\S]+?)\n\n?  , equals fn:\n(?P<exp>[\s\S]+?)(?=\n\n\n|\n\n---|\n\ntest |\z)"
  )]
  fn test_cps(src: &str, exp: &str, path: &str) {
    assert_eq!(
      cps_debug(&dedent(src).trim().to_string()),
      dedent(exp).trim().to_string(),
      "{}",
      path
    );
  }
}
