#![allow(dead_code)]

use std::collections::HashMap;

use crate::ast::{Ast, AstBuilder, AstId, CmpPart, Exprs, NodeKind};
use crate::lexer::{Lexer, Loc, Pos, Token, TokenKind};

// --- block modes ---

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BlockMode {
  /// Block body is parsed as AST (standard behavior).
  Ast,
  /// Block body is lexed into tokens but not parsed.
  Tokens,
}

// --- error ---

#[derive(Debug)]
pub struct ParseError {
  pub message: String,
  pub loc: Loc,
}

pub type ParseResult = Result<AstId, ParseError>;

// --- parser ---

pub struct Parser<'src> {
  lexer: Lexer<'src>,
  src: &'src str,
  current: Token<'src>,
  block_names: HashMap<&'src str, BlockMode>,
  /// Append-only arena that the parser builds up. Every `node()` call
  /// pushes into this builder; `parse()` hands it back wrapped in `Ast`.
  ast: AstBuilder<'src>,
  /// End position of the last comment skipped by `skip_trivia`.
  /// Used to extend block/fn locs past trailing comments.
  trivia_end: Pos,
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
    let mut block_names = HashMap::new();
    block_names.insert("fn", BlockMode::Ast);
    block_names.insert("match", BlockMode::Ast);
    let mut p = Parser {
      lexer,
      src,
      current,
      block_names,
      ast: AstBuilder::new(),
      trivia_end: Pos { idx: 0, line: 0, col: 0 },
    };
    p.skip_trivia();
    p
  }

  pub fn register_block(&mut self, name: &'src str, mode: BlockMode) {
    self.block_names.insert(name, mode);
  }

  /// Inspect a binding for `{...} = import '...'` and register block names
  /// from the imported module. Currently hardcoded for known URLs.
  fn handle_import(&mut self, lhs: AstId, rhs: AstId) {
    // Match rhs: Apply(Ident("import"), [LitStr(url)])
    // Clone out the pieces we need so we can drop the borrow before
    // `self.register_block` mutably borrows self.
    let (func_id, first_arg_id) = match &self.get(rhs).kind {
      NodeKind::Apply { func, args } => match args.items.first() {
        Some(&arg_id) => (*func, arg_id),
        None => return,
      },
      _ => return,
    };
    if !matches!(self.get(func_id).kind, NodeKind::Ident("import")) { return; }
    // Extract the URL content (needs a clone since it's owned String).
    let url: String = match &self.get(first_arg_id).kind {
      NodeKind::LitStr { content, .. } => content.clone(),
      _ => return,
    };

    // Extract imported names from lhs destructure.
    let names: Vec<&'src str> = match &self.get(lhs).kind {
      NodeKind::LitRec { items, .. } => {
        items.items.iter().filter_map(|&id| match &self.get(id).kind {
          NodeKind::Ident(name) => Some(*name),
          _ => None,
        }).collect()
      }
      _ => return,
    };

    // Only register blocks from the known block-definitions module.
    if url != "@fink/parse/blocks.fnk" { return; }

    for name in names {
      let mode = match name {
        "ƒink" => BlockMode::Ast,
        "ƒtok" => BlockMode::Tokens,
        _ => continue, // unknown block name — skip silently for now
      };
      self.register_block(name, mode);
    }
  }

  fn node(&mut self, kind: NodeKind<'src>, loc: Loc) -> AstId {
    self.ast.append(kind, loc)
  }

  /// Look up a previously-allocated node. Used by the parser internals
  /// that need to inspect a child's shape (e.g. `handle_import`).
  pub(super) fn get(&self, id: AstId) -> &crate::ast::Node<'src> {
    self.ast.read(id)
  }

  /// Lookup helper that returns only the `Loc` by value — cheaper for the
  /// very common "re-span a new parent node over two child nodes" pattern
  /// because it doesn't hold an `&Node` borrow across the append.
  fn loc_of(&self, id: AstId) -> Loc {
    self.ast.read(id).loc
  }

  /// Consume the parser and return its builder — called at the top of
  /// `parse()` after the root node has been allocated.
  fn into_ast(self) -> AstBuilder<'src> {
    self.ast
  }

  // --- cursor ---

  fn peek(&self) -> &Token<'src> {
    &self.current
  }

  fn bump(&mut self) -> Token<'src> {
    let tok = self.current;
    self.current = self.lexer.next_token();
    self.skip_trivia();
    tok
  }

  fn skip_trivia(&mut self) {
    while matches!(self.current.kind, TokenKind::Comment | TokenKind::CommentStart | TokenKind::CommentText | TokenKind::CommentEnd) {
      if self.current.loc.end.idx > self.trivia_end.idx {
        self.trivia_end = self.current.loc.end;
      }
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

  fn parse_expr(&mut self) -> ParseResult {
    self.parse_binding()
  }

  // --- binding (= and |=, lowest precedence) ---

  fn parse_binding(&mut self) -> ParseResult {
    let lhs = self.parse_pipe()?;

    if self.at(TokenKind::Sep) && self.peek().src == "=" {
      let op = self.bump();
      self.skip_block_tokens();
      let rhs = self.parse_expr()?;
      self.handle_import(lhs, rhs);
      let loc = Loc { start: self.loc_of(lhs).start, end: self.loc_of(rhs).end };
      return Ok(self.node(NodeKind::Bind { op, lhs: lhs, rhs: rhs }, loc));
    }

    if self.at(TokenKind::Sep) && self.peek().src == "|=" {
      let op = self.bump();
      self.skip_block_tokens();
      let rhs = self.parse_expr()?;
      let loc = Loc { start: self.loc_of(lhs).start, end: self.loc_of(rhs).end };
      return Ok(self.node(NodeKind::BindRight { op, lhs: lhs, rhs: rhs }, loc));
    }

    Ok(lhs)
  }

  // --- pipe (|) ---

  // Returns true if the current position has a pipe operator,
  // possibly preceded by a BlockCont (multiline pipe continuation).
  // If a BlockCont precedes the "|", it is consumed.
  // Returns the "|" token if a pipe was found, else None.
  fn try_consume_pipe(&mut self) -> Option<Token<'src>> {
    if self.at(TokenKind::Sep) && self.peek().src == "|" {
      return Some(self.bump()); // consume "|"
    }
    if self.at(TokenKind::BlockCont) {
      // Consume BlockCont and check for "|"
      self.bump();
      if self.at(TokenKind::Sep) && self.peek().src == "|" {
        return Some(self.bump()); // consume "|"
      }
      // BlockCont consumed but no "|" — problematic (can't put back).
      // This means the BlockCont was a statement separator, not a pipe continuation.
      // We'll handle this gracefully: the parse_expr caller saw BlockCont but it was consumed.
      // This should not happen in valid Fink code at the pipe level.
    }
    None
  }

  fn parse_pipe(&mut self) -> ParseResult {
    let first = self.parse_apply()?;

    // Check for pipe: inline "|" or multiline "BlockCont |"
    if (self.at(TokenKind::Sep) && self.peek().src == "|")
      || self.at(TokenKind::BlockCont)
    {
      // Try to start a pipe chain
      if let Some(pipe_tok) = self.try_consume_pipe() {
        let mut parts = vec![first];
        let mut seps = vec![pipe_tok];
        parts.push(self.parse_apply()?);
        while let Some(pipe_tok) = self.try_consume_pipe() {
          seps.push(pipe_tok);
          parts.push(self.parse_apply()?);
        }
        let start = self.loc_of(parts[0]).start;
        let end = self.loc_of(*parts.last().unwrap()).end;
        return Ok(self.node(NodeKind::Pipe(Exprs { items: parts.into_boxed_slice(), seps }), Loc { start, end }));
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
    Self::is_infix_keyword(s) || matches!(s, "true" | "false" | "_" | "try")
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
      _ => false,
    }
  }

  // Returns true if the current token is a comma or semicolon.
  fn at_sep(&self) -> bool {
    self.at(TokenKind::Comma) || self.at(TokenKind::Semicolon)
  }

  // Parse an application chain starting with an expression.
  // If the expression is an ident and args follow, builds Apply.
  fn parse_apply(&mut self) -> ParseResult {
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
      if self.block_names.contains_key(name) { return self.parse_block(loc, name); }

      // Tagged template string: ident immediately adjacent to StrStart → raw template
      if self.at(TokenKind::StrStart) && self.peek().loc.start.idx == loc.end.idx {
        let func = self.node(NodeKind::Ident(name), loc);
        let raw_str = self.parse_string(true)?;
        let end = self.loc_of(raw_str).end;
        return Ok(self.node(NodeKind::Apply { func: func, args: Exprs { items: Box::new([raw_str]), seps: vec![] } }, Loc { start: loc.start, end }));
      }

      let func = self.node(NodeKind::Ident(name), loc);
      let result = self.collect_apply_or_block(func, false)?;
      // If no args were collected (bare ident returned), allow infix operators to continue.
      if matches!(self.get(result).kind, NodeKind::Ident(_)) {
        return self.parse_infix_from(result, 0);
      }
      return Ok(result);
    }

    let head = self.parse_infix(0)?;

    // Non-ident head: check postfix-tagged application: [1,2,3]foo or (expr)tag
    if self.at(TokenKind::Ident) && self.peek().loc.start.idx == self.loc_of(head).end.idx {
      let tag = self.bump();
      let func = self.node(NodeKind::Ident(tag.src), tag.loc);
      let loc = Loc { start: self.loc_of(head).start, end: tag.loc.end };
      return Ok(self.node(NodeKind::Apply { func: func, args: Exprs { items: Box::new([head]), seps: vec![] } }, loc));
    }

    Ok(head)
  }

  // Like parse_apply but no block detection — used for arm patterns and record keys.
  fn parse_apply_no_block(&mut self) -> ParseResult {
    // Prefix unary: not
    if self.at(TokenKind::Ident) && self.peek().src == "not" {
      let op_tok = self.bump();
      let operand = self.parse_infix(35)?;
      let loc = Loc { start: op_tok.loc.start, end: self.loc_of(operand).end };
      return Ok(self.node(NodeKind::UnaryOp { op: op_tok, operand: operand }, loc));
    }

    let head = self.parse_infix(0)?;

    if let NodeKind::Ident(name) = self.get(head).kind {
      if name == "fn" { return self.parse_fn(self.loc_of(head)); }
      if name == "match" { return self.parse_match_expr(self.loc_of(head)); }
      if self.block_names.contains_key(name) && name != "fn" && name != "match" {
        return self.parse_block(self.loc_of(head), name);
      }
      let func = self.node(NodeKind::Ident(name), self.loc_of(head));

      // Tagged template string in argument position: ident immediately adjacent to StrStart
      if self.at(TokenKind::StrStart) && self.peek().loc.start.idx == self.loc_of(func).end.idx {
        let raw_str = self.parse_string(true)?;
        let end = self.loc_of(raw_str).end;
        return Ok(self.node(
          NodeKind::Apply { func: func, args: Exprs { items: Box::new([raw_str]), seps: vec![] } },
          Loc { start: self.loc_of(head).start, end },
        ));
      }

      return self.collect_apply_args_no_block(func, true); // no block detection, nested=true to not eat ":"
    }

    if self.at(TokenKind::Ident) && self.peek().loc.start.idx == self.loc_of(head).end.idx {
      let tag = self.bump();
      let func = self.node(NodeKind::Ident(tag.src), tag.loc);
      let loc = Loc { start: self.loc_of(head).start, end: tag.loc.end };
      return Ok(self.node(NodeKind::Apply { func: func, args: Exprs { items: Box::new([head]), seps: vec![] } }, loc));
    }

    Ok(head)
  }

  // Collect args for a function application OR detect block syntax.
  // If after collecting args we see ":", treat as a block.
  // Only called for non-keyword idents (not fn/match/registered blocks).
  fn collect_apply_or_block(&mut self, func: AstId, inside_nested: bool) -> ParseResult {
    let func_loc = self.loc_of(func);
    // Collect params (args for potential block or args for application)
    let mut params: Vec<AstId> = vec![];
    let mut seps: Vec<Token<'src>> = vec![];
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
      last_end = self.loc_of(arg).end.idx;
      params.push(arg);
      if self.at(TokenKind::Comma) {
        let comma = self.bump();
        seps.push(comma);
        // Handle trailing comma that continues onto an indented next line
        if self.at(TokenKind::BlockStart) {
          has_block_tok = true;
          self.bump();
        } else if self.at(TokenKind::BlockCont) && has_block_tok {
          self.bump();
        } else if self.at(TokenKind::BlockEnd) && has_block_tok {
          return Err(ParseError {
            message: "unexpected , followed by dedent".into(),
            loc: comma.loc,
          });
        }
        continue;
      }
      if self.at(TokenKind::Semicolon) {
        if inside_nested { break; }
        let semi = self.bump();
        if self.at(TokenKind::BlockEnd) && has_block_tok {
          return Err(ParseError {
            message: "unexpected ; followed by dedent".into(),
            loc: semi.loc,
          });
        }
        seps.push(semi);
        if self.is_arg_start() { params.push(self.parse_apply_no_block()?); }
        break;
      }
      // No separator: continue if more args follow
    }
    if has_block_tok {
      while self.at(TokenKind::BlockCont) { self.bump(); }
      if self.at(TokenKind::BlockEnd) { self.bump(); }
    }

    // Check for block syntax — only for registered block names.
    // Rules:
    //   func: body             → Block(func, [], body)           [func is registered]
    //   func a, b: body        → Block(func, [a, b], body)      [func is registered]
    //   outer inner: body      → Apply(outer, Block(inner, [], body))   [inner is registered]
    let func_is_block = matches!(&self.get(func).kind, NodeKind::Ident(name) if self.block_names.contains_key(name));

    if self.at(TokenKind::Colon) {
      if params.is_empty() && func_is_block {
        // "func: body" — func is the block name, no params
        let params_node = self.node(NodeKind::Patterns(Exprs::empty()), func_loc);
        let (sep, body) = self.parse_colon_body_or_arms()?;
        let end = body.items.last().map(|&id| self.loc_of(id).end).unwrap_or(func_loc.end);
        return Ok(self.node(
          NodeKind::Block { name: func, params: params_node, sep, body },
          Loc { start: func_loc.start, end },
        ));
      }

      // Single ident param that is a registered block name: nested block in application
      if params.len() == 1
        && let NodeKind::Ident(name) = &self.get(params[0]).kind
        && self.block_names.contains_key(name) {
          let block_name = params.remove(0);
          let block_start = self.loc_of(block_name).start;
          let params_node = self.node(NodeKind::Patterns(Exprs::empty()), self.loc_of(block_name));
          let (sep, body) = self.parse_colon_body_or_arms()?;
          let end = body.items.last().map(|&id| self.loc_of(id).end).unwrap_or(self.loc_of(block_name).end);
          let block_node = self.node(
            NodeKind::Block { name: block_name, params: params_node, sep, body },
            Loc { start: block_start, end },
          );
          return Ok(self.node(
            NodeKind::Apply { func: func, args: Exprs { items: Box::new([block_node]), seps: vec![] } },
            Loc { start: func_loc.start, end },
          ));
      }

      // Func is a registered block name with params
      if func_is_block {
        let params_end = params.last().map(|&id| self.loc_of(id).end).unwrap_or(func_loc.end);
        let params_loc = Loc { start: func_loc.end, end: params_end };
        let params_node = self.node(NodeKind::Patterns(Exprs { items: params.into_boxed_slice(), seps }), params_loc);
        let (sep, body) = self.parse_colon_body_or_arms()?;
        let end = body.items.last().map(|&id| self.loc_of(id).end).unwrap_or(params_loc.end);
        return Ok(self.node(
          NodeKind::Block { name: func, params: params_node, sep, body },
          Loc { start: func_loc.start, end },
        ));
      }
    }

    // Not a block: it's a regular application
    if params.is_empty() {
      return Ok(func);
    }
    let end = self.loc_of(*params.last().unwrap()).end;
    let loc = Loc { start: func_loc.start, end };
    Ok(self.node(NodeKind::Apply { func: func, args: Exprs { items: params.into_boxed_slice(), seps } }, loc))
  }

  // Collect args for a function application.
  // If `inside_nested` is true, stop at semicolons without consuming them.
  // If `no_block` is true, use parse_single_arg_no_block (disables block detection for sub-args).
  fn collect_apply_args(&mut self, func: AstId, inside_nested: bool) -> ParseResult {
    self.collect_apply_args_inner(func, inside_nested, false)
  }

  fn collect_apply_args_no_block(&mut self, func: AstId, inside_nested: bool) -> ParseResult {
    self.collect_apply_args_inner(func, inside_nested, true)
  }

  fn collect_apply_args_inner(&mut self, func: AstId, inside_nested: bool, no_block: bool) -> ParseResult {
    let mut args = vec![];
    let mut seps = vec![];
    let mut last_end = self.loc_of(func).end.idx;

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
          let cont = self.bump(); // consume BlockCont
          last_end = 0; // new line — whitespace implied
          if (self.at(TokenKind::Sep) && (self.peek().src == "|=" || self.peek().src == "|"))
            || self.at(TokenKind::EOF)
            || self.at(TokenKind::BlockEnd)
          {
            break;
          }
          // Record BlockCont as separator between args (only if we already have an arg)
          if !args.is_empty() { seps.push(cont); }
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
      last_end = self.loc_of(arg).end.idx;
      args.push(arg);

      // Check separators between args
      if self.at(TokenKind::Comma) {
        let comma = self.bump();
        seps.push(comma);
        // Trailing comma: allow continuation on next line
        if self.at(TokenKind::BlockStart) {
          has_block = true;
          self.bump();
        } else if self.at(TokenKind::BlockCont) && has_block {
          self.bump(); // consume BlockCont after trailing comma
        } else if self.at(TokenKind::BlockEnd) && has_block {
          return Err(ParseError {
            message: "unexpected , followed by dedent".into(),
            loc: comma.loc,
          });
        }
        // Continue to next arg
        continue;
      }

      if self.at(TokenKind::Semicolon) {
        if inside_nested {
          // Leave the semicolon for the outer function to handle
          break;
        }
        let semi = self.bump();
        if self.at(TokenKind::BlockEnd) && has_block {
          return Err(ParseError {
            message: "unexpected ; followed by dedent".into(),
            loc: semi.loc,
          });
        }
        // Outer function: semicolon is a strong boundary.
        // Collect ONE more grouped arg from after the semicolon.
        seps.push(semi);
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

    let end = self.loc_of(*args.last().unwrap()).end;
    let loc = Loc { start: self.loc_of(func).start, end };
    Ok(self.node(NodeKind::Apply { func: func, args: Exprs { items: args.into_boxed_slice(), seps } }, loc))
  }

  // Parse one argument. If `no_block` is true, block detection is disabled.
  fn parse_single_arg(&mut self) -> ParseResult {
    self.parse_single_arg_inner(false)
  }

  fn parse_single_arg_no_block(&mut self) -> ParseResult {
    self.parse_single_arg_inner(true)
  }

  fn parse_single_arg_inner(&mut self, no_block: bool) -> ParseResult {
    // Ident: may start a nested application or block
    // Skip fast path for keywords/operators handled by parse_infix.
    if self.at(TokenKind::Ident)
      && !Self::is_dispatch_keyword(self.peek().src)
    {
      let name_tok = *self.peek();
      // Special keywords get full parse
      if name_tok.src == "fn" {
        self.bump();
        return self.parse_fn(name_tok.loc);
      }
      if name_tok.src == "match" {
        self.bump();
        return self.parse_match_expr(name_tok.loc);
      }
      if self.block_names.contains_key(name_tok.src) {
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
      if matches!(self.get(result).kind, NodeKind::Ident(_)) {
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

  fn parse_infix(&mut self, min_bp: u8) -> ParseResult {
    let mut lhs = self.parse_unary_or_atom()?;

    // If the atom is a bare ident followed by args or a BlockStart, collect as
    // application. This handles infix RHS like `a - add b` where `add b` should
    // parse as `Apply(add, b)`, not leave `b` as a separate expression.
    // Only when min_bp > 0 (infix RHS) — at min_bp == 0 the caller handles apply.
    if min_bp > 0
      && matches!(self.get(lhs).kind, NodeKind::Ident(_))
      && !Self::is_infix_keyword(match &self.get(lhs).kind { NodeKind::Ident(s) => s, _ => "" })
      && (self.at(TokenKind::BlockStart) || self.is_arg_start())
    {
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
          let loc = Loc { start: self.loc_of(lhs).start, end: self.loc_of(rhs).end };
          lhs = self.node(
            NodeKind::InfixOp { op, lhs: lhs, rhs: rhs },
            loc,
          );
          continue;
        } else {
          // "not" wasn't followed by "in": treat as unary and restart
          // We've consumed "not" — parse what follows as its operand
          let operand = self.parse_infix(35)?;
          let loc = Loc { start: not_tok.loc.start, end: self.loc_of(operand).end };
          lhs = self.node(
            NodeKind::UnaryOp { op: not_tok, operand: operand },
            loc,
          );
          break;
        }
      }

      let tok = *self.peek();
      let Some((l_bp, r_bp)) = Self::infix_bp(&tok) else { break };
      if l_bp < min_bp { break; }

      // Member access: foo.bar, foo.'str key', or foo.(expr)
      if tok.kind == TokenKind::Sep && tok.src == "." {
        let dot = self.bump(); // consume "."
        let rhs = if self.at(TokenKind::BracketOpen) && self.peek().src == "(" {
          // computed: foo.(expr)
          self.parse_group()?
        } else if self.at(TokenKind::StrStart) {
          // string key: foo.'bar baz'
          self.parse_string(false)?
        } else {
          let t = self.expect(TokenKind::Ident)?;
          self.node(NodeKind::Ident(t.src), t.loc)
        };
        let loc = Loc { start: self.loc_of(lhs).start, end: self.loc_of(rhs).end };
        lhs = self.node(NodeKind::Member { op: dot, lhs: lhs, rhs: rhs }, loc);
        continue;
      }

      // Range: .. and ...
      if Self::is_range_op(&tok) {
        let op = self.bump();
        let rhs = self.parse_infix(r_bp)?;
        let loc = Loc { start: self.loc_of(lhs).start, end: self.loc_of(rhs).end };
        lhs = self.node(
          NodeKind::InfixOp { op, lhs: lhs, rhs: rhs },
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
          let start = if let CmpPart::Operand(n) = &parts[0] { self.loc_of(*n).start } else { unreachable!() };
          let end = if let CmpPart::Operand(n) = parts.last().unwrap() { self.loc_of(*n).end } else { unreachable!() };
          lhs = self.node(NodeKind::ChainedCmp(parts.into_boxed_slice()), Loc { start, end });
        } else {
          // Single comparison
          let loc = Loc { start: self.loc_of(lhs).start, end: self.loc_of(first_rhs).end };
          lhs = self.node(
            NodeKind::InfixOp { op: first_op, lhs: lhs, rhs: first_rhs },
            loc,
          );
        }
        continue;
      }

      // General infix
      let op = self.bump();
      let rhs = self.parse_infix(r_bp)?;
      let loc = Loc { start: self.loc_of(lhs).start, end: self.loc_of(rhs).end };
      lhs = self.node(
        NodeKind::InfixOp { op, lhs: lhs, rhs: rhs },
        loc,
      );
    }

    Ok(lhs)
  }

  // Like parse_infix but starts with an already-parsed lhs node.
  fn parse_infix_from(&mut self, lhs: AstId, min_bp: u8) -> ParseResult {
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
          let loc = Loc { start: self.loc_of(lhs).start, end: self.loc_of(rhs).end };
          lhs = self.node(NodeKind::InfixOp { op, lhs: lhs, rhs: rhs }, loc);
          continue;
        } else {
          let operand = self.parse_infix(35)?;
          let loc = Loc { start: not_tok.loc.start, end: self.loc_of(operand).end };
          lhs = self.node(NodeKind::UnaryOp { op: not_tok, operand: operand }, loc);
          break;
        }
      }
      let tok = *self.peek();
      let Some((l_bp, r_bp)) = Self::infix_bp(&tok) else { break };
      if l_bp < min_bp { break; }
      if tok.kind == TokenKind::Sep && tok.src == "." {
        let dot = self.bump();
        let rhs = if self.at(TokenKind::BracketOpen) && self.peek().src == "(" {
          self.parse_group()?
        } else if self.at(TokenKind::StrStart) {
          self.parse_string(false)?
        } else {
          let t = self.expect(TokenKind::Ident)?;
          self.node(NodeKind::Ident(t.src), t.loc)
        };
        let loc = Loc { start: self.loc_of(lhs).start, end: self.loc_of(rhs).end };
        lhs = self.node(NodeKind::Member { op: dot, lhs: lhs, rhs: rhs }, loc);
        continue;
      }
      if Self::is_range_op(&tok) {
        let op = self.bump();
        let rhs = self.parse_infix(r_bp)?;
        let loc = Loc { start: self.loc_of(lhs).start, end: self.loc_of(rhs).end };
        lhs = self.node(NodeKind::InfixOp { op, lhs: lhs, rhs: rhs }, loc);
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
          let start = if let CmpPart::Operand(n) = &parts[0] { self.loc_of(*n).start } else { unreachable!() };
          let end = if let CmpPart::Operand(n) = parts.last().unwrap() { self.loc_of(*n).end } else { unreachable!() };
          lhs = self.node(NodeKind::ChainedCmp(parts.into_boxed_slice()), Loc { start, end });
        } else {
          let loc = Loc { start: self.loc_of(lhs).start, end: self.loc_of(first_rhs).end };
          lhs = self.node(NodeKind::InfixOp { op: first_op, lhs: lhs, rhs: first_rhs }, loc);
        }
        continue;
      }
      let op = self.bump();
      let rhs = self.parse_infix(r_bp)?;
      let loc = Loc { start: self.loc_of(lhs).start, end: self.loc_of(rhs).end };
      lhs = self.node(NodeKind::InfixOp { op, lhs: lhs, rhs: rhs }, loc);
    }
    Ok(lhs)
  }

  fn parse_unary_or_atom(&mut self) -> ParseResult {
    // "fn" / "match" — allow as infix operands (e.g. `x == fn $: [1, $]`)
    if self.at(TokenKind::Ident) && self.peek().src == "fn" {
      let tok = self.bump();
      return self.parse_fn(tok.loc);
    }
    if self.at(TokenKind::Ident) && self.peek().src == "match" {
      let tok = self.bump();
      return self.parse_match_expr(tok.loc);
    }
    // "try" — unwrap Ok or propagate Err; parsed like application
    if self.at(TokenKind::Ident) && self.peek().src == "try" {
      let try_tok = self.bump();
      let inner = self.parse_apply()?;
      let loc = Loc { start: try_tok.loc.start, end: self.loc_of(inner).end };
      return Ok(self.node(NodeKind::Try(inner), loc));
    }
    // "not" prefix unary — bp 35 so it binds tighter than and/or but looser than comparisons
    if self.at(TokenKind::Ident) && self.peek().src == "not" {
      let op_tok = self.bump();
      let operand = self.parse_infix(35)?;
      let loc = Loc { start: op_tok.loc.start, end: self.loc_of(operand).end };
      return Ok(self.node(NodeKind::UnaryOp { op: op_tok, operand: operand }, loc));
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
        let loc = Loc { start: sign.loc.start, end: self.loc_of(operand).end };
        return Ok(self.node(
          NodeKind::UnaryOp { op: sign, operand: operand },
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

  fn parse_string(&mut self, raw: bool) -> ParseResult {
    let start_tok = self.expect(TokenKind::StrStart)?;
    let start_loc = start_tok.loc;
    let mut parts: Vec<AstId> = vec![];
    // Track the open token for the next LitStr segment.
    // First segment opens with StrStart; after interpolation, opens with StrExprEnd.
    let mut next_open = start_tok;
    // For block strings (":" syntax), track the strip_level (indent floor) which is
    // the col of the first StrText token. All segments of the same block string share
    // the same strip_level; it's 0 for quoted strings.
    let mut block_indent: u32 = 0;
    let is_block_str = start_tok.src == "\":" ;

    loop {
      match self.peek().kind {
        TokenKind::StrEnd => {
          let end_tok = self.bump();
          // Close the last LitStr segment if present
          self.close_lit_str(&mut parts, end_tok);
          let loc = Loc { start: start_loc.start, end: end_tok.loc.end };
          if !raw && parts.is_empty() {
            return Ok(self.node(NodeKind::LitStr { open: start_tok, close: end_tok, content: String::new(), indent: 0 }, loc));
          }
          if !raw && parts.len() == 1
            && let NodeKind::LitStr { .. } = &self.get(parts[0]).kind {
              // Return the single child id with its loc widened to cover the
              // full quote span. Node::new can't rewrite a slot in place; we
              // append a fresh loc-corrected copy instead.
              let child_id = parts.remove(0);
              let child_kind = self.get(child_id).kind.clone();
              return Ok(self.node(child_kind, loc));
          }
          let kind = if raw {
            NodeKind::StrRawTempl { open: start_tok, close: end_tok, children: parts.into_boxed_slice() }
          } else {
            NodeKind::StrTempl { open: start_tok, close: end_tok, children: parts.into_boxed_slice() }
          };
          return Ok(self.node(kind, loc));
        }
        TokenKind::StrText => {
          let t = self.bump();
          let text = t.src.to_string();
          // Merge consecutive StrText tokens into a single LitStr. Under
          // append-only we can't mutate the existing slot — we read it,
          // construct an extended copy, append as a fresh node, and
          // replace the id in `parts`.
          let last_is_litstr = parts.last()
            .map(|&id| matches!(self.get(id).kind, NodeKind::LitStr { .. }))
            .unwrap_or(false);
          if last_is_litstr {
            let prev_id = *parts.last().unwrap();
            let (open, close, mut content, indent, prev_loc_start) = match &self.get(prev_id).kind {
              NodeKind::LitStr { open, close, content, indent } => {
                (*open, *close, content.clone(), *indent, self.get(prev_id).loc.start)
              }
              _ => unreachable!(),
            };
            content.push_str(&text);
            let new_loc = Loc { start: prev_loc_start, end: t.loc.end };
            let new_id = self.node(NodeKind::LitStr { open, close, content, indent }, new_loc);
            *parts.last_mut().unwrap() = new_id;
          } else {
            // For block strings, strip_level == the first StrText token's col.
            // Capture it once from the first segment; subsequent segments reuse it.
            let indent = if is_block_str {
              if block_indent == 0 && next_open.src == "\":" {
                block_indent = t.loc.start.col;
              }
              block_indent
            } else { 0 };
            parts.push(self.node(NodeKind::LitStr { open: next_open, close: next_open, content: text, indent }, t.loc));
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

  /// Set the `close` token on the last LitStr in `parts`, if the last
  /// element is a LitStr. Append-only: we read the old node, build a new
  /// one with the updated close token, and swap the id in `parts`.
  fn close_lit_str(&mut self, parts: &mut [AstId], close: Token<'src>) {
    let Some(&last_id) = parts.last() else { return };
    let (open, content, indent, loc) = match &self.get(last_id).kind {
      NodeKind::LitStr { open, content, indent, .. } => {
        (*open, content.clone(), *indent, self.get(last_id).loc)
      }
      _ => return,
    };
    let new_id = self.node(NodeKind::LitStr { open, close, content, indent }, loc);
    *parts.last_mut().unwrap() = new_id;
  }

  // --- sequence/record/group/spread ---

  fn parse_seq_items(&mut self) -> Result<Exprs<'src>, ParseError> {
    let mut items = vec![];
    let mut seps = vec![];
    self.skip_block_tokens();
    // Consume leading commas as implicit wildcards: [, , n]
    while self.at(TokenKind::Comma) || self.at(TokenKind::Semicolon) {
      let sep = self.bump();
      items.push(self.node(NodeKind::Wildcard, sep.loc));
      seps.push(sep);
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
          let loc = Loc { start: self.loc_of(item).start, end: self.loc_of(rhs).end };
          self.node(NodeKind::Bind { op, lhs: item, rhs: rhs }, loc)
        } else if self.at(TokenKind::Sep) && self.peek().src == "|=" {
          let op = self.bump();
          self.skip_block_tokens();
          let rhs = self.parse_expr()?;
          let loc = Loc { start: self.loc_of(item).start, end: self.loc_of(rhs).end };
          self.node(NodeKind::BindRight { op, lhs: item, rhs: rhs }, loc)
        } else {
          item
        };
        items.push(item);
      }
      self.skip_block_tokens();
      if self.at(TokenKind::Comma) || self.at(TokenKind::Semicolon) {
        let sep = self.bump();
        seps.push(sep);
        self.skip_block_tokens();
        // Implicit wildcard: consecutive comma with no expression between
        while self.at(TokenKind::Comma) || self.at(TokenKind::Semicolon) {
          items.push(self.node(NodeKind::Wildcard, sep.loc));
          seps.push(self.bump());
          self.skip_block_tokens();
        }
      }
    }
    Ok(Exprs { items: items.into_boxed_slice(), seps })
  }

  fn parse_rec_items(&mut self) -> Result<Exprs<'src>, ParseError> {
    let mut items = vec![];
    let mut seps = vec![];
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
          let loc = Loc { start: self.loc_of(first).start, end: self.loc_of(val).end };
          // TODO: record fields reuse Arm nodes, but they're semantically different
          // from match arms (no scope introduction, key is a literal not a pattern).
          // Consider a dedicated RecField variant to avoid downstream confusion.
          items.push(self.node(NodeKind::Arm { lhs: first, sep, body: Exprs { items: Box::new([val]), seps: vec![] } }, loc));
        } else {
          items.push(first);
        }
      }
      self.skip_block_tokens();
      if self.at(TokenKind::Comma) || self.at(TokenKind::Semicolon) {
        seps.push(self.bump());
        self.skip_block_tokens();
      }
    }
    Ok(Exprs { items: items.into_boxed_slice(), seps })
  }

  fn parse_spread(&mut self) -> ParseResult {
    let op_tok = self.bump(); // consume ".."
    let start = op_tok.loc.start;

    let maybe_inner: Option<AstId> = if self.at(TokenKind::BracketOpen) && self.peek().src == "(" {
      // ..(expr) — parse group, preserve Group node for faithful AST
      let group = self.parse_group()?;
      // ..(expr)..(expr) — `)` directly followed by `..`/`...` is a range, not a second spread
      if Self::is_range_op(self.peek()) {
        Some(self.parse_infix_from(group, 0)?)
      } else {
        Some(group)
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
        let loc = Loc { start: self.loc_of(inner).start, end: self.loc_of(rhs).end };
        self.node(NodeKind::BindRight { op, lhs: inner, rhs: rhs }, loc)
      } else {
        inner
      };
      let end = self.loc_of(node).end;
      Ok(self.node(NodeKind::Spread { op: op_tok, inner: Some(node) }, Loc { start, end }))
    } else {
      let loc = op_tok.loc;
      Ok(self.node(NodeKind::Spread { op: op_tok, inner: None }, loc))
    }
  }

  fn parse_group(&mut self) -> ParseResult {
    let open = self.bump(); // consume "("
    let inner = if self.at(TokenKind::BlockStart) {
      // Multi-expr block group: wrap in a zero-param Fn so the CPS pass
      // can detect "group with bindings" and emit a scope.
      self.bump(); // consume BlockStart
      let exprs = self.parse_block_exprs()?;
      let params = self.node(NodeKind::Patterns(Exprs::empty()), open.loc);
      let sep = Token { kind: TokenKind::Colon, loc: open.loc, src: ":" };
      self.node(NodeKind::Fn { params: params, sep, body: exprs }, open.loc)
    } else {
      self.skip_block_tokens();
      let expr = self.parse_expr()?;
      self.skip_block_tokens();
      expr
    };
    let close = self.expect(TokenKind::BracketClose)?;
    let loc = Loc { start: open.loc.start, end: close.loc.end };
    Ok(self.node(NodeKind::Group { open, close, inner: inner }, loc))
  }

  // --- atom ---

  fn parse_atom(&mut self) -> ParseResult {
    let tok = *self.peek();
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
        Ok(self.node(NodeKind::LitSeq { open, close, items }, loc))
      }
      TokenKind::BracketOpen if tok.src == "{" => {
        let open = self.bump();
        let items = self.parse_rec_items()?;
        let close = self.expect(TokenKind::BracketClose)?;
        let loc = Loc { start: open.loc.start, end: close.loc.end };
        Ok(self.node(NodeKind::LitRec { open, close, items }, loc))
      }
      TokenKind::BracketOpen if tok.src == "(" => {
        // Parenthesised group — inner is a full expression (may be application)
        let group = self.parse_group()?;
        let group_end = self.loc_of(group).end;
        // Postfix tag: (expr)tag where tag is immediately adjacent
        if self.at(TokenKind::Ident) && self.peek().loc.start.idx == group_end.idx {
          let tag = self.bump();
          let func = self.node(NodeKind::Ident(tag.src), tag.loc);
          let loc = Loc { start: self.loc_of(group).start, end: tag.loc.end };
          return Ok(self.node(NodeKind::Apply { func: func, args: Exprs { items: Box::new([group]), seps: vec![] } }, loc));
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

  fn parse_fn(&mut self, fn_loc: Loc) -> ParseResult {
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
      // Extract subject expressions from Patterns wrapper
      let subjects = match self.get(params).kind.clone() {
        NodeKind::Patterns(exprs) => exprs,
        _ => Exprs { items: Box::new([params]), seps: vec![] },
      };
      let match_end = arms.items.last().map(|&id| self.loc_of(id).end).unwrap_or(self.loc_of(params).end);
      fn_end_loc = Loc { start: fn_loc.start, end: match_end };
      let match_node = self.node(
        NodeKind::Match { subjects, sep, arms },
        fn_end_loc,
      );
      Ok(self.node(
        NodeKind::Fn { params: params, sep, body: Exprs { items: Box::new([match_node]), seps: vec![] } },
        fn_end_loc,
      ))
    } else {
      let (sep, body) = self.parse_colon_body()?;
      let end = body.items.last().map(|&id| self.loc_of(id).end).unwrap_or(self.loc_of(params).end);
      fn_end_loc = Loc { start: fn_loc.start, end };
      Ok(self.node(NodeKind::Fn { params: params, sep, body }, fn_end_loc))
    }
  }

  // Parse comma-separated params until ":"
  fn parse_params(&mut self) -> Result<(AstId, Loc), ParseError> {
    let start = self.peek().loc.start;
    let mut items: Vec<AstId> = vec![];
    let mut seps: Vec<Token<'src>> = vec![];

    // Leading commas as implicit wildcards: fn , , c: c
    while self.at(TokenKind::Comma) {
      let sep = self.bump();
      items.push(self.node(NodeKind::Wildcard, sep.loc));
      seps.push(sep);
    }

    self.skip_block_tokens();
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
          let loc = Loc { start: self.loc_of(param).start, end: self.loc_of(rhs).end };
          self.node(NodeKind::Bind { op, lhs: param, rhs: rhs }, loc)
        } else {
          param
        };
        items.push(param);
      }
      if self.at(TokenKind::Comma) {
        let sep = self.bump();
        seps.push(sep);
        // Trailing comma: continue onto indented next line
        if self.at(TokenKind::BlockStart) || self.at(TokenKind::BlockCont) { self.bump(); }
        // Consecutive commas as implicit wildcards
        while self.at(TokenKind::Comma) {
          items.push(self.node(NodeKind::Wildcard, sep.loc));
          seps.push(self.bump());
        }
      } else if self.at(TokenKind::BlockCont) || self.at(TokenKind::Semicolon) {
        seps.push(self.bump());
        self.skip_block_tokens();
      } else {
        self.skip_block_tokens();
        break;
      }
    }

    let end = items.last().map(|&id| self.loc_of(id).end).unwrap_or(start);
    let loc = Loc { start, end };
    Ok((self.node(NodeKind::Patterns(Exprs { items: items.into_boxed_slice(), seps }), loc), loc))
  }

  // Parse ":" then either inline expression(s) or indented block.
  // Returns the colon token and an Exprs (items + seps).
  fn parse_colon_body(&mut self) -> Result<(Token<'src>, Exprs<'src>), ParseError> {
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
  fn parse_inline_exprs(&mut self) -> Result<Exprs<'src>, ParseError> {
    let mut items = vec![self.parse_expr()?];
    let mut seps = vec![];
    while self.at(TokenKind::Comma) || self.at(TokenKind::Semicolon) {
      seps.push(self.bump());
      items.push(self.parse_expr()?);
    }
    Ok(Exprs { items: items.into_boxed_slice(), seps })
  }

  fn parse_block_items<F>(&mut self, mut f: F) -> Result<Exprs<'src>, ParseError>
  where F: FnMut(&mut Self) -> ParseResult {
    let mut items = vec![];
    let mut seps = vec![];
    loop {
      if self.at(TokenKind::BlockEnd) || self.at(TokenKind::EOF) { break; }
      if self.at(TokenKind::BlockCont) {
        if !items.is_empty() { seps.push(self.bump()); } else { self.bump(); }
        continue;
      }
      items.push(f(self)?);
      // Comma and semicolon separate block expressions on the same line
      if self.at(TokenKind::Comma) || self.at(TokenKind::Semicolon) {
        let sep = self.bump();
        if self.at(TokenKind::BlockCont) {
          return Err(ParseError {
            message: format!("trailing '{}' without indented continuation", sep.src),
            loc: sep.loc,
          });
        }
        // BlockStart after separator = line continuation (indented next line)
        if self.at(TokenKind::BlockStart) { self.bump(); }
        seps.push(sep);
      }
    }
    if self.at(TokenKind::BlockEnd) { self.bump(); }
    Ok(Exprs { items: items.into_boxed_slice(), seps })
  }

  fn parse_block_exprs(&mut self) -> Result<Exprs<'src>, ParseError> {
    self.parse_block_items(|p| p.parse_expr())
  }

  // Parse BlockStart already consumed: inline expr or indented block.
  fn parse_block_body(&mut self) -> Result<Exprs<'src>, ParseError> {
    if self.at(TokenKind::BlockStart) {
      self.bump();
      self.parse_block_exprs()
    } else {
      let id = self.parse_expr()?;
      Ok(Exprs { items: Box::new([id]), seps: vec![] })
    }
  }

  // --- match ---

  fn parse_match_expr(&mut self, match_loc: Loc) -> ParseResult {
    let (params_node, params_loc) = self.parse_params()?;
    // Extract subject expressions from Patterns wrapper — subjects are expressions, not patterns
    let subjects = match self.get(params_node).kind.clone() {
      NodeKind::Patterns(exprs) => exprs,
      _ => Exprs { items: Box::new([params_node]), seps: vec![] },
    };
    let (sep, arms) = self.parse_colon_arms()?;
    let end = arms.items.last().map(|&id| self.loc_of(id).end).unwrap_or(params_loc.end);
    Ok(self.node(
      NodeKind::Match { subjects, sep, arms },
      Loc { start: match_loc.start, end },
    ))
  }

  fn parse_colon_arms(&mut self) -> Result<(Token<'src>, Exprs<'src>), ParseError> {
    let sep = self.expect(TokenKind::Colon)?;
    let arms = if self.at(TokenKind::BlockStart) {
      self.bump();
      self.parse_block_items(|p| p.parse_arm())?
    } else {
      let id = self.parse_arm()?;
      Exprs { items: Box::new([id]), seps: vec![] }
    };
    Ok((sep, arms))
  }

  // Parse one arm: pattern(s) ":" rhs
  fn parse_arm(&mut self) -> ParseResult {
    let start = self.peek().loc.start;
    let mut patterns = vec![];
    let mut pat_seps = vec![];

    loop {
      if self.at(TokenKind::Sep) && self.peek().src == ".." {
        patterns.push(self.parse_spread()?);
      } else {
        // Use parse_apply_no_block: application patterns like `str s` are supported,
        // but block detection is disabled since ":" here is the arm separator
        patterns.push(self.parse_apply_no_block()?);
      }
      if self.at(TokenKind::Comma) {
        pat_seps.push(self.bump());
      } else {
        break;
      }
    }

    let sep = self.expect(TokenKind::Colon)?;
    while self.at(TokenKind::BlockCont) { self.bump(); }

    let body = self.parse_block_body()?;
    let end = body.items.last().map(|&id| self.loc_of(id).end).unwrap_or(start);

    // Always wrap arm LHS in Patterns, consistent with fn params
    let pats_end = patterns.last().map(|&id| self.loc_of(id).end).unwrap_or(start);
    let lhs_node = self.node(NodeKind::Patterns(Exprs { items: patterns.into_boxed_slice(), seps: pat_seps }), Loc { start, end: pats_end });

    Ok(self.node(NodeKind::Arm { lhs: lhs_node, sep, body }, Loc { start, end }))
  }

  // --- custom block ---

  fn parse_block(&mut self, name_loc: Loc, name: &'src str) -> ParseResult {
    let mode = self.block_names.get(name).copied().unwrap_or(BlockMode::Ast);
    let name_node = self.node(NodeKind::Ident(name), name_loc);
    let (params, _) = self.parse_params()?;
    let (sep, body) = match mode {
      BlockMode::Ast    => self.parse_colon_body_or_arms()?,
      BlockMode::Tokens => self.parse_colon_body_tokens()?,
    };
    let mut end = body.items.last().map(|&id| self.loc_of(id).end).unwrap_or(self.loc_of(params).end);
    // Extend past trailing comments that were skipped inside the block body
    if self.trivia_end.idx > end.idx {
      end = self.trivia_end;
    }
    Ok(self.node(
      NodeKind::Block { name: name_node, params: params, sep, body },
      Loc { start: name_loc.start, end },
    ))
  }

  /// Collect all tokens in the block body without parsing.
  /// Skips BlockStart/BlockEnd/BlockCont structural tokens.
  fn parse_colon_body_tokens(&mut self) -> Result<(Token<'src>, Exprs<'src>), ParseError> {
    let sep = self.expect(TokenKind::Colon)?;
    let mut items = vec![];
    if self.at(TokenKind::BlockStart) {
      self.bump();
      loop {
        if self.at(TokenKind::BlockEnd) || self.at(TokenKind::EOF) { break; }
        if self.at(TokenKind::BlockCont) { self.bump(); continue; }
        let tok = self.bump();
        items.push(self.node(NodeKind::Token(tok.src), tok.loc));
      }
    } else {
      // Inline: collect tokens until end of line (BlockCont/BlockEnd/EOF)
      while !self.at(TokenKind::BlockCont)
         && !self.at(TokenKind::BlockEnd)
         && !self.at(TokenKind::EOF) {
        let tok = self.bump();
        items.push(self.node(NodeKind::Token(tok.src), tok.loc));
      }
    }
    Ok((sep, Exprs { items: items.into_boxed_slice(), seps: vec![] }))
  }


  // For custom blocks: body may contain arms (key: val) or plain expressions
  fn parse_colon_body_or_arms(&mut self) -> Result<(Token<'src>, Exprs<'src>), ParseError> {
    let sep = self.expect(TokenKind::Colon)?;
    let body = if self.at(TokenKind::BlockStart) {
      self.bump();
      self.parse_block_items(|p| p.parse_expr_or_arm())?
    } else {
      let id = self.parse_expr()?;
      Exprs { items: Box::new([id]), seps: vec![] }
    };
    Ok((sep, body))
  }

  // Parse either an arm (pattern ":" body) or a plain expression
  fn parse_expr_or_arm(&mut self) -> ParseResult {
    let expr = self.parse_expr()?;
    // If followed by ":", it was a pattern; convert to arm
    if self.at(TokenKind::Colon) {
      let start = self.loc_of(expr).start;
      let sep = self.bump();
      while self.at(TokenKind::BlockCont) { self.bump(); }
      let body = self.parse_block_body()?;
      let end = body.items.last().map(|&id| self.loc_of(id).end).unwrap_or(start);
      Ok(self.node(NodeKind::Arm { lhs: expr, sep, body }, Loc { start, end }))
    } else {
      Ok(expr)
    }
  }
}

pub fn parse<'src>(src: &'src str, url: &str) -> Result<Ast<'src>, ParseError> {
  let mut p = Parser::new(src);
  // Consume the implicit root BlockStart emitted by the lexer
  p.expect(TokenKind::BlockStart)?;
  let exprs = p.parse_block_exprs()?;
  let loc = compute_exprs_loc(&p, &exprs, p.peek().loc);
  let root = p.node(NodeKind::Module { exprs, url: url.to_string() }, loc);
  Ok(p.into_ast().finish(root))
}

pub fn parse_with_blocks<'src>(
  src: &'src str,
  url: &str,
  blocks: &[(&'src str, BlockMode)],
) -> Result<Ast<'src>, ParseError> {
  let mut p = Parser::new(src);
  for &(name, mode) in blocks {
    p.register_block(name, mode);
  }
  p.expect(TokenKind::BlockStart)?;
  let exprs = p.parse_block_exprs()?;
  let loc = compute_exprs_loc(&p, &exprs, p.peek().loc);
  let root = p.node(NodeKind::Module { exprs, url: url.to_string() }, loc);
  Ok(p.into_ast().finish(root))
}

/// Compute the span loc covering every item in an Exprs, falling back to
/// `default` when empty. Reads first/last node locs via the parser's arena.
fn compute_exprs_loc<'src>(p: &Parser<'src>, exprs: &Exprs<'src>, default: Loc) -> Loc {
  match (exprs.items.first(), exprs.items.last()) {
    (Some(&first_id), Some(&last_id)) => Loc {
      start: p.get(first_id).loc.start,
      end: p.get(last_id).loc.end,
    },
    _ => default,
  }
}


#[cfg(test)]
mod tests {
  use crate::ast::NodeKind;
  use super::BlockMode;

  use crate::ast::{Ast, AstId};

  /// Extract the AstId of the single expression from a Module root, panicking
  /// if not exactly one.
  fn unwrap_single<'src>(ast: &Ast<'src>) -> AstId {
    let root = ast.nodes.get(ast.root);
    let NodeKind::Module { exprs, .. } = &root.kind else {
      panic!("expected Module, got {:?}", root.kind);
    };
    assert_eq!(exprs.items.len(), 1, "expected single expression in Module");
    exprs.items[0]
  }

  #[test]
  fn test_str_escape_stored_verbatim() {
    // LitStr must store raw source bytes — no rendering at parse time.
    let ast = super::parse_with_blocks(r"'\n\t\\'", "test", &[("test_block", BlockMode::Ast)]).unwrap();
    let id = unwrap_single(&ast);
    let node = ast.nodes.get(id);
    let NodeKind::LitStr { content: s, .. } = &node.kind else { panic!("expected LitStr, got {:?}", node.kind) };
    assert_eq!(*s, r"\n\t\\");
  }

  #[test]
  fn test_multiline_str_escape_stored_verbatim() {
    // Multi-StrText assembly must also preserve raw source bytes.
    let src = r#"'
  \n
  \t
'"#;
    let ast = super::parse_with_blocks(src, "test", &[("test_block", BlockMode::Ast)]).unwrap();
    let id = unwrap_single(&ast);
    let node = ast.nodes.get(id);
    let NodeKind::LitStr { content: s, .. } = &node.kind else { panic!("expected LitStr, got {:?}", node.kind) };
    assert_eq!(*s, "\n\\n\n\\t\n");
  }

  fn parse_debug(src: &str) -> String {
    match super::parse_with_blocks(src, "test", &[("test_block", BlockMode::Ast)]) {
      Ok(ast) => ast.print(),
      Err(e) => format!("ERROR [{}:{}]: {}", e.loc.start.line, e.loc.start.col, e.message),
    }
  }

  /// Parse source and print the full AST including Module root.
  fn ast(src: &str) -> String {
    parse_debug(src)
  }

  /// Alias for ast — used in module-level tests for clarity.
  fn module(src: &str) -> String {
    parse_debug(src)
  }

  #[test]
  fn str_templ_trailing_interp_last_child_is_expression() {
    // 'hello ${expr}' — when interpolation is the last child, the AST ends with
    // the expression node. The `}` delimiter is implied (inferred by the print stage
    // from close.loc). The StrTempl.close token holds the closing `'`.
    let ast = super::parse("'hello ${1}'", "test").unwrap();
    let id = unwrap_single(&ast);
    let node = ast.nodes.get(id);
    let NodeKind::StrTempl { children, close, .. } = &node.kind else {
      panic!("expected StrTempl, got {:?}", node.kind);
    };
    assert_eq!(close.src, "'");
    let &last_id = children.last().expect("expected children");
    let last = ast.nodes.get(last_id);
    assert!(matches!(last.kind, NodeKind::LitInt(_)), "last child should be expression, got {:?}", last.kind);
  }

  #[test]
  // TODO: port to a .fnk test file (test_parser.fnk or test_functions.fnk) once
  // the test macro supports asserting on seps, not just node shape.
  // The real coverage for this fix lives in src/fmt/test_print.fnk (t_multi_arg_application etc).
  fn apply_args_preserve_comma_seps() {
    // collect_apply_or_block was dropping comma tokens from args.seps.
    // Commas between Apply args must be stored so the formatter can reproduce them.
    let ast = super::parse("add 1, 2", "test").unwrap();
    let id = unwrap_single(&ast);
    let node = ast.nodes.get(id);
    let NodeKind::Apply { args, .. } = &node.kind else {
      panic!("expected Apply, got {:?}", node.kind);
    };
    assert_eq!(args.items.len(), 2, "expected 2 args");
    assert_eq!(args.seps.len(), 1, "expected 1 comma sep");
    assert_eq!(args.seps[0].src, ",");
  }

  #[test]
  fn block_loc_excludes_dedented_comment() {
    let src = "test_block:\n  42\n# outside";
    let ast = super::parse_with_blocks(src, "test", &[("test_block", BlockMode::Ast)]).unwrap();
    let root = ast.nodes.get(ast.root);
    let NodeKind::Module { exprs, .. } = &root.kind else { panic!("expected Module") };
    let node_id = exprs.items[0];
    let node = ast.nodes.get(node_id);
    let NodeKind::Block { .. } = &node.kind else { panic!("expected Block") };
    assert_eq!(
      node.loc.end.idx as usize, "test_block:\n  42".len(),
      "block loc must not extend into dedented comment",
    );
  }

  #[test]
  fn block_loc_includes_indented_trailing_comment() {
    // A comment at the block's own indent level IS part of the block body.
    let src = "test_block:\n  42\n  # inside";
    let ast = super::parse_with_blocks(src, "test", &[("test_block", BlockMode::Ast)]).unwrap();
    let root = ast.nodes.get(ast.root);
    let NodeKind::Module { exprs, .. } = &root.kind else { panic!("expected Module") };
    let node_id = exprs.items[0];
    let node = ast.nodes.get(node_id);
    let NodeKind::Block { .. } = &node.kind else { panic!("expected Block") };
    assert_eq!(
      node.loc.end.idx as usize, src.len(),
      "block loc must extend past trailing comment inside the block",
    );
  }

  #[test]
  fn nested_block_loc_excludes_sibling_comment() {
    // Inner block `b:` must not consume `# sibling` which is at `a:`'s indent level
    let src = "test_block:\n  test_block:\n    42\n  # sibling\n  99";
    let ast = super::parse_with_blocks(src, "test", &[("test_block", BlockMode::Ast)]).unwrap();
    let root = ast.nodes.get(ast.root);
    let NodeKind::Module { exprs, .. } = &root.kind else { panic!("expected Module") };
    let outer_id = exprs.items[0];
    let outer = ast.nodes.get(outer_id);
    let NodeKind::Block { body: outer_body, .. } = &outer.kind else { panic!("expected outer Block") };
    let inner_id = outer_body.items[0];
    let inner = ast.nodes.get(inner_id);
    let NodeKind::Block { .. } = &inner.kind else { panic!("expected inner Block, got {:?}", inner.kind) };
    // Inner block is "test_block:\n    42" starting at idx 14
    // "  test_block:\n    42" → inner ends at idx 30 (end of "42")
    // Must NOT include "  # sibling"
    assert!(
      (inner.loc.end.idx as usize) <= "test_block:\n  test_block:\n    42".len(),
      "inner block loc ({}) must not extend into sibling comment",
      inner.loc.end.idx,
    );
  }

  test_macros::include_fink_tests!("src/passes/ast/test_try.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_literals.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_operators.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_grouping.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_spread_ranges.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_bindings.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_functions.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_match.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_blocks.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_block_modes.fnk");
  test_macros::include_fink_tests!("src/passes/ast/test_module.fnk");
}
