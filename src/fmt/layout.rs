// Stage 2 — layout: AST → AST with canonical locs
//
// Walks the input AST and produces a clone with locs rewritten to canonical
// positions that satisfy the formatting rules in FmtConfig.
//
// Design
// ------
// The layout pass is a recursive tree rewrite. Each node is visited top-down
// and assigned a starting position (line, col). Children are placed relative
// to their parent using the formatting rules below.
//
// The pass operates in two modes:
//
//   Preserve mode  — input has locs (idx > 0 or line > 1). The existing
//                    layout is kept unless it violates a hard rule.
//   Canonical mode — input has no locs (all idx/line/col == 0). Canonical
//                    default representation is produced.
//
// Hard rules that trigger reformatting even in preserve mode:
//   1. Wrong indentation depth — normalised to indent_width * depth spaces.
//   2. Apply with ≥2 direct (ungrouped) args where any arg is itself an
//      ungrouped Apply — expand to one arg per indented line.
//   3. Apply whose single arg is a multi-arg bare Apply — expand.
//   4. Any line exceeds max_width — break args/ops to new lines.
//   5. LitRec where any field value contains a Fn with a body block — expand
//      to one field per line.
//
// Position model
// --------------
// Pos { line (1-based), col (0-based), idx (byte offset from file start) }.
// Layout works in a virtual document starting at line=1, col=0, idx=0.
// Each node is placed at a specific (line, col) and produces an end Pos.

use crate::passes::ast::{Node, NodeKind, CmpPart, Exprs};
use crate::lexer::{Loc, Pos, Token};
use super::FmtConfig;

/// Run the layout pass on `root` and return a new Node tree with canonical locs.
///
/// If `root` has real locs (parsed from source), the pass operates in
/// *preserve mode*: existing layout is kept unless a hard formatting rule is
/// violated (wrong indentation, ambiguous apply, line too wide, etc.).
///
/// If `root` has no locs (synthesised AST, all positions zero), the pass
/// operates in *canonical mode*: a fresh canonical layout is produced from
/// scratch using `FmtConfig` defaults.
pub fn layout<'src>(root: &Node<'src>, cfg: &FmtConfig) -> Node<'src> {
    let canonical = root.loc.start.idx == 0 && root.loc.end.idx == 0
        && root.loc.start.line <= 1;
    let mut ctx = Ctx::new(cfg);
    if canonical {
        ctx.node(root, Pos { idx: 0, line: 1, col: 0 })
    } else {
        ctx.fix(root)
    }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

struct Ctx<'cfg> {
    cfg: &'cfg FmtConfig,
    /// Column of the enclosing block/statement. Body indentation is always
    /// `block_col + indent_width`, regardless of where the fn/match keyword sits.
    block_col: u32,
}

impl<'cfg> Ctx<'cfg> {
    fn new(cfg: &'cfg FmtConfig) -> Self {
        Self { cfg, block_col: 0 }
    }

    fn indent_width(&self) -> u32 { self.cfg.indent }
    fn max_width(&self) -> u32 { self.cfg.max_width }

    /// Run `f` with `block_col` set to `col`, then restore the previous value.
    fn with_block_col<R>(&mut self, col: u32, f: impl FnOnce(&mut Self) -> R) -> R {
        let prev = self.block_col;
        self.block_col = col;
        let r = f(self);
        self.block_col = prev;
        r
    }

    // -----------------------------------------------------------------------
    // Preserve-mode: fix violations, keep everything else
    // -----------------------------------------------------------------------

    /// Preserve-mode entry: walk the tree, fixing violations in-place.
    /// Nodes that don't violate any rule are returned with their original locs.
    fn fix<'src>(&mut self, node: &Node<'src>) -> Node<'src> {
        match &node.kind {
            // Leaves — always fine as-is.
            NodeKind::LitBool(_)
            | NodeKind::LitInt(_)
            | NodeKind::LitFloat(_)
            | NodeKind::LitDecimal(_)
            | NodeKind::LitStr { .. }
            | NodeKind::Ident(_)
            | NodeKind::Partial
            | NodeKind::Wildcard => node.clone(),

            // Fn: fix indentation of body block.
            NodeKind::Fn { params, sep, body } => {
                self.fix_fn(node, params, sep, body)
            }

            // Match: fix indentation of arms block.
            NodeKind::Match { subjects, sep, arms } => {
                self.fix_match(node, subjects, sep, arms)
            }

            // Arm: fix indentation of body block.
            NodeKind::Arm { lhs, sep, body } => {
                self.fix_arm(node, lhs, sep, body)
            }

            // Apply: fix ambiguous / too-wide cases.
            NodeKind::Apply { func, args } => {
                self.fix_apply(node, func, args)
            }

            // InfixOp: fix if line exceeds max_width.
            NodeKind::InfixOp { op, lhs, rhs } => {
                self.fix_infix(node, op, lhs, rhs)
            }

            // LitRec: expand if any value is fn-with-body.
            NodeKind::LitRec { items, .. } => {
                if items.items.iter().any(|i| rec_item_needs_expand(i)) {
                    // Rewrite from scratch at original position.
                    self.node(node, node.loc.start)
                } else {
                    self.fix_children(node)
                }
            }

            // Everything else: recurse into children to fix nested violations.
            _ => self.fix_children(node),
        }
    }

    /// Recursively fix all children of a node, returning a cloned node with
    /// fixed children but the same locs as the original.
    fn fix_children<'src>(&mut self, node: &Node<'src>) -> Node<'src> {
        // For most structural nodes, recursing into children is sufficient.
        // We clone and rebuild only the child fields.
        match &node.kind {
            NodeKind::UnaryOp { op, operand } => {
                Node::new(NodeKind::UnaryOp { op: *op, operand: Box::new(self.fix(operand)) }, node.loc)
            }
            NodeKind::InfixOp { op, lhs, rhs } => {
                Node::new(NodeKind::InfixOp { op: *op, lhs: Box::new(self.fix(lhs)), rhs: Box::new(self.fix(rhs)) }, node.loc)
            }
            NodeKind::Bind { op, lhs, rhs } => {
                Node::new(NodeKind::Bind { op: *op, lhs: Box::new(self.fix(lhs)), rhs: Box::new(self.fix(rhs)) }, node.loc)
            }
            NodeKind::BindRight { op, lhs, rhs } => {
                Node::new(NodeKind::BindRight { op: *op, lhs: Box::new(self.fix(lhs)), rhs: Box::new(self.fix(rhs)) }, node.loc)
            }
            NodeKind::Group { open, close, inner } => {
                Node::new(NodeKind::Group { open: *open, close: *close, inner: Box::new(self.fix(inner)) }, node.loc)
            }
            NodeKind::Member { op, lhs, rhs } => {
                Node::new(NodeKind::Member { op: *op, lhs: Box::new(self.fix(lhs)), rhs: Box::new(self.fix(rhs)) }, node.loc)
            }
            NodeKind::Spread { op, inner } => {
                Node::new(NodeKind::Spread { op: *op, inner: inner.as_ref().map(|n| Box::new(self.fix(n))) }, node.loc)
            }
            NodeKind::Try(inner) => {
                Node::new(NodeKind::Try(Box::new(self.fix(inner))), node.loc)
            }
            NodeKind::Yield(inner) => {
                Node::new(NodeKind::Yield(Box::new(self.fix(inner))), node.loc)
            }
            NodeKind::ChainedCmp(parts) => {
                let new_parts = parts.iter().map(|p| match p {
                    crate::passes::ast::CmpPart::Operand(n) => crate::passes::ast::CmpPart::Operand(self.fix(n)),
                    crate::passes::ast::CmpPart::Op(op) => crate::passes::ast::CmpPart::Op(*op),
                }).collect();
                Node::new(NodeKind::ChainedCmp(new_parts), node.loc)
            }
            NodeKind::Pipe(exprs) => {
                let new_exprs = self.fix_exprs(exprs);
                Node::new(NodeKind::Pipe(new_exprs), node.loc)
            }
            NodeKind::Patterns(exprs) => {
                let new_exprs = self.fix_exprs(exprs);
                Node::new(NodeKind::Patterns(new_exprs), node.loc)
            }
            NodeKind::LitSeq { open, close, items } => {
                Node::new(NodeKind::LitSeq { open: *open, close: *close, items: self.fix_exprs(items) }, node.loc)
            }
            NodeKind::LitRec { open, close, items } => {
                Node::new(NodeKind::LitRec { open: *open, close: *close, items: self.fix_exprs(items) }, node.loc)
            }
            NodeKind::StrTempl { open, close, children } => {
                Node::new(NodeKind::StrTempl { open: *open, close: *close, children: children.iter().map(|c| self.fix(c)).collect() }, node.loc)
            }
            NodeKind::StrRawTempl { open, close, children } => {
                Node::new(NodeKind::StrRawTempl { open: *open, close: *close, children: children.iter().map(|c| self.fix(c)).collect() }, node.loc)
            }
            NodeKind::Block { name, params, sep, body } => {
                Node::new(NodeKind::Block {
                    name: Box::new(self.fix(name)),
                    params: Box::new(self.fix(params)),
                    sep: *sep,
                    body: self.fix_exprs(body),
                }, node.loc)
            }
            // Leaves — no children to fix.
            _ => node.clone(),
        }
    }

    fn fix_exprs<'src>(&mut self, exprs: &Exprs<'src>) -> Exprs<'src> {
        Exprs {
            items: exprs.items.iter().map(|n| self.fix(n)).collect(),
            seps: exprs.seps.clone(),
        }
    }

    fn fix_fn<'src>(
        &mut self,
        node: &Node<'src>,
        params: &Node<'src>,
        sep: &Token<'src>,
        body: &Exprs<'src>,
    ) -> Node<'src> {
        if body.items.len() <= 1 {
            // Single-expr body — recurse into children, no block fix needed.
            return self.fix_children(node);
        }
        // Multi-line body: check that each statement is at the correct indentation.
        let expected_col = self.block_col + self.indent_width();
        let wrong_indent = body.items.iter().any(|s| s.loc.start.col != expected_col);
        if wrong_indent {
            // Rewrite the fn from its original start position with canonical body layout.
            self.node(node, node.loc.start)
        } else {
            // Correct indentation — recurse into body stmts.
            let new_params = self.fix(params);
            let new_body = self.with_block_col(expected_col, |ctx| {
                Exprs {
                    items: body.items.iter().map(|s| ctx.fix(s)).collect(),
                    seps: body.seps.clone(),
                }
            });
            Node::new(NodeKind::Fn { params: Box::new(new_params), sep: *sep, body: new_body }, node.loc)
        }
    }

    fn fix_match<'src>(
        &mut self,
        node: &Node<'src>,
        subjects: &Exprs<'src>,
        sep: &Token<'src>,
        arms: &Exprs<'src>,
    ) -> Node<'src> {
        let expected_col = self.block_col + self.indent_width();
        let wrong_indent = arms.items.iter().any(|a| a.loc.start.col != expected_col);
        if wrong_indent {
            self.node(node, node.loc.start)
        } else {
            let new_subjects = Exprs {
                items: subjects.items.iter().map(|s| self.fix(s)).collect(),
                seps: subjects.seps.clone(),
            };
            let new_arms = self.with_block_col(expected_col, |ctx| {
                Exprs {
                    items: arms.items.iter().map(|a| ctx.fix(a)).collect(),
                    seps: arms.seps.clone(),
                }
            });
            Node::new(NodeKind::Match { subjects: new_subjects, sep: *sep, arms: new_arms }, node.loc)
        }
    }

    fn fix_arm<'src>(
        &mut self,
        node: &Node<'src>,
        lhs: &Node<'src>,
        sep: &Token<'src>,
        body: &Exprs<'src>,
    ) -> Node<'src> {
        if body.items.len() <= 1 {
            return self.fix_children(node);
        }
        let expected_col = self.block_col + self.indent_width();
        let wrong_indent = body.items.iter().any(|s| s.loc.start.col != expected_col);
        if wrong_indent {
            self.node(node, node.loc.start)
        } else {
            let new_lhs = self.fix(lhs);
            let new_body = self.with_block_col(expected_col, |ctx| {
                Exprs {
                    items: body.items.iter().map(|s| ctx.fix(s)).collect(),
                    seps: body.seps.clone(),
                }
            });
            Node::new(NodeKind::Arm { lhs: Box::new(new_lhs), sep: *sep, body: new_body }, node.loc)
        }
    }

    fn fix_apply<'src>(
        &mut self,
        node: &Node<'src>,
        func: &Node<'src>,
        args: &Exprs<'src>,
    ) -> Node<'src> {
        // If any arg is already on its own line (multiline fn body, or block-indented),
        // preserve the existing layout — the user explicitly chose this form.
        let already_expanded = args.items.iter()
            .any(|a| a.loc.start.line > func.loc.start.line);
        if already_expanded {
            return self.fix_children(node);
        }

        if should_expand_apply(args) {
            // Ambiguous — rewrite from original position.
            self.node(node, node.loc.start)
        } else {
            // Check width.
            let inline_w = inline_width_apply(func, args);
            if node.loc.start.col + inline_w > self.max_width() {
                self.node(node, node.loc.start)
            } else {
                self.fix_children(node)
            }
        }
    }

    fn fix_infix<'src>(
        &mut self,
        node: &Node<'src>,
        op: &Token<'src>,
        lhs: &Node<'src>,
        rhs: &Node<'src>,
    ) -> Node<'src> {
        let inline_w = inline_width_infix(op, lhs, rhs);
        if node.loc.start.col + inline_w > self.max_width() {
            self.node(node, node.loc.start)
        } else {
            self.fix_children(node)
        }
    }

    // -----------------------------------------------------------------------
    // Node placement
    // -----------------------------------------------------------------------

    /// Place `node` starting at `at`, return the rewritten node.
    fn node<'src>(&mut self, node: &Node<'src>, at: Pos) -> Node<'src> {
        match &node.kind {
            // Leaves — just place at `at`.
            NodeKind::LitBool(_)
            | NodeKind::LitInt(_)
            | NodeKind::LitFloat(_)
            | NodeKind::LitDecimal(_)
            | NodeKind::Ident(_)
            | NodeKind::Partial
            | NodeKind::Wildcard => {
                let src_len = src_len_of(node);
                Node::new(node.kind.clone(), loc(at, src_len))
            }

            NodeKind::LitStr { open, close, content, indent } => {
                // For block strings, recompute indent as block_col + indent_width.
                // For quoted strings, indent is 0 and unused.
                let new_indent = if open.src == "\":" {
                    self.block_col + self.cfg.indent
                } else {
                    *indent
                };
                self.lit_str(open, close, content, new_indent, at)
            }

            NodeKind::LitSeq { open, close, items } => {
                self.collection(open, close, items, at, false)
            }
            NodeKind::LitRec { open, close, items } => {
                // Expand if any field value is a fn-with-body.
                let force_expand = items.items.iter().any(|item| rec_item_needs_expand(item));
                self.collection_maybe_expand(open, close, items, at, force_expand)
            }

            NodeKind::StrTempl { open, close, children } => {
                self.str_templ(open, close, children, at, false)
            }
            NodeKind::StrRawTempl { open, close, children } => {
                self.str_templ(open, close, children, at, true)
            }

            NodeKind::UnaryOp { op, operand } => {
                self.unary_op(op, operand, at)
            }
            NodeKind::InfixOp { op, lhs, rhs } => {
                self.infix_op(op, lhs, rhs, at)
            }
            NodeKind::ChainedCmp(parts) => {
                self.chained_cmp(parts, at)
            }
            NodeKind::Spread { op, inner } => {
                self.spread(op, inner, at)
            }
            NodeKind::Member { op, lhs, rhs } => {
                self.member(op, lhs, rhs, at)
            }
            NodeKind::Group { open, close, inner } => {
                self.group(open, close, inner, at)
            }
            NodeKind::Bind { op, lhs, rhs } => {
                self.bind(op, lhs, rhs, at)
            }
            NodeKind::BindRight { op, lhs, rhs } => {
                self.bind_right(op, lhs, rhs, at)
            }
            NodeKind::Apply { func, args } => {
                self.apply(func, args, at)
            }
            NodeKind::Pipe(exprs) => {
                self.pipe(exprs, at)
            }
            NodeKind::Fn { params, sep, body } => {
                self.fn_node(params, sep, body, at)
            }
            NodeKind::Patterns(exprs) => {
                self.patterns(exprs, at)
            }
            NodeKind::Match { subjects, sep, arms } => {
                self.match_node(subjects, sep, arms, at)
            }
            NodeKind::Arm { lhs, sep, body } => {
                self.arm(lhs, sep, body, at)
            }
            NodeKind::Try(inner) => {
                self.try_node(inner, at)
            }
            NodeKind::Yield(inner) => {
                self.yield_node(inner, at)
            }
            NodeKind::Block { name, params, sep, body } => {
                self.block_node(name, params, sep, body, at)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Leaves
    // -----------------------------------------------------------------------

    fn lit_str<'src>(
        &mut self,
        open: &Token<'src>,
        close: &Token<'src>,
        content: &str,
        indent: u32,
        at: Pos,
    ) -> Node<'src> {
        // Re-place open at `at`. Content and close follow using original relative
        // positions within the string (multiline strings must preserve their
        // internal newlines and indentation).
        let open_len = open.src.len() as u32;
        let open_end = Pos { idx: at.idx + open_len, line: at.line, col: at.col + open_len };
        let new_open = place_tok(open, at);
        // Compute content_end. For block strings, print.rs emits each content line
        // as \n + indent_col spaces + line, so the byte count and final position
        // differ from raw content.len(). Compute from the actual lines written.
        let content_end = if open.src == "\":" {
            // Lines that will be written: content split by \n, skipping leading empty
            // (from mid-template "\n..." segments) and trailing empty (from final \n).
            // This mirrors write_block_str_content's exact behavior.
            let all_parts: Vec<&str> = content.split('\n').collect();
            let n = all_parts.len();
            let mut lines: Vec<&str> = Vec::new();
            let mut started = false;
            for (i, &line) in all_parts.iter().enumerate() {
                if !started && line.is_empty() { continue; }
                if i == n - 1 && line.is_empty() { break; }
                started = true;
                lines.push(line);
            }
            let line_count = lines.len() as u32;
            let last_len = lines.last().map(|l| l.len() as u32).unwrap_or(0);
            // Each line contributes 1 (newline) + indent + line.len() bytes.
            let total_bytes: u32 = lines.iter().map(|l| 1 + indent + l.len() as u32).sum();
            Pos {
                idx: open_end.idx + total_bytes,
                line: open_end.line + line_count,
                col: indent + last_len,
            }
        } else {
            let content_len = content.len() as u32;
            Pos {
                idx: open_end.idx + content_len,
                line: open_end.line,
                col: open_end.col + content_len,
            }
        };
        let new_close = place_tok(close, content_end);
        let close_end = advance_pos(content_end, close.src);
        Node::new(
            NodeKind::LitStr { open: new_open, close: new_close, content: content.to_string(), indent },
            Loc { start: at, end: close_end },
        )
    }

    // -----------------------------------------------------------------------
    // Collections
    // -----------------------------------------------------------------------

    fn collection<'src>(
        &mut self,
        open: &Token<'src>,
        close: &Token<'src>,
        items: &Exprs<'src>,
        at: Pos,
        _is_rec: bool,
    ) -> Node<'src> {
        self.collection_maybe_expand(open, close, items, at, false)
    }

    fn collection_maybe_expand<'src>(
        &mut self,
        open: &Token<'src>,
        close: &Token<'src>,
        items: &Exprs<'src>,
        at: Pos,
        force_expand: bool,
    ) -> Node<'src> {
        // Try inline first.
        let inline = self.try_inline_collection(open, close, items, at);
        if !force_expand && let Some(n) = inline { return n; }
        // Expanded: one item per line at at.col + indent.
        self.expanded_collection(open, close, items, at)
    }

    fn try_inline_collection<'src>(
        &mut self,
        open: &Token<'src>,
        close: &Token<'src>,
        items: &Exprs<'src>,
        at: Pos,
    ) -> Option<Node<'src>> {
        // Measure inline width.
        let width = inline_width_collection(open, close, items);
        if at.col + width > self.max_width() {
            return None;
        }
        Some(self.place_inline_collection(open, close, items, at))
    }

    fn place_inline_collection<'src>(
        &mut self,
        open: &Token<'src>,
        close: &Token<'src>,
        items: &Exprs<'src>,
        at: Pos,
    ) -> Node<'src> {
        let new_open = place_tok(open, at);
        let mut pos = advance_pos(at, open.src);
        let mut new_items = Vec::new();
        let mut new_seps = Vec::new();
        for (i, item) in items.items.iter().enumerate() {
            let item_node = self.node(item, pos);
            pos = item_node.loc.end;
            new_items.push(item_node);
            if let Some(sep) = items.seps.get(i) {
                let new_sep = place_tok(sep, pos);
                pos = advance_pos(pos, sep.src);
                pos.col += 1; // space after sep
                pos.idx += 1;
                new_seps.push(new_sep);
            }
        }
        let new_close = place_tok(close, pos);
        let end = advance_pos(pos, close.src);
        let new_exprs = Exprs { items: new_items, seps: new_seps };
        // Determine whether this was a LitSeq or LitRec by the open token.
        let kind = if open.src == "[" {
            NodeKind::LitSeq { open: new_open, close: new_close, items: new_exprs }
        } else {
            NodeKind::LitRec { open: new_open, close: new_close, items: new_exprs }
        };
        Node::new(kind, Loc { start: at, end })
    }

    fn expanded_collection<'src>(
        &mut self,
        open: &Token<'src>,
        close: &Token<'src>,
        items: &Exprs<'src>,
        at: Pos,
    ) -> Node<'src> {
        let new_open = place_tok(open, at);
        // For collections, indentation is relative to the `{`/`[` position, not the block base.
        let child_col = at.col + self.indent_width();
        let mut prev = advance_pos(at, open.src);
        let mut new_items = Vec::new();
        let new_seps = Vec::new(); // block-style: no inline seps
        for item in &items.items {
            let item_at = newline_pos(prev, prev.line + 1, child_col);
            let item_node = self.with_block_col(child_col, |ctx| ctx.node(item, item_at));
            prev = item_node.loc.end;
            new_items.push(item_node);
        }
        let close_at = newline_pos(prev, prev.line + 1, at.col);
        let new_close = place_tok(close, close_at);
        let end = advance_pos(close_at, close.src);
        let new_exprs = Exprs { items: new_items, seps: new_seps };
        let kind = if open.src == "[" {
            NodeKind::LitSeq { open: new_open, close: new_close, items: new_exprs }
        } else {
            NodeKind::LitRec { open: new_open, close: new_close, items: new_exprs }
        };
        Node::new(kind, Loc { start: at, end })
    }

    // -----------------------------------------------------------------------
    // String templates
    // -----------------------------------------------------------------------

    fn str_templ<'src>(
        &mut self,
        open: &Token<'src>,
        close: &Token<'src>,
        children: &[Node<'src>],
        at: Pos,
        raw: bool,
    ) -> Node<'src> {
        // String templates are preserved as-is (internal layout is semantic).
        // We just relocate the entire thing to `at`.
        let new_open = place_tok(open, at);
        let mut pos = advance_pos(at, open.src);
        let mut new_children = Vec::new();
        for child in children {
            let new_child = self.node(child, pos);
            pos = new_child.loc.end;
            new_children.push(new_child);
        }
        let new_close = place_tok(close, pos);
        let end = advance_pos(pos, close.src);
        let kind = if raw {
            NodeKind::StrRawTempl { open: new_open, close: new_close, children: new_children }
        } else {
            NodeKind::StrTempl { open: new_open, close: new_close, children: new_children }
        };
        Node::new(kind, Loc { start: at, end })
    }

    // -----------------------------------------------------------------------
    // Operators
    // -----------------------------------------------------------------------

    fn unary_op<'src>(&mut self, op: &Token<'src>, operand: &Node<'src>, at: Pos) -> Node<'src> {
        let new_op = place_tok(op, at);
        let operand_at = advance_pos(at, op.src);
        let new_operand = self.node(operand, operand_at);
        let end = new_operand.loc.end;
        Node::new(
            NodeKind::UnaryOp { op: new_op, operand: Box::new(new_operand) },
            Loc { start: at, end },
        )
    }

    fn infix_op<'src>(
        &mut self,
        op: &Token<'src>,
        lhs: &Node<'src>,
        rhs: &Node<'src>,
        at: Pos,
    ) -> Node<'src> {
        // Try inline first.
        let inline_w = inline_width_infix(op, lhs, rhs);
        if at.col + inline_w <= self.max_width() {
            return self.place_infix_inline(op, lhs, rhs, at);
        }
        // Exceeded width — operator-first continuation on next line.
        self.place_infix_expanded(op, lhs, rhs, at)
    }

    fn place_infix_inline<'src>(
        &mut self,
        op: &Token<'src>,
        lhs: &Node<'src>,
        rhs: &Node<'src>,
        at: Pos,
    ) -> Node<'src> {
        let new_lhs = self.node(lhs, at);
        let op_at = space_after(new_lhs.loc.end);
        let new_op = place_tok(op, op_at);
        let rhs_at = space_after(advance_pos(op_at, op.src));
        let new_rhs = self.node(rhs, rhs_at);
        let end = new_rhs.loc.end;
        Node::new(
            NodeKind::InfixOp { op: new_op, lhs: Box::new(new_lhs), rhs: Box::new(new_rhs) },
            Loc { start: at, end },
        )
    }

    fn place_infix_expanded<'src>(
        &mut self,
        op: &Token<'src>,
        lhs: &Node<'src>,
        rhs: &Node<'src>,
        at: Pos,
    ) -> Node<'src> {
        // lhs stays on the `at` line — but if lhs is also an infix with the same op
        // that exceeds width, recurse to expand it too. This flattens the chain.
        let op_col = self.block_col + self.indent_width();
        let new_lhs = if let NodeKind::InfixOp { op: inner_op, lhs: inner_lhs, rhs: inner_rhs } = &lhs.kind {
            if inner_op.src == op.src {
                let inner_inline_w = inline_width_infix(inner_op, inner_lhs, inner_rhs);
                if at.col + inner_inline_w > self.max_width() {
                    self.place_infix_expanded(inner_op, inner_lhs, inner_rhs, at)
                } else {
                    self.node(lhs, at)
                }
            } else {
                self.node(lhs, at)
            }
        } else {
            self.node(lhs, at)
        };
        let op_at = newline_pos(new_lhs.loc.end, new_lhs.loc.end.line + 1, op_col);
        let new_op = place_tok(op, op_at);
        let rhs_at = space_after(advance_pos(op_at, op.src));
        let new_rhs = self.node(rhs, rhs_at);
        let end = new_rhs.loc.end;
        Node::new(
            NodeKind::InfixOp { op: new_op, lhs: Box::new(new_lhs), rhs: Box::new(new_rhs) },
            Loc { start: at, end },
        )
    }

    fn chained_cmp<'src>(&mut self, parts: &[CmpPart<'src>], at: Pos) -> Node<'src> {
        // Inline only for now (wrapping chained cmps is rare / complex).
        let mut pos = at;
        let mut new_parts = Vec::new();
        for part in parts {
            match part {
                CmpPart::Operand(n) => {
                    let new_n = self.node(n, pos);
                    pos = new_n.loc.end;
                    new_parts.push(CmpPart::Operand(new_n));
                }
                CmpPart::Op(op) => {
                    let op_at = space_after(pos);
                    let new_op = place_tok(op, op_at);
                    pos = space_after(advance_pos(op_at, op.src));
                    new_parts.push(CmpPart::Op(new_op));
                }
            }
        }
        let end = match new_parts.last() {
            Some(CmpPart::Operand(n)) => n.loc.end,
            Some(CmpPart::Op(op)) => advance_pos(op.loc.start, op.src),
            None => at,
        };
        Node::new(NodeKind::ChainedCmp(new_parts), Loc { start: at, end })
    }

    fn spread<'src>(&mut self, op: &Token<'src>, inner: &Option<Box<Node<'src>>>, at: Pos) -> Node<'src> {
        let new_op = place_tok(op, at);
        let mut end = advance_pos(at, op.src);
        let new_inner = inner.as_ref().map(|n| {
            let new_n = self.node(n, end);
            end = new_n.loc.end;
            Box::new(new_n)
        });
        Node::new(NodeKind::Spread { op: new_op, inner: new_inner }, Loc { start: at, end })
    }

    fn member<'src>(
        &mut self,
        op: &Token<'src>,
        lhs: &Node<'src>,
        rhs: &Node<'src>,
        at: Pos,
    ) -> Node<'src> {
        let new_lhs = self.node(lhs, at);
        let op_at = new_lhs.loc.end;
        let new_op = place_tok(op, op_at);
        let rhs_at = advance_pos(op_at, op.src);
        let new_rhs = self.node(rhs, rhs_at);
        let end = new_rhs.loc.end;
        Node::new(
            NodeKind::Member { op: new_op, lhs: Box::new(new_lhs), rhs: Box::new(new_rhs) },
            Loc { start: at, end },
        )
    }

    fn group<'src>(
        &mut self,
        open: &Token<'src>,
        close: &Token<'src>,
        inner: &Node<'src>,
        at: Pos,
    ) -> Node<'src> {
        let new_open = place_tok(open, at);
        let inner_at = advance_pos(at, open.src);
        let new_inner = self.node(inner, inner_at);
        let close_at = new_inner.loc.end;
        let new_close = place_tok(close, close_at);
        let end = advance_pos(close_at, close.src);
        Node::new(
            NodeKind::Group { open: new_open, close: new_close, inner: Box::new(new_inner) },
            Loc { start: at, end },
        )
    }

    // -----------------------------------------------------------------------
    // Binding
    // -----------------------------------------------------------------------

    fn bind<'src>(
        &mut self,
        op: &Token<'src>,
        lhs: &Node<'src>,
        rhs: &Node<'src>,
        at: Pos,
    ) -> Node<'src> {
        let new_lhs = self.node(lhs, at);
        let op_at = space_after(new_lhs.loc.end);
        let new_op = place_tok(op, op_at);
        let rhs_at = space_after(advance_pos(op_at, op.src));
        let new_rhs = self.node(rhs, rhs_at);
        let end = new_rhs.loc.end;
        Node::new(
            NodeKind::Bind { op: new_op, lhs: Box::new(new_lhs), rhs: Box::new(new_rhs) },
            Loc { start: at, end },
        )
    }

    fn bind_right<'src>(
        &mut self,
        op: &Token<'src>,
        lhs: &Node<'src>,
        rhs: &Node<'src>,
        at: Pos,
    ) -> Node<'src> {
        // lhs may be multiline; |= and rhs go on the line after lhs ends.
        let new_lhs = self.node(lhs, at);
        let op_at = newline_pos(new_lhs.loc.end, new_lhs.loc.end.line + 1, at.col);
        let new_op = place_tok(op, op_at);
        let rhs_at = space_after(advance_pos(op_at, op.src));
        let new_rhs = self.node(rhs, rhs_at);
        let end = new_rhs.loc.end;
        Node::new(
            NodeKind::BindRight { op: new_op, lhs: Box::new(new_lhs), rhs: Box::new(new_rhs) },
            Loc { start: at, end },
        )
    }

    // -----------------------------------------------------------------------
    // Application
    // -----------------------------------------------------------------------

    fn apply<'src>(
        &mut self,
        func: &Node<'src>,
        args: &Exprs<'src>,
        at: Pos,
    ) -> Node<'src> {
        // Decision: expand or inline?
        if args.items.is_empty() {
            let new_func = self.node(func, at);
            let end = new_func.loc.end;
            return Node::new(
                NodeKind::Apply { func: Box::new(new_func), args: Exprs::empty() },
                Loc { start: at, end },
            );
        }

        if should_expand_apply(args) {
            return self.apply_expanded(func, args, at);
        }

        // Try inline.
        let inline_w = inline_width_apply(func, args);
        if at.col + inline_w <= self.max_width() {
            self.apply_inline(func, args, at)
        } else {
            self.apply_expanded(func, args, at)
        }
    }

    fn apply_inline<'src>(
        &mut self,
        func: &Node<'src>,
        args: &Exprs<'src>,
        at: Pos,
    ) -> Node<'src> {
        let new_func = self.node(func, at);
        let mut pos = space_after(new_func.loc.end);
        let mut new_items = Vec::new();
        let mut new_seps = Vec::new();
        for (i, arg) in args.items.iter().enumerate() {
            let new_arg = self.node(arg, pos);
            pos = new_arg.loc.end;
            new_items.push(new_arg);
            if let Some(sep) = args.seps.get(i) {
                let new_sep = place_tok(sep, pos);
                pos = space_after(advance_pos(pos, sep.src));
                new_seps.push(new_sep);
            }
        }
        let end = new_items.last().map(|n| n.loc.end).unwrap_or(new_func.loc.end);
        Node::new(
            NodeKind::Apply {
                func: Box::new(new_func),
                args: Exprs { items: new_items, seps: new_seps },
            },
            Loc { start: at, end },
        )
    }

    fn apply_expanded<'src>(
        &mut self,
        func: &Node<'src>,
        args: &Exprs<'src>,
        at: Pos,
    ) -> Node<'src> {
        let new_func = self.node(func, at);
        let child_col = self.block_col + self.indent_width();
        let mut prev = new_func.loc.end;

        // If any arg has a block body (fn, match), put each arg on its own line.
        // Otherwise all args go on one continuation line, comma-separated.
        let each_on_own_line = args.items.iter().any(|a| body_has_block(a));

        let mut new_items = Vec::new();
        let mut new_seps = Vec::new();

        if each_on_own_line {
            for (i, arg) in args.items.iter().enumerate() {
                let arg_at = newline_pos(prev, prev.line + 1, child_col);
                let new_arg = self.node(arg, arg_at);
                prev = new_arg.loc.end;
                new_items.push(new_arg);
                if let Some(sep) = args.seps.get(i) {
                    let new_sep = place_tok(sep, prev);
                    prev = advance_pos(prev, sep.src);
                    new_seps.push(new_sep);
                }
            }
        } else {
            // All args on one continuation line.
            let line_at = newline_pos(prev, prev.line + 1, child_col);
            let mut pos = line_at;
            for (i, arg) in args.items.iter().enumerate() {
                let new_arg = self.node(arg, pos);
                pos = new_arg.loc.end;
                new_items.push(new_arg);
                if let Some(sep) = args.seps.get(i) {
                    let new_sep = place_tok(sep, pos);
                    pos = space_after(advance_pos(pos, sep.src));
                    new_seps.push(new_sep);
                }
            }
            let _ = pos; // pos is the end of the args line; end computed from items below
        }
        let end = new_items.last().map(|n| n.loc.end).unwrap_or(new_func.loc.end);
        Node::new(
            NodeKind::Apply {
                func: Box::new(new_func),
                args: Exprs { items: new_items, seps: new_seps },
            },
            Loc { start: at, end },
        )
    }

    // -----------------------------------------------------------------------
    // Pipe
    // -----------------------------------------------------------------------

    fn pipe<'src>(&mut self, exprs: &Exprs<'src>, at: Pos) -> Node<'src> {
        // Pipes: preserve inline if all segments are simple (no args); otherwise one per line.
        let expand = pipe_needs_expand(exprs);
        let mut new_items = Vec::new();
        let mut new_seps = Vec::new();

        if expand {
            // First segment on current line; each subsequent on its own line.
            let first = self.node(&exprs.items[0], at);
            let mut prev = first.loc.end;
            new_items.push(first);
            for (i, item) in exprs.items[1..].iter().enumerate() {
                if let Some(sep) = exprs.seps.get(i) {
                    let sep_at = newline_pos(prev, prev.line + 1, at.col);
                    let new_sep = place_tok(sep, sep_at);
                    let after_sep = space_after(advance_pos(sep_at, sep.src));
                    let new_item = self.node(item, after_sep);
                    prev = new_item.loc.end;
                    new_seps.push(new_sep);
                    new_items.push(new_item);
                }
            }
        } else {
            // Inline: segments separated by ` | `.
            let first = self.node(&exprs.items[0], at);
            let mut pos = first.loc.end;
            new_items.push(first);
            for (i, item) in exprs.items[1..].iter().enumerate() {
                if let Some(sep) = exprs.seps.get(i) {
                    let sep_at = space_after(pos);
                    let new_sep = place_tok(sep, sep_at);
                    pos = space_after(advance_pos(sep_at, sep.src));
                    let new_item = self.node(item, pos);
                    pos = new_item.loc.end;
                    new_seps.push(new_sep);
                    new_items.push(new_item);
                }
            }
        }

        let end = new_items.last().map(|n| n.loc.end).unwrap_or(at);
        Node::new(
            NodeKind::Pipe(Exprs { items: new_items, seps: new_seps }),
            Loc { start: at, end },
        )
    }

    // -----------------------------------------------------------------------
    // Functions
    // -----------------------------------------------------------------------

    fn fn_node<'src>(
        &mut self,
        params: &Node<'src>,
        sep: &Token<'src>,
        body: &Exprs<'src>,
        at: Pos,
    ) -> Node<'src> {
        // `fn` keyword at `at`.
        let fn_end = advance_pos(at, "fn");
        // Params follow `fn` with a space (unless empty).
        let params_at = if params_is_empty(params) { fn_end } else { space_after(fn_end) };
        let new_params = self.node(params, params_at);

        // Determine if body fits on one line.
        let body_inline = body.items.len() == 1 && !body_has_block(&body.items[0]);
        if body_inline {
            // `:` immediately after params.
            let sep_at = new_params.loc.end;
            let new_sep = place_tok(sep, sep_at);
            let body_at = space_after(advance_pos(sep_at, sep.src));
            let new_body_item = self.node(&body.items[0], body_at);
            let end = new_body_item.loc.end;
            let new_seps = body.seps.clone(); // no inline body seps for single-item
            Node::new(
                NodeKind::Fn {
                    params: Box::new(new_params),
                    sep: new_sep,
                    body: Exprs { items: vec![new_body_item], seps: new_seps },
                },
                Loc { start: at, end },
            )
        } else {
            // Multi-line body — `:` after params, body indented.
            let sep_at = new_params.loc.end;
            let new_sep = place_tok(sep, sep_at);
            let sep_end = advance_pos(sep_at, sep.src);
            let child_col = self.block_col + self.indent_width();
            let mut prev = sep_end;
            let mut new_body_items = Vec::new();
            for stmt in &body.items {
                let stmt_at = newline_pos(prev, prev.line + 1, child_col);
                let new_stmt = self.with_block_col(child_col, |ctx| ctx.node(stmt, stmt_at));
                prev = new_stmt.loc.end;
                new_body_items.push(new_stmt);
            }
            let end = new_body_items.last().map(|n| n.loc.end).unwrap_or(sep_end);
            Node::new(
                NodeKind::Fn {
                    params: Box::new(new_params),
                    sep: new_sep,
                    body: Exprs { items: new_body_items, seps: vec![] },
                },
                Loc { start: at, end },
            )
        }
    }

    fn patterns<'src>(&mut self, exprs: &Exprs<'src>, at: Pos) -> Node<'src> {
        if exprs.items.is_empty() {
            return Node::new(NodeKind::Patterns(Exprs::empty()), Loc { start: at, end: at });
        }
        let mut pos = at;
        let mut new_items = Vec::new();
        let mut new_seps = Vec::new();
        for (i, item) in exprs.items.iter().enumerate() {
            let new_item = self.node(item, pos);
            pos = new_item.loc.end;
            new_items.push(new_item);
            if let Some(sep) = exprs.seps.get(i) {
                let new_sep = place_tok(sep, pos);
                pos = space_after(advance_pos(pos, sep.src));
                new_seps.push(new_sep);
            }
        }
        let end = new_items.last().map(|n| n.loc.end).unwrap_or(at);
        Node::new(
            NodeKind::Patterns(Exprs { items: new_items, seps: new_seps }),
            Loc { start: at, end },
        )
    }

    // -----------------------------------------------------------------------
    // Match
    // -----------------------------------------------------------------------

    fn match_node<'src>(
        &mut self,
        subjects: &Exprs<'src>,
        sep: &Token<'src>,
        arms: &Exprs<'src>,
        at: Pos,
    ) -> Node<'src> {
        let kw_end = advance_pos(at, "match");
        let mut prev_pos = space_after(kw_end);
        let mut new_subj_items = Vec::new();
        for (i, subj) in subjects.items.iter().enumerate() {
            if i > 0 {
                prev_pos = advance_pos(prev_pos, ", ");
            }
            let new_subj = self.node(subj, prev_pos);
            prev_pos = new_subj.loc.end;
            new_subj_items.push(new_subj);
        }
        let new_subjects = Exprs { items: new_subj_items, seps: subjects.seps.clone() };
        let sep_at = prev_pos;
        let new_sep = place_tok(sep, sep_at);
        let sep_end = advance_pos(sep_at, sep.src);
        let child_col = self.block_col + self.indent_width();
        let mut prev = sep_end;
        let mut new_arms = Vec::new();
        for arm in &arms.items {
            let arm_at = newline_pos(prev, prev.line + 1, child_col);
            let new_arm = self.with_block_col(child_col, |ctx| ctx.node(arm, arm_at));
            prev = new_arm.loc.end;
            new_arms.push(new_arm);
        }
        let end = new_arms.last().map(|n| n.loc.end).unwrap_or(sep_at);
        Node::new(
            NodeKind::Match {
                subjects: new_subjects,
                sep: new_sep,
                arms: Exprs { items: new_arms, seps: vec![] },
            },
            Loc { start: at, end },
        )
    }

    fn arm<'src>(
        &mut self,
        lhs: &Node<'src>,
        sep: &Token<'src>,
        body: &Exprs<'src>,
        at: Pos,
    ) -> Node<'src> {
        let new_lhs = self.node(lhs, at);
        let sep_at = new_lhs.loc.end;
        let new_sep = place_tok(sep, sep_at);

        // Body: inline if single expression, else indented block.
        let body_inline = body.items.len() == 1 && !body_has_block(&body.items[0]);
        if body_inline {
            let body_at = space_after(advance_pos(sep_at, sep.src));
            let new_body_item = self.node(&body.items[0], body_at);
            let end = new_body_item.loc.end;
            Node::new(
                NodeKind::Arm {
                    lhs: Box::new(new_lhs),
                    sep: new_sep,
                    body: Exprs { items: vec![new_body_item], seps: vec![] },
                },
                Loc { start: at, end },
            )
        } else {
            let child_col = self.block_col + self.indent_width();
            let sep_end = advance_pos(sep_at, sep.src);
            let mut prev = sep_end;
            let mut new_body_items = Vec::new();
            for stmt in &body.items {
                let stmt_at = newline_pos(prev, prev.line + 1, child_col);
                let new_stmt = self.with_block_col(child_col, |ctx| ctx.node(stmt, stmt_at));
                prev = new_stmt.loc.end;
                new_body_items.push(new_stmt);
            }
            let end = new_body_items.last().map(|n| n.loc.end).unwrap_or(sep_end);
            Node::new(
                NodeKind::Arm {
                    lhs: Box::new(new_lhs),
                    sep: new_sep,
                    body: Exprs { items: new_body_items, seps: vec![] },
                },
                Loc { start: at, end },
            )
        }
    }

    // -----------------------------------------------------------------------
    // Error handling / suspension
    // -----------------------------------------------------------------------

    fn try_node<'src>(&mut self, inner: &Node<'src>, at: Pos) -> Node<'src> {
        let kw_end = advance_pos(at, "try");
        let inner_at = space_after(kw_end);
        let new_inner = self.node(inner, inner_at);
        let end = new_inner.loc.end;
        Node::new(NodeKind::Try(Box::new(new_inner)), Loc { start: at, end })
    }

    fn yield_node<'src>(&mut self, inner: &Node<'src>, at: Pos) -> Node<'src> {
        let kw_end = advance_pos(at, "yield");
        let inner_at = space_after(kw_end);
        let new_inner = self.node(inner, inner_at);
        let end = new_inner.loc.end;
        Node::new(NodeKind::Yield(Box::new(new_inner)), Loc { start: at, end })
    }

    // -----------------------------------------------------------------------
    // Custom blocks
    // -----------------------------------------------------------------------

    fn block_node<'src>(
        &mut self,
        name: &Node<'src>,
        params: &Node<'src>,
        sep: &Token<'src>,
        body: &Exprs<'src>,
        at: Pos,
    ) -> Node<'src> {
        let new_name = self.node(name, at);
        let params_at = space_after(new_name.loc.end);
        let new_params = self.node(params, params_at);
        let sep_at = new_params.loc.end;
        let new_sep = place_tok(sep, sep_at);
        let sep_end = advance_pos(sep_at, sep.src);
        let child_col = at.col + self.indent_width();
        let mut prev = sep_end;
        let mut new_body_items = Vec::new();
        for stmt in &body.items {
            let stmt_at = newline_pos(prev, prev.line + 1, child_col);
            let new_stmt = self.node(stmt, stmt_at);
            prev = new_stmt.loc.end;
            new_body_items.push(new_stmt);
        }
        let end = new_body_items.last().map(|n| n.loc.end).unwrap_or(sep_at);
        Node::new(
            NodeKind::Block {
                name: Box::new(new_name),
                params: Box::new(new_params),
                sep: new_sep,
                body: Exprs { items: new_body_items, seps: vec![] },
            },
            Loc { start: at, end },
        )
    }
}

// ---------------------------------------------------------------------------
// Decision helpers
// ---------------------------------------------------------------------------

/// Returns true if the Apply args should be expanded to multiple lines.
fn should_expand_apply(args: &Exprs) -> bool {
    // Single arg that is itself a multi-arg bare apply → expand (ambiguous).
    if args.items.len() == 1 {
        if let NodeKind::Apply { args: inner_args, .. } = &args.items[0].kind {
            return inner_args.items.len() >= 2;
        }
        return false;
    }
    // Multiple args — expand if any direct arg is an ungrouped Apply.
    args.items.iter().any(|arg| matches!(arg.kind, NodeKind::Apply { .. }))
}

/// Returns true if any pipe segment has args (making inline ambiguous).
fn pipe_needs_expand(exprs: &Exprs) -> bool {
    exprs.items.iter().skip(1).any(|seg| matches!(seg.kind, NodeKind::Apply { .. }))
}

/// Returns true if a LitRec field item's value is a fn-with-body.
fn rec_item_needs_expand(item: &Node) -> bool {
    // Arm: value is the body. Check if any Arm value is a Fn node.
    match &item.kind {
        NodeKind::Arm { body, .. } => {
            body.items.len() == 1 && matches!(body.items[0].kind, NodeKind::Fn { .. })
        }
        _ => false,
    }
}

/// Returns true if a node's layout occupies multiple lines (has a block body).
fn body_has_block(node: &Node) -> bool {
    match &node.kind {
        NodeKind::Fn { body, .. } => body.items.len() > 1,
        NodeKind::Match { .. } => true,
        _ => false,
    }
}

fn params_is_empty(params: &Node) -> bool {
    matches!(&params.kind, NodeKind::Patterns(e) if e.items.is_empty())
}

// ---------------------------------------------------------------------------
// Width measurement helpers (inline)
// ---------------------------------------------------------------------------

fn src_len_of(node: &Node) -> u32 {
    match &node.kind {
        NodeKind::LitBool(v) => if *v { 4 } else { 5 },
        NodeKind::LitInt(s) | NodeKind::LitFloat(s) | NodeKind::LitDecimal(s) | NodeKind::Ident(s) => s.len() as u32,
        NodeKind::Partial => 1,
        NodeKind::Wildcard => 1,
        _ => 0, // non-leaf; not used by this helper
    }
}

fn inline_width_node(node: &Node) -> u32 {
    // If the node has real locs and spans a single line, use the actual source width.
    // This avoids needing to recursively compute widths for complex nodes.
    if node.loc.start.idx > 0 || node.loc.end.idx > 0 {
        if node.loc.start.line == node.loc.end.line {
            return node.loc.end.col - node.loc.start.col;
        } else {
            // Multiline node — can never be inlined as-is.
            return u32::MAX / 2;
        }
    }
    match &node.kind {
        NodeKind::LitBool(v) => if *v { 4 } else { 5 },
        NodeKind::LitInt(s) | NodeKind::LitFloat(s) | NodeKind::LitDecimal(s) | NodeKind::Ident(s) => s.len() as u32,
        NodeKind::Partial => 1,
        NodeKind::Wildcard => 1,
        NodeKind::Group { open, close, inner } => {
            open.src.len() as u32 + inline_width_node(inner) + close.src.len() as u32
        }
        NodeKind::InfixOp { op, lhs, rhs } => {
            inline_width_node(lhs) + 1 + op.src.len() as u32 + 1 + inline_width_node(rhs)
        }
        NodeKind::UnaryOp { op, operand } => {
            op.src.len() as u32 + inline_width_node(operand)
        }
        NodeKind::Apply { func, args } => {
            inline_width_apply(func, args)
        }
        NodeKind::LitStr { open, close, content, .. } => {
            open.src.len() as u32 + content.len() as u32 + close.src.len() as u32
        }
        NodeKind::LitSeq { open, close, items } | NodeKind::LitRec { open, close, items } => {
            inline_width_collection(open, close, items)
        }
        NodeKind::Member { op, lhs, rhs } => {
            inline_width_node(lhs) + op.src.len() as u32 + inline_width_node(rhs)
        }
        _ => 40, // conservative estimate for complex nodes — will trigger expansion
    }
}

fn inline_width_apply<'src>(func: &Node<'src>, args: &Exprs<'src>) -> u32 {
    let mut w = inline_width_node(func);
    for (i, arg) in args.items.iter().enumerate() {
        w += 1 + inline_width_node(arg); // space + arg
        if let Some(sep) = args.seps.get(i) {
            w += sep.src.len() as u32;
        }
    }
    w
}

fn inline_width_infix<'src>(op: &Token<'src>, lhs: &Node<'src>, rhs: &Node<'src>) -> u32 {
    inline_width_node(lhs) + 1 + op.src.len() as u32 + 1 + inline_width_node(rhs)
}

fn inline_width_collection<'src>(
    open: &Token<'src>,
    close: &Token<'src>,
    items: &Exprs<'src>,
) -> u32 {
    let mut w = open.src.len() as u32 + close.src.len() as u32;
    for (i, item) in items.items.iter().enumerate() {
        w += inline_width_node(item);
        if let Some(sep) = items.seps.get(i) {
            w += sep.src.len() as u32 + 1; // sep + space
        }
    }
    w
}

// ---------------------------------------------------------------------------
// Position helpers
// ---------------------------------------------------------------------------

/// Compute the Pos for a token that starts on a new line at `target_col`,
/// given `prev` is the position after the last written byte.
///
/// The byte offset accounts for the newline characters and leading spaces
/// required to reach `(target_line, target_col)`.
fn newline_pos(prev: Pos, target_line: u32, target_col: u32) -> Pos {
    debug_assert!(target_line > prev.line, "newline_pos: target_line must be > prev.line");
    let newlines = target_line - prev.line; // number of '\n' chars
    let idx = prev.idx + newlines + target_col; // each space counts as 1 byte
    Pos { idx, line: target_line, col: target_col }
}

/// Place a token at position `at`, preserving its src content.
fn place_tok<'src>(tok: &Token<'src>, at: Pos) -> Token<'src> {
    Token {
        kind: tok.kind,
        src: tok.src,
        loc: Loc { start: at, end: advance_pos(at, tok.src) },
    }
}

/// Advance a position by the byte length of `s` (single-line only).
fn advance_pos(pos: Pos, s: &str) -> Pos {
    Pos {
        idx: pos.idx + s.len() as u32,
        line: pos.line,
        col: pos.col + s.len() as u32,
    }
}

/// Emit a single space — advance col and idx by 1.
fn space_after(pos: Pos) -> Pos {
    Pos { idx: pos.idx + 1, line: pos.line, col: pos.col + 1 }
}

/// Build a Loc spanning from `at` for `len` bytes.
fn loc(at: Pos, len: u32) -> Loc {
    Loc { start: at, end: Pos { idx: at.idx + len, line: at.line, col: at.col + len } }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::parser;
    use crate::fmt::{print, FmtConfig};
    use super::layout;

    /// Parse source, run through layout + print, return result.
    /// Returns "NO-DIFF" if output equals input.
    fn fmt(src: &str) -> String {
        let result = parser::parse(src)
            .unwrap_or_else(|e| panic!("parse error: {}", e.message));
        let cfg = FmtConfig::default();
        let laid_out = layout(&result.root, &cfg);
        let output = print::print(&laid_out);
        if output == src { "NO-DIFF".to_string() } else { output }
    }

    test_macros::include_fink_tests!("src/fmt/test_fmt.fnk");
}
