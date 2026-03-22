pub mod fmt;
pub mod lexer;
pub mod parser;
pub mod transform;

use lexer::{Loc, Token};

/// Separated sequence of expressions — the shared structural building block
/// for params, args, body statements, seq/rec items, and pipe segments.
/// Separators are `,`, `;`, `|`, or block-continuation tokens.
///
/// Invariant: `seps.len() <= items.len()`. Typically `seps.len() == items.len() - 1`
/// (no trailing separator) or `seps.len() == items.len()` (trailing separator present).
/// Empty sequences have both vecs empty.
#[derive(Debug, Clone, PartialEq)]
pub struct Exprs<'src> {
  pub items: Vec<Node<'src>>,
  pub seps: Vec<Token<'src>>,
}

impl<'src> Exprs<'src> {
  pub fn empty() -> Self {
    Self { items: vec![], seps: vec![] }
  }
}

/// Output of the parse pass — the AST tree plus metadata.
pub struct ParseResult<'src> {
  pub root: Node<'src>,
  pub node_count: u32,
}

/// Unique identifier for an AST node, assigned by the parser.
/// Used as a key into property graphs for attaching pass-computed metadata
/// (name resolution, types, etc.) without modifying the AST structure.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AstId(pub u32);

impl std::fmt::Debug for AstId {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "#{}", self.0)
  }
}

impl From<AstId> for usize {
  fn from(id: AstId) -> usize { id.0 as usize }
}

impl From<usize> for AstId {
  fn from(n: usize) -> AstId { AstId(n as u32) }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node<'src> {
  pub id: AstId,
  pub kind: NodeKind<'src>,
  pub loc: Loc,
}

impl<'src> Node<'src> {
  /// Create a node with a dummy ID. Used by transforms and formatters that
  /// reconstruct or synthesize nodes outside the parser.
  pub fn new(kind: NodeKind<'src>, loc: Loc) -> Self {
    Self { id: AstId(0), kind, loc }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeKind<'src> {
  // --- literals ---

  // LitBool true | false
  LitBool(bool),

  // LitInt '1_234_567' | '+1' | '-1' | '0xFF' | '0b_0101'
  // raw source slice — value parsing deferred
  LitInt(&'src str),

  // LitFloat '1.0' | '1.0e100_000'
  LitFloat(&'src str),

  // LitDecimal '1.0d' | '1.0d-100'
  LitDecimal(&'src str),

  // LitStr — string literal or string segment inside a template
  // open/close are delimiter tokens: ' .. ', ' .. ${, } .. ', } .. ${, ": .. dedent
  // content is owned since escape sequences are processed at parse time
  // indent: for block strings (":" syntax), the number of leading spaces stripped from
  // each content line (strip_level from the lexer). 0 for quoted strings.
  LitStr { open: Token<'src>, close: Token<'src>, content: String, indent: u32 },

  // LitSeq — sequence literal; items are elements separated by , ; or block tokens
  LitSeq { open: Token<'src>, close: Token<'src>, items: Exprs<'src> },

  // LitRec — record literal; items are Ident (shorthand), Arm (key:val), or Spread
  LitRec { open: Token<'src>, close: Token<'src>, items: Exprs<'src> },

  // --- string templates ---

  // StrTempl — interpolated string; open/close mirror first/last child's delimiters
  StrTempl { open: Token<'src>, close: Token<'src>, children: Vec<Node<'src>> },

  // StrRawTempl — tagged template; raw parts + expressions, passed to tag fn unprocessed
  StrRawTempl { open: Token<'src>, close: Token<'src>, children: Vec<Node<'src>> },

  // --- identifiers ---

  // Ident 'foo' | 'foo-bar'
  Ident(&'src str),

  // --- operators ---

  // UnaryOp '-' | 'not' | '~'
  UnaryOp { op: Token<'src>, operand: Box<Node<'src>> },

  // InfixOp '+' | '-' | 'srcnd' | '>' | '&' | '..' | '...' | ...
  InfixOp { op: Token<'src>, lhs: Box<Node<'src>>, rhs: Box<Node<'src>> },

  // ChainedCmp — flat interleaved: operand, op, operand, op, operand, ...
  // e.g. a > b > c => [Operand(a), Op(">"), Operand(b), Op(">"), Operand(c)]
  ChainedCmp(Vec<CmpPart<'src>>),

  // Spread — bare (..) or with guard/expr child
  Spread { op: Token<'src>, inner: Option<Box<Node<'src>>> },

  // Member — lhs.rhs; rhs is Ident (name) or Group (expr key)
  Member { op: Token<'src>, lhs: Box<Node<'src>>, rhs: Box<Node<'src>> },

  // Group — parenthesised expr; only preserved where semantically significant
  // (record computed keys, member expression keys, spread guards)
  Group { open: Token<'src>, close: Token<'src>, inner: Box<Node<'src>> },

  // Partial — ? hole for partial application
  Partial,

  // Wildcard — _ non-binding placeholder; lexed as Ident("_"), promoted by parser
  Wildcard,

  // --- binding ---

  // Bind lhs = rhs
  Bind { op: Token<'src>, lhs: Box<Node<'src>>, rhs: Box<Node<'src>> },

  // BindRight lhs |= rhs
  BindRight { op: Token<'src>, lhs: Box<Node<'src>>, rhs: Box<Node<'src>> },

  // --- application ---

  // Apply func arg arg ...
  Apply { func: Box<Node<'src>>, args: Exprs<'src> },

  // Pipe — left-to-right chain: [a, b, c] means c(b(a)); separated by |
  Pipe(Exprs<'src>),

  // --- functions ---

  // Fn — params (Patterns node) + sep (:) + body
  Fn { params: Box<Node<'src>>, sep: Token<'src>, body: Exprs<'src> },

  // Patterns — expression sequence in pattern position (fn params, match subjects)
  // separated by , ; or block tokens
  Patterns(Exprs<'src>),

  // --- match ---

  // Match — subject expressions + sep (:) + arms
  Match { subjects: Exprs<'src>, sep: Token<'src>, arms: Exprs<'src> },

  // Arm — lhs (Patterns node) + sep (:) + body
  Arm { lhs: Box<Node<'src>>, sep: Token<'src>, body: Exprs<'src> },

  // --- error handling ---

  // Try — unwrap Ok or propagate Err from enclosing function
  Try(Box<Node<'src>>),

  // --- suspension ---

  // Yield — suspend execution, yield a value; resumed with a result
  // result = yield value
  Yield(Box<Node<'src>>),

  // --- custom blocks ---

  // Block — name (Ident) + params (Patterns) + sep (:) + body
  Block { name: Box<Node<'src>>, params: Box<Node<'src>>, sep: Token<'src>, body: Exprs<'src> },
}

// For ChainedCmp interleaved representation
#[derive(Debug, Clone, PartialEq)]
pub enum CmpPart<'src> {
  Operand(Node<'src>),
  Op(Token<'src>),
}

// --- tree walker ---

/// Walk every node in the AST in pre-order, calling `f` on each.
pub fn walk<'src>(node: &'src Node<'src>, f: &mut impl FnMut(&'src Node<'src>)) {
  f(node);
  match &node.kind {
    NodeKind::LitBool(_)
    | NodeKind::LitInt(_)
    | NodeKind::LitFloat(_)
    | NodeKind::LitDecimal(_)
    | NodeKind::LitStr { .. }
    | NodeKind::Ident(_)
    | NodeKind::Partial
    | NodeKind::Wildcard => {}

    NodeKind::LitSeq { items, .. }
    | NodeKind::LitRec { items, .. }
    | NodeKind::Pipe(items)
    | NodeKind::Patterns(items) => {
      for child in &items.items { walk(child, f); }
    }
    NodeKind::StrTempl { children, .. }
    | NodeKind::StrRawTempl { children, .. } => {
      for child in children { walk(child, f); }
    }

    NodeKind::UnaryOp { operand, .. } => walk(operand, f),
    NodeKind::InfixOp { lhs, rhs, .. } => {
      walk(lhs, f);
      walk(rhs, f);
    }
    NodeKind::ChainedCmp(parts) => {
      for part in parts {
        if let CmpPart::Operand(n) = part { walk(n, f); }
      }
    }
    NodeKind::Spread { inner, .. } => {
      if let Some(n) = inner { walk(n, f); }
    }
    NodeKind::Member { lhs, rhs, .. } => {
      walk(lhs, f);
      walk(rhs, f);
    }
    NodeKind::Group { inner, .. } => walk(inner, f),
    NodeKind::Try(inner) | NodeKind::Yield(inner) => walk(inner, f),
    NodeKind::Bind { lhs, rhs, .. } | NodeKind::BindRight { lhs, rhs, .. } => {
      walk(lhs, f);
      walk(rhs, f);
    }
    NodeKind::Apply { func, args } => {
      walk(func, f);
      for arg in &args.items { walk(arg, f); }
    }
    NodeKind::Fn { params, body, .. } => {
      walk(params, f);
      for stmt in &body.items { walk(stmt, f); }
    }
    NodeKind::Match { subjects, arms, .. } => {
      for subj in &subjects.items { walk(subj, f); }
      for arm in &arms.items { walk(arm, f); }
    }
    NodeKind::Arm { lhs, body, .. } => {
      walk(lhs, f);
      for stmt in &body.items { walk(stmt, f); }
    }
    NodeKind::Block { name, params, body, .. } => {
      walk(name, f);
      walk(params, f);
      for stmt in &body.items { walk(stmt, f); }
    }
  }
}

// --- index builder ---

/// Build a PropGraph mapping AstId → &Node for O(1) lookup by ID.
/// Walks the tree once, placing each node at its AstId position.
pub fn build_index<'src>(result: &'src ParseResult<'src>) -> crate::propgraph::PropGraph<AstId, Option<&'src Node<'src>>> {
  let mut index: crate::propgraph::PropGraph<AstId, Option<&'src Node<'src>>> =
    crate::propgraph::PropGraph::with_size(result.node_count as usize, None);
  walk(&result.root, &mut |node| {
    index.set(node.id, Some(node));
  });
  index
}

// --- s-expression printer ---

impl<'src> Node<'src> {
  pub fn print(&self) -> String {
    let mut out = String::new();
    print_node(self, &mut out, 0);
    out
  }
}

fn indent(out: &mut String, depth: usize) {
  for _ in 0..depth {
    out.push_str("  ");
  }
}

fn print_node(node: &Node, out: &mut String, depth: usize) {
  indent(out, depth);
  match &node.kind {
    NodeKind::LitBool(v) => {
      out.push_str(if *v { "LitBool true" } else { "LitBool false" });
    }
    NodeKind::LitInt(s) => { out.push_str("LitInt '"); out.push_str(s); out.push('\''); }
    NodeKind::LitFloat(s) => { out.push_str("LitFloat '"); out.push_str(s); out.push('\''); }
    NodeKind::LitDecimal(s) => { out.push_str("LitDecimal '"); out.push_str(s); out.push('\''); }
    NodeKind::LitStr { content, .. } => {
      out.push_str("LitStr '");
      out.push_str(
        &content.replace('\\', "\\\\")
          .replace('\n', "\\n")
          .replace('\r', "\\r")
          .replace('\t', "\\t")
          .replace('\x0B', "\\v")
          .replace('\x08', "\\b")
          .replace('\x0C', "\\f")
          .replace('\'', "\\'")
      );
      out.push('\'');
    }
    NodeKind::LitSeq { open, close, items } => {
      out.push_str("LitSeq '"); out.push_str(open.src); out.push_str(".."); out.push_str(close.src); out.push('\'');
      if !items.items.is_empty() { out.push(','); }
      print_exprs(items, out, depth);
    }
    NodeKind::LitRec { open, close, items } => {
      out.push_str("LitRec '"); out.push_str(open.src); out.push_str(".."); out.push_str(close.src); out.push('\'');
      if !items.items.is_empty() { out.push(','); }
      print_exprs(items, out, depth);
    }
    NodeKind::StrTempl { children, .. } => {
      out.push_str("StrTempl");
      print_children(children, out, depth);
    }
    NodeKind::StrRawTempl { children, .. } => {
      out.push_str("StrRawTempl");
      print_children(children, out, depth);
    }
    NodeKind::Ident(s) => { out.push_str("Ident '"); out.push_str(s); out.push('\''); }
    NodeKind::UnaryOp { op, operand } => {
      out.push_str("UnaryOp '"); out.push_str(op.src); out.push_str("',");
      out.push('\n');
      print_node(operand, out, depth + 1);
    }
    NodeKind::InfixOp { op, lhs, rhs } => {
      out.push_str("InfixOp '"); out.push_str(op.src); out.push_str("',");
      out.push('\n');
      print_node(lhs, out, depth + 1);
      out.push('\n');
      print_node(rhs, out, depth + 1);
    }
    NodeKind::ChainedCmp(parts) => {
      out.push_str("ChainedCmp");
      for part in parts {
        out.push('\n');
        match part {
          CmpPart::Operand(n) => print_node(n, out, depth + 1),
          CmpPart::Op(op) => { indent(out, depth + 1); out.push('\''); out.push_str(op.src); out.push('\''); }
        }
      }
    }
    NodeKind::Spread { op, inner: child } => {
      out.push_str("Spread '"); out.push_str(op.src); out.push('\'');
      if child.is_some() { out.push(','); }
      if let Some(n) = child {
        out.push('\n');
        print_node(n, out, depth + 1);
      }
    }
    NodeKind::Member { op, lhs, rhs } => {
      out.push_str("Member '"); out.push_str(op.src); out.push_str("',");
      out.push('\n');
      print_node(lhs, out, depth + 1);
      out.push('\n');
      print_node(rhs, out, depth + 1);
    }
    NodeKind::Group { open, close, inner } => {
      out.push_str("Group '"); out.push_str(open.src); out.push_str(".."); out.push_str(close.src); out.push_str("',");
      out.push('\n');
      print_node(inner, out, depth + 1);
    }
    NodeKind::Partial => { out.push_str("Partial"); }
    NodeKind::Wildcard => { out.push_str("Wildcard"); }
    NodeKind::Try(inner) => {
      out.push_str("Try");
      out.push('\n');
      print_node(inner, out, depth + 1);
    }
    NodeKind::Yield(inner) => {
      out.push_str("Yield");
      out.push('\n');
      print_node(inner, out, depth + 1);
    }
    NodeKind::Bind { op, lhs, rhs } => {
      out.push_str("Bind '"); out.push_str(op.src); out.push_str("',");
      out.push('\n');
      print_node(lhs, out, depth + 1);
      out.push('\n');
      print_node(rhs, out, depth + 1);
    }
    NodeKind::BindRight { op, lhs, rhs } => {
      out.push_str("BindRight '"); out.push_str(op.src); out.push_str("',");
      out.push('\n');
      print_node(lhs, out, depth + 1);
      out.push('\n');
      print_node(rhs, out, depth + 1);
    }
    NodeKind::Apply { func, args } => {
      out.push_str("Apply");
      out.push('\n');
      print_node(func, out, depth + 1);
      print_exprs(args, out, depth);
    }
    NodeKind::Pipe(exprs) => {
      out.push_str("Pipe");
      print_children(&exprs.items, out, depth);
    }
    NodeKind::Fn { params, sep, body } => {
      out.push_str("Fn '"); out.push_str(sep.src); out.push_str("',");
      out.push('\n');
      print_node(params, out, depth + 1);
      for node in &body.items {
        out.push('\n');
        print_node(node, out, depth + 1);
      }
    }
    NodeKind::Patterns(exprs) => {
      out.push_str("Patterns");
      print_exprs(exprs, out, depth);
    }
    NodeKind::Match { subjects, sep, arms } => {
      out.push_str("Match '"); out.push_str(sep.src); out.push_str("',");
      for subj in &subjects.items {
        out.push('\n');
        print_node(subj, out, depth + 1);
      }
      for arm in &arms.items {
        out.push('\n');
        print_node(arm, out, depth + 1);
      }
    }
    NodeKind::Arm { lhs, sep, body } => {
      out.push_str("Arm '"); out.push_str(sep.src); out.push_str("',");
      out.push('\n');
      print_node(lhs, out, depth + 1);
      for node in &body.items {
        out.push('\n');
        print_node(node, out, depth + 1);
      }
    }
    NodeKind::Block { name, params, sep, body } => {
      out.push_str("Block '"); out.push_str(sep.src); out.push_str("',");
      out.push('\n');
      print_node(name, out, depth + 1);
      out.push('\n');
      print_node(params, out, depth + 1);
      for node in &body.items {
        out.push('\n');
        print_node(node, out, depth + 1);
      }
    }
  }
}

fn print_children(children: &[Node], out: &mut String, depth: usize) {
  for child in children {
    out.push('\n');
    print_node(child, out, depth + 1);
  }
}

fn print_exprs(exprs: &Exprs, out: &mut String, depth: usize) {
  print_children(&exprs.items, out, depth);
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::lexer::{Loc, Pos, Token, TokenKind};

  fn loc() -> Loc {
    Loc { start: Pos { idx: 0, line: 1, col: 0 }, end: Pos { idx: 0, line: 1, col: 0 } }
  }

  fn tok(src: &str) -> Token<'_> {
    Token { kind: TokenKind::Sep, loc: loc(), src }
  }

  fn node(kind: NodeKind) -> Node {
    Node::new(kind, loc())
  }

  #[test]
  fn print_simple_binding() {
    // foo = 1
    let tree = node(NodeKind::Bind {
      op: tok("="),
      lhs: Box::new(node(NodeKind::Ident("foo"))),
      rhs: Box::new(node(NodeKind::LitInt("1"))),
    });
    assert_eq!(tree.print(), "Bind '=',\n  Ident 'foo'\n  LitInt '1'");
  }

  #[test]
  fn print_infix_op() {
    // a + b
    let tree = node(NodeKind::InfixOp {
      op: tok("+"),
      lhs: Box::new(node(NodeKind::Ident("a"))),
      rhs: Box::new(node(NodeKind::Ident("b"))),
    });
    assert_eq!(tree.print(), "InfixOp '+',\n  Ident 'a'\n  Ident 'b'");
  }

  #[test]
  fn print_lit_seq_empty() {
    let tree = node(NodeKind::LitSeq { open: tok("["), close: tok("]"), items: Exprs::empty() });
    assert_eq!(tree.print(), "LitSeq '[..]'");
  }

  #[test]
  fn print_spread_bare() {
    let tree = node(NodeKind::Spread { op: tok(".."), inner: None });
    assert_eq!(tree.print(), "Spread '..'");
  }

  #[test]
  fn print_patterns_empty() {
    let tree = node(NodeKind::Patterns(Exprs::empty()));
    assert_eq!(tree.print(), "Patterns");
  }

  #[test]
  fn build_index_returns_nodes_by_id() {
    let r = crate::parser::parse("foo = 1").unwrap();
    assert_eq!(r.node_count, 3);
    let index = super::build_index(&r);
    // Verify each slot is populated and id matches position
    for i in 0..3 {
      let node = index.get(AstId(i)).unwrap();
      assert_eq!(node.id, AstId(i));
    }
  }

  #[test]
  fn walk_visits_all_nodes() {
    let r = crate::parser::parse("foo = [1, 2]").unwrap();
    let mut kinds = vec![];
    super::walk(&r.root, &mut |n| {
      kinds.push(std::mem::discriminant(&n.kind));
    });
    // Bind, Ident("foo"), LitSeq, LitInt("1"), LitInt("2") = 5 nodes
    assert_eq!(kinds.len(), 5);
  }

  #[test]
  fn walk_visits_in_pre_order() {
    let r = crate::parser::parse("a + b").unwrap();
    let mut names = vec![];
    super::walk(&r.root, &mut |n| {
      match &n.kind {
        NodeKind::InfixOp { op, .. } => names.push(op.src),
        NodeKind::Ident(s) => names.push(s),
        _ => {}
      }
    });
    assert_eq!(names, vec!["+", "a", "b"]);
  }

  #[test]
  fn print_chained_cmp() {
    // a > b > c
    let tree = node(NodeKind::ChainedCmp(vec![
      CmpPart::Operand(node(NodeKind::Ident("a"))),
      CmpPart::Op(tok(">")),
      CmpPart::Operand(node(NodeKind::Ident("b"))),
      CmpPart::Op(tok(">")),
      CmpPart::Operand(node(NodeKind::Ident("c"))),
    ]));
    assert_eq!(tree.print(), "ChainedCmp\n  Ident 'a'\n  '>'\n  Ident 'b'\n  '>'\n  Ident 'c'");
  }
}
