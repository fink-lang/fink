#![allow(dead_code)]

// TODO: multiline application as infix RHS operand fails —
//   `[3, 7] == seq\n  add 1, 2\n  add 3, 4` gives "unexpected BlockStart".
//   parse_apply doesn't consume BlockStart for indented args in infix position.

use std::collections::HashSet;

use crate::ast::{CmpPart, Exprs, Node, NodeKind};
use crate::lexer::{Lexer, Loc, Token, TokenKind};

// --- error ---

#[derive(Debug)]
pub struct ParseError {
  pub message: String,
  pub loc: Loc,
}

pub type ParseResult<'src> = Result<Node<'src>, ParseError>;

// --- parser ---

pub struct Parser<'src> {
  lexer: Lexer<'src>,
  src: &'src str,
  current: Token<'src>,
  block_names: HashSet<&'static str>,
  next_id: u32,
}

impl<'src> Parser<'src> {
  pub fn new(src: &'src str) -> Self {
    let mut lexer = Lexer::new(src);
    for sep in &[
      b"+" as &[u8],
      b"-",
      b"*",
      b"/",
      b"//",
      b"**",
      b"%",
      b"%%",
      b"/%",
      b"==",
      b"!=",
      b"<",
      b"<=",
      b">",
      b">=",
      b"><",
      b"&",
      b"^",
      b"~",
      b">>",
      b"<<",
      b">>>",
      b"<<<",
      b".",
      b"|",
      b"|=",
      b"=",
      b"..",
      b"...",
    ] {
      lexer.register_separator(sep);
    }
    let current = lexer.next_token();
    let mut block_names = HashSet::new();
    block_names.insert("fn");
    block_names.insert("match");
    let mut p = Parser { lexer, src, current, block_names, next_id: 0 };
    p.skip_trivia();
    p
  }

  pub fn register_block(&mut self, name: &'static str) {
    self.block_names.insert(name);
  }

  fn node(&mut self, kind: NodeKind<'src>, loc: Loc) -> Node<'src> {
    let id = crate::ast::AstId(self.next_id);
    self.next_id += 1;
    Node { id, kind, loc }
  }

  // --- cursor ---

  fn peek(&self) -> &Token<'src> {
    &self.current
  }

  fn bump(&mut self) -> Token<'src> {
    let tok = self.current.clone();
    self.current = self.lexer.next_token();
    self.skip_trivia();
    tok
  }

  fn skip_trivia(&mut self) {
    while matches!(self.current.kind, TokenKind::Comment | TokenKind::CommentStart | TokenKind::CommentText | TokenKind::CommentEnd) {
      self.current = self.lexer.next_token();
    }
  }

  fn at(&self, kind: TokenKind) -> bool {
    self.current.kind == kind
  }

  fn expect(&mut self, kind: TokenKind) -> Result<Token<'src>, ParseError> {
    if self.current.kind == kind {
      Ok(self.bump())
    } else {
      Err(ParseError {
        message: format!("expected {:?}, got {:?}", kind, self.current.kind),
        loc: self.current.loc,
      })
    }
  }

  fn skip_block_tokens(&mut self) {
    while matches!(
      self.current.kind,
      TokenKind::BlockStart | TokenKind::BlockCont | TokenKind::BlockEnd
    ) {
      self.current = self.lexer.next_token();
      self.skip_trivia();
    }
  }

  // --- expression entry ---

  fn parse_expr(&mut self) -> ParseResult<'src> {
    self.parse_binding()
  }

  // --- binding (= and |=, lowest precedence) ---

  fn parse_binding(&mut self) -> ParseResult<'src> {
    let lhs = self.parse_pipe()?;

    if self.at(TokenKind::Sep) && self.peek().src == "=" {
      let op = self.bump();
      self.skip_block_tokens();
      let rhs = self.parse_expr()?;
      let loc = Loc { start: lhs.loc.start, end: rhs.loc.end };
      return Ok(self.node(NodeKind::Bind { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc));
    }

    if self.at(TokenKind::Sep) && self.peek().src == "|=" {
      let op = self.bump();
      self.skip_block_tokens();
      let rhs = self.parse_expr()?;
      let loc = Loc { start: lhs.loc.start, end: rhs.loc.end };
      return Ok(self.node(NodeKind::BindRight { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc));
    }

    Ok(lhs)
  }

  // --- pipe (|) ---

  // Returns true if the current position has a pipe operator,
  // possibly preceded by a BlockCont (multiline pipe continuation).
  // If a BlockCont precedes the "|", it is consumed.
  fn try_consume_pipe(&mut self) -> bool {
    if self.at(TokenKind::Sep) && self.peek().src == "|" {
      self.bump(); // consume "|"
      return true;
    }
    if self.at(TokenKind::BlockCont) {
      // Consume BlockCont and check for "|"
      self.bump();
      if self.at(TokenKind::Sep) && self.peek().src == "|" {
        self.bump(); // consume "|"
        return true;
      }
      // BlockCont consumed but no "|" — problematic (can't put back).
      // This means the BlockCont was a statement separator, not a pipe continuation.
      // We'll handle this gracefully: the parse_expr caller saw BlockCont but it was consumed.
      // This should not happen in valid Fink code at the pipe level.
    }
    false
  }

  fn parse_pipe(&mut self) -> ParseResult<'src> {
    let first = self.parse_apply()?;

    // Check for pipe: inline "|" or multiline "BlockCont |"
    if (self.at(TokenKind::Sep) && self.peek().src == "|")
      || self.at(TokenKind::BlockCont)
    {
      // Try to start a pipe chain
      if self.try_consume_pipe() {
        let mut parts = vec![first];
        parts.push(self.parse_apply()?);
        while self.try_consume_pipe() {
          parts.push(self.parse_apply()?);
        }
        let start = parts[0].loc.start;
        let end = parts.last().unwrap().loc.end;
        return Ok(self.node(NodeKind::Pipe(Exprs { items: parts, seps: vec![] }), Loc { start, end }));
      }
      // try_consume_pipe consumed a BlockCont but found no "|" — not a pipe
      // Return first as-is (the BlockCont was a statement separator, caller handles)
    }

    Ok(first)
  }

  // --- reserved keyword helpers ---

  // Ident tokens that act as infix operators/control flow — cannot start an argument.
  // Future: replace body with a registry lookup.
  fn is_infix_keyword(s: &str) -> bool {
    matches!(s, "and" | "or" | "xor" | "not" | "in")
  }

  // Broader set for apply fast-path dispatch: infix keywords plus literals/special forms
  // that need their own parse path and must not be consumed as a plain ident.
  // Future: replace body with a registry lookup.
  fn is_dispatch_keyword(s: &str) -> bool {
    Self::is_infix_keyword(s) || matches!(s, "true" | "false" | "_" | "try" | "yield")
  }

  // --- application ---

  // Returns true if the current token can start an argument.
  fn is_arg_start(&self) -> bool {
    match self.current.kind {
      TokenKind::Ident => !Self::is_infix_keyword(self.current.src),
      TokenKind::Int
      | TokenKind::Float
      | TokenKind::Decimal
      | TokenKind::Partial
      | TokenKind::StrStart => true,
      TokenKind::BracketOpen => true,
      TokenKind::Sep if self.current.src == "~" => true,
      _ => false,
    }
  }

  // Returns true if the current token is a comma or semicolon.
  fn at_sep(&self) -> bool {
    self.at(TokenKind::Comma) || self.at(TokenKind::Semicolon)
  }

  // Parse an application chain starting with an expression.
  // If the expression is an ident and args follow, builds Apply.
  fn parse_apply(&mut self) -> ParseResult<'src> {
    // Fast path for ident head: bump directly so `..` after ident is not consumed as a range.
    // Skip fast path for keywords/operators that need special handling downstream.
    if self.at(TokenKind::Ident)
      && !Self::is_dispatch_keyword(self.peek().src)
    {
      let name_tok = self.bump();
      let name = name_tok.src;
      let loc = name_tok.loc;

      if name == "fn" { return self.parse_fn(loc); }
      if name == "match" { return self.parse_match_expr(loc); }
      if self.block_names.contains(name) { return self.parse_block(loc, name); }

      // Tagged template string: ident immediately adjacent to StrStart → raw template
      if self.at(TokenKind::StrStart) && self.peek().loc.start.idx == loc.end.idx {
        let func = self.node(NodeKind::Ident(name), loc);
        let raw_str = self.parse_string(true)?;
        let end = raw_str.loc.end;
        return Ok(self.node(NodeKind::Apply { func: Box::new(func), args: Exprs { items: vec![raw_str], seps: vec![] } }, Loc { start: loc.start, end }));
      }

      let func = self.node(NodeKind::Ident(name), loc);
      let result = self.collect_apply_or_block(func, false)?;
      // If no args were collected (bare ident returned), allow infix operators to continue.
      if matches!(result.kind, NodeKind::Ident(_)) {
        return self.parse_infix_from(result, 0);
      }
      return Ok(result);
    }

    let head = self.parse_infix(0)?;

    // Non-ident head: check postfix-tagged application: [1,2,3]foo or (expr)tag
    if self.at(TokenKind::Ident) && self.peek().loc.start.idx == head.loc.end.idx {
      let tag = self.bump();
      let func = self.node(NodeKind::Ident(tag.src), tag.loc);
      let loc = Loc { start: head.loc.start, end: tag.loc.end };
      return Ok(self.node(NodeKind::Apply { func: Box::new(func), args: Exprs { items: vec![head], seps: vec![] } }, loc));
    }

    Ok(head)
  }

  // Like parse_apply but no block detection — used for arm patterns and record keys.
  fn parse_apply_no_block(&mut self) -> ParseResult<'src> {
    // Prefix unary: not, ~
    if self.at(TokenKind::Ident) && self.peek().src == "not" {
      let op_tok = self.bump();
      let operand = self.parse_infix(35)?;
      let loc = Loc { start: op_tok.loc.start, end: operand.loc.end };
      return Ok(self.node(NodeKind::UnaryOp { op: op_tok, operand: Box::new(operand) }, loc));
    }
    if self.at(TokenKind::Sep) && self.peek().src == "~" {
      let op_tok = self.bump();
      let operand = self.parse_infix(130)?;
      let loc = Loc { start: op_tok.loc.start, end: operand.loc.end };
      return Ok(self.node(NodeKind::UnaryOp { op: op_tok, operand: Box::new(operand) }, loc));
    }

    let head = self.parse_infix(0)?;

    if let NodeKind::Ident(name) = head.kind {
      if name == "fn" { return self.parse_fn(head.loc); }
      if name == "match" { return self.parse_match_expr(head.loc); }
      if self.block_names.contains(name) && name != "fn" && name != "match" {
        return self.parse_block(head.loc, name);
      }
      let func = self.node(NodeKind::Ident(name), head.loc);

      // Tagged template string in argument position: ident immediately adjacent to StrStart
      if self.at(TokenKind::StrStart) && self.peek().loc.start.idx == func.loc.end.idx {
        let raw_str = self.parse_string(true)?;
        let end = raw_str.loc.end;
        return Ok(self.node(
          NodeKind::Apply { func: Box::new(func), args: Exprs { items: vec![raw_str], seps: vec![] } },
          Loc { start: head.loc.start, end },
        ));
      }

      return self.collect_apply_args_no_block(func, true); // no block detection, nested=true to not eat ":"
    }

    if self.at(TokenKind::Ident) && self.peek().loc.start.idx == head.loc.end.idx {
      let tag = self.bump();
      let func = self.node(NodeKind::Ident(tag.src), tag.loc);
      let loc = Loc { start: head.loc.start, end: tag.loc.end };
      return Ok(self.node(NodeKind::Apply { func: Box::new(func), args: Exprs { items: vec![head], seps: vec![] } }, loc));
    }

    Ok(head)
  }

  // Collect args for a function application OR detect block syntax.
  // If after collecting args we see ":", treat as a block.
  // Only called for non-keyword idents (not fn/match/registered blocks).
  fn collect_apply_or_block(&mut self, func: Node<'src>, inside_nested: bool) -> ParseResult<'src> {
    let func_loc = func.loc;
    // Collect params (args for potential block or args for application)
    let mut params: Vec<Node<'src>> = vec![];
    let mut last_end = func_loc.end.idx;

    // Use the same logic as collect_apply_args but we collect into `params`
    // and check for ":" after
    let mut has_block_tok = self.at(TokenKind::BlockStart);
    if has_block_tok {
      self.bump();
    }
    loop {
      if self.at(TokenKind::EOF) || self.at(TokenKind::BlockEnd) { break; }
      if self.at(TokenKind::BlockCont) {
        if has_block_tok { self.bump(); last_end = 0; continue; }
        break;
      }
      let tok_start = self.peek().loc.start.idx;
      let has_ws = tok_start > last_end;
      let is_spread = has_ws && self.at(TokenKind::Sep) && self.peek().src == "..";
      if !self.is_arg_start() && !is_spread { break; }
      // Peek: if next is an ident followed by ":", it may be a block name in arg position.
      // We use parse_apply_no_block for params; if the last param (an ident) is immediately
      // followed by ":", the block-detection check at the end handles it.
      let arg = if is_spread { self.parse_spread()? } else { self.parse_apply_no_block()? };
      last_end = arg.loc.end.idx;
      params.push(arg);
      if self.at(TokenKind::Comma) {
        self.bump();
        // Handle trailing comma that continues onto an indented next line
        if self.at(TokenKind::BlockStart) {
          has_block_tok = true;
          self.bump();
        } else if self.at(TokenKind::BlockCont) && has_block_tok {
          self.bump();
        }
        continue;
      }
      if self.at(TokenKind::Semicolon) {
        if inside_nested { break; }
        self.bump();
        if self.is_arg_start() { params.push(self.parse_apply_no_block()?); }
        break;
      }
      // No separator: continue if more args follow
    }
    if has_block_tok {
      while self.at(TokenKind::BlockCont) { self.bump(); }
      if self.at(TokenKind::BlockEnd) { self.bump(); }
    }

    // Check for block syntax.
    // Rules:
    //   func: body             → Block(func, [], body)
    //   func a, b: body        → Block(func, [a, b], body)   [func is the outer ident]
    //   outer inner: body      → Apply(outer, Block(inner, [], body))   [last arg is block name]
    //   outer inner a, b: body → Apply(outer, Block(inner, [a, b], body))  [last arg is block with params]
    if self.at(TokenKind::Colon) {
      if params.is_empty() {
        // "func: body" — func is the block name, no params
        let params_node = self.node(NodeKind::Patterns(Exprs::empty()), func_loc);
        let (sep, body) = self.parse_colon_body_or_arms()?;
        let end = body.last().map(|n: &Node| n.loc.end).unwrap_or(func_loc.end);
        return Ok(self.node(
          NodeKind::Block { name: Box::new(func), params: Box::new(params_node), sep, body: Exprs { items: body, seps: vec![] } },
          Loc { start: func_loc.start, end },
        ));
      }

      // Has params. Check if last param is ident or apply — it could be a nested block
      // OR all params belong to func as a block.
      // Heuristic: if there are params and they all contain non-ident things, or there are
      // multiple comma-separated items, func is the block name.
      // Key test: "test_block a, b: body" vs "log test_block: body"
      // In "log test_block: body": only ONE param (test_block), which is a bare ident. → test_block is block.
      // In "test_block a, b: body": TWO params (a, b). → test_block is the block.
      // In "log test_block a, b: body": THREE params (test_block, a, b), last two are a, b.
      //   Wait, this doesn't appear in tests. Let's not over-engineer.
      //
      // Decision: if there's exactly ONE param and it's a bare ident, treat THAT ident as the block.
      // Otherwise, func is the block name.
      if params.len() == 1 {
        if let NodeKind::Ident(_) = &params[0].kind {
          // One bare ident param followed by ":": the ident is the block name
          let block_name = params.remove(0);
          let block_start = block_name.loc.start;
          let params_node = self.node(NodeKind::Patterns(Exprs::empty()), block_name.loc);
          let (sep, body) = self.parse_colon_body_or_arms()?;
          let end = body.last().map(|n: &Node| n.loc.end).unwrap_or(block_name.loc.end);
          let block_node = self.node(
            NodeKind::Block { name: Box::new(block_name), params: Box::new(params_node), sep, body: Exprs { items: body, seps: vec![] } },
            Loc { start: block_start, end },
          );
          // Wrap in Apply(func, [block_node])
          return Ok(self.node(
            NodeKind::Apply { func: Box::new(func), args: Exprs { items: vec![block_node], seps: vec![] } },
            Loc { start: func_loc.start, end },
          ));
        }
      }

      // Func is the block name, params are its patterns
      let params_end = params.last().map(|n: &Node| n.loc.end).unwrap_or(func_loc.end);
      let params_loc = Loc { start: func_loc.end, end: params_end };
      let params_node = self.node(NodeKind::Patterns(Exprs { items: params, seps: vec![] }), params_loc);
      let (sep, body) = self.parse_colon_body_or_arms()?;
      let end = body.last().map(|n: &Node| n.loc.end).unwrap_or(params_loc.end);
      return Ok(self.node(
        NodeKind::Block { name: Box::new(func), params: Box::new(params_node), sep, body: Exprs { items: body, seps: vec![] } },
        Loc { start: func_loc.start, end },
      ));
    }

    // Not a block: it's a regular application
    if params.is_empty() {
      return Ok(func);
    }
    let end = params.last().unwrap().loc.end;
    let loc = Loc { start: func_loc.start, end };
    Ok(self.node(NodeKind::Apply { func: Box::new(func), args: Exprs { items: params, seps: vec![] } }, loc))
  }

  // Collect args for a function application.
  // If `inside_nested` is true, stop at semicolons without consuming them.
  // If `no_block` is true, use parse_single_arg_no_block (disables block detection for sub-args).
  fn collect_apply_args(&mut self, func: Node<'src>, inside_nested: bool) -> ParseResult<'src> {
    self.collect_apply_args_inner(func, inside_nested, false)
  }

  fn collect_apply_args_no_block(&mut self, func: Node<'src>, inside_nested: bool) -> ParseResult<'src> {
    self.collect_apply_args_inner(func, inside_nested, true)
  }

  fn collect_apply_args_inner(&mut self, func: Node<'src>, inside_nested: bool, no_block: bool) -> ParseResult<'src> {
    let mut args = vec![];
    let mut last_end = func.loc.end.idx;

    // Check for multiline indented args block
    let mut has_block = self.at(TokenKind::BlockStart);
    if has_block {
      self.bump(); // consume BlockStart
    }

    loop {
      if self.at(TokenKind::EOF) { break; }
      if self.at(TokenKind::BlockEnd) { break; }
      if self.at(TokenKind::BlockCont) {
        if has_block {
          // multiline arg separator — check if next line continues with |= or | for pipe/bind
          // Peek ahead: if BlockCont followed by |= or |, stop
          self.bump(); // consume BlockCont
          last_end = 0; // new line — whitespace implied
          if (self.at(TokenKind::Sep) && (self.peek().src == "|=" || self.peek().src == "|"))
            || self.at(TokenKind::EOF)
            || self.at(TokenKind::BlockEnd)
          {
            break;
          }
          // Otherwise, continue collecting args on the next line
          continue;
        }
        break;
      }

      let has_ws = self.peek().loc.start.idx > last_end;
      let is_spread = has_ws && self.at(TokenKind::Sep) && self.peek().src == "..";
      if !self.is_arg_start() && !is_spread { break; }

      // Semicolon between args: the NEXT arg is a strong-grouped arg for the OUTER function
      // Actually: semicolon terminates the current nested app; outer collects next arg
      if inside_nested && self.at(TokenKind::Semicolon) { break; }

      // Parse one arg
      let arg = if is_spread {
        self.parse_spread()?
      } else if no_block {
        self.parse_single_arg_no_block()?
      } else {
        self.parse_single_arg()?
      };
      last_end = arg.loc.end.idx;
      args.push(arg);

      // Check separators between args
      if self.at(TokenKind::Comma) {
        self.bump();
        // Trailing comma: allow continuation on next line
        if self.at(TokenKind::BlockStart) {
          has_block = true;
          self.bump();
        } else if self.at(TokenKind::BlockCont) && has_block {
          self.bump(); // consume BlockCont after trailing comma
        }
        // Continue to next arg
        continue;
      }

      if self.at(TokenKind::Semicolon) {
        if inside_nested {
          // Leave the semicolon for the outer function to handle
          break;
        }
        // Outer function: semicolon is a strong boundary.
        // Collect ONE more grouped arg from after the semicolon.
        self.bump();
        if self.is_arg_start() {
          let grouped = if no_block { self.parse_single_arg_no_block()? } else { self.parse_single_arg()? };
          args.push(grouped);
        }
        break;
      }

      // No separator: continue if there are more args (no-comma space-separated args)
      // The loop top checks is_arg_start, so we just continue.
      // (Bare-ident args consumed everything via nested collect_apply_args, so no extra args remain)
    }

    if has_block {
      while self.at(TokenKind::BlockCont) { self.bump(); }
      if self.at(TokenKind::BlockEnd) { self.bump(); }
    }

    if args.is_empty() {
      return Ok(func);
    }

    let end = args.last().unwrap().loc.end;
    let loc = Loc { start: func.loc.start, end };
    Ok(self.node(NodeKind::Apply { func: Box::new(func), args: Exprs { items: args, seps: vec![] } }, loc))
  }

  // Parse one argument. If `no_block` is true, block detection is disabled.
  fn parse_single_arg(&mut self) -> ParseResult<'src> {
    self.parse_single_arg_inner(false)
  }

  fn parse_single_arg_no_block(&mut self) -> ParseResult<'src> {
    self.parse_single_arg_inner(true)
  }

  fn parse_single_arg_inner(&mut self, no_block: bool) -> ParseResult<'src> {
    // Ident: may start a nested application or block
    // Skip fast path for keywords/operators handled by parse_infix.
    if self.at(TokenKind::Ident)
      && !Self::is_dispatch_keyword(self.peek().src)
    {
      let name_tok = self.peek().clone();
      // Special keywords get full parse
      if name_tok.src == "fn" {
        self.bump();
        return self.parse_fn(name_tok.loc);
      }
      if name_tok.src == "match" {
        self.bump();
        return self.parse_match_expr(name_tok.loc);
      }
      if self.block_names.contains(name_tok.src) {
        self.bump();
        return self.parse_block(name_tok.loc, name_tok.src);
      }
      // Bump the ident directly so that `..` after it is not consumed as a range operator.
      self.bump();
      let func = self.node(NodeKind::Ident(name_tok.src), name_tok.loc);
      let result = if no_block {
        self.collect_apply_args(func, true)?
      } else {
        self.collect_apply_or_block(func, true)?
      };
      // If no args were collected (bare ident returned), allow infix operators to continue.
      if matches!(result.kind, NodeKind::Ident(_)) {
        return self.parse_infix_from(result, 0);
      }
      return Ok(result);
    }

    // Non-ident: parse as infix expression (no application)
    self.parse_infix(0)
  }

  // --- Pratt infix operator parser ---

  fn infix_bp(tok: &Token) -> Option<(u8, u8)> {
    match tok.kind {
      TokenKind::Ident => match tok.src {
        "or" | "xor" => Some((20, 21)),
        "and" => Some((30, 31)),
        "in" => Some((40, 41)),
        // "not in" handled specially in parse_infix loop
        _ => None,
      },
      TokenKind::Sep => match tok.src {
        ".." | "..." => Some((50, 51)),
        "==" | "!=" | "<" | "<=" | ">" | ">=" | "><" => Some((60, 61)),
        "&" => Some((70, 71)),
        "^" => Some((80, 81)),
        ">>" | "<<" | ">>>" | "<<<" => Some((90, 91)),
        "+" | "-" => Some((100, 101)),
        "*" | "/" | "//" | "%" | "%%" | "/%" => Some((110, 111)),
        "**" => Some((121, 120)), // right-associative
        "." => Some((140, 141)),
        _ => None,
      },
      _ => None,
    }
  }

  fn is_range_op(tok: &Token) -> bool {
    tok.kind == TokenKind::Sep && matches!(tok.src, ".." | "...")
  }

  fn is_cmp_op(tok: &Token) -> bool {
    // Comparison ops sit at bp 60 in infix_bp; "in" is also a comparison.
    matches!(Self::infix_bp(tok), Some((60, 61)))
      || (tok.kind == TokenKind::Ident && tok.src == "in")
  }

  fn parse_infix(&mut self, min_bp: u8) -> ParseResult<'src> {
    let mut lhs = self.parse_unary_or_atom()?;

    // If the atom is a bare ident followed by a BlockStart, collect block args.
    // This handles infix RHS like `a == seq\n  add 1, 2` where `seq` would
    // otherwise be returned bare, leaving the BlockStart as an unexpected token.
    if matches!(lhs.kind, NodeKind::Ident(_)) && self.at(TokenKind::BlockStart) {
      lhs = self.collect_apply_or_block(lhs, false)?;
    }

    loop {
      // Check for "not in" two-token operator
      if min_bp <= 40
        && self.at(TokenKind::Ident)
        && self.peek().src == "not"
      {
        // Speculatively consume "not" — check if next is "in"
        let not_tok = self.bump();
        if self.at(TokenKind::Ident) && self.peek().src == "in" {
          let in_tok = self.bump();
          // Construct a token spanning "not in" from source
          let op = Token {
            kind: TokenKind::Sep,
            loc: Loc { start: not_tok.loc.start, end: in_tok.loc.end },
            src: &self.src[not_tok.loc.start.idx as usize..in_tok.loc.end.idx as usize],
          };
          let rhs = self.parse_infix(41)?;
          let loc = Loc { start: lhs.loc.start, end: rhs.loc.end };
          lhs = self.node(
            NodeKind::InfixOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) },
            loc,
          );
          continue;
        } else {
          // "not" wasn't followed by "in": treat as unary and restart
          // We've consumed "not" — parse what follows as its operand
          let operand = self.parse_infix(35)?;
          let loc = Loc { start: not_tok.loc.start, end: operand.loc.end };
          lhs = self.node(
            NodeKind::UnaryOp { op: not_tok, operand: Box::new(operand) },
            loc,
          );
          break;
        }
      }

      let tok = self.peek().clone();
      let Some((l_bp, r_bp)) = Self::infix_bp(&tok) else { break };
      if l_bp < min_bp { break; }

      // Member access: foo.bar or foo.(expr)
      if tok.kind == TokenKind::Sep && tok.src == "." {
        let dot = self.bump(); // consume "."
        let rhs = if self.at(TokenKind::BracketOpen) && self.peek().src == "(" {
          // computed: foo.(expr)
          self.parse_group()?
        } else {
          let t = self.expect(TokenKind::Ident)?;
          self.node(NodeKind::Ident(t.src), t.loc)
        };
        let loc = Loc { start: lhs.loc.start, end: rhs.loc.end };
        lhs = self.node(NodeKind::Member { op: dot, lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc);
        continue;
      }

      // Range: .. and ...
      if Self::is_range_op(&tok) {
        let op = self.bump();
        let rhs = self.parse_infix(r_bp)?;
        let loc = Loc { start: lhs.loc.start, end: rhs.loc.end };
        lhs = self.node(
          NodeKind::InfixOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) },
          loc,
        );
        continue;
      }

      // Comparison operators: chain into ChainedCmp or single InfixOp
      if Self::is_cmp_op(&tok) {
        let first_op = self.bump();
        let first_rhs = self.parse_infix(r_bp)?;

        // Check if next is also a comparison op (chained)
        if Self::is_cmp_op(self.peek()) {
          let mut parts = vec![
            CmpPart::Operand(lhs),
            CmpPart::Op(first_op),
            CmpPart::Operand(first_rhs),
          ];
          while Self::is_cmp_op(self.peek()) {
            let next_op = self.bump();
            let next_rhs = self.parse_infix(r_bp)?;
            parts.push(CmpPart::Op(next_op));
            parts.push(CmpPart::Operand(next_rhs));
          }
          let start = if let CmpPart::Operand(n) = &parts[0] { n.loc.start } else { unreachable!() };
          let end = if let CmpPart::Operand(n) = parts.last().unwrap() { n.loc.end } else { unreachable!() };
          lhs = self.node(NodeKind::ChainedCmp(parts), Loc { start, end });
        } else {
          // Single comparison
          let loc = Loc { start: lhs.loc.start, end: first_rhs.loc.end };
          lhs = self.node(
            NodeKind::InfixOp { op: first_op, lhs: Box::new(lhs), rhs: Box::new(first_rhs) },
            loc,
          );
        }
        continue;
      }

      // General infix
      let op = self.bump();
      let rhs = self.parse_infix(r_bp)?;
      let loc = Loc { start: lhs.loc.start, end: rhs.loc.end };
      lhs = self.node(
        NodeKind::InfixOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) },
        loc,
      );
    }

    Ok(lhs)
  }

  // Like parse_infix but starts with an already-parsed lhs node.
  fn parse_infix_from(&mut self, lhs: Node<'src>, min_bp: u8) -> ParseResult<'src> {
    // Reuse parse_infix by temporarily wrapping — simpler: inline the loop entry.
    // We create a temporary closure that mimics parse_infix's loop but skips the initial atom parse.
    let mut lhs = lhs;
    loop {
      if min_bp <= 40
        && self.at(TokenKind::Ident)
        && self.peek().src == "not"
      {
        let not_tok = self.bump();
        if self.at(TokenKind::Ident) && self.peek().src == "in" {
          let in_tok = self.bump();
          let op = Token {
            kind: TokenKind::Sep,
            loc: Loc { start: not_tok.loc.start, end: in_tok.loc.end },
            src: &self.src[not_tok.loc.start.idx as usize..in_tok.loc.end.idx as usize],
          };
          let rhs = self.parse_infix(41)?;
          let loc = Loc { start: lhs.loc.start, end: rhs.loc.end };
          lhs = self.node(NodeKind::InfixOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc);
          continue;
        } else {
          let operand = self.parse_infix(35)?;
          let loc = Loc { start: not_tok.loc.start, end: operand.loc.end };
          lhs = self.node(NodeKind::UnaryOp { op: not_tok, operand: Box::new(operand) }, loc);
          break;
        }
      }
      let tok = self.peek().clone();
      let Some((l_bp, r_bp)) = Self::infix_bp(&tok) else { break };
      if l_bp < min_bp { break; }
      if tok.kind == TokenKind::Sep && tok.src == "." {
        let dot = self.bump();
        let rhs = if self.at(TokenKind::BracketOpen) && self.peek().src == "(" {
          self.parse_group()?
        } else {
          let t = self.expect(TokenKind::Ident)?;
          self.node(NodeKind::Ident(t.src), t.loc)
        };
        let loc = Loc { start: lhs.loc.start, end: rhs.loc.end };
        lhs = self.node(NodeKind::Member { op: dot, lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc);
        continue;
      }
      if Self::is_range_op(&tok) {
        let op = self.bump();
        let rhs = self.parse_infix(r_bp)?;
        let loc = Loc { start: lhs.loc.start, end: rhs.loc.end };
        lhs = self.node(NodeKind::InfixOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc);
        continue;
      }
      if Self::is_cmp_op(&tok) {
        let first_op = self.bump();
        let first_rhs = self.parse_infix(r_bp)?;
        if Self::is_cmp_op(self.peek()) {
          let mut parts = vec![CmpPart::Operand(lhs), CmpPart::Op(first_op), CmpPart::Operand(first_rhs)];
          while Self::is_cmp_op(self.peek()) {
            let next_op = self.bump();
            let next_rhs = self.parse_infix(r_bp)?;
            parts.push(CmpPart::Op(next_op));
            parts.push(CmpPart::Operand(next_rhs));
          }
          let start = if let CmpPart::Operand(n) = &parts[0] { n.loc.start } else { unreachable!() };
          let end = if let CmpPart::Operand(n) = parts.last().unwrap() { n.loc.end } else { unreachable!() };
          lhs = self.node(NodeKind::ChainedCmp(parts), Loc { start, end });
        } else {
          let loc = Loc { start: lhs.loc.start, end: first_rhs.loc.end };
          lhs = self.node(NodeKind::InfixOp { op: first_op, lhs: Box::new(lhs), rhs: Box::new(first_rhs) }, loc);
        }
        continue;
      }
      let op = self.bump();
      let rhs = self.parse_infix(r_bp)?;
      let loc = Loc { start: lhs.loc.start, end: rhs.loc.end };
      lhs = self.node(NodeKind::InfixOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) }, loc);
    }
    Ok(lhs)
  }

  fn parse_unary_or_atom(&mut self) -> ParseResult<'src> {
    // "try" — unwrap Ok or propagate Err; parsed like application
    if self.at(TokenKind::Ident) && self.peek().src == "try" {
      let try_tok = self.bump();
      let inner = self.parse_apply()?;
      let loc = Loc { start: try_tok.loc.start, end: inner.loc.end };
      return Ok(self.node(NodeKind::Try(Box::new(inner)), loc));
    }
    // "yield" — suspend execution, yield a value; parsed like application
    if self.at(TokenKind::Ident) && self.peek().src == "yield" {
      let yield_tok = self.bump();
      let inner = self.parse_apply()?;
      let loc = Loc { start: yield_tok.loc.start, end: inner.loc.end };
      return Ok(self.node(NodeKind::Yield(Box::new(inner)), loc));
    }
    // "not" prefix unary — bp 35 so it binds tighter than and/or but looser than comparisons
    if self.at(TokenKind::Ident) && self.peek().src == "not" {
      let op_tok = self.bump();
      let operand = self.parse_infix(35)?;
      let loc = Loc { start: op_tok.loc.start, end: operand.loc.end };
      return Ok(self.node(NodeKind::UnaryOp { op: op_tok, operand: Box::new(operand) }, loc));
    }
    // "~" bitwise not
    if self.at(TokenKind::Sep) && self.peek().src == "~" {
      let op_tok = self.bump();
      let operand = self.parse_infix(130)?;
      let loc = Loc { start: op_tok.loc.start, end: operand.loc.end };
      return Ok(self.node(NodeKind::UnaryOp { op: op_tok, operand: Box::new(operand) }, loc));
    }
    // Handle prefix sign: +/- followed by number.
    // Consume the sign, check adjacency.
    if self.at(TokenKind::Sep) && (self.peek().src == "+" || self.peek().src == "-") {
      let sign = self.bump();
      let adjacent = self.peek().loc.start.idx == sign.loc.end.idx;

      if adjacent {
        match self.peek().kind {
          TokenKind::Int | TokenKind::Float | TokenKind::Decimal => {
            let num = self.bump();
            let src = &self.src[sign.loc.start.idx as usize..num.loc.end.idx as usize];
            let loc = Loc { start: sign.loc.start, end: num.loc.end };
            return Ok(match num.kind {
              TokenKind::Int => self.node(NodeKind::LitInt(src), loc),
              TokenKind::Float => self.node(NodeKind::LitFloat(src), loc),
              TokenKind::Decimal => self.node(NodeKind::LitDecimal(src), loc),
              _ => unreachable!(),
            });
          }
          _ => {}
        }
      }

      // Not adjacent to a number, or not followed by a number: treat as unary
      if sign.src == "-" {
        let operand = self.parse_unary_or_atom()?;
        let loc = Loc { start: sign.loc.start, end: operand.loc.end };
        return Ok(self.node(
          NodeKind::UnaryOp { op: sign, operand: Box::new(operand) },
          loc,
        ));
      }

      // "+" without adjacent number is not valid
      return Err(ParseError {
        message: "unexpected '+'".into(),
        loc: sign.loc,
      });
    }

    self.parse_atom()
  }

  // --- string helpers ---

  fn hex_digit(b: u8) -> Option<u8> {
    match b {
      b'0'..=b'9' => Some(b - b'0'),
      b'a'..=b'f' => Some(b - b'a' + 10),
      b'A'..=b'F' => Some(b - b'A' + 10),
      _ => None,
    }
  }

  pub fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
      if bytes[i] == b'\\' && i + 1 < bytes.len() {
        i += 1;
        match bytes[i] {
          b'n' => out.push('\n'),
          b't' => out.push('\t'),
          b'r' => out.push('\r'),
          b'v' => out.push('\x0B'),
          b'b' => out.push('\x08'),
          b'f' => out.push('\x0C'),
          b'\'' => out.push('\''),
          b'\\' => out.push('\\'),
          b'$' => out.push('$'),
          b'x' => {
            // \xNN — two hex digits
            let hi = Self::hex_digit(bytes.get(i + 1).copied().unwrap_or(0));
            let lo = Self::hex_digit(bytes.get(i + 2).copied().unwrap_or(0));
            if let (Some(hi), Some(lo)) = (hi, lo) {
              out.push((hi << 4 | lo) as char);
              i += 2;
            } else {
              out.push_str("\\x");
            }
          }
          b'u' => {
            // \uNNNN or \uNN_NN_NN — up to 6 hex digits with optional _ separators
            let mut codepoint: u32 = 0;
            let mut digits = 0;
            let mut j = i + 1;
            while j < bytes.len() && digits < 6 {
              match bytes[j] {
                b'_' => { j += 1; }
                b => {
                  if let Some(d) = Self::hex_digit(b) {
                    codepoint = codepoint << 4 | d as u32;
                    digits += 1;
                    j += 1;
                  } else {
                    break;
                  }
                }
              }
            }
            if digits > 0 {
              if let Some(ch) = char::from_u32(codepoint) {
                out.push(ch);
              }
              i = j - 1;
            } else {
              out.push_str("\\u");
            }
          }
          b => {
            out.push('\\');
            out.push(b as char);
          }
        }
      } else {
        out.push(bytes[i] as char);
      }
      i += 1;
    }
    out
  }

  fn parse_string(&mut self, raw: bool) -> ParseResult<'src> {
    let start_tok = self.expect(TokenKind::StrStart)?;
    let start_loc = start_tok.loc;
    let mut parts: Vec<Node<'src>> = vec![];
    // Track the open token for the next LitStr segment.
    // First segment opens with StrStart; after interpolation, opens with StrExprEnd.
    let mut next_open = start_tok;

    loop {
      match self.peek().kind {
        TokenKind::StrEnd => {
          let end_tok = self.bump();
          // Close the last LitStr segment if present
          self.close_lit_str(&mut parts, end_tok);
          let loc = Loc { start: start_loc.start, end: end_tok.loc.end };
          if !raw && parts.is_empty() {
            return Ok(self.node(NodeKind::LitStr { open: start_tok, close: end_tok, content: String::new() }, loc));
          }
          if !raw && parts.len() == 1 {
            if let NodeKind::LitStr { .. } = &parts[0].kind {
              let mut node = parts.remove(0);
              node.loc = loc;
              return Ok(node);
            }
          }
          let kind = if raw {
            NodeKind::StrRawTempl { open: start_tok, close: end_tok, children: parts }
          } else {
            NodeKind::StrTempl { open: start_tok, close: end_tok, children: parts }
          };
          return Ok(self.node(kind, loc));
        }
        TokenKind::StrText => {
          let t = self.bump();
          let text = t.src.to_string();
          // Merge consecutive StrText tokens into a single LitStr
          if let Some(Node { kind: NodeKind::LitStr { content, .. }, loc: prev_loc, .. }) = parts.last_mut() {
            content.push_str(&text);
            prev_loc.end = t.loc.end;
          } else {
            parts.push(self.node(NodeKind::LitStr { open: next_open, close: next_open, content: text }, t.loc));
          }
        }
        TokenKind::StrExprStart => {
          let expr_start = self.bump();
          // Close the last LitStr segment if present
          self.close_lit_str(&mut parts, expr_start);
          // Inside string interpolation: parse a full expression
          // But spread inside string is special: `..rest` in StrTempl
          let expr = if self.at(TokenKind::Sep) && self.peek().src == ".." {
            self.parse_spread()?
          } else {
            self.parse_expr()?
          };
          let expr_end = self.expect(TokenKind::StrExprEnd)?;
          parts.push(expr);
          next_open = expr_end;
        }
        _ => {
          return Err(ParseError {
            message: format!("unexpected token {:?} in string", self.peek().kind),
            loc: self.peek().loc,
          });
        }
      }
    }
  }

  /// Set the `close` token on the last LitStr in `parts`, if the last element is a LitStr.
  fn close_lit_str(&self, parts: &mut [Node<'src>], close: Token<'src>) {
    if let Some(Node { kind: NodeKind::LitStr { close: c, .. }, .. }) = parts.last_mut() {
      *c = close;
    }
  }

  // --- sequence/record/group/spread ---

  fn parse_seq_items(&mut self) -> Result<Vec<Node<'src>>, ParseError> {
    let mut items = vec![];
    self.skip_block_tokens();
    // Consume leading commas as implicit wildcards: [, , n]
    while self.at(TokenKind::Comma) || self.at(TokenKind::Semicolon) {
      let sep = self.bump();
      items.push(self.node(NodeKind::Wildcard, sep.loc));
      self.skip_block_tokens();
    }
    while !self.at(TokenKind::BracketClose) && !self.at(TokenKind::EOF) {
      if self.at(TokenKind::Sep) && self.peek().src == ".." {
        items.push(self.parse_spread()?);
      } else {
        // Use parse_single_arg so that `;` acts as a seq element separator
        // rather than being consumed as a strong-arg boundary by a nested apply.
        // e.g. `[foo 1, 2; bar 3]` → two elements: `foo(1, 2)` and `bar(3)`.
        // Then check for `=`/`|=` binding, which parse_single_arg does not cover.
        let item = self.parse_single_arg()?;
        let item = if self.at(TokenKind::Sep) && self.peek().src == "=" {
          let op = self.bump();
          self.skip_block_tokens();
          let rhs = self.parse_expr()?;
          let loc = Loc { start: item.loc.start, end: rhs.loc.end };
          self.node(NodeKind::Bind { op, lhs: Box::new(item), rhs: Box::new(rhs) }, loc)
        } else if self.at(TokenKind::Sep) && self.peek().src == "|=" {
          let op = self.bump();
          self.skip_block_tokens();
          let rhs = self.parse_expr()?;
          let loc = Loc { start: item.loc.start, end: rhs.loc.end };
          self.node(NodeKind::BindRight { op, lhs: Box::new(item), rhs: Box::new(rhs) }, loc)
        } else {
          item
        };
        items.push(item);
      }
      self.skip_block_tokens();
      if self.at(TokenKind::Comma) || self.at(TokenKind::Semicolon) {
        let sep = self.bump();
        self.skip_block_tokens();
        // Implicit wildcard: consecutive comma with no expression between
        while self.at(TokenKind::Comma) || self.at(TokenKind::Semicolon) {
          items.push(self.node(NodeKind::Wildcard, sep.loc));
          self.bump();
          self.skip_block_tokens();
        }
      }
    }
    Ok(items)
  }

  fn parse_rec_items(&mut self) -> Result<Vec<Node<'src>>, ParseError> {
    let mut items = vec![];
    self.skip_block_tokens();
    while !self.at(TokenKind::BracketClose) && !self.at(TokenKind::EOF) {
      if self.at(TokenKind::Sep) && self.peek().src == ".." {
        items.push(self.parse_spread()?);
      } else {
        // Parse the key. If it starts with "(", preserve the Group wrapper
        // (computed key: {(expr): val}).
        let first = if self.at(TokenKind::BracketOpen) && self.peek().src == "(" {
          self.parse_group()?  // returns Group(inner)
        } else {
          self.parse_infix(0)?
        };
        self.skip_block_tokens();
        if self.at(TokenKind::Colon) {
          let sep = self.bump();
          self.skip_block_tokens();
          let val = self.parse_expr()?;
          let loc = Loc { start: first.loc.start, end: val.loc.end };
          items.push(self.node(NodeKind::Arm { lhs: Exprs { items: vec![first], seps: vec![] }, sep, body: Exprs { items: vec![val], seps: vec![] } }, loc));
        } else {
          items.push(first);
        }
      }
      self.skip_block_tokens();
      if self.at(TokenKind::Comma) || self.at(TokenKind::Semicolon) {
        self.bump();
        self.skip_block_tokens();
      }
    }
    Ok(items)
  }

  fn parse_spread(&mut self) -> ParseResult<'src> {
    let op_tok = self.bump(); // consume ".."
    let start = op_tok.loc.start;

    let maybe_inner: Option<Node<'src>> = if self.at(TokenKind::BracketOpen) && self.peek().src == "(" {
      // ..(expr) — parse group and strip the Group wrapper
      let group = self.parse_group()?;
      let inner = if let NodeKind::Group { inner, .. } = group.kind { *inner } else { group };
      // ..(expr)..(expr) — `)` directly followed by `..`/`...` is a range, not a second spread
      if Self::is_range_op(self.peek()) {
        Some(self.parse_infix_from(inner, 0)?)
      } else {
        Some(inner)
      }
    } else if (self.is_arg_start() && !self.at_sep()) || self.at(TokenKind::Partial) {
      Some(self.parse_infix(0)?)
    } else {
      None
    };

    if let Some(inner) = maybe_inner {
      // ..(expr) |= name — spread guard with rhs binding
      let node = if self.at(TokenKind::Sep) && self.peek().src == "|=" {
        let op = self.bump();
        self.skip_block_tokens();
        let rhs = self.parse_expr()?;
        let loc = Loc { start: inner.loc.start, end: rhs.loc.end };
        self.node(NodeKind::BindRight { op, lhs: Box::new(inner), rhs: Box::new(rhs) }, loc)
      } else {
        inner
      };
      let end = node.loc.end;
      Ok(self.node(NodeKind::Spread { op: op_tok, inner: Some(Box::new(node)) }, Loc { start, end }))
    } else {
      let loc = op_tok.loc;
      Ok(self.node(NodeKind::Spread { op: op_tok, inner: None }, loc))
    }
  }

  fn parse_group(&mut self) -> ParseResult<'src> {
    let open = self.bump(); // consume "("
    let inner = if self.at(TokenKind::BlockStart) {
      // Multi-expr block group: wrap in a zero-param Fn so the CPS pass
      // can detect "group with bindings" and emit a scope.
      self.bump(); // consume BlockStart
      let exprs = self.parse_block_exprs()?;
      let params = self.node(NodeKind::Patterns(Exprs::empty()), open.loc);
      let sep = Token { kind: TokenKind::Colon, loc: open.loc, src: ":" };
      self.node(NodeKind::Fn { params: Box::new(params), sep, body: Exprs { items: exprs, seps: vec![] } }, open.loc)
    } else {
      self.skip_block_tokens();
      let expr = self.parse_expr()?;
      self.skip_block_tokens();
      expr
    };
    let close = self.expect(TokenKind::BracketClose)?;
    let loc = Loc { start: open.loc.start, end: close.loc.end };
    Ok(self.node(NodeKind::Group { open, close, inner: Box::new(inner) }, loc))
  }

  // --- atom ---

  fn parse_atom(&mut self) -> ParseResult<'src> {
    let tok = self.peek().clone();
    match tok.kind {
      TokenKind::Ident => {
        let t = self.bump();
        let kind = match t.src {
          "true" => NodeKind::LitBool(true),
          "false" => NodeKind::LitBool(false),
          "_" => NodeKind::Wildcard,
          _ => NodeKind::Ident(t.src),
        };
        Ok(self.node(kind, t.loc))
      }
      TokenKind::Int => {
        let t = self.bump();
        Ok(self.node(NodeKind::LitInt(t.src), t.loc))
      }
      TokenKind::Float => {
        let t = self.bump();
        Ok(self.node(NodeKind::LitFloat(t.src), t.loc))
      }
      TokenKind::Decimal => {
        let t = self.bump();
        Ok(self.node(NodeKind::LitDecimal(t.src), t.loc))
      }
      TokenKind::Partial => {
        let t = self.bump();
        Ok(self.node(NodeKind::Partial, t.loc))
      }
      TokenKind::StrStart => self.parse_string(false),
      TokenKind::BracketOpen if tok.src == "[" => {
        let open = self.bump();
        let items = self.parse_seq_items()?;
        let close = self.expect(TokenKind::BracketClose)?;
        let loc = Loc { start: open.loc.start, end: close.loc.end };
        Ok(self.node(NodeKind::LitSeq { open, close, items: Exprs { items, seps: vec![] } }, loc))
      }
      TokenKind::BracketOpen if tok.src == "{" => {
        let open = self.bump();
        let items = self.parse_rec_items()?;
        let close = self.expect(TokenKind::BracketClose)?;
        let loc = Loc { start: open.loc.start, end: close.loc.end };
        Ok(self.node(NodeKind::LitRec { open, close, items: Exprs { items, seps: vec![] } }, loc))
      }
      TokenKind::BracketOpen if tok.src == "(" => {
        // Parenthesised group — inner is a full expression (may be application)
        let group = self.parse_group()?;
        let group_end = group.loc.end;
        // Postfix tag: (expr)tag where tag is immediately adjacent
        if self.at(TokenKind::Ident) && self.peek().loc.start.idx == group_end.idx {
          let tag = self.bump();
          let inner = if let NodeKind::Group { inner, .. } = group.kind { *inner } else { group };
          let func = self.node(NodeKind::Ident(tag.src), tag.loc);
          let loc = Loc { start: inner.loc.start, end: tag.loc.end };
          return Ok(self.node(NodeKind::Apply { func: Box::new(func), args: Exprs { items: vec![inner], seps: vec![] } }, loc));
        }
        // Preserve Group — it's explicit syntax and semantically significant (e.g. partial scope boundary)
        Ok(group)
      }
      _ => Err(ParseError {
        message: format!("unexpected token {:?}", tok.kind),
        loc: tok.loc,
      }),
    }
  }

  // --- fn ---

  fn parse_fn(&mut self, fn_loc: Loc) -> ParseResult<'src> {
    // "fn" already consumed
    let is_fn_match = self.at(TokenKind::Ident) && self.peek().src == "match";
    if is_fn_match {
      self.bump(); // consume "match"
    }

    let (params, _) = self.parse_params()?;

    let fn_end_loc;
    if is_fn_match {
      // fn match: parse arms as the body
      let (sep, arms) = self.parse_colon_arms()?;
      let subjects = params.clone();
      let match_end = arms.last().map(|n: &Node| n.loc.end).unwrap_or(subjects.loc.end);
      fn_end_loc = Loc { start: fn_loc.start, end: match_end };
      let match_node = self.node(
        NodeKind::Match { subjects: Box::new(subjects), sep: sep.clone(), arms: Exprs { items: arms, seps: vec![] } },
        fn_end_loc,
      );
      Ok(self.node(
        NodeKind::Fn { params: Box::new(params), sep, body: Exprs { items: vec![match_node], seps: vec![] } },
        fn_end_loc,
      ))
    } else {
      let (sep, body) = self.parse_colon_body()?;
      let end = body.last().map(|n: &Node| n.loc.end).unwrap_or(params.loc.end);
      fn_end_loc = Loc { start: fn_loc.start, end };
      Ok(self.node(NodeKind::Fn { params: Box::new(params), sep, body: Exprs { items: body, seps: vec![] } }, fn_end_loc))
    }
  }

  // Parse comma-separated params until ":"
  fn parse_params(&mut self) -> Result<(Node<'src>, Loc), ParseError> {
    let start = self.peek().loc.start;
    let mut items: Vec<Node<'src>> = vec![];

    // Leading commas as implicit wildcards: fn , , c: c
    while self.at(TokenKind::Comma) {
      let sep = self.bump();
      items.push(self.node(NodeKind::Wildcard, sep.loc));
    }

    while !self.at(TokenKind::Colon) && !self.at(TokenKind::EOF) {
      if self.at(TokenKind::Sep) && self.peek().src == ".." {
        items.push(self.parse_spread()?);
      } else {
        // Parse param without block detection (no `:` consumption).
        // Also support default args: name = 'default'.
        let param = self.parse_apply_no_block()?;
        let param = if self.at(TokenKind::Sep) && self.peek().src == "=" {
          let op = self.bump();
          let rhs = self.parse_infix(0)?;
          let loc = Loc { start: param.loc.start, end: rhs.loc.end };
          self.node(NodeKind::Bind { op, lhs: Box::new(param), rhs: Box::new(rhs) }, loc)
        } else {
          param
        };
        items.push(param);
      }
      if self.at(TokenKind::Comma) {
        let sep = self.bump();
        // Trailing comma: continue onto indented next line
        if self.at(TokenKind::BlockStart) { self.bump(); }
        else if self.at(TokenKind::BlockCont) { self.bump(); }
        // Consecutive commas as implicit wildcards
        while self.at(TokenKind::Comma) {
          items.push(self.node(NodeKind::Wildcard, sep.loc));
          self.bump();
        }
      } else {
        break;
      }
    }

    let end = items.last().map(|n| n.loc.end).unwrap_or(start);
    let loc = Loc { start, end };
    Ok((self.node(NodeKind::Patterns(Exprs { items, seps: vec![] }), loc), loc))
  }

  // Parse ":" then either inline expression(s) or indented block.
  // Returns the colon token and a Vec of statements/arms.
  fn parse_colon_body(&mut self) -> Result<(Token<'src>, Vec<Node<'src>>), ParseError> {
    let sep = self.expect(TokenKind::Colon)?;
    let body = if self.at(TokenKind::BlockStart) {
      self.bump();
      self.parse_block_exprs()?
    } else {
      self.parse_inline_exprs()?
    };
    Ok((sep, body))
  }

  // Parse a comma-separated list of expressions on a single line (no block).
  // `,` and `;` act as expression separators, equivalent to newlines in a block body.
  fn parse_inline_exprs(&mut self) -> Result<Vec<Node<'src>>, ParseError> {
    let mut exprs = vec![self.parse_expr()?];
    while self.at(TokenKind::Comma) || self.at(TokenKind::Semicolon) {
      self.bump();
      exprs.push(self.parse_expr()?);
    }
    Ok(exprs)
  }

  fn parse_block_items<F>(&mut self, mut f: F) -> Result<Vec<Node<'src>>, ParseError>
  where F: FnMut(&mut Self) -> ParseResult<'src> {
    let mut items = vec![];
    loop {
      if self.at(TokenKind::BlockEnd) || self.at(TokenKind::EOF) { break; }
      if self.at(TokenKind::BlockCont) { self.bump(); continue; }
      items.push(f(self)?);
    }
    if self.at(TokenKind::BlockEnd) { self.bump(); }
    Ok(items)
  }

  fn parse_block_exprs(&mut self) -> Result<Vec<Node<'src>>, ParseError> {
    self.parse_block_items(|p| p.parse_expr())
  }

  // Parse BlockStart already consumed: inline expr or indented block.
  fn parse_block_body(&mut self) -> Result<Vec<Node<'src>>, ParseError> {
    if self.at(TokenKind::BlockStart) {
      self.bump();
      self.parse_block_exprs()
    } else {
      Ok(vec![self.parse_expr()?])
    }
  }

  // --- match ---

  fn parse_match_expr(&mut self, match_loc: Loc) -> ParseResult<'src> {
    let (subjects, _) = self.parse_params()?;
    let (sep, arms) = self.parse_colon_arms()?;
    let end = arms.last().map(|n: &Node| n.loc.end).unwrap_or(subjects.loc.end);
    Ok(self.node(
      NodeKind::Match { subjects: Box::new(subjects), sep, arms: Exprs { items: arms, seps: vec![] } },
      Loc { start: match_loc.start, end },
    ))
  }

  fn parse_colon_arms(&mut self) -> Result<(Token<'src>, Vec<Node<'src>>), ParseError> {
    let sep = self.expect(TokenKind::Colon)?;
    let arms = if self.at(TokenKind::BlockStart) {
      self.bump();
      self.parse_block_items(|p| p.parse_arm())?
    } else {
      vec![self.parse_arm()?]
    };
    Ok((sep, arms))
  }

  // Parse one arm: pattern(s) ":" rhs
  fn parse_arm(&mut self) -> ParseResult<'src> {
    let start = self.peek().loc.start;
    let mut patterns = vec![];

    loop {
      if self.at(TokenKind::Sep) && self.peek().src == ".." {
        patterns.push(self.parse_spread()?);
      } else {
        // Use parse_apply_no_block: application patterns like `str s` are supported,
        // but block detection is disabled since ":" here is the arm separator
        patterns.push(self.parse_apply_no_block()?);
      }
      if self.at(TokenKind::Comma) {
        self.bump();
      } else {
        break;
      }
    }

    let sep = self.expect(TokenKind::Colon)?;
    while self.at(TokenKind::BlockCont) { self.bump(); }

    let body = self.parse_block_body()?;
    let end = body.last().map(|n: &Node| n.loc.end).unwrap_or(start);

    // For multi-pattern arms (fn match multi-arg): wrap in Patterns
    let lhs_node = if patterns.len() == 1 {
      patterns.remove(0)
    } else {
      let pats_end = patterns.last().map(|n: &Node| n.loc.end).unwrap_or(start);
      self.node(NodeKind::Patterns(Exprs { items: patterns, seps: vec![] }), Loc { start, end: pats_end })
    };

    Ok(self.node(NodeKind::Arm { lhs: Exprs { items: vec![lhs_node], seps: vec![] }, sep, body: Exprs { items: body, seps: vec![] } }, Loc { start, end }))
  }

  // --- custom block ---

  fn parse_block(&mut self, name_loc: Loc, name: &'src str) -> ParseResult<'src> {
    let name_node = self.node(NodeKind::Ident(name), name_loc);
    let (params, _) = self.parse_params()?;
    let (sep, body) = self.parse_colon_body_or_arms()?;
    let end = body.last().map(|n: &Node| n.loc.end).unwrap_or(params.loc.end);
    Ok(self.node(
      NodeKind::Block { name: Box::new(name_node), params: Box::new(params), sep, body: Exprs { items: body, seps: vec![] } },
      Loc { start: name_loc.start, end },
    ))
  }

  // For custom blocks: body may contain arms (key: val) or plain expressions
  fn parse_colon_body_or_arms(&mut self) -> Result<(Token<'src>, Vec<Node<'src>>), ParseError> {
    let sep = self.expect(TokenKind::Colon)?;
    let body = if self.at(TokenKind::BlockStart) {
      self.bump();
      self.parse_block_items(|p| p.parse_expr_or_arm())?
    } else {
      vec![self.parse_expr()?]
    };
    Ok((sep, body))
  }

  // Parse either an arm (pattern ":" body) or a plain expression
  fn parse_expr_or_arm(&mut self) -> ParseResult<'src> {
    let expr = self.parse_expr()?;
    // If followed by ":", it was a pattern; convert to arm
    if self.at(TokenKind::Colon) {
      let start = expr.loc.start;
      let sep = self.bump();
      while self.at(TokenKind::BlockCont) { self.bump(); }
      let body = self.parse_block_body()?;
      let end = body.last().map(|n: &Node| n.loc.end).unwrap_or(start);
      Ok(self.node(NodeKind::Arm { lhs: Exprs { items: vec![expr], seps: vec![] }, sep, body: Exprs { items: body, seps: vec![] } }, Loc { start, end }))
    } else {
      Ok(expr)
    }
  }
}

pub fn parse(src: &str) -> Result<crate::ast::ParseResult<'_>, ParseError> {
  let mut p = Parser::new(src);
  // Consume the implicit root BlockStart emitted by the lexer
  p.expect(TokenKind::BlockStart)?;
  let exprs = p.parse_block_exprs()?;
  let root = match exprs.len() {
    0 => return Err(ParseError {
      message: "empty input".into(),
      loc: p.peek().loc,
    }),
    1 => exprs.into_iter().next().unwrap(),
    _ => {
      let start = exprs.first().unwrap().loc.start;
      let end = exprs.last().unwrap().loc.end;
      let loc = Loc { start, end };
      let params = p.node(NodeKind::Patterns(Exprs::empty()), loc);
      let sep = Token { kind: TokenKind::Colon, loc, src: ":" };
      p.node(
        NodeKind::Fn { params: Box::new(params), sep, body: Exprs { items: exprs, seps: vec![] } },
        loc,
      )
    }
  };
  Ok(crate::ast::ParseResult { root, node_count: p.next_id })
}

pub fn parse_with_blocks<'a>(src: &'a str, blocks: &[&'static str]) -> Result<crate::ast::ParseResult<'a>, ParseError> {
  let mut p = Parser::new(src);
  for &name in blocks {
    p.register_block(name);
  }
  p.expect(TokenKind::BlockStart)?;
  let exprs = p.parse_block_exprs()?;
  let root = match exprs.len() {
    0 => return Err(ParseError {
      message: "empty input".into(),
      loc: p.peek().loc,
    }),
    1 => exprs.into_iter().next().unwrap(),
    _ => {
      let start = exprs.first().unwrap().loc.start;
      let end = exprs.last().unwrap().loc.end;
      let loc = Loc { start, end };
      let params = p.node(NodeKind::Patterns(Exprs::empty()), loc);
      let sep = Token { kind: TokenKind::Colon, loc, src: ":" };
      p.node(
        NodeKind::Fn { params: Box::new(params), sep, body: Exprs { items: exprs, seps: vec![] } },
        loc,
      )
    }
  };
  Ok(crate::ast::ParseResult { root, node_count: p.next_id })
}


#[cfg(test)]
mod tests {
  #[test]
  fn test_str_escape_stored_verbatim() {
    // LitStr must store raw source bytes — no rendering at parse time.
    use crate::ast::NodeKind;
    let r = super::parse_with_blocks(r"'\n\t\\'", &["test_block"]).unwrap();
    let NodeKind::LitStr { content: s, .. } = &r.root.kind else { panic!("expected LitStr, got {:?}", r.root.kind) };
    assert_eq!(*s, r"\n\t\\");
  }

  #[test]
  fn test_multiline_str_escape_stored_verbatim() {
    // Multi-StrText assembly must also preserve raw source bytes.
    use crate::ast::NodeKind;
    let src = r#"'
  \n
  \t
'"#;
    let r = super::parse_with_blocks(src, &["test_block"]).unwrap();
    let NodeKind::LitStr { content: s, .. } = &r.root.kind else { panic!("expected LitStr, got {:?}", r.root.kind) };
    assert_eq!(*s, "\n\\n\n\\t\n");
  }

  fn parse_debug(src: &str) -> String {
    match super::parse_with_blocks(src, &["test_block"]) {
      Ok(r) => r.root.print(),
      Err(e) => format!("ERROR: {}", e.message),
    }
  }

  fn ast(src: &str) -> String {
    parse_debug(src)
  }

  test_macros::include_fink_tests!("src/passes/ast/test_yield.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_try.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_literals.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_operators.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_grouping.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_spread_ranges.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_bindings.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_functions.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_match.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_blocks.fnk");
}
