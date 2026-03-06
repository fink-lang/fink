// CPS transform pass

use crate::ast::{Node, NodeKind};
use crate::parser::parse;

// ---------------------------------------------------------------------------
// CPS IR types
// ---------------------------------------------------------------------------

/// A value that appears in argument position — literals, identifiers, tagged ids/ops,
/// inline continuations, or spread of an ident.
#[derive(Debug, Clone, PartialEq)]
pub enum CpsVal<'src> {
  /// Boolean literal: true / false
  Bool(bool),
  /// Integer literal (raw source slice)
  Int(&'src str),
  /// Float literal
  Float(&'src str),
  /// Decimal literal
  Decimal(&'src str),
  /// String literal (processed value — owned since escape-processed)
  Str(String),
  /// Tagged raw-string: str_raw'...'
  StrRaw(&'src str),
  /// Empty sequence literal: []
  EmptySeq,
  /// Empty record literal: {}
  EmptyRec,
  /// User variable or primitive ident: ·foo, env, state, ƒ_cont, etc.
  Ident(&'src str),
  /// Compile-time identifier tag: id'foo'
  Id(&'src str),
  /// Compile-time operator tag: op'+'
  Op(&'src str),
  /// Spread of an ident: ..·rest (in apply args)
  Spread(&'src str),
  /// Inline continuation passed as argument: fn params: body
  Fn(CpsFn<'src>),
  /// Wildcard (ignored binding): _
  Wildcard,
}

/// A continuation function node used both as values and in closure/module bodies.
#[derive(Debug, Clone, PartialEq)]
pub struct CpsFn<'src> {
  /// Parameter names in order (may include spread as the last entry).
  pub params: Vec<CpsParam<'src>>,
  /// The body — a single chained CPS expression.
  pub body: Box<CpsExpr<'src>>,
}

/// A parameter in a `fn` — plain name or spread.
#[derive(Debug, Clone, PartialEq)]
pub enum CpsParam<'src> {
  /// Plain binding: ·name, env, state, ƒ_cont, etc.
  Ident(&'src str),
  /// Spread/varargs: ..·rest, ..vs
  Spread(&'src str),
  /// Wildcard: _
  Wildcard,
}

/// A CPS expression — always a primitive call or a terminal.
#[derive(Debug, Clone, PartialEq)]
pub enum CpsExpr<'src> {
  // ---- environment primitives ----

  /// store env, id'name', val, fn ·name, env: body
  Store {
    env: &'src str,
    key: &'src str,
    val: Box<CpsVal<'src>>,
    cont: CpsFn<'src>,
  },

  /// load env, id'name' | op'op', fn ·name, env: body
  Load {
    env: &'src str,
    key: CpsKey<'src>,
    cont: CpsFn<'src>,
  },

  // ---- application ----

  /// apply func, arg…, state, ƒ_cont
  Apply {
    func: Box<CpsVal<'src>>,
    args: Vec<CpsVal<'src>>,
    state: &'src str,
    cont: Box<CpsVal<'src>>,
  },

  // ---- closure / module ----

  /// closure env, fn params: body, fn ·fn_val, chld_env: cont_body
  Closure {
    env: &'src str,
    func: CpsFn<'src>,
    cont: CpsFn<'src>,
  },

  /// module fn {imports…}, env, state, ƒ_cont: body
  Module {
    imports: Vec<&'src str>,
    env: &'src str,
    state: &'src str,
    cont: &'src str,
    body: Box<CpsExpr<'src>>,
  },

  // ---- scope ----

  /// scope env, fn env, ƒ_ok: body, fn result, state: ƒ_cont_body
  Scope {
    env: &'src str,
    inner: CpsFn<'src>,
    cont: CpsFn<'src>,
  },

  // ---- sequence construction ----

  /// seq_append seq, val, state, fn $seq, state: body
  SeqAppend {
    seq: Box<CpsVal<'src>>,
    val: Box<CpsVal<'src>>,
    state: &'src str,
    cont: CpsFn<'src>,
  },

  /// seq_concat seq, other, state, fn $seq, state: body
  SeqConcat {
    seq: Box<CpsVal<'src>>,
    other: Box<CpsVal<'src>>,
    state: &'src str,
    cont: CpsFn<'src>,
  },

  // ---- record construction ----

  /// rec_put rec, id'key', val, state, fn $rec, state: body
  RecPut {
    rec: Box<CpsVal<'src>>,
    key: &'src str,
    val: Box<CpsVal<'src>>,
    state: &'src str,
    cont: CpsFn<'src>,
  },

  /// rec_merge rec, other, state, fn $rec, state: body
  RecMerge {
    rec: Box<CpsVal<'src>>,
    other: Box<CpsVal<'src>>,
    state: &'src str,
    cont: CpsFn<'src>,
  },

  // ---- range ----

  /// range_excl start, end, state, fn $range, state: body
  RangeExcl {
    start: Box<CpsVal<'src>>,
    end: Box<CpsVal<'src>>,
    state: &'src str,
    cont: CpsFn<'src>,
  },

  /// range_incl start, end, state, fn $range, state: body
  RangeIncl {
    start: Box<CpsVal<'src>>,
    end: Box<CpsVal<'src>>,
    state: &'src str,
    cont: CpsFn<'src>,
  },

  // ---- error handling ----

  /// err res, state, fn e, state: err_body, fn val, state: ok_body
  Err {
    res: Box<CpsVal<'src>>,
    state: &'src str,
    err_cont: CpsFn<'src>,
    ok_cont: CpsFn<'src>,
  },

  // ---- control flow ----

  /// if cond, fn state: then_body, fn state: else_body
  If {
    cond: Box<CpsVal<'src>>,
    then_cont: CpsFn<'src>,
    else_cont: CpsFn<'src>,
  },

  /// panic message, state
  Panic {
    message: Box<CpsVal<'src>>,
    state: &'src str,
  },

  // ---- pattern matching ----

  /// match_bind val, state, fn_arm, fn_fail, fn_cont
  MatchBind {
    val: Box<CpsVal<'src>>,
    state: &'src str,
    arm: CpsFn<'src>,
    fail: CpsFn<'src>,
    cont: CpsFn<'src>,
  },

  /// match_block val…, state, match_branch…, fn_fail, fn_cont
  MatchBlock {
    vals: Vec<CpsVal<'src>>,
    state: &'src str,
    branches: Vec<CpsExpr<'src>>,
    fail: CpsFn<'src>,
    cont: CpsFn<'src>,
  },

  /// match_branch env, fn v, env, state, ƒ_err, ƒ_ok: body
  MatchBranch {
    env: &'src str,
    arm: CpsFn<'src>,
  },

  /// matcher val, 'PatternKind', state, fn m, ƒ_err, state: body, fn state: fail_body
  Matcher {
    val: Box<CpsVal<'src>>,
    kind: &'src str,
    state: &'src str,
    cont: CpsFn<'src>,
    fail: CpsFn<'src>,
  },

  /// match_pop_at m, index, state, fn m, val, ƒ_err, state: body, fn m, state: fail_body
  MatchPopAt {
    matcher: Box<CpsVal<'src>>,
    index: usize,
    state: &'src str,
    cont: CpsFn<'src>,
    fail: CpsFn<'src>,
  },

  /// match_pop_field m, id'key', state, fn m, val, state: body, fn m, state: fail_body
  MatchPopField {
    matcher: Box<CpsVal<'src>>,
    key: &'src str,
    state: &'src str,
    cont: CpsFn<'src>,
    fail: CpsFn<'src>,
  },

  /// match_done m, state, fn m, state: non_empty_body, fn m, state: empty_body
  MatchDone {
    matcher: Box<CpsVal<'src>>,
    state: &'src str,
    non_empty: CpsFn<'src>,
    empty: CpsFn<'src>,
  },

  /// match_rest m, state, fn m, rest, state: body
  MatchRest {
    matcher: Box<CpsVal<'src>>,
    state: &'src str,
    cont: CpsFn<'src>,
  },

  /// match_len m, n, state, fn m, state: ok_body, fn m, state: fail_body
  MatchLen {
    matcher: Box<CpsVal<'src>>,
    len: usize,
    state: &'src str,
    ok: CpsFn<'src>,
    fail: CpsFn<'src>,
  },

  // ---- terminal ----

  /// Tail call to a continuation: ƒ_cont arg…
  TailCall {
    cont: Box<CpsVal<'src>>,
    args: Vec<CpsVal<'src>>,
  },
}

/// The key used in a `load` call — either an ident name (`id'foo'`) or an op (`op'+'`).
#[derive(Debug, Clone, PartialEq)]
pub enum CpsKey<'src> {
  Id(&'src str),
  Op(&'src str),
}

// ---------------------------------------------------------------------------
// Compiler — owns the generated-name arena
// ---------------------------------------------------------------------------

pub struct Cps {
  /// Interned compiler-generated names; slices into this vec are returned as &'static str.
  /// Only used for debug/test output via cps_fmt — leaking small strings is acceptable here.
  generated: Vec<String>,
}

impl Cps {
  pub fn new() -> Self {
    Self { generated: Vec::new() }
  }

  /// Allocate a name in the arena and leak it so it can be used as &'static str.
  pub fn alloc(&mut self, name: String) -> &'static str {
    self.generated.push(name);
    Box::leak(self.generated.last().unwrap().clone().into_boxed_str())
  }

  pub fn fresh(&mut self, prefix: &str) -> &'static str {
    let name = format!("{}{}", prefix, self.generated.len());
    self.alloc(name)
  }
}

// ---------------------------------------------------------------------------
// Transform
// ---------------------------------------------------------------------------

/// Map an op symbol to a readable local variable name.
fn op_local(op: &str) -> String {
  let suffix = match op {
    "+" => "plus",
    "-" => "neg",
    "*" => "mul",
    "/" => "div",
    "%" => "rem",
    "==" => "eq",
    "!=" => "neq",
    "<" => "lt",
    "<=" => "lte",
    ">" => "gt",
    ">=" => "gte",
    "." => "dot",
    _ => op,
  };
  format!("op_{}", suffix)
}

/// Whether an op string is a word (loaded via id'') vs a symbol (loaded via op'').
fn is_word_op(op: &str) -> bool {
  op.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

pub fn transform<'src>(src: &'src str) -> Result<CpsExpr<'src>, String> {
  let node = parse(src).map_err(|e| e.message)?;
  let mut cps = Cps::new();
  let tail = CpsExpr::TailCall {
    cont: Box::new(CpsVal::Ident("ƒ_cont")),
    args: vec![CpsVal::Ident("v_result"), CpsVal::Ident("state")],
  };
  Ok(cps.expr_cps(&node, tail))
}

impl Cps {
  /// Transform `node`, placing `cont_expr` as the innermost continuation body.
  /// `cont_expr` will be substituted where the result variable is needed.
  /// For now we use a simple hole-passing style: `k` is the CpsExpr that
  /// consumes the result, and the result value is implicit in the context.
  fn expr_cps<'src>(&mut self, node: &Node<'src>, k: CpsExpr<'src>) -> CpsExpr<'src>
  where 'src: 'src
  {
    match &node.kind {
      // Atoms — just pass value directly to k via TailCall substitution.
      // For unary ops we need to thread through load → apply → k.
      NodeKind::UnaryOp { op, operand } => {
        let local: &'static str = self.alloc(op_local(op));
        let key = if is_word_op(op) {
          CpsKey::Id(op)
        } else {
          CpsKey::Op(op)
        };
        // Load the operand, then apply the op.
        self.load_node(operand, |operand_val| {
          CpsExpr::Apply {
            func: Box::new(CpsVal::Ident(local)),
            args: vec![operand_val],
            state: "state",
            cont: Box::new(CpsVal::Ident("ƒ_cont")),
          }
        }, |body| {
          CpsExpr::Load {
            env: "env",
            key,
            cont: CpsFn {
              params: vec![CpsParam::Ident(local), CpsParam::Ident("env")],
              body: Box::new(body),
            },
          }
        })
      }

      _ => CpsExpr::TailCall {
        cont: Box::new(CpsVal::Str("not implemented".into())),
        args: vec![],
      },
    }
  }

  /// For an ident node: emit `load env, id'name', fn ·name, env: inner_body`.
  /// For literals: no load needed, call `make_body` directly with the literal value.
  /// `wrap` receives the completed body and wraps it in the outer context.
  fn load_node<'src, F, W>(&mut self, node: &Node<'src>, make_body: F, wrap: W) -> CpsExpr<'src>
  where
    F: FnOnce(CpsVal<'src>) -> CpsExpr<'src>,
    W: FnOnce(CpsExpr<'src>) -> CpsExpr<'src>,
  {
    match &node.kind {
      NodeKind::Ident(s) => {
        let local: &'static str = self.alloc(format!("·{}", s));
        let body = make_body(CpsVal::Ident(local));
        wrap(CpsExpr::Load {
          env: "env",
          key: CpsKey::Id(s),
          cont: CpsFn {
            params: vec![CpsParam::Ident(local), CpsParam::Ident("env")],
            body: Box::new(body),
          },
        })
      }
      _ => {
        let val = self.lit_to_val(node);
        wrap(make_body(val))
      }
    }
  }

  fn lit_to_val<'src>(&mut self, node: &Node<'src>) -> CpsVal<'src> {
    match &node.kind {
      NodeKind::LitBool(b) => CpsVal::Bool(*b),
      NodeKind::LitInt(s) => CpsVal::Int(s),
      NodeKind::LitFloat(s) => CpsVal::Float(s),
      NodeKind::LitDecimal(s) => CpsVal::Decimal(s),
      NodeKind::LitStr(s) => CpsVal::Str(s.clone()),
      _ => CpsVal::Str("?".into()),
    }
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use test_macros::test_template;
  use pretty_assertions::assert_eq;
  use super::{transform, super::cps_fmt};

  fn dedent(s: &str) -> String {
    s.lines()
      .map(|line| line.strip_prefix("    ").unwrap_or(line))
      .collect::<Vec<_>>()
      .join("\n")
  }

  fn cps_debug(src: &str) -> String {
    match transform(src) {
      Ok(expr) => cps_fmt::fmt(&expr),
      Err(e) => format!("ERROR: {}", e),
    }
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
