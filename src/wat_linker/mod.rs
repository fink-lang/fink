//! WAT linker — merge multiple `.wat` source files into a single
//! `(module ...)` text, with per-file identifier scoping so authors
//! don't have to manually FQN-prefix every symbol.
//!
//! Status: M1 — single-file rename pass.
//! See `.brain/.scratch/wat-linker-plan.md` for the milestone plan.
//!
//! Rules:
//!
//! * Locally-declared module-level ids (func/global/type/memory/table/
//!   data/elem) get the importer's path prefix:
//!   `$Foo` → `$test-wats/foo.wat:Foo`.
//! * Import-bound ids get the *exporter*'s path prefix, because after
//!   merge the import line disappears and only the exporter's
//!   definition remains. The exporter path is computed by resolving
//!   the import's module string against the importer's directory.
//! * Function-internal scopes (params, locals, block labels) and
//!   type-internal scopes (struct/array field names) are left alone;
//!   they can't clash across files.
//! * Inline `(export "X")` strings whose name matches a locally-
//!   declared id are rewritten to `(export "<path>:X")` so the
//!   merged module exposes a unique export name per origin file.
//! * The local handle id of an import must match the import's name
//!   string. `(import "./bar.wat" "Bar" (type $Bar ...))` is legal;
//!   `(... "Bar" (type $MyAlias ...))` is rejected.

use std::collections::HashMap;

use wast::lexer::{Lexer, TokenKind};

/// Link a set of `.wat` modules into a single merged `(module ...)`.
///
/// `modules[0]` is the entry; the rest are deps reachable via imports.
/// Each entry is `(url, src)` — the URL is what other modules use in
/// their `(import "<url>" ...)` strings.
///
/// The linker:
///
/// 1. DFS-walks the import graph from the entry, deduping via URL.
///    Cycles are tolerated: a re-visit while still on the stack just
///    stops the recursion (each module appears once in the output).
/// 2. Renames each visited module's locally-declared ids per the
///    rules at the top of this file.
/// 3. Hoists every `(type ...)` and `(rec ...)` form into one merged
///    `(rec ...)` block at the top of the output (single rec group
///    so all types share nominal identity).
/// 4. Drops resolved `(import "...wat" ...)` forms — the importing
///    references have been rewritten to point at the exporter's
///    canonical name.
/// 5. Concatenates remaining bodies in DFS post-order, wraps in
///    `(module ...)`.
pub fn link(modules: &[(&str, &str)]) -> String {
    let url_to_index: HashMap<&str, usize> = modules
        .iter()
        .enumerate()
        .map(|(i, (url, _))| (*url, i))
        .collect();

    // DFS the import graph from index 0 (the entry).
    let mut visit = Visit {
        modules,
        url_to_index: &url_to_index,
        on_stack: vec![false; modules.len()],
        visited: vec![false; modules.len()],
        order: Vec::new(),
    };
    visit.dfs(0);

    // For each visited module: rename first to produce internally-
    // consistent text, then extract type/body spans from the renamed
    // text via a metadata-only re-walk.
    let mut all_types: Vec<String> = Vec::new();
    let mut all_bodies: Vec<String> = Vec::new();
    for &i in &visit.order {
        let (url, src) = modules[i];
        let renamed = rename_locals(url, src);
        let spans = collect_spans(&renamed);
        let (types, body) = split_chunks(&renamed, &spans);
        all_types.extend(types);
        all_bodies.push(body);
    }

    // Stitch the merged module.
    let mut out = String::new();
    out.push_str("(module\n");
    if !all_types.is_empty() {
        out.push_str("  (rec");
        for t in &all_types {
            out.push_str("\n    ");
            out.push_str(t);
        }
        out.push_str(")\n\n");
    }
    for body in &all_bodies {
        out.push_str(body);
    }
    out.push_str(")\n");
    out
}

struct Visit<'a> {
    modules: &'a [(&'a str, &'a str)],
    url_to_index: &'a HashMap<&'a str, usize>,
    on_stack: Vec<bool>,
    visited: Vec<bool>,
    order: Vec<usize>,
}

impl<'a> Visit<'a> {
    fn dfs(&mut self, i: usize) {
        if self.visited[i] || self.on_stack[i] {
            // Cycle or already-emitted: just stop. Cycles are
            // tolerated; the module already appears (or will appear)
            // earlier in `order`.
            return;
        }
        self.on_stack[i] = true;

        let (url, src) = self.modules[i];
        let plan = collect_plan(url, src);
        for target in &plan.wat_imports {
            let dep_index = self.url_to_index.get(target.as_str()).unwrap_or_else(|| {
                panic!(
                    "wat-linker: module {url} imports \"{target}\" but it is not \
                     in the merge set"
                )
            });
            self.dfs(*dep_index);
        }

        self.on_stack[i] = false;
        self.visited[i] = true;
        self.order.push(i);
    }
}

/// Span-only metadata for a renamed module's text.
#[derive(Default)]
struct Spans {
    type_spans: Vec<Span>,
    import_spans: Vec<Span>,
    /// Inline `(export "...")` forms inside top-level definitions.
    /// Dropped at merge time — visibility between wat modules is a
    /// build-time concern that the import-rewrite already resolved.
    /// Real binary-level exports come from `@export` annotations
    /// (handled separately).
    export_spans: Vec<Span>,
}

/// Walk a (renamed) module text and record the byte ranges of every
/// top-level `(type ...)`, `(rec ...)`, `(import "...wat" ...)`, and
/// every inline `(export "...")` subform. Pure metadata — no
/// validation, no rename.
fn collect_spans(src: &str) -> Spans {
    let lexer = Lexer::new(src);
    let mut spans = Spans::default();
    let mut tokens = lexer.iter(0).filter_map(Result::ok).filter(is_significant);
    let mut depth: usize = 0;

    while let Some(tok) = tokens.next() {
        match tok.kind {
            TokenKind::LParen => {
                depth += 1;
                if depth == 2 {
                    let form_start = tok.offset;
                    let head = match tokens.next() {
                        Some(t) => t,
                        None => break,
                    };
                    if head.kind != TokenKind::Keyword {
                        continue;
                    }
                    let kw = slice(src, &head);
                    let is_type = matches!(kw, "type" | "rec");
                    let is_import_wat = kw == "import"
                        && peek_module_string_is_wat(src, &mut tokens, &mut depth);
                    let close = walk_form_recording_exports(src, &mut tokens, &mut depth, &mut spans);
                    if let Some(close) = close {
                        let span = Span {
                            start: form_start,
                            end: close + 1,
                        };
                        if is_type {
                            spans.type_spans.push(span);
                        } else if is_import_wat {
                            spans.import_spans.push(span);
                        }
                    }
                }
            }
            TokenKind::RParen => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    spans
}

/// Walk to the end of the current top-level form. Along the way,
/// record every `(export "...")` subform's byte span so the merger
/// can drop them. Returns the closing `)` offset.
fn walk_form_recording_exports<I>(
    src: &str,
    tokens: &mut I,
    depth: &mut usize,
    spans: &mut Spans,
) -> Option<usize>
where
    I: Iterator<Item = wast::lexer::Token>,
{
    let mut last_close = None;
    while *depth >= 2 {
        let tok = tokens.next()?;
        match tok.kind {
            TokenKind::LParen => {
                let sub_start = tok.offset;
                *depth += 1;
                // Peek the keyword to spot `(export ...)`.
                let kw_tok = tokens.next()?;
                if kw_tok.kind == TokenKind::Keyword && slice(src, &kw_tok) == "export" {
                    // Record the span. Walk to this sub-form's close.
                    let close = naive_form_end_at(*depth - 1, tokens, depth)?;
                    spans.export_spans.push(Span {
                        start: sub_start,
                        end: close + 1,
                    });
                    last_close = Some(close);
                }
                // Otherwise just keep walking (depth tracking handles nested closes).
            }
            TokenKind::RParen => {
                last_close = Some(tok.offset);
                *depth -= 1;
            }
            _ => {}
        }
    }
    last_close
}

/// Walk to a closing `)` at the given target depth. Returns the
/// byte offset of that closing paren.
fn naive_form_end_at<I>(target: usize, tokens: &mut I, depth: &mut usize) -> Option<usize>
where
    I: Iterator<Item = wast::lexer::Token>,
{
    while *depth > target {
        let tok = tokens.next()?;
        match tok.kind {
            TokenKind::LParen => *depth += 1,
            TokenKind::RParen => {
                *depth -= 1;
                if *depth == target {
                    return Some(tok.offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// Peek the next significant token; if it's a `String` ending in
/// `.wat"`, return true. Used to decide whether an `(import ...)` is
/// a wat-to-wat import (linker should drop it) vs an env import
/// (linker preserves it).
fn peek_module_string_is_wat<I>(src: &str, tokens: &mut I, depth: &mut usize) -> bool
where
    I: Iterator<Item = wast::lexer::Token>,
{
    // Note: this consumes the module-string token. Caller must rely
    // on `naive_form_end` afterwards to walk to the form's close.
    while let Some(tok) = tokens.next() {
        match tok.kind {
            TokenKind::String => {
                let raw = slice(src, &tok);
                return raw.trim_matches('"').ends_with(".wat");
            }
            TokenKind::LParen => *depth += 1,
            TokenKind::RParen => {
                *depth -= 1;
                return false;
            }
            _ => {}
        }
    }
    false
}

/// Walk to the end of the current top-level form; return the byte
/// offset of the closing `)`.
fn naive_form_end<I>(tokens: &mut I, depth: &mut usize) -> Option<usize>
where
    I: Iterator<Item = wast::lexer::Token>,
{
    let mut last_close = None;
    while *depth >= 2 {
        let tok = tokens.next()?;
        match tok.kind {
            TokenKind::LParen => *depth += 1,
            TokenKind::RParen => {
                last_close = Some(tok.offset);
                *depth -= 1;
            }
            _ => {}
        }
    }
    last_close
}

/// From a renamed module body + its spans, extract:
/// * the body text with type forms, wat-imports, and inline exports
///   removed,
/// * a list of individual `(type ...)` forms (rec groups flattened).
fn split_chunks(src: &str, spans: &Spans) -> (Vec<String>, String) {
    // Top-level removals get their preceding `;; …` headers pulled
    // along, so dropping `(import …)` doesn't leave the header orphaned.
    let mut top_level: Vec<Span> = Vec::new();
    top_level.extend(spans.type_spans.iter().copied());
    top_level.extend(spans.import_spans.iter().copied());
    top_level.sort_by_key(|s| s.start);
    let top_level = extend_spans_over_preceding_comments(src, top_level);

    // Inline export subforms are removed verbatim — their context
    // (the surrounding func/global declaration) stays.
    let mut removals: Vec<Span> = top_level;
    removals.extend(spans.export_spans.iter().copied());
    removals.sort_by_key(|s| s.start);
    let removals = dedupe_contained_spans(removals);

    let mut body = String::with_capacity(src.len());
    let mut cursor = 0usize;
    for span in &removals {
        body.push_str(&src[cursor..span.start]);
        cursor = span.end;
    }
    body.push_str(&src[cursor..]);

    let body = strip_module_wrapper(&body);
    let body = collapse_blank_runs(&body);

    let types: Vec<String> = spans
        .type_spans
        .iter()
        .flat_map(|span| extract_type_forms(&src[span.start..span.end]))
        .collect();

    (types, body)
}

/// Extend each span's start backwards over preceding whitespace and
/// `;; …` line comments, up to (and including) a blank line. This
/// pulls the descriptive header for a removed form along with the
/// form itself, so dropping `(import …)` doesn't leave its header
/// orphaned.
fn extend_spans_over_preceding_comments(src: &str, spans: Vec<Span>) -> Vec<Span> {
    let bytes = src.as_bytes();
    spans
        .into_iter()
        .map(|s| {
            // Walk back past leading whitespace on the form's own line
            // to reach the previous `\n`. Without this, a form like
            // `  (import …)` starts mid-line and we never find the
            // line-terminator above.
            let mut line_end_exclusive = s.start;
            while line_end_exclusive > 0 {
                let prev = bytes[line_end_exclusive - 1];
                if prev == b'\n' {
                    break;
                }
                if prev == b' ' || prev == b'\t' {
                    line_end_exclusive -= 1;
                } else {
                    // Mid-line content before the form — bail out.
                    return Span { start: s.start, end: s.end };
                }
            }
            // Now line_end_exclusive sits right after a `\n` (or at 0).
            loop {
                if line_end_exclusive == 0 {
                    break;
                }
                let nl_pos = line_end_exclusive - 1;
                debug_assert_eq!(bytes[nl_pos], b'\n');
                let mut line_start = nl_pos;
                while line_start > 0 && bytes[line_start - 1] != b'\n' {
                    line_start -= 1;
                }
                let content = &src[line_start..nl_pos];
                let trimmed = content.trim_start();
                let is_blank = trimmed.is_empty();
                let is_comment = trimmed.starts_with(";;");
                if !is_blank && !is_comment {
                    break;
                }
                // Eat this line (including its trailing newline).
                line_end_exclusive = line_start;
            }
            Span {
                start: line_end_exclusive,
                end: s.end,
            }
        })
        .collect()
}

/// Collapse runs of 3+ blank lines to 2, so removed forms don't
/// leave gaping holes in the merged output.
fn collapse_blank_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0usize;
    for line in text.split_inclusive('\n') {
        let is_blank = line.trim().is_empty();
        if is_blank {
            blank_run += 1;
            if blank_run <= 2 {
                out.push_str(line);
            }
        } else {
            blank_run = 0;
            out.push_str(line);
        }
    }
    out
}

fn dedupe_contained_spans(sorted: Vec<Span>) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::with_capacity(sorted.len());
    for span in sorted {
        if let Some(last) = out.last() {
            if span.start >= last.start && span.end <= last.end {
                // Fully contained: skip.
                continue;
            }
        }
        out.push(span);
    }
    out
}

/// Strip the surrounding `(module ... )` from a top-level WAT text,
/// returning just the inner body. Tolerates leading comments before
/// `(module` and trailing whitespace/comments after the closing `)`.
fn strip_module_wrapper(text: &str) -> String {
    let module_start = match text.find("(module") {
        Some(i) => i + "(module".len(),
        None => return format!("{}\n", text.trim_end()),
    };
    // Find the matching closing `)` by paren-counting from after
    // `(module`. We're already inside one open paren.
    let bytes = text.as_bytes();
    let mut depth = 1usize;
    let mut i = module_start;
    while i < bytes.len() && depth > 0 {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            // Skip over `;; ... \n` line comments so a `)` inside
            // doesn't fool the counter (rare but possible).
            b';' if i + 1 < bytes.len() && bytes[i + 1] == b';' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    let inner = &text[module_start..i];
    format!("{}\n", inner.trim_end())
}

/// From a top-level type-bearing form (`(type ...)` or `(rec ...)`),
/// return the individual `(type ...)` forms inside it. For a bare
/// `(type ...)` that's just the text itself; for `(rec (type ...) (type ...))`
/// it's each inner `(type ...)`.
fn extract_type_forms(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    if let Some(inner) = trimmed.strip_prefix("(rec") {
        let inner = inner.trim_end_matches(')').trim();
        // Split into top-level `(type ...)` forms by paren-counting.
        let mut forms = Vec::new();
        let bytes = inner.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            // Skip whitespace.
            while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            if bytes[i] != b'(' {
                // Stray content; skip char.
                i += 1;
                continue;
            }
            // Read paren-balanced form starting at i.
            let start = i;
            let mut depth = 0;
            while i < bytes.len() {
                match bytes[i] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            forms.push(inner[start..i].trim().to_string());
        }
        forms
    } else {
        vec![trimmed.to_string()]
    }
}

/// Single-file rename pass. Public for direct testing; the merger
/// uses it internally.
pub fn rename_locals(path: &str, src: &str) -> String {
    let plan = collect_plan(path, src);
    apply_plan(src, &plan)
}

/// What the rewrite pass needs to know.
#[derive(Default)]
struct Plan {
    /// `$id` (without `$`) → renamed target (without `$`).
    id_renames: HashMap<String, String>,
    /// Byte-range edits keyed by start offset. Each entry replaces
    /// `src[start..end]` with the given text.
    string_edits: Vec<Edit>,
    /// Byte ranges of top-level `(import "...wat" ...)` forms so the
    /// merger can drop them from the output.
    import_spans: Vec<Span>,
    /// Byte ranges of top-level `(type ...)` and `(rec ...)` forms so
    /// the merger can hoist them into a single merged rec group.
    type_spans: Vec<Span>,
    /// Imports of `*.wat` modules: target URL (resolved) for each.
    /// Used by the merger to walk the dep graph.
    wat_imports: Vec<String>,
}

#[derive(Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
}

struct Edit {
    start: usize,
    end: usize,
    replacement: String,
}

/// Walk the file and produce the rewrite plan.
fn collect_plan(path: &str, src: &str) -> Plan {
    let lexer = Lexer::new(src);
    let mut plan = Plan::default();
    let mut tokens = lexer.iter(0).filter_map(Result::ok).filter(is_significant);
    let mut depth: usize = 0;

    while let Some(tok) = tokens.next() {
        match tok.kind {
            TokenKind::LParen => {
                depth += 1;
                if depth == 2 {
                    let form_start = tok.offset;
                    if let Some(head) = tokens.next() {
                        if head.kind == TokenKind::Keyword {
                            let kw = slice(src, &head);
                            handle_top_form(
                                kw,
                                form_start,
                                path,
                                src,
                                &mut tokens,
                                &mut depth,
                                &mut plan,
                            );
                        }
                    }
                }
            }
            TokenKind::RParen => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    plan
}

/// Top-level form dispatch. We've just consumed `( <keyword>` at depth 2.
/// `form_start` is the byte offset of the opening `(`.
fn handle_top_form<I>(
    keyword: &str,
    form_start: usize,
    path: &str,
    src: &str,
    tokens: &mut I,
    depth: &mut usize,
    plan: &mut Plan,
) where
    I: Iterator<Item = wast::lexer::Token>,
{
    match keyword {
        "type" => {
            consume_optional_id(path, src, tokens, depth, plan);
            let close = walk_inline_exports(path, src, tokens, depth, plan);
            if let Some(close) = close {
                plan.type_spans.push(Span {
                    start: form_start,
                    end: close + 1,
                });
            }
        }
        "func" | "global" | "memory" | "table" | "data" => {
            consume_optional_id(path, src, tokens, depth, plan);
            walk_inline_exports(path, src, tokens, depth, plan);
        }
        "elem" => {
            // `(elem declare func $a $b $c)` declares no new ids — its
            // identifiers are all references to existing funcs. Walk
            // to end without recording a declaration.
            skip_to_form_end(tokens, depth);
        }
        "import" => {
            handle_import(form_start, path, src, tokens, depth, plan);
        }
        "rec" => {
            // The whole `(rec ...)` block is a type group. Inside, walk
            // each `(type $X ...)` to record the rename, then capture
            // the whole block's span for hoisting into the merged rec.
            let close = walk_rec_group(path, src, tokens, depth, plan);
            if let Some(close) = close {
                plan.type_spans.push(Span {
                    start: form_start,
                    end: close + 1,
                });
            }
        }
        _ => {
            skip_to_form_end(tokens, depth);
        }
    }
}

/// Walk inside a `(rec ...)` block. Each `(type $X ...)` inside binds
/// a name we rename. We don't record per-type spans — the whole rec
/// block is the unit the merger hoists.
fn walk_rec_group<I>(
    path: &str,
    src: &str,
    tokens: &mut I,
    depth: &mut usize,
    plan: &mut Plan,
) -> Option<usize>
where
    I: Iterator<Item = wast::lexer::Token>,
{
    let mut last_close = None;
    while *depth >= 2 {
        let tok = tokens.next()?;
        match tok.kind {
            TokenKind::LParen => {
                *depth += 1;
                if *depth == 3 {
                    // sub-form inside rec — likely (type $X ...).
                    let kw = tokens.next()?;
                    if kw.kind == TokenKind::Keyword && slice(src, &kw) == "type" {
                        consume_optional_id(path, src, tokens, depth, plan);
                    }
                    walk_inline_exports(path, src, tokens, depth, plan);
                }
            }
            TokenKind::RParen => {
                last_close = Some(tok.offset);
                *depth -= 1;
            }
            _ => {}
        }
    }
    last_close
}

/// Consume the immediate next significant token. If it's an `Id`,
/// record a local rename for it. If it's `(`, the form is anonymous —
/// open the sub-form (increment depth) and return so `walk_inline_exports`
/// can handle the inner forms (including `(export ...)`).
fn consume_optional_id<I>(
    path: &str,
    src: &str,
    tokens: &mut I,
    depth: &mut usize,
    plan: &mut Plan,
) where
    I: Iterator<Item = wast::lexer::Token>,
{
    let tok = match tokens.next() {
        Some(t) => t,
        None => return,
    };
    match tok.kind {
        TokenKind::Id => {
            let name = strip_dollar(slice(src, &tok));
            plan.id_renames
                .insert(name.clone(), format!("{path}:{name}"));
        }
        TokenKind::LParen => {
            *depth += 1;
            // Hand the just-opened sub-form to walk_inline_exports
            // by inspecting its keyword now.
            inspect_subform_for_export(path, src, tokens, depth, plan);
        }
        TokenKind::RParen => {
            *depth -= 1;
        }
        _ => {}
    }
}

/// We've just entered a sub-form (the `(` was consumed by the caller,
/// `*depth` is the sub-form depth). Read its keyword; if it's `export`,
/// queue a string rewrite. Walk to the form's end either way.
fn inspect_subform_for_export<I>(
    path: &str,
    src: &str,
    tokens: &mut I,
    depth: &mut usize,
    plan: &mut Plan,
) where
    I: Iterator<Item = wast::lexer::Token>,
{
    let kw_tok = match tokens.next() {
        Some(t) => t,
        None => return,
    };
    if kw_tok.kind == TokenKind::Keyword && slice(src, &kw_tok) == "export" {
        if let Some(str_tok) = tokens.next() {
            if str_tok.kind == TokenKind::String {
                queue_export_rewrite(path, src, &str_tok, plan);
            }
        }
    }
    // Skip to end of this sub-form.
    let target = *depth - 1;
    while *depth > target {
        let tok = match tokens.next() {
            Some(t) => t,
            None => return,
        };
        match tok.kind {
            TokenKind::LParen => *depth += 1,
            TokenKind::RParen => *depth -= 1,
            _ => {}
        }
    }
}

/// Queue a rewrite for an `(export "name")` string. Only fires when
/// the name is unqualified (no `:` or `/`); pre-qualified export
/// strings are passed through untouched.
fn queue_export_rewrite(path: &str, src: &str, str_tok: &wast::lexer::Token, plan: &mut Plan) {
    let raw = slice(src, str_tok);
    let name = raw.trim_matches('"');
    if name.contains(':') || name.contains('/') {
        return;
    }
    plan.string_edits.push(Edit {
        start: str_tok.offset,
        end: str_tok.offset + str_tok.len as usize,
        replacement: format!("\"{path}:{name}\""),
    });
}

/// Walk to the end of the current top-level form (depth 2). Along the
/// way, find any inline `(export "...")` forms and queue an edit that
/// prefixes the export name with `<path>:`. Returns the byte offset
/// of the closing `)` of the top-level form.
fn walk_inline_exports<I>(
    path: &str,
    src: &str,
    tokens: &mut I,
    depth: &mut usize,
    plan: &mut Plan,
) -> Option<usize>
where
    I: Iterator<Item = wast::lexer::Token>,
{
    let mut last_close = None;
    while *depth >= 2 {
        let tok = tokens.next()?;
        match tok.kind {
            TokenKind::LParen => {
                *depth += 1;
                inspect_subform_for_export(path, src, tokens, depth, plan);
            }
            TokenKind::RParen => {
                last_close = Some(tok.offset);
                *depth -= 1;
            }
            _ => {}
        }
    }
    last_close
}

/// Process `(import "<module>" "<name>" (<kind> $<id> ...))`.
/// We're at depth 2 (just inside the import). `form_start` is the byte
/// offset of the opening `(`.
fn handle_import<I>(
    form_start: usize,
    path: &str,
    src: &str,
    tokens: &mut I,
    depth: &mut usize,
    plan: &mut Plan,
) where
    I: Iterator<Item = wast::lexer::Token>,
{
    let module_str = expect_string(src, tokens, depth);
    let import_name = expect_string(src, tokens, depth);
    let inner_id = walk_to_inner_id(src, tokens, depth);

    let mut wat_target: Option<String> = None;
    if let (Some(module_str), Some(import_name), Some(id)) =
        (module_str.clone(), import_name.clone(), inner_id)
    {
        if id != import_name {
            panic!(
                "wat-linker: in {path}, import \"{module_str}\" \"{import_name}\" \
                 binds local handle $\"{id}\" — handle id must equal the import \
                 name (\"{import_name}\"). Aliasing imports is not supported."
            );
        }
        let exporter_path = resolve_import_path(path, &module_str);
        plan.id_renames
            .insert(id.clone(), format!("{exporter_path}:{import_name}"));
        if module_str.ends_with(".wat") {
            wat_target = Some(exporter_path);
        }
    }

    let close = skip_to_form_end(tokens, depth);
    if let Some(close) = close {
        // Only record the import as merger-removable when it's a .wat
        // import. Other imports (host env etc.) survive into the merged
        // module untouched.
        if let Some(target) = wat_target {
            plan.import_spans.push(Span {
                start: form_start,
                end: close + 1,
            });
            plan.wat_imports.push(target);
        }
    }
}

/// Resolve a relative import path. `./X` is taken relative to the
/// directory of `importer_path`. Anything else is returned verbatim.
fn resolve_import_path(importer_path: &str, module_str: &str) -> String {
    if let Some(rest) = module_str.strip_prefix("./") {
        let dir = match importer_path.rsplit_once('/') {
            Some((d, _)) => d,
            None => "",
        };
        if dir.is_empty() {
            rest.to_string()
        } else {
            format!("{dir}/{rest}")
        }
    } else {
        module_str.to_string()
    }
}

fn expect_string<I>(src: &str, tokens: &mut I, depth: &mut usize) -> Option<String>
where
    I: Iterator<Item = wast::lexer::Token>,
{
    while let Some(tok) = tokens.next() {
        match tok.kind {
            TokenKind::String => {
                let raw = slice(src, &tok);
                return Some(raw.trim_matches('"').to_string());
            }
            TokenKind::LParen => *depth += 1,
            TokenKind::RParen => {
                *depth -= 1;
                return None;
            }
            _ => {}
        }
    }
    None
}

fn walk_to_inner_id<I>(src: &str, tokens: &mut I, depth: &mut usize) -> Option<String>
where
    I: Iterator<Item = wast::lexer::Token>,
{
    while let Some(tok) = tokens.next() {
        match tok.kind {
            TokenKind::LParen => {
                *depth += 1;
                let _ = tokens.next(); // keyword
                if let Some(id_tok) = tokens.next() {
                    if id_tok.kind == TokenKind::Id {
                        return Some(strip_dollar(slice(src, &id_tok)));
                    }
                }
                return None;
            }
            TokenKind::RParen => {
                *depth -= 1;
                return None;
            }
            _ => {}
        }
    }
    None
}

fn next_top_id_at<I>(
    src: &str,
    tokens: &mut I,
    depth: &mut usize,
    form_depth: usize,
) -> Option<String>
where
    I: Iterator<Item = wast::lexer::Token>,
{
    while let Some(tok) = tokens.next() {
        match tok.kind {
            TokenKind::Id if *depth == form_depth => {
                return Some(strip_dollar(slice(src, &tok)));
            }
            TokenKind::LParen => *depth += 1,
            TokenKind::RParen => {
                if *depth == form_depth {
                    *depth -= 1;
                    return None;
                }
                *depth -= 1;
            }
            _ => {}
        }
    }
    None
}

fn skip_to_form_end<I>(tokens: &mut I, depth: &mut usize) -> Option<usize>
where
    I: Iterator<Item = wast::lexer::Token>,
{
    let mut last_close = None;
    while *depth >= 2 {
        let tok = tokens.next()?;
        match tok.kind {
            TokenKind::LParen => *depth += 1,
            TokenKind::RParen => {
                last_close = Some(tok.offset);
                *depth -= 1;
            }
            _ => {}
        }
    }
    last_close
}

/// Apply the plan: combine id renames (per-token splice) and string
/// edits (per-byte-range replace) into a single linear copy of `src`.
fn apply_plan(src: &str, plan: &Plan) -> String {
    let lexer = Lexer::new(src);

    // Build a flat list of (start, end, replacement) edits sorted by start.
    let mut edits: Vec<Edit> = Vec::new();

    for tok in lexer.iter(0).filter_map(Result::ok) {
        if tok.kind != TokenKind::Id {
            continue;
        }
        let bare = strip_dollar(slice(src, &tok));
        if let Some(target) = plan.id_renames.get(&bare) {
            let start = tok.offset;
            let end = start + tok.len as usize;
            edits.push(Edit {
                start,
                end,
                replacement: format!("${target}"),
            });
        }
    }

    edits.extend(plan.string_edits.iter().map(|e| Edit {
        start: e.start,
        end: e.end,
        replacement: e.replacement.clone(),
    }));
    edits.sort_by_key(|e| e.start);

    let mut out = String::with_capacity(src.len() * 2);
    let mut cursor = 0usize;
    for edit in &edits {
        out.push_str(&src[cursor..edit.start]);
        out.push_str(&edit.replacement);
        cursor = edit.end;
    }
    out.push_str(&src[cursor..]);
    out
}

fn slice<'a>(src: &'a str, tok: &wast::lexer::Token) -> &'a str {
    &src[tok.offset..tok.offset + tok.len as usize]
}

fn strip_dollar(s: &str) -> String {
    debug_assert!(s.starts_with('$'));
    s[1..].to_string()
}

fn is_significant(tok: &wast::lexer::Token) -> bool {
    !matches!(
        tok.kind,
        TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end merge target. foo imports from bar; the merged
    /// output should hoist all types into one rec group, drop resolved
    /// imports, and concatenate the renamed bodies in dep-first order.
    /// Run with `BLESS=1` to overwrite foo.expected.wat with the
    /// actual output.
    #[test]
    fn merge_foo_with_bar() {
        let modules: &[(&str, &str)] = &[
            ("test-wats/foo.wat", include_str!("test-wats/foo.wat")),
            ("test-wats/bar.wat", include_str!("test-wats/bar.wat")),
        ];
        let got = link(modules);

        let expected_path =
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/wat_linker/test-wats/foo.expected.wat");
        if std::env::var("BLESS").is_ok() {
            std::fs::write(expected_path, &got).expect("BLESS write failed");
            return;
        }
        let expected = std::fs::read_to_string(expected_path).expect("read expected");
        assert_eq!(
            got, expected,
            "\nlink produced output that does not match foo.expected.wat. \
             Re-run with BLESS=1 to update.",
        );
    }

    #[test]
    fn resolve_relative_import() {
        assert_eq!(
            resolve_import_path("test-wats/foo.wat", "./bar.wat"),
            "test-wats/bar.wat"
        );
        assert_eq!(
            resolve_import_path("rt/protocols.wat", "./types.wat"),
            "rt/types.wat"
        );
        assert_eq!(resolve_import_path("foo.wat", "./bar.wat"), "bar.wat");
    }

    /// Diamond import graph: A → {B, C} → D. D must appear once in
    /// the merged output (deduped despite two paths reaching it),
    /// types all share one rec group, deps appear before dependents.
    #[test]
    fn merge_diamond() {
        let modules: &[(&str, &str)] = &[
            ("test-wats/diamond/a.wat", include_str!("test-wats/diamond/a.wat")),
            ("test-wats/diamond/b.wat", include_str!("test-wats/diamond/b.wat")),
            ("test-wats/diamond/c.wat", include_str!("test-wats/diamond/c.wat")),
            ("test-wats/diamond/d.wat", include_str!("test-wats/diamond/d.wat")),
        ];
        let got = link(modules);
        let expected_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/wat_linker/test-wats/diamond/expected.wat"
        );
        if std::env::var("BLESS").is_ok() {
            std::fs::write(expected_path, &got).expect("BLESS write failed");
            return;
        }
        let expected = std::fs::read_to_string(expected_path).expect("read expected");
        assert_eq!(got, expected, "\nlink (diamond) mismatch. BLESS=1 to update.");
    }

    /// Two-way cycle: E ↔ F. Linker must tolerate the cycle — each
    /// module appears once, references resolve via id-rename.
    #[test]
    fn merge_cycle() {
        let modules: &[(&str, &str)] = &[
            ("test-wats/cycle/e.wat", include_str!("test-wats/cycle/e.wat")),
            ("test-wats/cycle/f.wat", include_str!("test-wats/cycle/f.wat")),
        ];
        let got = link(modules);
        let expected_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/wat_linker/test-wats/cycle/expected.wat"
        );
        if std::env::var("BLESS").is_ok() {
            std::fs::write(expected_path, &got).expect("BLESS write failed");
            return;
        }
        let expected = std::fs::read_to_string(expected_path).expect("read expected");
        assert_eq!(got, expected, "\nlink (cycle) mismatch. BLESS=1 to update.");
    }

    /// Diamond merged output must parse as valid WAT.
    #[test]
    fn merged_diamond_parses() {
        let modules: &[(&str, &str)] = &[
            ("test-wats/diamond/a.wat", include_str!("test-wats/diamond/a.wat")),
            ("test-wats/diamond/b.wat", include_str!("test-wats/diamond/b.wat")),
            ("test-wats/diamond/c.wat", include_str!("test-wats/diamond/c.wat")),
            ("test-wats/diamond/d.wat", include_str!("test-wats/diamond/d.wat")),
        ];
        let got = link(modules);
        if let Err(e) = wat_crate::parse_str(&got) {
            panic!("merged diamond failed to parse: {e}\n\noutput:\n{got}");
        }
    }

    /// Cycle merged output must parse as valid WAT.
    #[test]
    fn merged_cycle_parses() {
        let modules: &[(&str, &str)] = &[
            ("test-wats/cycle/e.wat", include_str!("test-wats/cycle/e.wat")),
            ("test-wats/cycle/f.wat", include_str!("test-wats/cycle/f.wat")),
        ];
        let got = link(modules);
        if let Err(e) = wat_crate::parse_str(&got) {
            panic!("merged cycle failed to parse: {e}\n\noutput:\n{got}");
        }
    }

    /// Merged output of foo+bar must be valid WAT — i.e. round-trip
    /// through `wat_crate::parse_str` without errors. Catches structural
    /// damage the text-equality test wouldn't (orphan parens, broken
    /// rec group, etc.).
    #[test]
    fn merged_foo_with_bar_parses() {
        let modules: &[(&str, &str)] = &[
            ("test-wats/foo.wat", include_str!("test-wats/foo.wat")),
            ("test-wats/bar.wat", include_str!("test-wats/bar.wat")),
        ];
        let got = link(modules);
        if let Err(e) = wat_crate::parse_str(&got) {
            panic!("merged output failed to parse: {e}\n\noutput:\n{got}");
        }
    }

    #[test]
    #[should_panic(expected = "handle id must equal the import name")]
    fn import_handle_must_match_name() {
        let src = r#"
            (module
              (import "./bar.wat" "Bar" (type $MyAlias (sub any))))
        "#;
        let _ = rename_locals("test-wats/foo.wat", src);
    }
}
