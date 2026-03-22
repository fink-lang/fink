// Stage 1 — AST → String
//
// Takes a Node tree whose locs are already canonical (produced by layout.rs)
// and materialises the source string by writing token bytes at their exact
// byte-offset positions (Pos::idx). No formatting decisions are made here.
//
// Algorithm:
//   Walk every node in document order. Before writing each token, fill the
//   gap from the current write position to the token's start position using
//   the Pos line/col fields: if the line number increases, emit newlines then
//   spaces to reach the target column; otherwise emit spaces.
//
//   Keywords (fn, match, try, yield, not, ~) are not stored as tokens in the
//   AST — they are always located at the node's own loc.start, with fixed text.
//   String interpolation delimiters (${ and }) are not stored either; they are
//   derived from the locs of adjacent children.
//
// Invariants assumed from the caller (layout.rs):
//   - Node locs are canonical and self-consistent (no overlaps).
//   - Gap bytes between tokens are either spaces or newline+indent sequences.

use crate::passes::ast::{Node, NodeKind, CmpPart, Exprs};
use crate::lexer::{Loc, Pos, Token};
use crate::sourcemap::SourceMap;

/// Render a formatted AST to a String.
/// The input must have been produced by `layout::layout` — locs are used as-is.
pub fn print(root: &Node) -> String {
    let mut w = Writer::new(false);
    w.node(root);
    w.finish_string()
}

/// Render a formatted AST to a String and a Source Map.
/// Each emitted token is mapped back to its original source location (preserved
/// by `layout::layout` on the token's `loc` field).
pub fn print_mapped(root: &Node, source_name: &str) -> (String, SourceMap) {
    let mut w = Writer::new(true);
    w.node(root);
    w.finish_mapped(source_name, None)
}

/// Like `print_mapped` but embeds the original source content in the map.
pub fn print_mapped_with_content(root: &Node, source_name: &str, content: &str) -> (String, SourceMap) {
    let mut w = Writer::new(true);
    w.node(root);
    w.finish_mapped(source_name, Some(content))
}

// ---------------------------------------------------------------------------
// Writer — gap-filling output buffer
// ---------------------------------------------------------------------------

struct Writer {
    buf: String,
    pos: Pos,
    // When Some, collects (out_line, out_col, src_line, src_col) per token.
    // out_line/col are 0-indexed; src_line/col match Pos (1-indexed line, 0-indexed col).
    mappings: Option<Vec<(u32, u32, u32, u32)>>,
    // Current output line/col (0-indexed), tracked only when mappings is Some.
    out_line: u32,
    out_col: u32,
}

impl Writer {
    fn new(mapped: bool) -> Self {
        Self {
            buf: String::new(),
            pos: Pos { idx: 0, line: 1, col: 0 },
            mappings: if mapped { Some(Vec::new()) } else { None },
            out_line: 0,
            out_col: 0,
        }
    }

    fn finish_string(self) -> String {
        self.buf
    }

    fn finish_mapped(self, source_name: &str, content: Option<&str>) -> (String, SourceMap) {
        let mappings = self.mappings.unwrap_or_default();
        let srcmap = if let Some(c) = content {
            SourceMap::from_raw_with_content(source_name, c, mappings.into_iter())
        } else {
            SourceMap::from_raw(source_name, mappings.into_iter())
        };
        (self.buf, srcmap)
    }

    /// Record a mapping from the current output position to a source location.
    /// `src_loc` is the original token loc (before layout rewrote positions).
    /// Called immediately before writing the token text.
    fn mark(&mut self, src_loc: Loc) {
        if let Some(ref mut m) = self.mappings {
            // SourceMap uses 0-indexed lines; Pos uses 1-indexed.
            let src_line = src_loc.start.line.saturating_sub(1);
            let src_col = src_loc.start.col;
            m.push((self.out_line, self.out_col, src_line, src_col));
        }
    }

    /// Update the output line/col tracker after emitting `text`.
    fn track_output(&mut self, text: &str) {
        if self.mappings.is_some() {
            for ch in text.chars() {
                if ch == '\n' {
                    self.out_line += 1;
                    self.out_col = 0;
                } else {
                    self.out_col += 1;
                }
            }
        }
    }

    /// Write a string slice, updating the position cursor.
    /// Caller must ensure `src` starts at `target` and contains no newlines
    /// (multi-line tokens are not part of the Fink token model for printable tokens).
    fn write(&mut self, target: Pos, src: &str) {
        self.gap(target);
        self.buf.push_str(src);
        self.track_output(src);
        self.pos = Pos {
            idx: target.idx + src.len() as u32,
            line: target.line,
            col: target.col + src.len() as u32,
        };
    }

    /// Write a fixed keyword string located at `target`.
    fn keyword(&mut self, target: Pos, kw: &'static str) {
        self.write(target, kw);
    }

    /// Fill the gap from current position to `target` using newlines + spaces
    /// or plain spaces, as determined by the line delta.
    fn gap(&mut self, target: Pos) {
        if target.idx <= self.pos.idx {
            return; // already at or past target (zero-width or overlapping)
        }
        if target.line > self.pos.line {
            // Emit newlines then indent to target column.
            let newlines = target.line - self.pos.line;
            for _ in 0..newlines {
                self.buf.push('\n');
                self.track_output("\n");
            }
            for _ in 0..target.col {
                self.buf.push(' ');
                self.track_output(" ");
            }
        } else {
            // Same line — emit spaces.
            let spaces = target.idx - self.pos.idx;
            for _ in 0..spaces {
                self.buf.push(' ');
                self.track_output(" ");
            }
        }
        self.pos = target;
    }

    fn tok(&mut self, tok: &Token) {
        self.mark(tok.loc);
        self.write(tok.loc.start, tok.src);
    }

    /// Write block string content (":" syntax).
    /// The content has the indent floor stripped; we re-emit each line prefixed
    /// by `indent_col` spaces, separated by newlines.
    ///
    /// Handles both standalone block strings (`content = "line1\nline2\n"`) and
    /// mid-template segments (`content = "\ncontinuation"` after an interpolation).
    /// Write block string content (":" syntax).
    /// The content has the indent floor stripped; we re-emit each line prefixed
    /// by `indent_col` spaces, separated by newlines.
    ///
    /// Handles both standalone block strings (`content = "line1\nline2\n"`) and
    /// mid-template segments (`content = "\ncontinuation"` after an interpolation):
    /// the leading `\n` in mid-template content means "start the next line", so
    /// leading empty elements are skipped.
    fn write_block_str_content(&mut self, content: &str, indent_col: u32) {
        let indent: String = " ".repeat(indent_col as usize);
        // Split by \n. Skip leading empty elements (from a leading \n in mid-template
        // content) and the trailing empty element (after a trailing \n).
        let parts: Vec<&str> = content.split('\n').collect();
        let n = parts.len();
        let mut started = false;
        for (i, &line) in parts.iter().enumerate() {
            // Skip leading empty element — just marks "content starts on new line".
            if !started && line.is_empty() {
                continue;
            }
            // Skip trailing empty element (after final \n).
            if i == n - 1 && line.is_empty() {
                break;
            }
            started = true;
            // Each non-empty-prefix part starts a new line.
            self.buf.push('\n');
            self.track_output("\n");
            self.buf.push_str(&indent);
            self.track_output(&indent);
            self.buf.push_str(line);
            self.track_output(line);
            self.pos = Pos {
                idx: self.pos.idx + 1 + indent_col + line.len() as u32,
                line: self.pos.line + 1,
                col: indent_col + line.len() as u32,
            };
        }
    }

    fn node(&mut self, node: &Node) {
        match &node.kind {
            // --- leaves ---
            NodeKind::LitBool(v) => {
                self.mark(node.loc);
                self.write(node.loc.start, if *v { "true" } else { "false" });
            }
            NodeKind::LitInt(s)
            | NodeKind::LitFloat(s)
            | NodeKind::LitDecimal(s)
            | NodeKind::Ident(s) => {
                self.mark(node.loc);
                self.write(node.loc.start, s);
            }
            NodeKind::Partial => { self.mark(node.loc); self.write(node.loc.start, "?"); }
            NodeKind::Wildcard => { self.mark(node.loc); self.write(node.loc.start, "_"); }

            // --- string literal ---
            NodeKind::LitStr { open, close, content, indent } => {
                self.tok(open);
                // Block strings (":" syntax) have multi-line content with the indent floor
                // stripped. Re-emit each line with the original strip_level (stored in indent).
                // Gap() cannot handle this because content has embedded newlines that
                // write() does not track, so we handle it explicitly here.
                if open.src == "\":" {
                    self.write_block_str_content(content, *indent);
                } else {
                    // Quoted strings: content sits between open.end and close.start.
                    self.write(open.loc.end, content);
                    self.tok(close);
                }
            }

            // --- collections ---
            NodeKind::LitSeq { open, close, items }
            | NodeKind::LitRec { open, close, items } => {
                self.tok(open);
                self.exprs(items);
                self.tok(close);
            }

            // --- string templates ---
            // StrTempl: open=' close=' children=[LitStr|expr ...]
            //
            // LitStr children:
            //   - open is either StrStart (first segment) or StrExprEnd (after interpolation)
            //   - close is either StrExprStart (before interpolation) or StrEnd (last segment)
            //   Writing open.src gives us the `${` or `}` delimiter that precedes/follows text.
            //
            // Expression children (non-LitStr):
            //   The `${` before them is on the preceding LitStr's close token.
            //   The `}` after them is on the following LitStr's open token — EXCEPT when the
            //   expression is the last child, in which case the `}` is lost from the AST and
            //   must be inferred: it sits at close.loc.start - 1 byte.
            NodeKind::StrTempl { open, close, children }
            | NodeKind::StrRawTempl { open, close, children } => {
                self.tok(open);
                for (i, child) in children.iter().enumerate() {
                    self.templ_child(child);
                    // If this is a non-LitStr (expression) and no LitStr follows,
                    // write the `}` delimiter which is otherwise not in the AST.
                    let is_expr = !matches!(child.kind, NodeKind::LitStr { .. });
                    let no_following_litstr = children.get(i + 1)
                        .map(|n| !matches!(n.kind, NodeKind::LitStr { .. }))
                        .unwrap_or(true);
                    if is_expr && no_following_litstr {
                        // `}` sits immediately before the next child's start or before close.
                        let rbrace_idx = children.get(i + 1)
                            .map(|n| n.loc.start.idx - 1)
                            .unwrap_or(close.loc.start.idx - 1);
                        let rbrace_pos = Pos {
                            idx: rbrace_idx,
                            line: close.loc.start.line,
                            col: close.loc.start.col - 1,
                        };
                        self.write(rbrace_pos, "}");
                    }
                }
                self.tok(close);
            }

            // --- operators ---
            NodeKind::UnaryOp { op, operand } => {
                self.tok(op);
                self.node(operand);
            }
            NodeKind::InfixOp { op, lhs, rhs } => {
                self.node(lhs);
                self.tok(op);
                self.node(rhs);
            }
            NodeKind::ChainedCmp(parts) => {
                for part in parts {
                    match part {
                        CmpPart::Operand(n) => self.node(n),
                        CmpPart::Op(op) => self.tok(op),
                    }
                }
            }
            NodeKind::Spread { op, inner } => {
                self.tok(op);
                if let Some(n) = inner { self.node(n); }
            }
            NodeKind::Member { op, lhs, rhs } => {
                self.node(lhs);
                self.tok(op);
                self.node(rhs);
            }
            NodeKind::Group { open, close, inner } => {
                self.tok(open);
                self.node(inner);
                self.tok(close);
            }

            // --- binding ---
            NodeKind::Bind { op, lhs, rhs }
            | NodeKind::BindRight { op, lhs, rhs } => {
                self.node(lhs);
                self.tok(op);
                self.node(rhs);
            }

            // --- application ---
            NodeKind::Apply { func, args } => {
                self.node(func);
                self.exprs(args);
            }
            NodeKind::Pipe(exprs) => {
                self.exprs(exprs);
            }

            // --- functions ---
            // `fn` keyword is at node.loc.start; params follow, then sep `:`, then body.
            NodeKind::Module(exprs) => {
                self.exprs(exprs);
            }
            NodeKind::Fn { params, sep, body } => {
                self.keyword(node.loc.start, "fn");
                self.node(params);
                self.tok(sep);
                self.exprs(body);
            }
            NodeKind::Patterns(exprs) => {
                self.exprs(exprs);
            }

            // --- match ---
            // `match` keyword is at node.loc.start.
            NodeKind::Match { subjects, sep, arms } => {
                self.keyword(node.loc.start, "match");
                self.exprs(subjects);
                self.tok(sep);
                self.exprs(arms);
            }
            NodeKind::Arm { lhs, sep, body } => {
                self.node(lhs);
                self.tok(sep);
                self.exprs(body);
            }

            // --- error handling / suspension ---
            // `try` / `yield` keywords at node.loc.start.
            NodeKind::Try(inner) => {
                self.keyword(node.loc.start, "try");
                self.node(inner);
            }
            NodeKind::Yield(inner) => {
                self.keyword(node.loc.start, "yield");
                self.node(inner);
            }

            // --- custom blocks ---
            NodeKind::Block { name, params, sep, body } => {
                self.node(name);
                self.node(params);
                self.tok(sep);
                self.exprs(body);
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
    /// Between open and content, and content and close, gap() fills spaces.
    fn templ_child(&mut self, node: &Node) {
        use crate::lexer::TokenKind;
        if let NodeKind::LitStr { open, close, content, indent } = &node.kind {
            // Write `}` if this segment follows an expression interpolation.
            if open.kind == TokenKind::StrExprEnd {
                self.tok(open);
            }
            // Write content (may be empty).
            if !content.is_empty() {
                // Block string segments (indent > 0): re-emit with proper indentation.
                // This covers both the first segment (open.src == "\":") and
                // mid-template continuation segments (open.src == "}").
                if *indent > 0 {
                    self.write_block_str_content(content, *indent);
                } else {
                    self.write(open.loc.end, content);
                }
            }
            // Write `${` if this segment precedes an expression interpolation.
            if close.kind == TokenKind::StrExprStart {
                self.tok(close);
            }
        } else {
            self.node(node);
        }
    }

    fn exprs(&mut self, exprs: &Exprs) {
        for (i, item) in exprs.items.iter().enumerate() {
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
    /// Returns "NO-DIFF" if the output is identical to the input — this is
    /// the expected result for any well-formed source the print stage should
    /// reproduce verbatim.
    fn print(src: &str) -> String {
        let result = parser::parse_with_blocks(src, &["test_block"])
            .unwrap_or_else(|e| panic!("parse error: {}", e.message));
        let output = super::print(&result.root);
        if output == src { "NO-DIFF".to_string() } else { output }
    }

    test_macros::include_fink_tests!("src/fmt/test_print.fnk");
}
