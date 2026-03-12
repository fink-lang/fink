pub mod fmt;
pub mod lexer;
pub mod parser;
pub mod transform;

use lexer::Loc;

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

  // LitStr 'hello world'
  // TODO: is it fully resolved string value (escape sequences processed)
  // owned since it differs from source
  LitStr(String),

  // LitSeq — children are elements
  LitSeq(Vec<Node<'src>>),

  // LitRec — children are Ident (shorthand), Arm (key:val), or Spread
  LitRec(Vec<Node<'src>>),

  // --- string templates ---

  // StrTempl — interpolated string; children are LitStr and expressions
  StrTempl(Vec<Node<'src>>),

  // StrRawTempl — tagged template; raw parts + expressions, passed to tag fn unprocessed
  StrRawTempl(Vec<Node<'src>>),

  // --- identifiers ---

  // Ident 'foo' | 'foo-bar'
  Ident(&'src str),

  // --- operators ---

  // UnaryOp '-' | 'not' | '~'
  UnaryOp { op: &'src str, operand: Box<Node<'src>> },

  // InfixOp '+' | '-' | 'and' | '>' | '&' | '..' | '...' | ...
  InfixOp { op: &'src str, lhs: Box<Node<'src>>, rhs: Box<Node<'src>> },

  // ChainedCmp — flat interleaved: operand, op, operand, op, operand, ...
  // e.g. a > b > c => [Operand(a), Op(">"), Operand(b), Op(">"), Operand(c)]
  ChainedCmp(Vec<CmpPart<'src>>),

  // Spread — bare (..) or with guard/expr child
  Spread(Option<Box<Node<'src>>>),

  // Member — lhs.rhs; rhs is Ident (name) or Group (expr key)
  Member { lhs: Box<Node<'src>>, rhs: Box<Node<'src>> },

  // Group — parenthesised expr; only preserved where semantically significant
  // (record computed keys, member expression keys, spread guards)
  Group(Box<Node<'src>>),

  // Partial — ? hole for partial application
  Partial,

  // Wildcard — _ non-binding placeholder; lexed as Ident("_"), promoted by parser
  Wildcard,

  // --- binding ---

  // Bind lhs = rhs
  Bind { lhs: Box<Node<'src>>, rhs: Box<Node<'src>> },

  // BindRight lhs |= rhs
  BindRight { lhs: Box<Node<'src>>, rhs: Box<Node<'src>> },

  // --- application ---

  // Apply func arg arg ...
  Apply { func: Box<Node<'src>>, args: Vec<Node<'src>> },

  // Pipe — left-to-right chain: [a, b, c] means c(b(a))
  Pipe(Vec<Node<'src>>),

  // --- functions ---

  // Fn — params (Patterns node) + body exprs (flat, may include Arms)
  Fn { params: Box<Node<'src>>, body: Vec<Node<'src>> },

  // Patterns — comma-separated param/subject list
  Patterns(Vec<Node<'src>>),

  // --- match ---

  // Match — subjects (Patterns node) + arms
  Match { subjects: Box<Node<'src>>, arms: Vec<Node<'src>> },

  // Arm — lhs patterns : body exprs (flat, like Fn body)
  // lhs is Vec to handle multi-pattern arms (match a, b: ...)
  Arm { lhs: Vec<Node<'src>>, body: Vec<Node<'src>> },

  // --- error handling ---

  // Try — unwrap Ok or propagate Err from enclosing function
  Try(Box<Node<'src>>),

  // --- suspension ---

  // Yield — suspend execution, yield a value; resumed with a result
  // result = yield value
  Yield(Box<Node<'src>>),

  // --- custom blocks ---

  // Block — name (Ident) + params (Patterns) + body (flat exprs/arms)
  Block { name: Box<Node<'src>>, params: Box<Node<'src>>, body: Vec<Node<'src>> },
}

// For ChainedCmp interleaved representation
#[derive(Debug, Clone, PartialEq)]
pub enum CmpPart<'src> {
  Operand(Node<'src>),
  Op(&'src str),
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
    NodeKind::LitStr(s) => {
      out.push_str("LitStr '");
      out.push_str(
        &s.replace('\\', "\\\\")
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
    NodeKind::LitSeq(children) => {
      out.push_str("LitSeq");
      print_children(children, out, depth);
    }
    NodeKind::LitRec(children) => {
      out.push_str("LitRec");
      print_children(children, out, depth);
    }
    NodeKind::StrTempl(children) => {
      out.push_str("StrTempl");
      print_children(children, out, depth);
    }
    NodeKind::StrRawTempl(children) => {
      out.push_str("StrRawTempl");
      print_children(children, out, depth);
    }
    NodeKind::Ident(s) => { out.push_str("Ident '"); out.push_str(s); out.push('\''); }
    NodeKind::UnaryOp { op, operand } => {
      out.push_str("UnaryOp '"); out.push_str(op); out.push('\'');
      out.push('\n');
      print_node(operand, out, depth + 1);
    }
    NodeKind::InfixOp { op, lhs, rhs } => {
      out.push_str("InfixOp '"); out.push_str(op); out.push('\'');
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
          CmpPart::Op(op) => { indent(out, depth + 1); out.push('\''); out.push_str(op); out.push('\''); }
        }
      }
    }
    NodeKind::Spread(child) => {
      out.push_str("Spread");
      if let Some(n) = child {
        out.push('\n');
        print_node(n, out, depth + 1);
      }
    }
    NodeKind::Member { lhs, rhs } => {
      out.push_str("Member");
      out.push('\n');
      print_node(lhs, out, depth + 1);
      out.push('\n');
      print_node(rhs, out, depth + 1);
    }
    NodeKind::Group(inner) => {
      out.push_str("Group");
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
    NodeKind::Bind { lhs, rhs } => {
      out.push_str("Bind");
      out.push('\n');
      print_node(lhs, out, depth + 1);
      out.push('\n');
      print_node(rhs, out, depth + 1);
    }
    NodeKind::BindRight { lhs, rhs } => {
      out.push_str("BindRight");
      out.push('\n');
      print_node(lhs, out, depth + 1);
      out.push('\n');
      print_node(rhs, out, depth + 1);
    }
    NodeKind::Apply { func, args } => {
      out.push_str("Apply");
      out.push('\n');
      print_node(func, out, depth + 1);
      for arg in args {
        out.push('\n');
        print_node(arg, out, depth + 1);
      }
    }
    NodeKind::Pipe(children) => {
      out.push_str("Pipe");
      print_children(children, out, depth);
    }
    NodeKind::Fn { params, body } => {
      out.push_str("Fn");
      out.push('\n');
      print_node(params, out, depth + 1);
      for node in body {
        out.push('\n');
        print_node(node, out, depth + 1);
      }
    }
    NodeKind::Patterns(children) => {
      out.push_str("Patterns");
      print_children(children, out, depth);
    }
    NodeKind::Match { subjects, arms } => {
      out.push_str("Match");
      out.push('\n');
      print_node(subjects, out, depth + 1);
      for arm in arms {
        out.push('\n');
        print_node(arm, out, depth + 1);
      }
    }
    NodeKind::Arm { lhs, body } => {
      out.push_str("Arm");
      for pat in lhs {
        out.push('\n');
        print_node(pat, out, depth + 1);
      }
      for node in body {
        out.push('\n');
        print_node(node, out, depth + 1);
      }
    }
    NodeKind::Block { name, params, body } => {
      out.push_str("Block");
      out.push('\n');
      print_node(name, out, depth + 1);
      out.push('\n');
      print_node(params, out, depth + 1);
      for node in body {
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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::lexer::{Loc, Pos};

  fn loc() -> Loc {
    Loc { start: Pos { idx: 0, line: 1, col: 0 }, end: Pos { idx: 0, line: 1, col: 0 } }
  }

  fn node(kind: NodeKind) -> Node {
    Node::new(kind, loc())
  }

  #[test]
  fn print_simple_binding() {
    // foo = 1
    let tree = node(NodeKind::Bind {
      lhs: Box::new(node(NodeKind::Ident("foo"))),
      rhs: Box::new(node(NodeKind::LitInt("1"))),
    });
    assert_eq!(tree.print(), "Bind\n  Ident 'foo'\n  LitInt '1'");
  }

  #[test]
  fn print_infix_op() {
    // a + b
    let tree = node(NodeKind::InfixOp {
      op: "+",
      lhs: Box::new(node(NodeKind::Ident("a"))),
      rhs: Box::new(node(NodeKind::Ident("b"))),
    });
    assert_eq!(tree.print(), "InfixOp '+'\n  Ident 'a'\n  Ident 'b'");
  }

  #[test]
  fn print_lit_seq_empty() {
    let tree = node(NodeKind::LitSeq(vec![]));
    assert_eq!(tree.print(), "LitSeq");
  }

  #[test]
  fn print_spread_bare() {
    let tree = node(NodeKind::Spread(None));
    assert_eq!(tree.print(), "Spread");
  }

  #[test]
  fn print_patterns_empty() {
    let tree = node(NodeKind::Patterns(vec![]));
    assert_eq!(tree.print(), "Patterns");
  }

  #[test]
  fn print_chained_cmp() {
    // a > b > c
    let tree = node(NodeKind::ChainedCmp(vec![
      CmpPart::Operand(node(NodeKind::Ident("a"))),
      CmpPart::Op(">"),
      CmpPart::Operand(node(NodeKind::Ident("b"))),
      CmpPart::Op(">"),
      CmpPart::Operand(node(NodeKind::Ident("c"))),
    ]));
    assert_eq!(tree.print(), "ChainedCmp\n  Ident 'a'\n  '>'\n  Ident 'b'\n  '>'\n  Ident 'c'");
  }
}
