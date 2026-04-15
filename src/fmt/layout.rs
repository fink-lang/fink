// Stage 1 — layout: AST → AST with canonical locs
//
// Walks the input AST and produces a new `Ast` whose nodes carry locs rewritten
// to canonical positions satisfying the rules in `FmtConfig`.
//
// Design
// ------
// The layout pass is a recursive tree rewrite. Each node is visited top-down
// and assigned a starting position (line, col, idx). Children are placed
// relative to their parent using the formatting rules below.
//
// The pass operates in two modes:
//
//   Preserve mode  — input has locs (idx > 0 or line > 1). The existing
//                    layout is kept unless it violates a hard rule.
//   Canonical mode — input has no locs (all idx/line/col == 0). Canonical
//                    default representation is produced.
//
// Hard rules that trigger reformatting even in preserve mode:
//   1. Wrong indentation depth — normalised to `indent * depth` spaces.
//   2. Apply with ≥2 direct (ungrouped) args where any arg is itself an
//      ungrouped Apply — expand to one arg per indented line.
//   3. Apply whose single arg is a multi-arg bare Apply — expand.
//   4. Any line exceeds max_width — break args/ops to new lines.
//   5. LitRec where any field value contains a Fn with a body block — expand
//      to one field per line.
//
// Flat-arena notes
// ----------------
// Layout reads the input `Ast` via `src: &Ast` and writes into a fresh
// `AstBuilder`. Every output node is newly appended — no id is shared with
// the input. Methods return `AstId` into the builder's arena. The final
// `Ast` is produced via `builder.finish(new_root)` and returned directly.

use crate::ast::{Ast, AstBuilder, AstId, CmpPart, Exprs, Node, NodeKind};
use crate::passes::ast::lexer::{Loc, Pos, Token};
use super::FmtConfig;

/// Run the layout pass on `ast` and return a new `Ast` with canonical locs.
///
/// If the root has real locs (parsed from source), the pass operates in
/// *preserve mode*: existing layout is kept unless a hard rule is violated.
///
/// If the root has no locs (all-zero positions), the pass operates in
/// *canonical mode*: a fresh layout is produced from scratch.
pub fn layout<'src>(ast: &Ast<'src>, cfg: &FmtConfig) -> Ast<'src> {
    let root_node = ast.nodes.get(ast.root);
    let canonical = root_node.loc.start.idx == 0
        && root_node.loc.end.idx == 0
        && root_node.loc.start.line <= 1;
    let mut ctx = Ctx::new(ast, cfg);
    let new_root = if canonical {
        ctx.node(ast.root, Pos { idx: 0, line: 1, col: 0 })
    } else {
        ctx.fix(ast.root)
    };
    ctx.builder.finish(new_root)
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

struct Ctx<'a, 'src> {
    src: &'a Ast<'src>,
    builder: AstBuilder<'src>,
    cfg: &'a FmtConfig,
    /// Column of the enclosing block/statement. Body indentation is always
    /// `block_col + indent_width`, regardless of where the `fn`/`match`
    /// keyword sits.
    block_col: u32,
}

impl<'a, 'src> Ctx<'a, 'src> {
    fn new(src: &'a Ast<'src>, cfg: &'a FmtConfig) -> Self {
        Self { src, builder: AstBuilder::new(), cfg, block_col: 0 }
    }

    fn indent_width(&self) -> u32 { self.cfg.indent }
    fn max_width(&self) -> u32 { self.cfg.max_width }

    fn read(&self, id: AstId) -> &Node<'src> {
        self.src.nodes.get(id)
    }

    fn append(&mut self, kind: NodeKind<'src>, loc: Loc) -> AstId {
        self.builder.append(kind, loc)
    }

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

    /// Preserve-mode entry: walk the tree, fixing violations.
    /// Nodes that don't violate any rule are re-emitted with their original locs.
    fn fix(&mut self, id: AstId) -> AstId {
        let node = self.read(id).clone();
        match &node.kind {
            // Leaves — always fine as-is.
            NodeKind::LitBool(_)
            | NodeKind::LitInt(_)
            | NodeKind::LitFloat(_)
            | NodeKind::LitDecimal(_)
            | NodeKind::LitStr { .. }
            | NodeKind::Ident(_)
            | NodeKind::SynthIdent(_)
            | NodeKind::Partial
            | NodeKind::Wildcard
            | NodeKind::Token(_) => {
                self.append(node.kind.clone(), node.loc)
            }

            NodeKind::Fn { .. } => self.fix_fn(id),
            NodeKind::Match { .. } => self.fix_match(id),
            NodeKind::Arm { .. } => self.fix_arm(id),
            NodeKind::Apply { .. } => self.fix_apply(id),
            NodeKind::InfixOp { .. } => self.fix_infix(id),

            NodeKind::LitRec { items, .. } => {
                let needs = items.items.iter()
                    .any(|&i| rec_item_needs_expand(self.src, i));
                if needs {
                    self.node(id, node.loc.start)
                } else {
                    self.fix_children(id)
                }
            }

            _ => self.fix_children(id),
        }
    }

    /// Recurse into children to fix nested violations, re-emitting the node
    /// with its original loc.
    ///
    /// The legacy `&Node`-owning version had a `_ => node.clone()` catch-all
    /// that covered `Fn`/`Match`/`Arm`/`Apply` — their children were not
    /// re-walked, and the deep tree clone made that safe because children
    /// were owned by the returned `Node`. In the flat arena every output
    /// node must be freshly appended — reusing an input `AstId` in an
    /// output-arena child slot points at the wrong node, so we explicitly
    /// rebuild every non-leaf shape here, recursing through `fix` so nested
    /// violations still get fixed. Only true leaves (`LitInt`, `Ident`, …)
    /// fall through to the catch-all.
    fn fix_children(&mut self, id: AstId) -> AstId {
        let node = self.read(id).clone();
        let loc = node.loc;
        let new_kind = match node.kind {
            NodeKind::UnaryOp { op, operand } => NodeKind::UnaryOp {
                op,
                operand: self.fix(operand),
            },
            NodeKind::InfixOp { op, lhs, rhs } => NodeKind::InfixOp {
                op,
                lhs: self.fix(lhs),
                rhs: self.fix(rhs),
            },
            NodeKind::Bind { op, lhs, rhs } => NodeKind::Bind {
                op,
                lhs: self.fix(lhs),
                rhs: self.fix(rhs),
            },
            NodeKind::BindRight { op, lhs, rhs } => NodeKind::BindRight {
                op,
                lhs: self.fix(lhs),
                rhs: self.fix(rhs),
            },
            NodeKind::Group { open, close, inner } => NodeKind::Group {
                open,
                close,
                inner: self.fix(inner),
            },
            NodeKind::Member { op, lhs, rhs } => NodeKind::Member {
                op,
                lhs: self.fix(lhs),
                rhs: self.fix(rhs),
            },
            NodeKind::Spread { op, inner } => NodeKind::Spread {
                op,
                inner: inner.map(|n| self.fix(n)),
            },
            NodeKind::Try(inner) => NodeKind::Try(self.fix(inner)),
            NodeKind::ChainedCmp(parts) => {
                let new_parts: Box<[CmpPart<'src>]> = parts.iter().map(|p| match p {
                    CmpPart::Operand(n) => CmpPart::Operand(self.fix(*n)),
                    CmpPart::Op(op) => CmpPart::Op(*op),
                }).collect();
                NodeKind::ChainedCmp(new_parts)
            }
            NodeKind::Module { exprs, url } => NodeKind::Module {
                exprs: self.fix_exprs(&exprs),
                url,
            },
            NodeKind::Pipe(exprs) => NodeKind::Pipe(self.fix_exprs(&exprs)),
            NodeKind::Patterns(exprs) => NodeKind::Patterns(self.fix_exprs(&exprs)),
            NodeKind::LitSeq { open, close, items } => NodeKind::LitSeq {
                open, close,
                items: self.fix_exprs(&items),
            },
            NodeKind::LitRec { open, close, items } => NodeKind::LitRec {
                open, close,
                items: self.fix_exprs(&items),
            },
            NodeKind::StrTempl { open, close, children } => NodeKind::StrTempl {
                open, close,
                children: children.iter().map(|&c| self.fix(c)).collect(),
            },
            NodeKind::StrRawTempl { open, close, children } => NodeKind::StrRawTempl {
                open, close,
                children: children.iter().map(|&c| self.fix(c)).collect(),
            },
            NodeKind::Apply { func, args } => NodeKind::Apply {
                func: self.fix(func),
                args: self.fix_exprs(&args),
            },
            NodeKind::Fn { params, sep, body } => NodeKind::Fn {
                params: self.fix(params),
                sep,
                body: self.fix_exprs(&body),
            },
            NodeKind::Match { subjects, sep, arms } => NodeKind::Match {
                subjects: self.fix_exprs(&subjects),
                sep,
                arms: self.fix_exprs(&arms),
            },
            NodeKind::Arm { lhs, sep, body } => NodeKind::Arm {
                lhs: self.fix(lhs),
                sep,
                body: self.fix_exprs(&body),
            },
            NodeKind::Block { name, params, sep, body } => NodeKind::Block {
                name: self.fix(name),
                params: self.fix(params),
                sep,
                body: self.fix_exprs(&body),
            },
            // Remaining: true leaves (LitBool/LitInt/Ident/Partial/Wildcard/
            // SynthIdent/Token/LitStr) which have no child AstIds to rewrite,
            // so reusing their kind verbatim in the output arena is safe.
            other => other,
        };
        self.append(new_kind, loc)
    }

    fn fix_exprs(&mut self, exprs: &Exprs<'src>) -> Exprs<'src> {
        Exprs {
            items: exprs.items.iter().map(|&id| self.fix(id)).collect(),
            seps: exprs.seps.clone(),
        }
    }

    fn fix_fn(&mut self, id: AstId) -> AstId {
        let node = self.read(id).clone();
        let (params, sep, body) = match &node.kind {
            NodeKind::Fn { params, sep, body } => (*params, *sep, body.clone()),
            _ => unreachable!(),
        };
        if body.items.len() <= 1 {
            return self.fix_children(id);
        }
        let expected_col = self.block_col + self.indent_width();
        let wrong_indent = body.items.iter()
            .any(|&s| self.read(s).loc.start.col != expected_col);
        if wrong_indent {
            return self.node(id, node.loc.start);
        }
        let new_params = self.fix(params);
        let new_body = self.with_block_col(expected_col, |ctx| Exprs {
            items: body.items.iter().map(|&s| ctx.fix(s)).collect(),
            seps: body.seps.clone(),
        });
        self.append(
            NodeKind::Fn { params: new_params, sep, body: new_body },
            node.loc,
        )
    }

    fn fix_match(&mut self, id: AstId) -> AstId {
        let node = self.read(id).clone();
        let (subjects, sep, arms) = match &node.kind {
            NodeKind::Match { subjects, sep, arms } => (subjects.clone(), *sep, arms.clone()),
            _ => unreachable!(),
        };
        let expected_col = self.block_col + self.indent_width();
        let wrong_indent = arms.items.iter()
            .any(|&a| self.read(a).loc.start.col != expected_col);
        if wrong_indent {
            return self.node(id, node.loc.start);
        }
        let new_subjects = Exprs {
            items: subjects.items.iter().map(|&s| self.fix(s)).collect(),
            seps: subjects.seps.clone(),
        };
        let new_arms = self.with_block_col(expected_col, |ctx| Exprs {
            items: arms.items.iter().map(|&a| ctx.fix(a)).collect(),
            seps: arms.seps.clone(),
        });
        self.append(
            NodeKind::Match { subjects: new_subjects, sep, arms: new_arms },
            node.loc,
        )
    }

    fn fix_arm(&mut self, id: AstId) -> AstId {
        let node = self.read(id).clone();
        let (lhs, sep, body) = match &node.kind {
            NodeKind::Arm { lhs, sep, body } => (*lhs, *sep, body.clone()),
            _ => unreachable!(),
        };
        if body.items.len() <= 1 {
            return self.fix_children(id);
        }
        let expected_col = self.block_col + self.indent_width();
        let wrong_indent = body.items.iter()
            .any(|&s| self.read(s).loc.start.col != expected_col);
        if wrong_indent {
            return self.node(id, node.loc.start);
        }
        let new_lhs = self.fix(lhs);
        let new_body = self.with_block_col(expected_col, |ctx| Exprs {
            items: body.items.iter().map(|&s| ctx.fix(s)).collect(),
            seps: body.seps.clone(),
        });
        self.append(
            NodeKind::Arm { lhs: new_lhs, sep, body: new_body },
            node.loc,
        )
    }

    fn fix_apply(&mut self, id: AstId) -> AstId {
        let node = self.read(id).clone();
        let (func, args) = match &node.kind {
            NodeKind::Apply { func, args } => (*func, args.clone()),
            _ => unreachable!(),
        };
        // If any arg is already on its own line, preserve the layout.
        let func_line = self.read(func).loc.start.line;
        let already_expanded = args.items.iter()
            .any(|&a| self.read(a).loc.start.line > func_line);
        if already_expanded {
            return self.fix_children(id);
        }
        if should_expand_apply(self.src, &args) {
            return self.node(id, node.loc.start);
        }
        let inline_w = inline_width_apply(self.src, func, &args);
        if node.loc.start.col + inline_w > self.max_width() {
            self.node(id, node.loc.start)
        } else {
            self.fix_children(id)
        }
    }

    fn fix_infix(&mut self, id: AstId) -> AstId {
        let node = self.read(id).clone();
        let (op, lhs, rhs) = match &node.kind {
            NodeKind::InfixOp { op, lhs, rhs } => (*op, *lhs, *rhs),
            _ => unreachable!(),
        };
        let inline_w = inline_width_infix(self.src, &op, lhs, rhs);
        if node.loc.start.col + inline_w > self.max_width() {
            self.node(id, node.loc.start)
        } else {
            self.fix_children(id)
        }
    }

    // -----------------------------------------------------------------------
    // Node placement — canonical / rewrite mode
    // -----------------------------------------------------------------------

    /// Place the node identified by `id` starting at `at`, returning the new
    /// `AstId` in the output arena.
    fn node(&mut self, id: AstId, at: Pos) -> AstId {
        let node = self.read(id).clone();
        match node.kind {
            NodeKind::LitBool(v) => {
                let len = if v { 4 } else { 5 };
                self.append(NodeKind::LitBool(v), loc(at, len))
            }
            NodeKind::LitInt(s) => self.append(NodeKind::LitInt(s), loc(at, s.len() as u32)),
            NodeKind::LitFloat(s) => self.append(NodeKind::LitFloat(s), loc(at, s.len() as u32)),
            NodeKind::LitDecimal(s) => self.append(NodeKind::LitDecimal(s), loc(at, s.len() as u32)),
            NodeKind::Ident(s) => self.append(NodeKind::Ident(s), loc(at, s.len() as u32)),
            NodeKind::SynthIdent(n) => {
                let text = format!("·$_{n}");
                self.append(NodeKind::SynthIdent(n), loc(at, text.len() as u32))
            }
            NodeKind::Partial => self.append(NodeKind::Partial, loc(at, 1)),
            NodeKind::Wildcard => self.append(NodeKind::Wildcard, loc(at, 1)),
            NodeKind::Token(s) => self.append(NodeKind::Token(s), loc(at, s.len() as u32)),

            NodeKind::LitStr { open, close, content, indent } => {
                // For block strings, recompute indent as block_col + indent_width.
                let new_indent = if open.src == "\":" {
                    self.block_col + self.cfg.indent
                } else {
                    indent
                };
                self.lit_str(open, close, content, new_indent, at)
            }

            NodeKind::LitSeq { open, close, items } => {
                self.collection_maybe_expand(open, close, &items, at, false, true)
            }
            NodeKind::LitRec { open, close, items } => {
                let force_expand = items.items.iter()
                    .any(|&i| rec_item_needs_expand(self.src, i));
                self.collection_maybe_expand(open, close, &items, at, force_expand, false)
            }

            NodeKind::StrTempl { open, close, children } => {
                self.str_templ(open, close, &children, at, false)
            }
            NodeKind::StrRawTempl { open, close, children } => {
                self.str_templ(open, close, &children, at, true)
            }

            NodeKind::UnaryOp { op, operand } => self.unary_op(op, operand, at),
            NodeKind::InfixOp { op, lhs, rhs } => self.infix_op(op, lhs, rhs, at),
            NodeKind::ChainedCmp(parts) => self.chained_cmp(&parts, at),
            NodeKind::Spread { op, inner } => self.spread(op, inner, at),
            NodeKind::Member { op, lhs, rhs } => self.member(op, lhs, rhs, at),
            NodeKind::Group { open, close, inner } => self.group(open, close, inner, at),
            NodeKind::Bind { op, lhs, rhs } => self.bind(op, lhs, rhs, at),
            NodeKind::BindRight { op, lhs, rhs } => self.bind_right(op, lhs, rhs, at),
            NodeKind::Apply { func, args } => self.apply(func, &args, at),
            NodeKind::Pipe(exprs) => self.pipe(&exprs, at),

            NodeKind::Module { exprs, url } => {
                let child_col = self.block_col;
                let mut pos = at;
                let mut new_items = Vec::new();
                for &stmt in exprs.items.iter() {
                    let placed = self.node(stmt, pos);
                    let placed_end = self.builder.read(placed).loc.end;
                    pos = newline_pos(placed_end, placed_end.line + 1, child_col);
                    new_items.push(placed);
                }
                let end = new_items.last()
                    .map(|&n| self.builder.read(n).loc.end)
                    .unwrap_or(at);
                self.append(
                    NodeKind::Module {
                        exprs: Exprs { items: new_items.into_boxed_slice(), seps: exprs.seps.clone() },
                        url,
                    },
                    Loc { start: at, end },
                )
            }
            NodeKind::Fn { params, sep, body } => self.fn_node(params, sep, &body, at),
            NodeKind::Patterns(exprs) => self.patterns(&exprs, at),
            NodeKind::Match { subjects, sep, arms } => {
                self.match_node(&subjects, sep, &arms, at)
            }
            NodeKind::Arm { lhs, sep, body } => self.arm(lhs, sep, &body, at),
            NodeKind::Try(inner) => self.try_node(inner, at),
            NodeKind::Block { name, params, sep, body } => {
                self.block_node(name, params, sep, &body, at)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Leaves with structure
    // -----------------------------------------------------------------------

    fn lit_str(
        &mut self,
        open: Token<'src>,
        close: Token<'src>,
        content: String,
        indent: u32,
        at: Pos,
    ) -> AstId {
        let open_len = open.src.len() as u32;
        let open_end = Pos { idx: at.idx + open_len, line: at.line, col: at.col + open_len };
        let new_open = place_tok(&open, at);
        let content_end = if open.src == "\":" {
            // Mirror write_block_str_content's line accounting.
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
        let new_close = place_tok(&close, content_end);
        let close_end = advance_pos(content_end, close.src);
        self.append(
            NodeKind::LitStr { open: new_open, close: new_close, content, indent },
            Loc { start: at, end: close_end },
        )
    }

    // -----------------------------------------------------------------------
    // Collections
    // -----------------------------------------------------------------------

    fn collection_maybe_expand(
        &mut self,
        open: Token<'src>,
        close: Token<'src>,
        items: &Exprs<'src>,
        at: Pos,
        force_expand: bool,
        is_seq: bool,
    ) -> AstId {
        if !force_expand
            && let Some(inline) = self.try_inline_collection(&open, &close, items, at, is_seq)
        {
            return inline;
        }
        self.expanded_collection(&open, &close, items, at, is_seq)
    }

    fn try_inline_collection(
        &mut self,
        open: &Token<'src>,
        close: &Token<'src>,
        items: &Exprs<'src>,
        at: Pos,
        is_seq: bool,
    ) -> Option<AstId> {
        let width = inline_width_collection(self.src, open, close, items);
        if at.col + width > self.max_width() {
            return None;
        }
        Some(self.place_inline_collection(open, close, items, at, is_seq))
    }

    fn place_inline_collection(
        &mut self,
        open: &Token<'src>,
        close: &Token<'src>,
        items: &Exprs<'src>,
        at: Pos,
        is_seq: bool,
    ) -> AstId {
        let new_open = place_tok(open, at);
        let mut pos = advance_pos(at, open.src);
        let mut new_items = Vec::new();
        let mut new_seps = Vec::new();
        for (i, &item) in items.items.iter().enumerate() {
            let item_id = self.node(item, pos);
            pos = self.builder.read(item_id).loc.end;
            new_items.push(item_id);
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
        let new_exprs = Exprs {
            items: new_items.into_boxed_slice(),
            seps: new_seps,
        };
        let kind = if is_seq {
            NodeKind::LitSeq { open: new_open, close: new_close, items: new_exprs }
        } else {
            NodeKind::LitRec { open: new_open, close: new_close, items: new_exprs }
        };
        self.append(kind, Loc { start: at, end })
    }

    fn expanded_collection(
        &mut self,
        open: &Token<'src>,
        close: &Token<'src>,
        items: &Exprs<'src>,
        at: Pos,
        is_seq: bool,
    ) -> AstId {
        let new_open = place_tok(open, at);
        let child_col = at.col + self.indent_width();
        let mut prev = advance_pos(at, open.src);
        let mut new_items = Vec::new();
        for &item in items.items.iter() {
            let item_at = newline_pos(prev, prev.line + 1, child_col);
            let item_id = self.with_block_col(child_col, |ctx| ctx.node(item, item_at));
            prev = self.builder.read(item_id).loc.end;
            new_items.push(item_id);
        }
        let close_at = newline_pos(prev, prev.line + 1, at.col);
        let new_close = place_tok(close, close_at);
        let end = advance_pos(close_at, close.src);
        // Block-style expanded collections have one item per line and no
        // inline separator tokens — each item sits on its own indented line.
        let new_exprs = Exprs {
            items: new_items.into_boxed_slice(),
            seps: vec![],
        };
        let kind = if is_seq {
            NodeKind::LitSeq { open: new_open, close: new_close, items: new_exprs }
        } else {
            NodeKind::LitRec { open: new_open, close: new_close, items: new_exprs }
        };
        self.append(kind, Loc { start: at, end })
    }

    // -----------------------------------------------------------------------
    // String templates
    // -----------------------------------------------------------------------

    fn str_templ(
        &mut self,
        open: Token<'src>,
        close: Token<'src>,
        children: &[AstId],
        at: Pos,
        raw: bool,
    ) -> AstId {
        let new_open = place_tok(&open, at);
        let mut pos = advance_pos(at, open.src);
        let mut new_children = Vec::new();
        for &child in children {
            let new_child = self.node(child, pos);
            pos = self.builder.read(new_child).loc.end;
            new_children.push(new_child);
        }
        let new_close = place_tok(&close, pos);
        let end = advance_pos(pos, close.src);
        let kind = if raw {
            NodeKind::StrRawTempl {
                open: new_open,
                close: new_close,
                children: new_children.into_boxed_slice(),
            }
        } else {
            NodeKind::StrTempl {
                open: new_open,
                close: new_close,
                children: new_children.into_boxed_slice(),
            }
        };
        self.append(kind, Loc { start: at, end })
    }

    // -----------------------------------------------------------------------
    // Operators
    // -----------------------------------------------------------------------

    fn unary_op(&mut self, op: Token<'src>, operand: AstId, at: Pos) -> AstId {
        let new_op = place_tok(&op, at);
        let operand_at = advance_pos(at, op.src);
        let new_operand = self.node(operand, operand_at);
        let end = self.builder.read(new_operand).loc.end;
        self.append(
            NodeKind::UnaryOp { op: new_op, operand: new_operand },
            Loc { start: at, end },
        )
    }

    fn infix_op(&mut self, op: Token<'src>, lhs: AstId, rhs: AstId, at: Pos) -> AstId {
        let inline_w = inline_width_infix(self.src, &op, lhs, rhs);
        if at.col + inline_w <= self.max_width() {
            self.place_infix_inline(op, lhs, rhs, at)
        } else {
            self.place_infix_expanded(op, lhs, rhs, at)
        }
    }

    fn place_infix_inline(&mut self, op: Token<'src>, lhs: AstId, rhs: AstId, at: Pos) -> AstId {
        let new_lhs = self.node(lhs, at);
        let lhs_end = self.builder.read(new_lhs).loc.end;
        let op_at = space_after(lhs_end);
        let new_op = place_tok(&op, op_at);
        let rhs_at = space_after(advance_pos(op_at, op.src));
        let new_rhs = self.node(rhs, rhs_at);
        let end = self.builder.read(new_rhs).loc.end;
        self.append(
            NodeKind::InfixOp { op: new_op, lhs: new_lhs, rhs: new_rhs },
            Loc { start: at, end },
        )
    }

    fn place_infix_expanded(&mut self, op: Token<'src>, lhs: AstId, rhs: AstId, at: Pos) -> AstId {
        let op_col = self.block_col + self.indent_width();
        // Flatten chains of same-op InfixOps that exceed width.
        let new_lhs = {
            let lhs_node = self.read(lhs).clone();
            if let NodeKind::InfixOp { op: inner_op, lhs: inner_lhs, rhs: inner_rhs } = lhs_node.kind {
                if inner_op.src == op.src {
                    let inner_inline_w = inline_width_infix(self.src, &inner_op, inner_lhs, inner_rhs);
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
            }
        };
        let lhs_end = self.builder.read(new_lhs).loc.end;
        let op_at = newline_pos(lhs_end, lhs_end.line + 1, op_col);
        let new_op = place_tok(&op, op_at);
        let rhs_at = space_after(advance_pos(op_at, op.src));
        let new_rhs = self.node(rhs, rhs_at);
        let end = self.builder.read(new_rhs).loc.end;
        self.append(
            NodeKind::InfixOp { op: new_op, lhs: new_lhs, rhs: new_rhs },
            Loc { start: at, end },
        )
    }

    fn chained_cmp(&mut self, parts: &[CmpPart<'src>], at: Pos) -> AstId {
        let mut pos = at;
        let mut new_parts: Vec<CmpPart<'src>> = Vec::new();
        for part in parts {
            match part {
                CmpPart::Operand(n) => {
                    let new_n = self.node(*n, pos);
                    pos = self.builder.read(new_n).loc.end;
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
            Some(CmpPart::Operand(n)) => self.builder.read(*n).loc.end,
            Some(CmpPart::Op(op)) => advance_pos(op.loc.start, op.src),
            None => at,
        };
        self.append(
            NodeKind::ChainedCmp(new_parts.into_boxed_slice()),
            Loc { start: at, end },
        )
    }

    fn spread(&mut self, op: Token<'src>, inner: Option<AstId>, at: Pos) -> AstId {
        let new_op = place_tok(&op, at);
        let mut end = advance_pos(at, op.src);
        let new_inner = inner.map(|n| {
            let new_n = self.node(n, end);
            end = self.builder.read(new_n).loc.end;
            new_n
        });
        self.append(
            NodeKind::Spread { op: new_op, inner: new_inner },
            Loc { start: at, end },
        )
    }

    fn member(&mut self, op: Token<'src>, lhs: AstId, rhs: AstId, at: Pos) -> AstId {
        let new_lhs = self.node(lhs, at);
        let op_at = self.builder.read(new_lhs).loc.end;
        let new_op = place_tok(&op, op_at);
        let rhs_at = advance_pos(op_at, op.src);
        let new_rhs = self.node(rhs, rhs_at);
        let end = self.builder.read(new_rhs).loc.end;
        self.append(
            NodeKind::Member { op: new_op, lhs: new_lhs, rhs: new_rhs },
            Loc { start: at, end },
        )
    }

    fn group(&mut self, open: Token<'src>, close: Token<'src>, inner: AstId, at: Pos) -> AstId {
        let new_open = place_tok(&open, at);
        let inner_at = advance_pos(at, open.src);
        let new_inner = self.node(inner, inner_at);
        let close_at = self.builder.read(new_inner).loc.end;
        let new_close = place_tok(&close, close_at);
        let end = advance_pos(close_at, close.src);
        self.append(
            NodeKind::Group { open: new_open, close: new_close, inner: new_inner },
            Loc { start: at, end },
        )
    }

    // -----------------------------------------------------------------------
    // Binding
    // -----------------------------------------------------------------------

    fn bind(&mut self, op: Token<'src>, lhs: AstId, rhs: AstId, at: Pos) -> AstId {
        let new_lhs = self.node(lhs, at);
        let lhs_end = self.builder.read(new_lhs).loc.end;
        let op_at = space_after(lhs_end);
        let new_op = place_tok(&op, op_at);
        let rhs_at = space_after(advance_pos(op_at, op.src));
        let new_rhs = self.node(rhs, rhs_at);
        let end = self.builder.read(new_rhs).loc.end;
        self.append(
            NodeKind::Bind { op: new_op, lhs: new_lhs, rhs: new_rhs },
            Loc { start: at, end },
        )
    }

    fn bind_right(&mut self, op: Token<'src>, lhs: AstId, rhs: AstId, at: Pos) -> AstId {
        let new_lhs = self.node(lhs, at);
        let lhs_end = self.builder.read(new_lhs).loc.end;
        let op_at = newline_pos(lhs_end, lhs_end.line + 1, at.col);
        let new_op = place_tok(&op, op_at);
        let rhs_at = space_after(advance_pos(op_at, op.src));
        let new_rhs = self.node(rhs, rhs_at);
        let end = self.builder.read(new_rhs).loc.end;
        self.append(
            NodeKind::BindRight { op: new_op, lhs: new_lhs, rhs: new_rhs },
            Loc { start: at, end },
        )
    }

    // -----------------------------------------------------------------------
    // Application
    // -----------------------------------------------------------------------

    fn apply(&mut self, func: AstId, args: &Exprs<'src>, at: Pos) -> AstId {
        if args.items.is_empty() {
            let new_func = self.node(func, at);
            let end = self.builder.read(new_func).loc.end;
            return self.append(
                NodeKind::Apply { func: new_func, args: Exprs::empty() },
                Loc { start: at, end },
            );
        }
        if should_expand_apply(self.src, args) {
            return self.apply_expanded(func, args, at);
        }
        let inline_w = inline_width_apply(self.src, func, args);
        if at.col + inline_w <= self.max_width() {
            self.apply_inline(func, args, at)
        } else {
            self.apply_expanded(func, args, at)
        }
    }

    fn apply_inline(&mut self, func: AstId, args: &Exprs<'src>, at: Pos) -> AstId {
        let new_func = self.node(func, at);
        let func_end = self.builder.read(new_func).loc.end;
        let mut pos = space_after(func_end);
        let mut new_items = Vec::new();
        let mut new_seps = Vec::new();
        for (i, &arg) in args.items.iter().enumerate() {
            let new_arg = self.node(arg, pos);
            pos = self.builder.read(new_arg).loc.end;
            new_items.push(new_arg);
            if let Some(sep) = args.seps.get(i) {
                let new_sep = place_tok(sep, pos);
                pos = space_after(advance_pos(pos, sep.src));
                new_seps.push(new_sep);
            }
        }
        let end = new_items.last()
            .map(|&n| self.builder.read(n).loc.end)
            .unwrap_or(func_end);
        self.append(
            NodeKind::Apply {
                func: new_func,
                args: Exprs { items: new_items.into_boxed_slice(), seps: new_seps },
            },
            Loc { start: at, end },
        )
    }

    fn apply_expanded(&mut self, func: AstId, args: &Exprs<'src>, at: Pos) -> AstId {
        let new_func = self.node(func, at);
        let child_col = self.block_col + self.indent_width();
        let mut prev = self.builder.read(new_func).loc.end;
        let each_on_own_line = args.items.iter().any(|&a| body_has_block(self.src, a));

        let mut new_items = Vec::new();
        let mut new_seps = Vec::new();

        if each_on_own_line {
            for (i, &arg) in args.items.iter().enumerate() {
                let arg_at = newline_pos(prev, prev.line + 1, child_col);
                let new_arg = self.node(arg, arg_at);
                prev = self.builder.read(new_arg).loc.end;
                new_items.push(new_arg);
                if let Some(sep) = args.seps.get(i) {
                    let new_sep = place_tok(sep, prev);
                    prev = advance_pos(prev, sep.src);
                    new_seps.push(new_sep);
                }
            }
        } else {
            let line_at = newline_pos(prev, prev.line + 1, child_col);
            let mut pos = line_at;
            for (i, &arg) in args.items.iter().enumerate() {
                let new_arg = self.node(arg, pos);
                pos = self.builder.read(new_arg).loc.end;
                new_items.push(new_arg);
                if let Some(sep) = args.seps.get(i) {
                    let new_sep = place_tok(sep, pos);
                    pos = space_after(advance_pos(pos, sep.src));
                    new_seps.push(new_sep);
                }
            }
        }
        let func_end = self.builder.read(new_func).loc.end;
        let end = new_items.last()
            .map(|&n| self.builder.read(n).loc.end)
            .unwrap_or(func_end);
        self.append(
            NodeKind::Apply {
                func: new_func,
                args: Exprs { items: new_items.into_boxed_slice(), seps: new_seps },
            },
            Loc { start: at, end },
        )
    }

    // -----------------------------------------------------------------------
    // Pipe
    // -----------------------------------------------------------------------

    fn pipe(&mut self, exprs: &Exprs<'src>, at: Pos) -> AstId {
        let expand = pipe_needs_expand(self.src, exprs);
        let mut new_items = Vec::new();
        let mut new_seps = Vec::new();

        if expand {
            let first_id = self.node(exprs.items[0], at);
            let mut prev = self.builder.read(first_id).loc.end;
            new_items.push(first_id);
            for (i, &item) in exprs.items[1..].iter().enumerate() {
                if let Some(sep) = exprs.seps.get(i) {
                    let sep_at = newline_pos(prev, prev.line + 1, at.col);
                    let new_sep = place_tok(sep, sep_at);
                    let after_sep = space_after(advance_pos(sep_at, sep.src));
                    let new_item = self.node(item, after_sep);
                    prev = self.builder.read(new_item).loc.end;
                    new_seps.push(new_sep);
                    new_items.push(new_item);
                }
            }
        } else {
            let first_id = self.node(exprs.items[0], at);
            let mut pos = self.builder.read(first_id).loc.end;
            new_items.push(first_id);
            for (i, &item) in exprs.items[1..].iter().enumerate() {
                if let Some(sep) = exprs.seps.get(i) {
                    let sep_at = space_after(pos);
                    let new_sep = place_tok(sep, sep_at);
                    pos = space_after(advance_pos(sep_at, sep.src));
                    let new_item = self.node(item, pos);
                    pos = self.builder.read(new_item).loc.end;
                    new_seps.push(new_sep);
                    new_items.push(new_item);
                }
            }
        }
        let end = new_items.last()
            .map(|&n| self.builder.read(n).loc.end)
            .unwrap_or(at);
        self.append(
            NodeKind::Pipe(Exprs { items: new_items.into_boxed_slice(), seps: new_seps }),
            Loc { start: at, end },
        )
    }

    // -----------------------------------------------------------------------
    // Functions
    // -----------------------------------------------------------------------

    fn fn_node(
        &mut self,
        params: AstId,
        sep: Token<'src>,
        body: &Exprs<'src>,
        at: Pos,
    ) -> AstId {
        let fn_end = advance_pos(at, "fn");
        let params_at = if params_is_empty(self.src, params) {
            fn_end
        } else {
            space_after(fn_end)
        };
        let new_params = self.node(params, params_at);
        let params_end = self.builder.read(new_params).loc.end;

        let body_inline = body.items.len() == 1 && !body_has_block(self.src, body.items[0]);
        if body_inline {
            let sep_at = params_end;
            let new_sep = place_tok(&sep, sep_at);
            let body_at = space_after(advance_pos(sep_at, sep.src));
            let new_body_item = self.node(body.items[0], body_at);
            let end = self.builder.read(new_body_item).loc.end;
            self.append(
                NodeKind::Fn {
                    params: new_params,
                    sep: new_sep,
                    body: Exprs {
                        items: Box::new([new_body_item]),
                        seps: body.seps.clone(),
                    },
                },
                Loc { start: at, end },
            )
        } else {
            let sep_at = params_end;
            let new_sep = place_tok(&sep, sep_at);
            let sep_end = advance_pos(sep_at, sep.src);
            let child_col = self.block_col + self.indent_width();
            let mut prev = sep_end;
            let mut new_body_items = Vec::new();
            for &stmt in body.items.iter() {
                let stmt_at = newline_pos(prev, prev.line + 1, child_col);
                let new_stmt = self.with_block_col(child_col, |ctx| ctx.node(stmt, stmt_at));
                prev = self.builder.read(new_stmt).loc.end;
                new_body_items.push(new_stmt);
            }
            let end = new_body_items.last()
                .map(|&n| self.builder.read(n).loc.end)
                .unwrap_or(sep_end);
            self.append(
                NodeKind::Fn {
                    params: new_params,
                    sep: new_sep,
                    body: Exprs {
                        items: new_body_items.into_boxed_slice(),
                        seps: vec![],
                    },
                },
                Loc { start: at, end },
            )
        }
    }

    fn patterns(&mut self, exprs: &Exprs<'src>, at: Pos) -> AstId {
        if exprs.items.is_empty() {
            return self.append(
                NodeKind::Patterns(Exprs::empty()),
                Loc { start: at, end: at },
            );
        }
        let mut pos = at;
        let mut new_items = Vec::new();
        let mut new_seps = Vec::new();
        for (i, &item) in exprs.items.iter().enumerate() {
            let new_item = self.node(item, pos);
            pos = self.builder.read(new_item).loc.end;
            new_items.push(new_item);
            if let Some(sep) = exprs.seps.get(i) {
                let new_sep = place_tok(sep, pos);
                pos = space_after(advance_pos(pos, sep.src));
                new_seps.push(new_sep);
            }
        }
        let end = new_items.last()
            .map(|&n| self.builder.read(n).loc.end)
            .unwrap_or(at);
        self.append(
            NodeKind::Patterns(Exprs {
                items: new_items.into_boxed_slice(),
                seps: new_seps,
            }),
            Loc { start: at, end },
        )
    }

    // -----------------------------------------------------------------------
    // Match
    // -----------------------------------------------------------------------

    fn match_node(
        &mut self,
        subjects: &Exprs<'src>,
        sep: Token<'src>,
        arms: &Exprs<'src>,
        at: Pos,
    ) -> AstId {
        let kw_end = advance_pos(at, "match");
        let mut prev_pos = space_after(kw_end);
        let mut new_subj_items = Vec::new();
        for (i, &subj) in subjects.items.iter().enumerate() {
            if i > 0 {
                prev_pos = advance_pos(prev_pos, ", ");
            }
            let new_subj = self.node(subj, prev_pos);
            prev_pos = self.builder.read(new_subj).loc.end;
            new_subj_items.push(new_subj);
        }
        let new_subjects = Exprs {
            items: new_subj_items.into_boxed_slice(),
            seps: subjects.seps.clone(),
        };
        let sep_at = prev_pos;
        let new_sep = place_tok(&sep, sep_at);
        let sep_end = advance_pos(sep_at, sep.src);
        let child_col = self.block_col + self.indent_width();
        let mut prev = sep_end;
        let mut new_arm_items = Vec::new();
        for &arm in arms.items.iter() {
            let arm_at = newline_pos(prev, prev.line + 1, child_col);
            let new_arm = self.with_block_col(child_col, |ctx| ctx.node(arm, arm_at));
            prev = self.builder.read(new_arm).loc.end;
            new_arm_items.push(new_arm);
        }
        let end = new_arm_items.last()
            .map(|&n| self.builder.read(n).loc.end)
            .unwrap_or(sep_at);
        self.append(
            NodeKind::Match {
                subjects: new_subjects,
                sep: new_sep,
                arms: Exprs {
                    items: new_arm_items.into_boxed_slice(),
                    seps: vec![],
                },
            },
            Loc { start: at, end },
        )
    }

    fn arm(
        &mut self,
        lhs: AstId,
        sep: Token<'src>,
        body: &Exprs<'src>,
        at: Pos,
    ) -> AstId {
        let new_lhs = self.node(lhs, at);
        let lhs_end = self.builder.read(new_lhs).loc.end;
        let sep_at = lhs_end;
        let new_sep = place_tok(&sep, sep_at);
        let body_inline = body.items.len() == 1 && !body_has_block(self.src, body.items[0]);
        if body_inline {
            let body_at = space_after(advance_pos(sep_at, sep.src));
            let new_body_item = self.node(body.items[0], body_at);
            let end = self.builder.read(new_body_item).loc.end;
            self.append(
                NodeKind::Arm {
                    lhs: new_lhs,
                    sep: new_sep,
                    body: Exprs {
                        items: Box::new([new_body_item]),
                        seps: vec![],
                    },
                },
                Loc { start: at, end },
            )
        } else {
            let child_col = self.block_col + self.indent_width();
            let sep_end = advance_pos(sep_at, sep.src);
            let mut prev = sep_end;
            let mut new_body_items = Vec::new();
            for &stmt in body.items.iter() {
                let stmt_at = newline_pos(prev, prev.line + 1, child_col);
                let new_stmt = self.with_block_col(child_col, |ctx| ctx.node(stmt, stmt_at));
                prev = self.builder.read(new_stmt).loc.end;
                new_body_items.push(new_stmt);
            }
            let end = new_body_items.last()
                .map(|&n| self.builder.read(n).loc.end)
                .unwrap_or(sep_end);
            self.append(
                NodeKind::Arm {
                    lhs: new_lhs,
                    sep: new_sep,
                    body: Exprs {
                        items: new_body_items.into_boxed_slice(),
                        seps: vec![],
                    },
                },
                Loc { start: at, end },
            )
        }
    }

    // -----------------------------------------------------------------------
    // Error handling
    // -----------------------------------------------------------------------

    fn try_node(&mut self, inner: AstId, at: Pos) -> AstId {
        let kw_end = advance_pos(at, "try");
        let inner_at = space_after(kw_end);
        let new_inner = self.node(inner, inner_at);
        let end = self.builder.read(new_inner).loc.end;
        self.append(NodeKind::Try(new_inner), Loc { start: at, end })
    }

    // -----------------------------------------------------------------------
    // Custom blocks
    // -----------------------------------------------------------------------

    fn block_node(
        &mut self,
        name: AstId,
        params: AstId,
        sep: Token<'src>,
        body: &Exprs<'src>,
        at: Pos,
    ) -> AstId {
        let new_name = self.node(name, at);
        let name_end = self.builder.read(new_name).loc.end;
        let params_at = space_after(name_end);
        let new_params = self.node(params, params_at);
        let params_end = self.builder.read(new_params).loc.end;
        let sep_at = params_end;
        let new_sep = place_tok(&sep, sep_at);
        let sep_end = advance_pos(sep_at, sep.src);
        let child_col = at.col + self.indent_width();
        let mut prev = sep_end;
        let mut new_body_items = Vec::new();
        for &stmt in body.items.iter() {
            let stmt_at = newline_pos(prev, prev.line + 1, child_col);
            let new_stmt = self.node(stmt, stmt_at);
            prev = self.builder.read(new_stmt).loc.end;
            new_body_items.push(new_stmt);
        }
        let end = new_body_items.last()
            .map(|&n| self.builder.read(n).loc.end)
            .unwrap_or(sep_at);
        self.append(
            NodeKind::Block {
                name: new_name,
                params: new_params,
                sep: new_sep,
                body: Exprs {
                    items: new_body_items.into_boxed_slice(),
                    seps: vec![],
                },
            },
            Loc { start: at, end },
        )
    }
}

// ---------------------------------------------------------------------------
// Decision helpers
// ---------------------------------------------------------------------------

fn should_expand_apply(ast: &Ast, args: &Exprs) -> bool {
    if args.items.len() == 1 {
        let arg = ast.nodes.get(args.items[0]);
        if let NodeKind::Apply { args: inner_args, .. } = &arg.kind {
            return inner_args.items.len() >= 2;
        }
        return false;
    }
    args.items.iter().any(|&a| matches!(ast.nodes.get(a).kind, NodeKind::Apply { .. }))
}

fn pipe_needs_expand(ast: &Ast, exprs: &Exprs) -> bool {
    exprs.items.iter().skip(1)
        .any(|&seg| matches!(ast.nodes.get(seg).kind, NodeKind::Apply { .. }))
}

fn rec_item_needs_expand(ast: &Ast, item_id: AstId) -> bool {
    let item = ast.nodes.get(item_id);
    match &item.kind {
        NodeKind::Arm { body, .. } => {
            body.items.len() == 1
                && matches!(ast.nodes.get(body.items[0]).kind, NodeKind::Fn { .. })
        }
        _ => false,
    }
}

fn body_has_block(ast: &Ast, id: AstId) -> bool {
    match &ast.nodes.get(id).kind {
        NodeKind::Fn { body, .. } => body.items.len() > 1,
        NodeKind::Match { .. } => true,
        _ => false,
    }
}

fn params_is_empty(ast: &Ast, id: AstId) -> bool {
    matches!(&ast.nodes.get(id).kind, NodeKind::Patterns(e) if e.items.is_empty())
}

// ---------------------------------------------------------------------------
// Width measurement helpers (inline)
// ---------------------------------------------------------------------------

fn inline_width_node(ast: &Ast, id: AstId) -> u32 {
    let node = ast.nodes.get(id);
    if node.loc.start.idx > 0 || node.loc.end.idx > 0 {
        if node.loc.start.line == node.loc.end.line {
            return node.loc.end.col - node.loc.start.col;
        } else {
            return u32::MAX / 2;
        }
    }
    match &node.kind {
        NodeKind::LitBool(v) => if *v { 4 } else { 5 },
        NodeKind::LitInt(s)
        | NodeKind::LitFloat(s)
        | NodeKind::LitDecimal(s)
        | NodeKind::Ident(s) => s.len() as u32,
        NodeKind::Partial | NodeKind::Wildcard => 1,
        NodeKind::Group { open, close, inner } => {
            open.src.len() as u32 + inline_width_node(ast, *inner) + close.src.len() as u32
        }
        NodeKind::InfixOp { op, lhs, rhs } => {
            inline_width_node(ast, *lhs) + 1 + op.src.len() as u32 + 1 + inline_width_node(ast, *rhs)
        }
        NodeKind::UnaryOp { op, operand } => {
            op.src.len() as u32 + inline_width_node(ast, *operand)
        }
        NodeKind::Apply { func, args } => inline_width_apply(ast, *func, args),
        NodeKind::LitStr { open, close, content, .. } => {
            open.src.len() as u32 + content.len() as u32 + close.src.len() as u32
        }
        NodeKind::LitSeq { open, close, items }
        | NodeKind::LitRec { open, close, items } => {
            inline_width_collection(ast, open, close, items)
        }
        NodeKind::Member { op, lhs, rhs } => {
            inline_width_node(ast, *lhs) + op.src.len() as u32 + inline_width_node(ast, *rhs)
        }
        _ => 40,
    }
}

fn inline_width_apply(ast: &Ast, func: AstId, args: &Exprs) -> u32 {
    let mut w = inline_width_node(ast, func);
    for (i, &arg) in args.items.iter().enumerate() {
        w += 1 + inline_width_node(ast, arg);
        if let Some(sep) = args.seps.get(i) {
            w += sep.src.len() as u32;
        }
    }
    w
}

fn inline_width_infix(ast: &Ast, op: &Token, lhs: AstId, rhs: AstId) -> u32 {
    inline_width_node(ast, lhs) + 1 + op.src.len() as u32 + 1 + inline_width_node(ast, rhs)
}

fn inline_width_collection(ast: &Ast, open: &Token, close: &Token, items: &Exprs) -> u32 {
    let mut w = open.src.len() as u32 + close.src.len() as u32;
    for (i, &item) in items.items.iter().enumerate() {
        w += inline_width_node(ast, item);
        if let Some(sep) = items.seps.get(i) {
            w += sep.src.len() as u32 + 1;
        }
    }
    w
}

// ---------------------------------------------------------------------------
// Position helpers
// ---------------------------------------------------------------------------

fn newline_pos(prev: Pos, target_line: u32, target_col: u32) -> Pos {
    debug_assert!(target_line > prev.line, "newline_pos: target_line must be > prev.line");
    let newlines = target_line - prev.line;
    let idx = prev.idx + newlines + target_col;
    Pos { idx, line: target_line, col: target_col }
}

fn place_tok<'src>(tok: &Token<'src>, at: Pos) -> Token<'src> {
    Token {
        kind: tok.kind,
        src: tok.src,
        loc: Loc { start: at, end: advance_pos(at, tok.src) },
    }
}

fn advance_pos(pos: Pos, s: &str) -> Pos {
    Pos {
        idx: pos.idx + s.len() as u32,
        line: pos.line,
        col: pos.col + s.len() as u32,
    }
}

fn space_after(pos: Pos) -> Pos {
    Pos { idx: pos.idx + 1, line: pos.line, col: pos.col + 1 }
}

fn loc(at: Pos, len: u32) -> Loc {
    Loc { start: at, end: Pos { idx: at.idx + len, line: at.line, col: at.col + len } }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::layout;
    use crate::fmt::{print, FmtConfig};
    use crate::parser;

    /// Parse source, run through layout + print, return result.
    /// Returns "NO-DIFF" if output equals input.
    fn fmt(src: &str) -> String {
        let ast = parser::parse(src, "test")
            .unwrap_or_else(|e| panic!("parse error: {}", e.message));
        let cfg = FmtConfig::default();
        let laid_out = layout(&ast, &cfg);
        let output = print::print(&laid_out);
        if output == src { "NO-DIFF".to_string() } else { output }
    }

    test_macros::include_fink_tests!("src/fmt/test_fmt.fnk");
}
