// Stage 2 — AST → String
//
// Takes an `Ast` whose locs are already canonical (produced by `layout::layout`)
// and materialises the source string by writing token bytes at their exact
// line/col positions. No formatting decisions are made here.
//
// Algorithm:
//   Walk every node in document order. Before writing each token, fill the
//   gap from the current write position to the token's start position using
//   the `Pos` line/col fields: if the line number increases, emit newlines
//   then spaces to reach the target column; otherwise emit spaces to reach
//   the target column on the same line.
//
//   Keywords (`fn`, `match`, `try`) are not stored as tokens in the AST — they
//   are always located at the node's own `loc.start`, with fixed text.
//   String interpolation delimiters (`${` and `}`) are not stored either; they
//   are derived from the locs of adjacent children.
//
// Invariants assumed from the caller (`layout.rs`):
//   - Node locs are canonical and self-consistent (no overlaps).
//   - Gap bytes between tokens are either spaces or newline+indent sequences.

use crate::ast::{Ast, AstId, CmpPart, Exprs, NodeKind};
use crate::passes::ast::lexer::{Loc, Pos, Token, TokenKind};
use crate::sourcemap::{MappedWriter, SourceMap};

/// Render a formatted AST to a String.
/// The input must have been produced by `layout::layout` — locs are used as-is.
pub fn print(ast: &Ast<'_>) -> String {
    let mut p = Printer::new(ast);
    p.node(ast.root);
    p.writer.finish_string()
}

/// Render a formatted AST to a String and a Source Map.
/// Each emitted token is mapped back to its source location stored on the
/// token's `loc` field (preserved unchanged by `layout::layout`).
pub fn print_mapped(ast: &Ast<'_>, source_name: &str) -> (String, SourceMap) {
    let mut p = Printer::new(ast);
    p.node(ast.root);
    p.writer.finish(source_name)
}

/// Like `print_mapped` but embeds the original source content in the map.
pub fn print_mapped_with_content(ast: &Ast<'_>, source_name: &str, content: &str) -> (String, SourceMap) {
    let mut p = Printer::new(ast);
    p.node(ast.root);
    p.writer.finish_with_content(source_name, content)
}

/// Render a formatted AST to a String and a native-form source map
/// (byte offsets in both generated output and source).
pub fn print_mapped_native(ast: &Ast<'_>) -> (String, crate::sourcemap_native::SourceMap) {
    let mut p = Printer::new(ast);
    p.node(ast.root);
    p.writer.finish_native()
}

// ---------------------------------------------------------------------------
// Printer — walks the flat AST arena and drives the MappedWriter
// ---------------------------------------------------------------------------

struct Printer<'a, 'src> {
    ast: &'a Ast<'src>,
    writer: MappedWriter,
    /// Byte offset in the canonical source the cursor is at. Mirrors `Pos::idx`
    /// so `gap()` can short-circuit when a previous token's `src` already
    /// bridged to (or past) the next target position — e.g. a `Sep` token whose
    /// body contains the newline + indent between two statements in a block.
    /// `line` and `col` are read directly from the `MappedWriter`, which
    /// tracks them accurately (UTF-16 code units for column) via its
    /// `push`/`push_str` scans.
    idx: u32,
}

impl<'a, 'src> Printer<'a, 'src> {
    fn new(ast: &'a Ast<'src>) -> Self {
        Self { ast, writer: MappedWriter::new(), idx: 0 }
    }

    /// Canonical line of the cursor (1-indexed, matches `Pos::line`).
    /// `MappedWriter` is 0-indexed.
    fn cur_line(&self) -> u32 { self.writer.line() + 1 }

    /// Canonical column of the cursor (0-indexed, matches `Pos::col`).
    fn cur_col(&self) -> u32 { self.writer.col() }

    /// Fill the gap from the current position to `target` using newlines
    /// and/or spaces so the next token lands at `target`.
    ///
    /// Short-circuits when `target.idx <= self.idx` — that happens when a
    /// previous token's `src` contained the byte span all the way to (or past)
    /// the next token. For example, a block `Sep` token whose `src` is
    /// `\n  ` already positions the output at the next statement's column;
    /// the following statement's `gap()` must not emit another newline.
    fn gap(&mut self, target: Pos) {
        if target.idx <= self.idx {
            return;
        }
        let cur_line = self.cur_line();
        if target.line > cur_line {
            let newlines = target.line - cur_line;
            for _ in 0..newlines {
                self.writer.push('\n');
            }
            for _ in 0..target.col {
                self.writer.push(' ');
            }
        } else {
            let spaces = target.col.saturating_sub(self.cur_col());
            for _ in 0..spaces {
                self.writer.push(' ');
            }
        }
        self.idx = target.idx;
    }

    /// Write `text` starting at canonical position `target`, advancing `idx`
    /// by the text's byte length. `text` may contain embedded newlines (e.g.
    /// the `src` of a block `Sep` token) — `MappedWriter::push_str` tracks
    /// line/col accurately across the newlines, and the next `gap()` call
    /// short-circuits via the `target.idx <= self.idx` check.
    fn write(&mut self, target: Pos, text: &str) {
        self.gap(target);
        self.writer.push_str(text);
        self.idx = target.idx + text.len() as u32;
    }

    /// Emit a stored token: mark its source loc, then write its source text.
    fn tok(&mut self, tok: &Token<'src>) {
        self.writer.mark(tok.loc);
        self.write(tok.loc.start, tok.src);
    }

    /// Emit a synthetic keyword (`fn`, `match`, `try`) located at `target`,
    /// marking it at the node's own loc so source maps have something to point at.
    fn keyword(&mut self, node_loc: Loc, kw: &'static str) {
        self.writer.mark(node_loc);
        self.write(node_loc.start, kw);
    }

    /// Write block string (`":` syntax) content. `content` has the indent
    /// floor stripped; re-emit each line prefixed by `indent_col` spaces,
    /// separated by newlines.
    ///
    /// Handles both standalone block strings (`content = "line1\nline2\n"`)
    /// and mid-template segments (`content = "\ncontinuation"` after an
    /// interpolation). The leading `\n` in mid-template content means "start
    /// the next line", so leading empty elements are skipped.
    fn write_block_str_content(&mut self, content: &str, indent_col: u32) {
        let parts: Vec<&str> = content.split('\n').collect();
        let n = parts.len();
        let mut started = false;
        for (i, &line) in parts.iter().enumerate() {
            if !started && line.is_empty() { continue; }
            if i == n - 1 && line.is_empty() { break; }
            started = true;
            self.writer.push('\n');
            for _ in 0..indent_col { self.writer.push(' '); }
            self.writer.push_str(line);
            // `idx` advances by the raw bytes we wrote: 1 newline + indent
            // spaces + the line's byte length. `line`/`col` are tracked by
            // `MappedWriter` itself, so no separate bookkeeping here.
            self.idx += 1 + indent_col + line.len() as u32;
        }
    }

    fn node(&mut self, id: AstId) {
        let node = self.ast.nodes.get(id).clone();
        let node_loc = node.loc;
        match node.kind {
            // --- leaves ---
            NodeKind::LitBool(v) => {
                self.writer.mark(node_loc);
                self.write(node_loc.start, if v { "true" } else { "false" });
            }
            NodeKind::LitInt(s)
            | NodeKind::LitFloat(s)
            | NodeKind::LitDecimal(s)
            | NodeKind::Ident(s) => {
                self.writer.mark(node_loc);
                self.write(node_loc.start, s);
            }
            NodeKind::SynthIdent(n) => {
                self.writer.mark(node_loc);
                let text = format!("·$_{n}");
                self.write(node_loc.start, &text);
            }
            NodeKind::Partial => {
                self.writer.mark(node_loc);
                self.write(node_loc.start, "?");
            }
            NodeKind::Wildcard => {
                self.writer.mark(node_loc);
                self.write(node_loc.start, "_");
            }
            NodeKind::Token(s) => {
                self.writer.mark(node_loc);
                self.write(node_loc.start, s);
            }

            // --- string literal ---
            NodeKind::LitStr { open, close, content, indent } => {
                self.tok(&open);
                // Block strings have multi-line content with the indent floor
                // stripped. `gap()` cannot cross embedded newlines, so handle
                // block bodies explicitly.
                if open.src == "\":" {
                    self.write_block_str_content(&content, indent);
                } else {
                    // Quoted strings: content sits between open.end and close.start.
                    self.write(open.loc.end, &content);
                    self.tok(&close);
                }
            }

            // --- collections ---
            NodeKind::LitSeq { open, close, items }
            | NodeKind::LitRec { open, close, items } => {
                self.tok(&open);
                self.exprs(&items);
                self.tok(&close);
            }

            // --- string templates ---
            // StrTempl: open=' close=' children=[LitStr|expr ...]
            //
            // LitStr children:
            //   - open is either StrStart (first segment) or StrExprEnd (after interpolation)
            //   - close is either StrExprStart (before interpolation) or StrEnd (last segment)
            //
            // Expression children (non-LitStr):
            //   The `${` before them is on the preceding LitStr's close token.
            //   The `}` after them is on the following LitStr's open token —
            //   EXCEPT when the expression is the last child, in which case
            //   the `}` is lost from the AST and must be synthesized at
            //   `close.loc.start - 1`.
            NodeKind::StrTempl { open, close, children }
            | NodeKind::StrRawTempl { open, close, children } => {
                self.tok(&open);
                for (i, &child_id) in children.iter().enumerate() {
                    self.templ_child(child_id);
                    let is_expr = !matches!(
                        self.ast.nodes.get(child_id).kind,
                        NodeKind::LitStr { .. }
                    );
                    let no_following_litstr = children.get(i + 1)
                        .map(|next_id| !matches!(
                            self.ast.nodes.get(*next_id).kind,
                            NodeKind::LitStr { .. }
                        ))
                        .unwrap_or(true);
                    if is_expr && no_following_litstr {
                        let rbrace_idx = children.get(i + 1)
                            .map(|next_id| self.ast.nodes.get(*next_id).loc.start.idx - 1)
                            .unwrap_or(close.loc.start.idx - 1);
                        let rbrace_pos = Pos {
                            idx: rbrace_idx,
                            line: close.loc.start.line,
                            col: close.loc.start.col - 1,
                        };
                        self.write(rbrace_pos, "}");
                    }
                }
                self.tok(&close);
            }

            // --- operators ---
            NodeKind::UnaryOp { op, operand } => {
                self.tok(&op);
                self.node(operand);
            }
            NodeKind::InfixOp { op, lhs, rhs } => {
                self.node(lhs);
                self.tok(&op);
                self.node(rhs);
            }
            NodeKind::ChainedCmp(parts) => {
                for part in parts.iter() {
                    match part {
                        CmpPart::Operand(n) => self.node(*n),
                        CmpPart::Op(op) => self.tok(op),
                    }
                }
            }
            NodeKind::Spread { op, inner } => {
                self.tok(&op);
                if let Some(n) = inner { self.node(n); }
            }
            NodeKind::Member { op, lhs, rhs } => {
                self.node(lhs);
                self.tok(&op);
                self.node(rhs);
            }
            NodeKind::Group { open, close, inner } => {
                self.tok(&open);
                self.node(inner);
                self.tok(&close);
            }

            // --- binding ---
            NodeKind::Bind { op, lhs, rhs }
            | NodeKind::BindRight { op, lhs, rhs } => {
                self.node(lhs);
                self.tok(&op);
                self.node(rhs);
            }

            // --- application ---
            NodeKind::Apply { func, args } => {
                self.node(func);
                self.exprs(&args);
            }
            NodeKind::Pipe(exprs) => {
                self.exprs(&exprs);
            }

            // --- functions / modules ---
            NodeKind::Module { exprs, .. } => {
                self.exprs(&exprs);
            }
            NodeKind::Fn { params, sep, body } => {
                self.keyword(node_loc, "fn");
                self.node(params);
                self.tok(&sep);
                self.exprs(&body);
            }
            NodeKind::Patterns(exprs) => {
                self.exprs(&exprs);
            }

            // --- match ---
            NodeKind::Match { subjects, sep, arms } => {
                self.keyword(node_loc, "match");
                self.exprs(&subjects);
                self.tok(&sep);
                self.exprs(&arms);
            }
            NodeKind::Arm { lhs, sep, body } => {
                self.node(lhs);
                self.tok(&sep);
                self.exprs(&body);
            }

            // --- error handling ---
            NodeKind::Try(inner) => {
                self.keyword(node_loc, "try");
                self.node(inner);
            }

            // --- custom blocks ---
            NodeKind::Block { name, params, sep, body } => {
                self.node(name);
                self.node(params);
                self.tok(&sep);
                self.exprs(&body);
            }
        }
    }

    /// Write a child node of a StrTempl/StrRawTempl.
    ///
    /// LitStr segments inside a template carry their interpolation delimiters:
    ///   - open: StrStart (first segment, outer `'`) or StrExprEnd (`}` after prev expression)
    ///   - close: StrExprStart (`${` before next expression) or StrEnd (outer `'`, last segment)
    ///
    /// We write open.src only when it's a `}` (StrExprEnd) — the outer `'` is
    /// written by the StrTempl handler. We write close.src only when it's `${`
    /// (StrExprStart) — the outer `'` is written by the StrTempl handler.
    fn templ_child(&mut self, id: AstId) {
        let node = self.ast.nodes.get(id).clone();
        if let NodeKind::LitStr { open, close, content, indent } = node.kind {
            if open.kind == TokenKind::StrExprEnd {
                self.tok(&open);
            }
            if !content.is_empty() {
                if indent > 0 {
                    self.write_block_str_content(&content, indent);
                } else {
                    self.write(open.loc.end, &content);
                }
            }
            if close.kind == TokenKind::StrExprStart {
                self.tok(&close);
            }
        } else {
            self.node(id);
        }
    }

    fn exprs(&mut self, exprs: &Exprs<'src>) {
        for (i, &item) in exprs.items.iter().enumerate() {
            self.node(item);
            if let Some(sep) = exprs.seps.get(i) {
                self.tok(sep);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::parser;

    /// Parse source, print it back, and return the output.
    /// Returns "NO-DIFF" if the output is identical to the input — the expected
    /// result for any well-formed source the print stage reproduces verbatim.
    fn print(src: &str) -> String {
        let ast = parser::parse_with_blocks(
            src,
            "test",
            &[("test_block", parser::BlockMode::Ast)],
        ).unwrap_or_else(|e| panic!("parse error: {}", e.message));
        let output = super::print(&ast);
        if output == src { "NO-DIFF".to_string() } else { output }
    }

    test_macros::include_fink_tests!("src/fmt/test_print.fnk");
}
