// Test support — public so the `include_fink_tests!` proc macro can emit
// calls into it. Not gated by `#[cfg(test)]` because the macro-generated
// test code lives in downstream `#[cfg(test)]` modules.

/// Assert two strings are equal; on mismatch panic with a chat-friendly
/// line-based unified diff instead of the multi-line coloured block
/// `pretty_assertions` produces (which renders poorly in narrow terminals
/// and chat).
///
/// `path_info` is prepended to the failure message — typically
/// `"file.fnk:line"`.
pub fn assert_eq_str(actual: impl AsRef<str>, expected: impl AsRef<str>, path_info: &str) {
  let actual = actual.as_ref();
  let expected = expected.as_ref();
  if actual == expected {
    return;
  }
  let diff = line_diff(expected, actual);
  panic!(
    "assertion failed: strings differ\n  at {path_info}\n\n{diff}"
  );
}

/// Render a unified-style line diff.
///
/// Uses the classic Myers longest-common-subsequence shape but keeps the
/// implementation small: quadratic in line count is fine for test
/// outputs (hundreds of lines at most). Output is plain ASCII, one line
/// per diff row, `-` for expected-only, `+` for actual-only, ` ` for
/// context.
fn line_diff(expected: &str, actual: &str) -> String {
  let exp: Vec<&str> = expected.lines().collect();
  let act: Vec<&str> = actual.lines().collect();

  let n = exp.len();
  let m = act.len();

  // LCS table.
  let mut lcs = vec![vec![0u32; m + 1]; n + 1];
  for i in 0..n {
    for j in 0..m {
      lcs[i + 1][j + 1] = if exp[i] == act[j] {
        lcs[i][j] + 1
      } else {
        lcs[i + 1][j].max(lcs[i][j + 1])
      };
    }
  }

  // Walk back to produce an edit script.
  enum Op<'a> {
    Keep(&'a str),
    Del(&'a str),
    Add(&'a str),
  }
  let mut ops: Vec<Op> = Vec::new();
  let (mut i, mut j) = (n, m);
  while i > 0 || j > 0 {
    if i > 0 && j > 0 && exp[i - 1] == act[j - 1] {
      ops.push(Op::Keep(exp[i - 1]));
      i -= 1;
      j -= 1;
    } else if j > 0 && (i == 0 || lcs[i][j - 1] >= lcs[i - 1][j]) {
      ops.push(Op::Add(act[j - 1]));
      j -= 1;
    } else {
      ops.push(Op::Del(exp[i - 1]));
      i -= 1;
    }
  }
  ops.reverse();

  let mut out = String::new();
  out.push_str("--- expected\n+++ actual\n");
  for op in ops {
    match op {
      Op::Keep(l) => {
        out.push(' ');
        out.push_str(l);
        out.push('\n');
      }
      Op::Del(l) => {
        out.push('-');
        out.push_str(l);
        out.push('\n');
      }
      Op::Add(l) => {
        out.push('+');
        out.push_str(l);
        out.push('\n');
      }
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn equal_strings_do_not_panic() {
    assert_eq_str("abc", "abc", "x:1");
  }

  #[test]
  #[should_panic(expected = "strings differ")]
  fn unequal_strings_panic() {
    assert_eq_str("abc", "abd", "x:1");
  }

  #[test]
  fn diff_shape_single_line_change() {
    let d = line_diff("one\ntwo\nthree", "one\nTWO\nthree");
    assert!(d.contains("-two"));
    assert!(d.contains("+TWO"));
    assert!(d.contains(" one"));
    assert!(d.contains(" three"));
  }

  #[test]
  fn diff_shape_insertion() {
    let d = line_diff("a\nc", "a\nb\nc");
    assert!(d.contains(" a"));
    assert!(d.contains("+b"));
    assert!(d.contains(" c"));
  }

  #[test]
  fn diff_shape_deletion() {
    let d = line_diff("a\nb\nc", "a\nc");
    assert!(d.contains(" a"));
    assert!(d.contains("-b"));
    assert!(d.contains(" c"));
  }
}
