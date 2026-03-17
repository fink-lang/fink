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
use crate::lexer::{Pos, Token};

/// Render a formatted AST to a String.
/// The input must have been produced by `layout::layout` — locs are used as-is.
pub fn print(root: &Node) -> String {
    let mut w = Writer::new();
    w.node(root);
    w.finish()
}

// ---------------------------------------------------------------------------
// Writer — gap-filling output buffer
// ---------------------------------------------------------------------------

struct Writer {
    buf: String,
    pos: Pos,
}

impl Writer {
    fn new() -> Self {
        Self {
            buf: String::new(),
            pos: Pos { idx: 0, line: 1, col: 0 },
        }
    }

    fn finish(self) -> String {
        self.buf
    }

    /// Write a string slice, updating the position cursor.
    /// Caller must ensure `src` starts at `target` and contains no newlines
    /// (multi-line tokens are not part of the Fink token model for printable tokens).
    fn write(&mut self, target: Pos, src: &str) {
        self.gap(target);
        self.buf.push_str(src);
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
            }
            for _ in 0..target.col {
                self.buf.push(' ');
            }
        } else {
            // Same line — emit spaces.
            let spaces = target.idx - self.pos.idx;
            for _ in 0..spaces {
                self.buf.push(' ');
            }
        }
        self.pos = target;
    }

    fn tok(&mut self, tok: &Token) {
        self.write(tok.loc.start, tok.src);
    }

    fn node(&mut self, node: &Node) {
        match &node.kind {
            // --- leaves ---
            NodeKind::LitBool(v) => {
                self.write(node.loc.start, if *v { "true" } else { "false" });
            }
            NodeKind::LitInt(s)
            | NodeKind::LitFloat(s)
            | NodeKind::LitDecimal(s)
            | NodeKind::Ident(s) => {
                self.write(node.loc.start, s);
            }
            NodeKind::Partial => self.write(node.loc.start, "?"),
            NodeKind::Wildcard => self.write(node.loc.start, "_"),

            // --- string literal ---
            NodeKind::LitStr { open, close, content } => {
                self.tok(open);
                // Content sits between open.end and close.start — gap() handles spacing.
                self.write(open.loc.end, content);
                self.tok(close);
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
                self.node(subjects);
                self.tok(sep);
                self.exprs(arms);
            }
            NodeKind::Arm { lhs, sep, body } => {
                self.exprs(lhs);
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
        if let NodeKind::LitStr { open, close, content } = &node.kind {
            // Write `}` if this segment follows an expression interpolation.
            if open.kind == TokenKind::StrExprEnd {
                self.tok(open);
            }
            // Write content (may be empty).
            if !content.is_empty() {
                self.write(open.loc.end, content);
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
        let result = parser::parse(src)
            .unwrap_or_else(|e| panic!("parse error: {}", e.message));
        let output = super::print(&result.root);
        if output == src { "NO-DIFF".to_string() } else { output }
    }

    test_macros::include_fink_tests!("src/fmt/test_print.fnk");
}
