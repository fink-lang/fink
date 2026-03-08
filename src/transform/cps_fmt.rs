// cps::Expr → Node → Fink source pretty-printer.
//
// Synthesizes ·store/·load/·scope/·state/·ƒ_cont from the clean structural IR.
// The output is valid runnable Fink — the visualization doubles as a runtime spec.

use crate::ast::{self, Node, NodeKind};
use crate::lexer::{Loc, Pos};
use super::cps::{Arg, Arm, Expr, ExprKind, KeyKind, Lit, Param, Pat, PatKind, Val, ValKind};

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

pub fn fmt(expr: &Expr<'_>) -> String {
  ast::fmt::fmt(&to_node(expr))
}

// ---------------------------------------------------------------------------
// Dummy loc — all reconstructed AST nodes use this
// ---------------------------------------------------------------------------

fn loc() -> Loc {
  let p = Pos { idx: 0, line: 1, col: 0 };
  Loc { start: p, end: p }
}

fn node(kind: NodeKind<'static>) -> Node<'static> {
  Node::new(kind, loc())
}

// ---------------------------------------------------------------------------
// AST builder helpers
// ---------------------------------------------------------------------------

fn ident(s: &str) -> Node<'static> {
  let s: &'static str = Box::leak(s.to_string().into_boxed_str());
  node(NodeKind::Ident(s))
}

fn spread_node(inner: Node<'static>) -> Node<'static> {
  node(NodeKind::Spread(Some(Box::new(inner))))
}

fn apply(func: Node<'static>, args: Vec<Node<'static>>) -> Node<'static> {
  node(NodeKind::Apply { func: Box::new(func), args })
}

fn patterns(params: Vec<Node<'static>>) -> Node<'static> {
  node(NodeKind::Patterns(params))
}

fn fn_node(params: Node<'static>, body: Vec<Node<'static>>) -> Node<'static> {
  node(NodeKind::Fn { params: Box::new(params), body })
}

fn tagged(tag: &str, s: &str) -> Node<'static> {
  let s: &'static str = Box::leak(s.to_string().into_boxed_str());
  let str_node = node(NodeKind::LitStr(s.to_string()));
  let raw = node(NodeKind::StrRawTempl(vec![str_node]));
  apply(ident(tag), vec![raw])
}

fn id_tag(s: &str)  -> Node<'static> { tagged("id",  s) }
fn op_tag(s: &str)  -> Node<'static> { tagged("op",  s) }

// ---------------------------------------------------------------------------
// Val → Node
// ---------------------------------------------------------------------------

/// Render a Val to an AST node for use in an already-resolved position.
/// Keys are rendered as their plain name (caller must have issued a load first).
fn val_to_node(v: &Val<'_>) -> Node<'static> {
  match &v.kind {
    ValKind::Lit(lit)    => lit_to_node(lit),
    ValKind::Ident(name) => ident(&sigil(name)),
    ValKind::Key(key)    => match &key.kind {
      KeyKind::Name(name) => ident(&sigil(name)),
      KeyKind::Op(op)     => ident(&sigil_op(op)),
    },
  }
}

/// Return the local name that a Val resolves to after loading.
/// For Ident/Lit this is the val itself; for Key it's the name that will be
/// bound by the synthesized load.
fn resolved_name(v: &Val<'_>) -> String {
  match &v.kind {
    ValKind::Ident(name) => sigil(name),
    ValKind::Key(key)    => match &key.kind {
      KeyKind::Name(name) => sigil(name),
      KeyKind::Op(op)     => sigil_op(op),
    },
    ValKind::Lit(_)      => String::new(),  // literals don't have a name
  }
}

/// Whether a Val needs a `load` synthesis before use.
fn needs_load(v: &Val<'_>) -> bool {
  matches!(v.kind, ValKind::Key(_))
}

/// Synthesize a `·load` wrapping `body_node`:
///   ·load ·scope, id'name' | op'sym', fn local, ·scope: body_node
fn emit_load(key: &super::cps::Key<'_>, local: &str, body_node: Node<'static>) -> Node<'static> {
  let key_node = match &key.kind {
    KeyKind::Name(name) => id_tag(name),
    KeyKind::Op(op)     => op_tag(op),
  };
  apply(ident("·load"), vec![
    ident("·scope"),
    key_node,
    fn_node(patterns(vec![ident(local), ident("·scope")]), vec![body_node]),
  ])
}

/// Wrap `inner_node` in loads for every `Key` val in `vals`.
/// Keys are resolved left-to-right; `val_to_node` can then be used on each val
/// since the name is now bound.
fn with_loads<F>(vals: &[&Val<'_>], inner: F) -> Node<'static>
where
  F: FnOnce(Vec<Node<'static>>) -> Node<'static>,
{
  // Collect which vals need loads, build the resolved name list.
  let resolved: Vec<(bool, String)> = vals.iter().map(|v| {
    (needs_load(v), resolved_name(v))
  }).collect();

  // Build inner node first (outermost continuation last = fold left).
  let inner_nodes: Vec<Node<'static>> = vals.iter().zip(resolved.iter())
    .map(|(v, (_, name))| {
      if name.is_empty() {
        val_to_node(v)  // literal
      } else {
        ident(name)     // already resolved (Ident or Key-after-load)
      }
    })
    .collect();
  let inner_node = inner(inner_nodes);

  // Wrap in loads right-to-left (innermost first in the fold).
  vals.iter().zip(resolved.iter()).rev()
    .fold(inner_node, |body, (v, (load, name))| {
      if *load {
        if let ValKind::Key(key) = &v.kind {
          emit_load(key, name, body)
        } else {
          body
        }
      } else {
        body
      }
    })
}

fn lit_to_node(lit: &Lit<'_>) -> Node<'static> {
  match lit {
    Lit::Bool(b) => node(NodeKind::LitBool(*b)),
    Lit::Int(n) => {
      let s: &'static str = Box::leak(n.to_string().into_boxed_str());
      node(NodeKind::LitInt(s))
    }
    Lit::Float(f) => {
      let s: &'static str = Box::leak(f.to_string().into_boxed_str());
      node(NodeKind::LitFloat(s))
    }
    Lit::Decimal(f) => {
      let s: &'static str = Box::leak(format!("{}d", f).into_boxed_str());
      node(NodeKind::LitDecimal(s))
    }
    Lit::Str(s) => node(NodeKind::LitStr(s.to_string())),
    Lit::Seq   => node(NodeKind::LitSeq(vec![])),
    Lit::Rec   => node(NodeKind::LitRec(vec![])),
  }
}

// ---------------------------------------------------------------------------
// Expr → Node
//
// Synthesis conventions:
//   LetVal { name, val, body }  → ·store ·scope, id'name', val, fn name, ·scope: body
//   LetFn  { name, params, ..} → ·closure ·scope, fn params…, ·scope, ·state, ·ƒ_cont: fn_body,
//                                               fn name, ·chld_scope: body
//   App    { func, args, result, body } → ·apply func_loaded, arg…, ·state, fn result, ·state: body
//   Ret(val)                   → ·ƒ_cont val, ·state
//
// Name sigil conventions (output only):
//   user names → plain (foo, bar)
//   runtime primitives → ·name (·store, ·load, ·scope, ·state, ·ƒ_cont, ·apply, ·closure, …)
//   compiler temps → ·v_0, ·fn_3
//   operator locals → ·op_plus, ·op_eq, …
// ---------------------------------------------------------------------------

fn sigil(name: &str) -> String {
  // User-defined names pass through plain.
  // Compiler-generated temps (v_N, fn_N) and runtime-injected names get · prefix.
  // Names already prefixed with · are left unchanged.
  if name.starts_with('·') {
    // already prefixed
    name.to_string()
  } else if name.starts_with("v_") || name.starts_with("fn_") || name.starts_with("op_")
    || matches!(
      name,
      "scope" | "state" | "_"
      // runtime primitives that appear as loaded locals
      | "range_excl" | "range_incl"
      | "seq_append" | "seq_concat"
      | "rec_put" | "rec_merge"
      | "str_fmt" | "str_raw"
      | "apply" | "closure" | "store" | "load"
      | "id" | "err" | "panic" | "if"
      | "match_block" | "match_branch" | "match_bind" | "match_done"
      | "match_pop_at" | "match_pop_field" | "match_rest" | "match_len"
      | "chld_scope"
    )
  {
    format!("·{}", name)
  } else {
    // user name — plain
    name.to_string()
  }
}

fn sigil_op(op: &str) -> String {
  // Operators are loaded under a readable local name: `·op_plus`, `·op_eq`, etc.
  let suffix = match op {
    "+"   => "plus",
    "-"   => "minus",
    "*"   => "mul",
    "/"   => "div",
    "%"   => "rem",
    "=="  => "eq",
    "!="  => "neq",
    "<"   => "lt",
    "<="  => "lte",
    ">"   => "gt",
    ">="  => "gte",
    "."   => "dot",
    "and" => "and",
    "or"  => "or",
    "not" => "not",
    _     => op,
  };
  format!("·op_{}", suffix)
}

pub fn to_node(expr: &Expr<'_>) -> Node<'static> {
  match &expr.kind {
    // Ret(val) → ·ƒ_cont val, ·state
    // If val is a Key, wrap in a load first.
    ExprKind::Ret(val) => {
      with_loads(&[val], |resolved| {
        apply(ident("·ƒ_cont"), vec![resolved.into_iter().next().unwrap(), ident("·state")])
      })
    }

    // LetVal { name, val, body } → ·store ·scope, id'name', val, fn name, ·scope: body
    // If val is a Key, wrap a load first.
    ExprKind::LetVal { name, val, body } => {
      let plain = sigil(name);
      let store_node = apply(ident("·store"), vec![
        ident("·scope"),
        id_tag(name),
        val_to_node(val),
        fn_node(
          patterns(vec![ident(&plain), ident("·scope")]),
          vec![to_node(body)],
        ),
      ]);
      with_loads(&[val], |_| store_node)
    }

    // LetFn { name, params, free_vars, fn_body, body }
    // → ·closure ·scope, fn params…, scope_arg, ·state, ·ƒ_cont: fn_body,
    //               fn name, ·chld_scope: body
    // scope_arg = ·scope when free_vars is empty;
    //             {..·scope, name1, name2, …} when captures are present.
    ExprKind::LetFn { name, params, free_vars, fn_body, body } => {
      let plain_name = sigil(name);
      // Build fn params: p1, p2, …, scope_arg, ·state, ·ƒ_cont
      // Spread params render as `..name`.
      let mut fn_params: Vec<Node<'static>> = params.iter()
        .map(|p| match p {
          Param::Name(n) => ident(&sigil(n)),
          Param::Spread(n) => spread_node(ident(&sigil(n))),
        })
        .collect();
      let scope_arg = if free_vars.is_empty() {
        ident("·scope")
      } else {
        // {..·scope, name1, name2, …}
        let mut fields: Vec<Node<'static>> = vec![
          node(NodeKind::Spread(Some(Box::new(ident("·scope"))))),
        ];
        fields.extend(free_vars.iter().map(|n| ident(&sigil(n))));
        node(NodeKind::LitRec(fields))
      };
      fn_params.push(scope_arg);
      fn_params.push(ident("·state"));
      fn_params.push(ident("·ƒ_cont"));
      apply(ident("·closure"), vec![
        ident("·scope"),
        fn_node(patterns(fn_params), vec![to_node(fn_body)]),
        fn_node(
          patterns(vec![ident(&plain_name), ident("·chld_scope")]),
          vec![to_node(body)],
        ),
      ])
    }

    // App { func, args, result, body }
    // → ·apply func, arg…, ·state, fn result, ·state: body
    // Loads synthesized for any Key vals in func or args.
    // Spread args render as `..resolved_val`.
    ExprKind::App { func, args, result, body } => {
      let result_plain = sigil(result);
      let result_fn = fn_node(
        patterns(vec![ident(&result_plain), ident("·state")]),
        vec![to_node(body)],
      );
      // Extract vals from args, tracking which are spreads.
      let is_spread: Vec<bool> = args.iter().map(|a| matches!(a, Arg::Spread(_))).collect();
      let arg_vals: Vec<&Val<'_>> = args.iter().map(|a| match a {
        Arg::Val(v) | Arg::Spread(v) => v,
      }).collect();
      let all_vals: Vec<&Val<'_>> = std::iter::once(func.as_ref())
        .chain(arg_vals.iter().copied())
        .collect();
      with_loads(&all_vals, |mut resolved| {
        // resolved[0] = func, resolved[1..] = args
        let func_node = resolved.remove(0);
        let mut apply_args: Vec<Node<'static>> = vec![func_node];
        apply_args.extend(resolved.into_iter()
          .zip(is_spread.iter())
          .map(|(n, &spread)| if spread { spread_node(n) } else { n }));
        apply_args.push(ident("·state"));
        apply_args.push(result_fn);
        apply(ident("·apply"), apply_args)
      })
    }

    // If { cond, then, else_ } → ·if cond, fn ·state: then, fn ·state: else_
    ExprKind::If { cond, then, else_ } => {
      apply(ident("·if"), vec![
        val_to_node(cond),
        fn_node(patterns(vec![ident("·state")]), vec![to_node(then)]),
        fn_node(patterns(vec![ident("·state")]), vec![to_node(else_)]),
      ])
    }

    // LetRec — not yet implemented; placeholder
    ExprKind::LetRec { .. } => {
      apply(ident("·let_rec"), vec![node(NodeKind::Wildcard)])
    }

    // Match { scrutinee, arms, result, body }
    // → ·match_block scrutinee, ·state,
    //     ·match_branch pat, fn bindings…, ·scope, ·state, ·ƒ_cont: arm_body,
    //     …,
    //     fn result, ·state: body
    ExprKind::Match { scrutinee, arms, result, body } => {
      let result_plain = sigil(result);
      let result_fn = fn_node(
        patterns(vec![ident(&result_plain), ident("·state")]),
        vec![to_node(body)],
      );
      with_loads(&[scrutinee], |resolved| {
        let scrutinee_node = resolved.into_iter().next().unwrap();
        let mut args = vec![scrutinee_node, ident("·state")];
        for arm in arms {
          args.push(arm_to_node(arm));
        }
        args.push(result_fn);
        apply(ident("·match_block"), args)
      })
    }
  }
}

// Render a single match arm as:
//   ·match_branch pat, fn bindings…, ·scope, ·state, ·ƒ_cont: arm_body
fn arm_to_node(arm: &Arm<'_>) -> Node<'static> {
  let mut fn_params: Vec<Node<'static>> = arm.bindings.iter()
    .map(|b| ident(&sigil(b)))
    .collect();
  fn_params.push(ident("·scope"));
  fn_params.push(ident("·state"));
  fn_params.push(ident("·ƒ_cont"));
  apply(ident("·match_branch"), vec![
    pat_to_node(&arm.pattern),
    fn_node(patterns(fn_params), vec![to_node(&arm.fn_body)]),
  ])
}

fn pat_to_node(pat: &Pat<'_>) -> Node<'static> {
  match &pat.kind {
    PatKind::Wildcard      => node(NodeKind::Wildcard),
    PatKind::Bind(name)    => ident(&sigil(name)),
    PatKind::Lit(lit)      => lit_to_node(lit),
    // Composite patterns — render as their source equivalent for readability.
    PatKind::Seq { .. }    => ident("_seq_pat"),    // placeholder; full rendering deferred
    PatKind::Rec { .. }    => ident("_rec_pat"),    // placeholder
    PatKind::Str(_)        => ident("_str_pat"),    // placeholder
    PatKind::Range { op, start, end } => {
      let op_s: &'static str = Box::leak(op.to_string().into_boxed_str());
      node(NodeKind::Range {
        op: op_s,
        start: Box::new(pat_to_node(start)),
        end: Box::new(pat_to_node(end)),
      })
    }
    PatKind::Guard { pat, .. } => pat_to_node(pat),  // drop guard for now
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use test_macros::test_template;
  use pretty_assertions::assert_eq;
  use crate::parser::parse;
  use crate::transform::cps_transform::lower_expr;
  use crate::transform::cps_free_vars::annotate;
  use super::fmt;

  fn dedent(s: &str) -> String {
    s.lines()
      .map(|line| line.strip_prefix("    ").unwrap_or(line))
      .collect::<Vec<_>>()
      .join("\n")
  }

  /// Lower only — no free-var pass.
  fn cps_expr(src: &str) -> String {
    match parse(src) {
      Ok(node) => fmt(&lower_expr(&node)),
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  /// Lower then annotate with free-var pass.
  fn cps_c_expr(src: &str) -> String {
    match parse(src) {
      Ok(node) => fmt(&annotate(lower_expr(&node))),
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  #[test_template(
    "src/transform", "./test_cps.fnk",
    r"(?ms)^test '(?P<name>[^']+)', fn:\n  expect (?P<func>\S+) fn:\n(?P<src>[\s\S]+?)\n\n?  [|,] equals(?:_fink)? fn:\n(?P<exp>[\s\S]+?)(?=\n\n\n|\n\n---|\n\ntest |\z)"
  )]
  fn test_cps(src: &str, exp: &str, func: &str, path: &str) {
    let actual = match func {
      "cps_c_expr" => cps_c_expr(&dedent(src).trim().to_string()),
      _            => cps_expr(&dedent(src).trim().to_string()),
    };
    assert_eq!(
      actual,
      dedent(exp).trim().to_string(),
      "{}",
      path
    );
  }
}
