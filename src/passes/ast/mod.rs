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

// ---------------------------------------------------------------------------
// Flat AST arena (Step A of flat-ast-arena refactor)
//
// The long-term home for every AST node is a `PropGraph<AstId, Node>` — the
// arena — paired with a `root: AstId`. This pair is `Ast`. Nodes reference
// each other by `AstId` rather than owning children, so the arena can hand
// out stable node lookups via index rather than `&Node` borrows.
//
// Every pass that produces an AST follows an **append-only** discipline:
// existing nodes are never mutated or overwritten. Passes extend the arena
// with new nodes and (where they change a subtree) append a fresh copy of
// each parent whose child-id must be updated. The old nodes stay at their
// original ids, unreachable from the new root but still valid lookups for
// any side-table keyed against them.
//
// `AstBuilder` is the only handle that can grow the arena. Its API is
// deliberately minimal — `append` and `read` — so no pass can accidentally
// rewrite an existing slot through it. Mutating `Ast.nodes` directly via
// `PropGraph::set` / `get_mut` remains possible from outside the builder,
// but is a glaring signal in code review and should only happen in debug
// tooling or a deliberate compaction pass.
//
// Step A is pure addition: these types exist alongside the current owning
// `Node` tree but nothing uses them yet. Steps B and C migrate `NodeKind`
// children to `AstId` references and wire the parser through `AstBuilder`.
// ---------------------------------------------------------------------------

/// The flat AST: an arena of nodes plus a root id.
///
/// Neither half is meaningful alone — `root` without `nodes` is a dangling
/// id, `nodes` without `root` is a bag of disconnected subtrees. `Ast` is
/// the one type that describes "an AST".
#[derive(Clone)]
pub struct Ast<'src> {
  pub nodes: crate::propgraph::PropGraph<AstId, Node<'src>>,
  pub root: AstId,
}

impl<'src> Ast<'src> {
  /// A sentinel empty AST — a single `Module` node with no expressions.
  /// Used as a placeholder where code needs an `&Ast` but has no real one
  /// (e.g. the `cps::fmt` stub formatter path).
  pub fn empty() -> Self {
    let zero = Loc {
      start: lexer::Pos { idx: 0, line: 0, col: 0 },
      end: lexer::Pos { idx: 0, line: 0, col: 0 },
    };
    let mut builder = AstBuilder::new();
    let root = builder.append(
      NodeKind::Module { exprs: Exprs::empty(), url: String::new() },
      zero,
    );
    builder.finish(root)
  }
}

/// Append-only arena builder. The only way to grow an `Ast.nodes` in an
/// append-safe manner. Owns its `PropGraph` internally and hands it back
/// via `finish()` once a new root id is known.
///
/// Passes typically look like:
///   let (mut builder, old_root) = AstBuilder::from_ast(input);
///   let new_root = rewrite(&mut builder, old_root);
///   builder.finish(new_root)
pub struct AstBuilder<'src> {
  nodes: crate::propgraph::PropGraph<AstId, Node<'src>>,
  /// Length at construction time — used by debug assertions to detect
  /// accidental shrinking of the arena across a pass boundary.
  #[cfg(debug_assertions)]
  start_len: usize,
}

impl<'src> AstBuilder<'src> {
  /// Start a fresh builder with an empty arena.
  pub fn new() -> Self {
    Self {
      nodes: crate::propgraph::PropGraph::new(),
      #[cfg(debug_assertions)]
      start_len: 0,
    }
  }

  /// Take ownership of an existing `Ast` for extension. The current root
  /// is returned alongside so the caller can use it as its walking entry
  /// point.
  pub fn from_ast(ast: Ast<'src>) -> (Self, AstId) {
    let root = ast.root;
    #[cfg(debug_assertions)]
    let start_len = ast.nodes.len();
    let builder = Self {
      nodes: ast.nodes,
      #[cfg(debug_assertions)]
      start_len,
    };
    (builder, root)
  }

  /// Append a new node to the arena. Returns the freshly allocated id.
  /// The id stored in `Node.id` is overwritten with the freshly assigned
  /// value, so callers never need to think about it.
  pub fn append(&mut self, kind: NodeKind<'src>, loc: Loc) -> AstId {
    let id = AstId(self.nodes.len() as u32);
    self.nodes.push(Node { id, kind, loc });
    id
  }

  /// Read an existing node from the arena. Panics if `id` is out of range.
  pub fn read(&self, id: AstId) -> &Node<'src> {
    self.nodes.get(id)
  }

  /// Current arena length (i.e. the id that the next `append` will return).
  pub fn len(&self) -> usize {
    self.nodes.len()
  }

  /// True if the arena has no nodes.
  pub fn is_empty(&self) -> bool {
    self.nodes.is_empty()
  }

  /// Finalise the arena into an `Ast` rooted at `root`. Consumes the
  /// builder so no further appends can happen.
  pub fn finish(self, root: AstId) -> Ast<'src> {
    #[cfg(debug_assertions)]
    debug_assert!(
      self.nodes.len() >= self.start_len,
      "AstBuilder shrank the arena: start_len={}, end_len={}",
      self.start_len,
      self.nodes.len(),
    );
    Ast { nodes: self.nodes, root }
  }
}

impl<'src> Default for AstBuilder<'src> {
  fn default() -> Self {
    Self::new()
  }
}

/// Verify the append-only invariant between two `Ast`s — `after` must be a
/// strict append-only extension of `before`. Concretely: every existing slot
/// in `before` must survive verbatim in `after`, and `after.nodes.len() >=
/// before.nodes.len()`. Returns `Ok(())` on success, or a descriptive error
/// string on the first violation.
///
/// This is the runtime tripwire that complements `AstBuilder`'s compile-time
/// append-only API. Pass tests can call it via `debug_assert!` to confirm
/// nothing mutated an old slot:
///
/// ```ignore
/// let before_snapshot = input.clone();      // before pass runs
/// let output = my_pass::apply(input);
/// debug_assert!(appended_only(&before_snapshot, &output).is_ok());
/// ```
///
/// Intended for debug builds and tests; the linear scan is `O(n)` over the
/// old arena length. Use in the body of a pass is valid but wasteful — the
/// compile-time `AstBuilder` API is the primary defence.
pub fn appended_only<'src>(
  before: &Ast<'src>,
  after: &Ast<'src>,
) -> Result<(), String> {
  if after.nodes.len() < before.nodes.len() {
    return Err(format!(
      "appended_only: after.nodes.len() = {} < before.nodes.len() = {}",
      after.nodes.len(),
      before.nodes.len(),
    ));
  }
  for i in 0..before.nodes.len() {
    let id = AstId(i as u32);
    let old_node = before.nodes.get(id);
    let new_node = after.nodes.get(id);
    if old_node != new_node {
      return Err(format!(
        "appended_only: slot {:?} was mutated — old kind = {:?}, new kind = {:?}",
        id, old_node.kind, new_node.kind,
      ));
    }
  }
  Ok(())
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

  // SynthIdent — compiler-generated identifier (e.g. partial desugaring).
  // Never produced by the parser. The u32 groups nodes with the same logical name
  // (e.g. param and body refs share the same value). Rendered as ·$_<n>.
  SynthIdent(u32),

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

  // --- module ---

  // Module — top-level container for a source file's expressions.
  // `url` is the module's stable identity (file path, "@fink/*" virtual URL,
  // "<stdin>" for stdin, "test" for in-memory test sources, etc.). It flows
  // from the caller of the parser into this field and is read by the WASM
  // emitter as the fragment's `module_name` for cross-module linking.
  Module { exprs: Exprs<'src>, url: String },

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

  // --- custom blocks ---

  // Block — name (Ident) + params (Patterns) + sep (:) + body
  Block { name: Box<Node<'src>>, params: Box<Node<'src>>, sep: Token<'src>, body: Exprs<'src> },

  // Token — raw token leaf in a Tokens-mode block body
  Token(&'src str),
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
    | NodeKind::SynthIdent(_)
    | NodeKind::Partial
    | NodeKind::Wildcard
    | NodeKind::Token(_) => {}

    NodeKind::Module { exprs: items, .. }
    | NodeKind::LitSeq { items, .. }
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
    NodeKind::Try(inner) => walk(inner, f),
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

// TODO: include node Loc (start/end) in output so .fnk AST tests can assert on source spans
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
    NodeKind::SynthIdent(n) => { out.push_str(&format!("SynthIdent '·$_{n}'")); }
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
    NodeKind::Token(s) => { out.push_str("Token '"); out.push_str(s); out.push('\''); }
    NodeKind::Try(inner) => {
      out.push_str("Try");
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
    NodeKind::Module { exprs, .. } => {
      // URL is intentionally not printed — the AST debug printer is used
      // in tests that compare strings, and including the URL would force
      // every test to carry an expected URL. The URL flows through to WASM
      // emission but isn't part of the structural AST view.
      out.push_str("Module");
      print_exprs(exprs, out, depth);
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
  use crate::lexer::{Loc, Pos};

  fn loc() -> Loc {
    Loc { start: Pos { idx: 0, line: 1, col: 0 }, end: Pos { idx: 0, line: 1, col: 0 } }
  }

  fn node(kind: NodeKind) -> Node {
    Node::new(kind, loc())
  }

  #[test]
  fn print_patterns_empty() {
    let tree = node(NodeKind::Patterns(Exprs::empty()));
    assert_eq!(tree.print(), "Patterns");
  }

  #[test]
  fn build_index_returns_nodes_by_id() {
    let r = crate::parser::parse("foo = 1", "test").unwrap();
    // Module + Bind + Ident + LitInt = 4 nodes
    assert_eq!(r.node_count, 4);
    let index = super::build_index(&r);
    // Verify each slot is populated and id matches position
    for i in 0..4 {
      let node = index.get(AstId(i)).unwrap();
      assert_eq!(node.id, AstId(i));
    }
  }

  #[test]
  fn walk_visits_all_nodes() {
    let r = crate::parser::parse("foo = [1, 2]", "test").unwrap();
    let mut kinds = vec![];
    super::walk(&r.root, &mut |n| {
      kinds.push(std::mem::discriminant(&n.kind));
    });
    // Module, Bind, Ident("foo"), LitSeq, LitInt("1"), LitInt("2") = 6 nodes
    assert_eq!(kinds.len(), 6);
  }

  #[test]
  fn walk_visits_in_pre_order() {
    let r = crate::parser::parse("a + b", "test").unwrap();
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

  // --- Step A: AstBuilder / Ast arena tests ---

  #[test]
  fn builder_append_returns_monotonic_ids() {
    let mut b = AstBuilder::new();
    let a = b.append(NodeKind::Ident("a"), loc());
    let b_id = b.append(NodeKind::Ident("b"), loc());
    let c = b.append(NodeKind::Ident("c"), loc());
    assert_eq!(a, AstId(0));
    assert_eq!(b_id, AstId(1));
    assert_eq!(c, AstId(2));
    assert_eq!(b.len(), 3);
  }

  #[test]
  fn builder_append_overwrites_node_id() {
    // Node::new stamps id=AstId(0); AstBuilder::append must replace it with
    // the real allocation slot so the stored node's id always matches its
    // arena position.
    let mut b = AstBuilder::new();
    let _ = b.append(NodeKind::Ident("first"), loc());
    let id = b.append(NodeKind::Ident("second"), loc());
    assert_eq!(b.read(id).id, AstId(1));
  }

  #[test]
  fn builder_read_returns_appended_node() {
    let mut b = AstBuilder::new();
    let id = b.append(NodeKind::Ident("hello"), loc());
    match &b.read(id).kind {
      NodeKind::Ident(name) => assert_eq!(*name, "hello"),
      _ => panic!("expected Ident"),
    }
  }

  #[test]
  fn builder_finish_preserves_all_nodes() {
    let mut b = AstBuilder::new();
    let _ = b.append(NodeKind::Ident("x"), loc());
    let root = b.append(NodeKind::Ident("root"), loc());
    let ast = b.finish(root);
    assert_eq!(ast.nodes.len(), 2);
    assert_eq!(ast.root, root);
    assert!(matches!(ast.nodes.get(AstId(0)).kind, NodeKind::Ident("x")));
    assert!(matches!(ast.nodes.get(AstId(1)).kind, NodeKind::Ident("root")));
  }

  #[test]
  fn builder_from_ast_preserves_arena_and_root() {
    let mut b = AstBuilder::new();
    let a = b.append(NodeKind::Ident("a"), loc());
    let b_id = b.append(NodeKind::Ident("b"), loc());
    let ast = b.finish(a);
    let (builder, root) = AstBuilder::from_ast(ast);
    assert_eq!(root, a);
    assert_eq!(builder.len(), 2);
    // Read an existing id through the reopened builder.
    assert!(matches!(builder.read(b_id).kind, NodeKind::Ident("b")));
  }

  #[test]
  fn builder_append_only_across_pass_boundary() {
    // Simulates a pass: take Ast by value, reopen, append one new node,
    // finish pointing at the new node as the root. Old nodes survive at
    // their original ids.
    let mut b = AstBuilder::new();
    let old_root = b.append(NodeKind::Ident("old"), loc());
    let input = b.finish(old_root);

    let (mut builder, old_root_id) = AstBuilder::from_ast(input);
    assert_eq!(builder.len(), 1);
    let new_root = builder.append(NodeKind::Ident("new"), loc());
    let output = builder.finish(new_root);

    assert_eq!(output.nodes.len(), 2);
    assert_eq!(output.root, new_root);
    // Old id still resolves to the original node — append-only guarantee.
    assert!(matches!(output.nodes.get(old_root_id).kind, NodeKind::Ident("old")));
    assert!(matches!(output.nodes.get(new_root).kind, NodeKind::Ident("new")));
  }

  #[test]
  fn ast_empty_has_module_root() {
    let ast = Ast::empty();
    assert_eq!(ast.nodes.len(), 1);
    assert_eq!(ast.root, AstId(0));
    match &ast.nodes.get(ast.root).kind {
      NodeKind::Module { exprs, url } => {
        assert!(exprs.items.is_empty());
        assert!(exprs.seps.is_empty());
        assert_eq!(url, "");
      }
      _ => panic!("expected Module root"),
    }
  }

  // --- appended_only invariant checker ---

  /// Build a small two-node Ast for the append-only tests.
  fn two_node_ast() -> Ast<'static> {
    let mut b = AstBuilder::new();
    let _ = b.append(NodeKind::Ident("a"), loc());
    let root = b.append(NodeKind::Ident("b"), loc());
    b.finish(root)
  }

  #[test]
  fn appended_only_accepts_identical_asts() {
    let before = two_node_ast();
    let after = before.clone();
    assert!(super::appended_only(&before, &after).is_ok());
  }

  #[test]
  fn ast_clone_produces_independent_snapshot() {
    // The two-handle pass apply pattern relies on being able to take a
    // read-only snapshot of the input Ast before opening the builder
    // over the original. Confirm that cloning works and produces a
    // fully independent arena.
    let before = two_node_ast();
    let snapshot = before.clone();
    assert_eq!(snapshot.nodes.len(), before.nodes.len());
    assert_eq!(snapshot.root, before.root);
    // And mutating the builder (via from_ast on before) does NOT affect
    // the snapshot — this is the critical property for the two-handle rule.
    let (mut builder, _) = AstBuilder::from_ast(before);
    let _ = builder.append(NodeKind::Ident("added"), loc());
    let after = builder.finish(AstId(0));
    assert_eq!(snapshot.nodes.len(), 2);
    assert_eq!(after.nodes.len(), 3);
  }

  #[test]
  fn appended_only_accepts_pure_append() {
    let before = two_node_ast();
    // Simulate a pass that appends one new node.
    let (mut builder, _root) = AstBuilder::from_ast(before.clone());
    let new_root = builder.append(NodeKind::Ident("c"), loc());
    let after = builder.finish(new_root);
    assert!(super::appended_only(&before, &after).is_ok());
    assert_eq!(after.nodes.len(), 3);
    assert_eq!(after.root, new_root);
  }

  #[test]
  fn appended_only_detects_shrinkage() {
    let before = two_node_ast();
    // Construct a smaller "after" Ast manually.
    let mut b = AstBuilder::new();
    let root = b.append(NodeKind::Ident("a"), loc());
    let after = b.finish(root);
    let err = super::appended_only(&before, &after).unwrap_err();
    assert!(err.contains("after.nodes.len() = 1"));
    assert!(err.contains("before.nodes.len() = 2"));
  }

  #[test]
  fn appended_only_detects_slot_mutation() {
    let before = two_node_ast();
    // Build an "after" where slot 0 has been changed.
    let mut b = AstBuilder::new();
    let _ = b.append(NodeKind::Ident("MUTATED"), loc()); // slot 0 rewritten
    let root = b.append(NodeKind::Ident("b"), loc());
    let after = b.finish(root);
    let err = super::appended_only(&before, &after).unwrap_err();
    assert!(err.contains("slot"));
    assert!(err.contains("mutated"));
  }

  #[test]
  fn appended_only_allows_empty_before() {
    // Edge case: an empty "before" vacuously extends to anything.
    let before = Ast { nodes: crate::propgraph::PropGraph::new(), root: AstId(0) };
    let after = two_node_ast();
    assert!(super::appended_only(&before, &after).is_ok());
  }

  #[test]
  fn appended_only_accepts_extend_with_parent_rewrite() {
    // Realistic pass pattern: a pass walks a tree, finds something to change,
    // appends a replacement for the target + an appended copy of the parent
    // pointing at the new child. Old parent + old target survive at their
    // original slots.
    //
    // before: [leaf("x"), group(leaf(0))]   root=1
    // after:  [leaf("x"), group(leaf(0)), leaf("y"), group(leaf(2))] root=3
    let mut b = AstBuilder::new();
    let leaf_x = b.append(NodeKind::Ident("x"), loc());
    let group_old = b.append(
      NodeKind::Group {
        open: Token { kind: crate::lexer::TokenKind::BracketOpen, loc: loc(), src: "(" },
        close: Token { kind: crate::lexer::TokenKind::BracketClose, loc: loc(), src: ")" },
        inner: Box::new(Node::new(NodeKind::Ident("x"), loc())),
      },
      loc(),
    );
    let before = b.finish(group_old);

    // Reopen and simulate a pass: append a fresh leaf then a fresh group
    // pointing at it. Old slots at leaf_x and group_old are left alone.
    let (mut builder, old_root) = AstBuilder::from_ast(before.clone());
    assert_eq!(old_root, group_old);
    let leaf_y = builder.append(NodeKind::Ident("y"), loc());
    let group_new = builder.append(
      NodeKind::Group {
        open: Token { kind: crate::lexer::TokenKind::BracketOpen, loc: loc(), src: "(" },
        close: Token { kind: crate::lexer::TokenKind::BracketClose, loc: loc(), src: ")" },
        inner: Box::new(Node::new(NodeKind::Ident("y"), loc())),
      },
      loc(),
    );
    let after = builder.finish(group_new);

    assert!(super::appended_only(&before, &after).is_ok());
    assert_eq!(after.nodes.len(), 4);
    // Old root still resolves to the original Group node.
    assert!(matches!(after.nodes.get(group_old).kind, NodeKind::Group { .. }));
    // New root is a fresh Group.
    assert_eq!(after.root, group_new);
    assert!(matches!(after.nodes.get(group_new).kind, NodeKind::Group { .. }));
    // And the interim fresh leaf survives too.
    assert!(matches!(after.nodes.get(leaf_y).kind, NodeKind::Ident("y")));
    // Old leaf_x unchanged.
    assert!(matches!(after.nodes.get(leaf_x).kind, NodeKind::Ident("x")));
  }

}
