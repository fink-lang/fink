//! Render a `Fragment` (unlinked wasm IR) to WAT text.
//!
//! Tracer-phase renderer — walks the fragment arenas and emits WAT.
//! No wasm-byte parsing. No runtime filtering. The output reads like
//! something a wasm engineer would have written by hand.
//!
//! Unknowns at this point (to decide as we grow the renderer):
//!
//! * Whether we render symbolic names or numeric ids for cross-refs.
//!   Decision: always render the display name if present, fall back
//!   to a synthesised `$t_N` / `$f_N` / `$g_N` / `$d_N`. The linker
//!   still resolves by `Sym(u32)` — names here are presentation.
//!
//! * Function bodies render as `local.set $a (struct.new $Num
//!   (f64.const 42.0))` style (s-expr "folded" WAT), not as
//!   stack-machine form. That matches how humans write WAT and keeps
//!   the shape close to the IR's expression tree.

use std::fmt::Write as _;

use crate::sourcemap::native::{Mapping, SourceMap};

use super::ir::*;

/// Render a fragment to WAT text. Convenience wrapper that discards
/// the sourcemap.
pub fn fmt_fragment(frag: &Fragment) -> String {
  fmt_fragment_with_sm(frag).0
}

/// Render a fragment to WAT text **and** the native sourcemap tying
/// each instruction's output byte offset back to its source origin.
/// Instructions without an origin emit a `Mapping { src: None }` so
/// output consumers know the slot is intentionally unmapped.
pub fn fmt_fragment_with_sm(frag: &Fragment) -> (String, SourceMap) {
  let mut out = String::new();
  let mut sm = SourceMap::new();
  out.push_str("(module\n");

  for (i, ty) in frag.types.iter().enumerate() {
    fmt_type(&mut out, frag, TypeSym::Local(i as u32), ty);
  }
  for (i, f) in frag.funcs.iter().enumerate() {
    fmt_func(&mut out, &mut sm, frag, FuncSym::Local(i as u32), f);
  }
  for (i, g) in frag.globals.iter().enumerate() {
    fmt_global(&mut out, frag, GlobalSym(i as u32), g);
  }
  for (i, d) in frag.data.iter().enumerate() {
    fmt_data(&mut out, DataSym(i as u32), d);
  }

  out.push(')');
  (out, sm)
}

// ──────────────────────────────────────────────────────────────────
// Names
// ──────────────────────────────────────────────────────────────────

// If `display` already contains the qualified form (`<module>:<...>`),
// use it as-is. Once every runtime export follows that convention this
// idempotency hop becomes unconditional and the prefix branch is dead.
fn import_alias(module: &str, display: &str) -> String {
  if display.contains(':') { format!("${}", display) }
  else { format!("${}:{}", module, display) }
}

fn type_name(frag: &Fragment, sym: TypeSym) -> String {
  let i = match sym {
    TypeSym::Local(i) => i,
    TypeSym::Runtime(s) => {
      let (m, n) = super::runtime_contract::import_key(s);
      return import_alias(m, n);
    }
  };
  let Some(ty) = frag.types.get(i as usize) else {
    return format!("$t_{}", i);
  };
  let display = ty.display.as_deref().unwrap_or("");
  match &ty.import {
    Some(ImportKey { module, .. }) => import_alias(module, display),
    None => {
      if display.is_empty() { format!("$t_{}", i) }
      else { format!("${}", display) }
    }
  }
}

fn func_name(frag: &Fragment, sym: FuncSym) -> String {
  let i = match sym {
    FuncSym::Local(i) => i,
    FuncSym::Runtime(s) => {
      let (m, n) = super::runtime_contract::import_key(s);
      return import_alias(m, n);
    }
  };
  let Some(f) = frag.funcs.get(i as usize) else {
    return format!("$f_{}", i);
  };
  let display = f.display.as_deref().unwrap_or("");
  match &f.import {
    Some(ImportKey { module, .. }) => import_alias(module, display),
    None => {
      if display.is_empty() { format!("$f_{}", i) }
      else { format!("${}", display) }
    }
  }
}

fn global_name(frag: &Fragment, sym: GlobalSym) -> String {
  let Some(g) = frag.globals.get(sym.0 as usize) else {
    return format!("$g_{}", sym.0);
  };
  let display = g.display.as_deref().unwrap_or("");
  match &g.import {
    Some(ImportKey { module, .. }) => import_alias(module, display),
    None => {
      if display.is_empty() { format!("$g_{}", sym.0) }
      else { format!("${}", display) }
    }
  }
}

fn local_name(f: &FuncDecl, idx: LocalIdx) -> String {
  let i = idx.0 as usize;
  let decl = if i < f.params.len() {
    f.params.get(i)
  } else {
    f.locals.get(i - f.params.len())
  };
  match decl.and_then(|l| l.display.as_deref()) {
    Some(n) => format!("${}", n),
    None => format!("$l_{}", idx.0),
  }
}

fn data_name(d: &DataDecl, sym: DataSym) -> String {
  match d.display.as_deref() {
    Some(n) => format!("${}", n),
    None => format!("$d_{}", sym.0),
  }
}

// ──────────────────────────────────────────────────────────────────
// Value / heap types
// ──────────────────────────────────────────────────────────────────

fn abs_heap(ht: AbsHeap) -> &'static str {
  match ht {
    AbsHeap::Any  => "any",
    AbsHeap::Eq   => "eq",
    AbsHeap::I31  => "i31",
    AbsHeap::Func => "func",
  }
}

fn fmt_val(frag: &Fragment, ty: &ValType) -> String {
  match ty {
    ValType::I32 => "i32".into(),
    ValType::F64 => "f64".into(),
    ValType::RefConcrete { nullable: true, ty }   => format!("(ref null {})", type_name(frag, *ty)),
    ValType::RefConcrete { nullable: false, ty }  => format!("(ref {})", type_name(frag, *ty)),
    ValType::RefAbstract { nullable: true, ht }   => format!("(ref null {})", abs_heap(*ht)),
    ValType::RefAbstract { nullable: false, ht }  => format!("(ref {})", abs_heap(*ht)),
  }
}

// ──────────────────────────────────────────────────────────────────
// Sections
// ──────────────────────────────────────────────────────────────────

fn fmt_type(out: &mut String, frag: &Fragment, sym: TypeSym, ty: &TypeDecl) {
  let name = type_name(frag, sym);

  // Imported type — WebAssembly "Type Imports and Exports" notation.
  if let Some(ImportKey { module, name: field }) = &ty.import {
    let body = fmt_type_body(frag, &ty.kind);
    writeln!(out, "  (import \"{}\" \"{}\" (type {} {}))", module, field, name, body).unwrap();
    return;
  }

  match &ty.kind {
    TypeKind::Struct { fields } => {
      writeln!(out, "  (type {} (struct", name).unwrap();
      for f in fields {
        let mutness = if f.mutable { "mut " } else { "" };
        let fname = f.display.as_deref().unwrap_or("");
        writeln!(out, "    (field ${} ({}{}))", fname, mutness, fmt_val(frag, &f.ty)).unwrap();
      }
      out.push_str("  ))\n");
    }
    TypeKind::Array { elem, mutable } => {
      let mutness = if *mutable { "mut " } else { "" };
      writeln!(out, "  (type {} (array ({}{})))", name, mutness, fmt_val(frag, elem)).unwrap();
    }
    TypeKind::Func { params, results } => {
      out.push_str("  (type ");
      out.push_str(&name);
      out.push_str(" (func");
      if !params.is_empty() {
        out.push_str(" (param");
        for p in params { out.push(' '); out.push_str(&fmt_val(frag, p)); }
        out.push(')');
      }
      if !results.is_empty() {
        out.push_str(" (result");
        for r in results { out.push(' '); out.push_str(&fmt_val(frag, r)); }
        out.push(')');
      }
      out.push_str("))\n");
    }
    TypeKind::SubBound { ht } => {
      writeln!(out, "  (type {} (sub {}))", name, abs_heap(*ht)).unwrap();
    }
  }
}

/// Render only the body of a type (the thing inside `(type $name ...)`).
/// Used by imported-type rendering where the body becomes the import's bound.
fn fmt_type_body(frag: &Fragment, kind: &TypeKind) -> String {
  match kind {
    TypeKind::SubBound { ht } => format!("(sub {})", abs_heap(*ht)),
    TypeKind::Struct { fields } => {
      let mut s = String::from("(struct");
      for f in fields {
        let mutness = if f.mutable { "mut " } else { "" };
        let fname = f.display.as_deref().unwrap_or("");
        s.push_str(&format!(" (field ${} ({}{}))", fname, mutness, fmt_val(frag, &f.ty)));
      }
      s.push(')');
      s
    }
    TypeKind::Array { elem, mutable } => {
      let mutness = if *mutable { "mut " } else { "" };
      format!("(array ({}{}))", mutness, fmt_val(frag, elem))
    }
    TypeKind::Func { params, results } => {
      let mut s = String::from("(func");
      if !params.is_empty() {
        s.push_str(" (param");
        for p in params { s.push(' '); s.push_str(&fmt_val(frag, p)); }
        s.push(')');
      }
      if !results.is_empty() {
        s.push_str(" (result");
        for r in results { s.push(' '); s.push_str(&fmt_val(frag, r)); }
        s.push(')');
      }
      s.push(')');
      s
    }
  }
}

fn fmt_func(out: &mut String, sm: &mut SourceMap, frag: &Fragment, sym: FuncSym, f: &FuncDecl) {
  let name = func_name(frag, sym);

  if let Some(ImportKey { module, name: field }) = &f.import {
    writeln!(out, "  (import \"{}\" \"{}\" (func {} (type {})))",
      module, field, name, type_name(frag, f.sig)).unwrap();
    return;
  }

  out.push_str("  (func ");
  out.push_str(&name);
  write!(out, " (type {})", type_name(frag, f.sig)).unwrap();

  for p in &f.params {
    let n = p.display.as_deref().unwrap_or("");
    write!(out, " (param ${} {})", n, fmt_val(frag, &p.ty)).unwrap();
  }
  out.push('\n');

  for l in &f.locals {
    let n = l.display.as_deref().unwrap_or("");
    writeln!(out, "    (local ${} {})", n, fmt_val(frag, &l.ty)).unwrap();
  }

  let body_indent = 4;
  for id in &f.body {
    let instr = &frag.instrs[id.0 as usize];
    // Record a mapping at the byte offset where this instruction's
    // *meaningful* output starts — past the indent pad, at the `(`
    // of the statement. Consumers (VSCode extension, etc.) place
    // decorations at that column.
    sm.push(Mapping {
      out: (out.len() + body_indent) as u32,
      src: instr.origin,
    });
    fmt_instr(out, frag, f, instr, body_indent);
  }

  out.push_str("  )\n");

  if let Some(exp) = &f.export {
    writeln!(out, "  (export \"{}\" (func {}))", exp, name).unwrap();
  }
}

fn fmt_global(out: &mut String, frag: &Fragment, sym: GlobalSym, g: &GlobalDecl) {
  let name = global_name(frag, sym);
  let mutness = if g.mutable { "mut " } else { "" };

  if let Some(ImportKey { module, name: field }) = &g.import {
    writeln!(out, "  (import \"{}\" \"{}\" (global {} ({}{})))",
      module, field, name, mutness, fmt_val(frag, &g.ty)).unwrap();
    return;
  }

  write!(out, "  (global {} ({}{}) ", name, mutness, fmt_val(frag, &g.ty)).unwrap();
  fmt_global_init(out, frag, &g.init);
  out.push_str(")\n");

  if let Some(exp) = &g.export {
    writeln!(out, "  (export \"{}\" (global {}))", exp, name).unwrap();
  }
}

fn fmt_global_init(out: &mut String, frag: &Fragment, init: &GlobalInit) {
  match init {
    GlobalInit::I32Const(v)       => write!(out, "(i32.const {})", v).unwrap(),
    GlobalInit::F64Const(v)       => write!(out, "(f64.const {})", v).unwrap(),
    GlobalInit::RefNull(ht)       => write!(out, "(ref.null {})", abs_heap(*ht)).unwrap(),
    GlobalInit::RefNullConcrete(t)=> write!(out, "(ref.null {})", type_name(frag, *t)).unwrap(),
    GlobalInit::RefFunc(f)        => write!(out, "(ref.func {})", func_name(frag, *f)).unwrap(),
  }
}

fn fmt_data(out: &mut String, sym: DataSym, d: &DataDecl) {
  let name = data_name(d, sym);
  writeln!(out, "  (data {} \"{}\")", name, escape_bytes(&d.bytes)).unwrap();
}

fn escape_bytes(bytes: &[u8]) -> String {
  let mut s = String::with_capacity(bytes.len());
  for &b in bytes {
    match b {
      b'\\' => s.push_str("\\\\"),
      b'"'  => s.push_str("\\\""),
      0x20..=0x7e => s.push(b as char),
      _ => { s.push_str(&format!("\\{:02x}", b)); }
    }
  }
  s
}

// ──────────────────────────────────────────────────────────────────
// Instructions — folded s-expr form
// ──────────────────────────────────────────────────────────────────

fn fmt_instr(out: &mut String, frag: &Fragment, f: &FuncDecl, instr: &Instr, indent: usize) {
  let pad = " ".repeat(indent);
  match &instr.kind {
    InstrKind::LocalSet { idx, src } => {
      writeln!(out, "{}(local.set {} {})", pad, local_name(f, *idx), fmt_operand(frag, f, src)).unwrap();
    }
    InstrKind::GlobalSet { sym, src } => {
      writeln!(out, "{}(global.set {} {})", pad, global_name(frag, *sym), fmt_operand(frag, f, src)).unwrap();
    }
    InstrKind::RefNull { ht, into } => {
      writeln!(out, "{}(local.set {} (ref.null {}))", pad, local_name(f, *into), abs_heap(*ht)).unwrap();
    }
    InstrKind::RefNullConcrete { ty, into } => {
      writeln!(out, "{}(local.set {} (ref.null {}))", pad, local_name(f, *into), type_name(frag, *ty)).unwrap();
    }
    InstrKind::RefI31 { src, into } => {
      writeln!(out, "{}(local.set {} (ref.i31 {}))", pad, local_name(f, *into), fmt_operand(frag, f, src)).unwrap();
    }
    InstrKind::I31GetS { src, into } => {
      writeln!(out, "{}(local.set {} (i31.get_s {}))", pad, local_name(f, *into), fmt_operand(frag, f, src)).unwrap();
    }
    InstrKind::RefFunc { func, into } => {
      writeln!(out, "{}(local.set {} (ref.func {}))", pad, local_name(f, *into), func_name(frag, *func)).unwrap();
    }
    InstrKind::StructNew { ty, fields, into } => {
      write!(out, "{}(local.set {} (struct.new {}", pad, local_name(f, *into), type_name(frag, *ty)).unwrap();
      for field in fields { write!(out, " {}", fmt_operand(frag, f, field)).unwrap(); }
      out.push_str("))\n");
    }
    InstrKind::ArrayNewFixed { ty, size, elems, into } => {
      write!(out, "{}(local.set {} (array.new_fixed {} {}",
        pad, local_name(f, *into), type_name(frag, *ty), size).unwrap();
      for e in elems { write!(out, " {}", fmt_operand(frag, f, e)).unwrap(); }
      out.push_str("))\n");
    }
    InstrKind::ArrayGet { ty, arr, idx, into } => {
      writeln!(out, "{}(local.set {} (array.get {} {} {}))",
        pad, local_name(f, *into), type_name(frag, *ty),
        fmt_operand(frag, f, arr), fmt_operand(frag, f, idx)).unwrap();
    }
    InstrKind::RefCastNonNull { ty, src, into } => {
      writeln!(out, "{}(local.set {} (ref.cast (ref {}) {}))",
        pad, local_name(f, *into), type_name(frag, *ty), fmt_operand(frag, f, src)).unwrap();
    }
    InstrKind::RefCastNullable { ty, src, into } => {
      writeln!(out, "{}(local.set {} (ref.cast (ref null {}) {}))",
        pad, local_name(f, *into), type_name(frag, *ty), fmt_operand(frag, f, src)).unwrap();
    }
    InstrKind::RefCastNonNullAbs { ht, src, into } => {
      writeln!(out, "{}(local.set {} (ref.cast (ref {}) {}))",
        pad, local_name(f, *into), abs_heap(*ht), fmt_operand(frag, f, src)).unwrap();
    }
    InstrKind::Call { target, args, into } => {
      let call_sexpr = fmt_call_sexpr(frag, f, "call", *target, args);
      match into {
        Some(local) => writeln!(out, "{}(local.set {} {})", pad, local_name(f, *local), call_sexpr).unwrap(),
        None        => writeln!(out, "{}{}", pad, call_sexpr).unwrap(),
      }
    }
    InstrKind::ReturnCall { target, args } => {
      writeln!(out, "{}{}", pad, fmt_call_sexpr(frag, f, "return_call", *target, args)).unwrap();
    }
    InstrKind::If { cond, then_body, else_body } => {
      writeln!(out, "{}(if {}", pad, fmt_operand(frag, f, cond)).unwrap();
      writeln!(out, "{}  (then", pad).unwrap();
      for id in then_body {
        fmt_instr(out, frag, f, &frag.instrs[id.0 as usize], indent + 4);
      }
      writeln!(out, "{}  )", pad).unwrap();
      if !else_body.is_empty() {
        writeln!(out, "{}  (else", pad).unwrap();
        for id in else_body {
          fmt_instr(out, frag, f, &frag.instrs[id.0 as usize], indent + 4);
        }
        writeln!(out, "{}  )", pad).unwrap();
      }
      writeln!(out, "{})", pad).unwrap();
    }
    InstrKind::Unreachable => writeln!(out, "{}unreachable", pad).unwrap(),
    InstrKind::Drop { src } => writeln!(out, "{}(drop {})", pad, fmt_operand(frag, f, src)).unwrap(),
  }
}

fn fmt_call_sexpr(frag: &Fragment, f: &FuncDecl, op: &str, target: FuncSym, args: &[Operand]) -> String {
  let mut s = format!("({} {}", op, func_name(frag, target));
  for a in args { s.push(' '); s.push_str(&fmt_operand(frag, f, a)); }
  s.push(')');
  s
}

fn fmt_operand(frag: &Fragment, f: &FuncDecl, op: &Operand) -> String {
  match op {
    Operand::I32(v)     => format!("(i32.const {})", v),
    Operand::F64(v)     => format!("(f64.const {})", v),
    Operand::Local(l)   => format!("(local.get {})", local_name(f, *l)),
    Operand::Global(g)  => format!("(global.get {})", global_name(frag, *g)),
    Operand::RefFunc(fs)=> format!("(ref.func {})", func_name(frag, *fs)),
    Operand::RefNull(h) => format!("(ref.null {})", abs_heap(*h)),
    Operand::DataRef { sym, len } => {
      // At link time this becomes (i32.const <resolved_offset>)
      // followed by (i32.const len). Until then — symbolic marker.
      format!("(data.ref {} {})", data_name(&frag.data[sym.0 as usize], *sym), len)
    }
  }
}

