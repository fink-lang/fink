//! IR-level linker — merges user-level `Fragment`s into a single
//! linked `Fragment`.
//!
//! # Scope
//!
//! * **Input:** a list of user-level Fragments. Index 0 is the entry
//!   module; the rest are deps in any deterministic order. Each
//!   fragment carries its `module_id` and a `module_imports` map of
//!   raw-URL → ModuleId (populated by `ir_compile_package` after
//!   BFS resolution).
//! * **Output:** one linked Fragment. Per-fragment symbol indices
//!   (FuncSym, TypeSym, GlobalSym, DataSym, InstrId) get remapped
//!   to the merged fragment's index space. Cross-fragment user
//!   imports declared via `FuncDecl.import = Some(ImportKey {
//!   module: "<canonical_url>", name: "fink_module" })` resolve to
//!   the producer fragment's local FuncSym. Runtime imports
//!   (`rt/*`, `std/*`, `interop/*`) pass through **unchanged**;
//!   resolving those is `ir_emit`'s job at byte time.
//!
//! The linker is pure IR → IR. It does not touch `wasm-encoder`,
//! does not parse `runtime.wasm`, does not emit bytes.

use std::collections::BTreeMap;

use super::ir::*;

/// Per-fragment offset accumulators for symbol remapping.
#[derive(Default, Clone, Copy, Debug)]
struct Offsets {
  types:   u32,
  funcs:   u32,
  globals: u32,
  data:    u32,
  instrs:  u32,
}

/// Link a set of user-level Fragments into a single linked Fragment.
///
/// Single-fragment inputs are passed through unchanged (other than a
/// `clone()`).
///
/// Multi-fragment inputs:
///   1. Compute per-fragment symbol-index offsets.
///   2. Walk each fragment's items, append into merged with all
///      symbol references rewritten through the offsets.
///   3. Resolve cross-fragment user imports: `FuncDecl.import =
///      Some(ImportKey { module: "<canonical_url>", name:
///      "fink_module" })` → rewrite to producer's local FuncSym +
///      drop the import marker.
pub fn link(fragments: &[Fragment]) -> Fragment {
  match fragments {
    [] => panic!("ir_link: empty fragment list"),
    [only] => only.clone(),
    _ => link_multi(fragments),
  }
}

fn link_multi(fragments: &[Fragment]) -> Fragment {
  // Step 1: compute per-fragment offsets.
  let mut offsets: Vec<Offsets> = vec![Offsets::default(); fragments.len()];
  let mut acc = Offsets::default();
  for (i, frag) in fragments.iter().enumerate() {
    offsets[i] = acc;
    acc.types   += frag.types.len()   as u32;
    acc.funcs   += frag.funcs.len()   as u32;
    acc.globals += frag.globals.len() as u32;
    acc.data    += frag.data.len()    as u32;
    acc.instrs  += frag.instrs.len()  as u32;
  }

  // Step 2: build a (canonical_url → producer's merged FuncSym for
  // its `fink_module`) lookup so cross-fragment user imports can be
  // rewritten.
  //
  // The producer's `fink_module` is the function whose display name
  // is `"<canonical_url>:fink_module"`. We find it by scanning each
  // fragment's funcs and matching the display.
  let mut producer_fink_module: BTreeMap<String, FuncSym> = BTreeMap::new();
  for (frag_idx, frag) in fragments.iter().enumerate() {
    let off = offsets[frag_idx].funcs;
    for (local_idx, f) in frag.funcs.iter().enumerate() {
      if let Some(display) = &f.display
        && let Some(stripped) = display.strip_suffix(":fink_module")
      {
        let sym = FuncSym(off + local_idx as u32);
        producer_fink_module.insert(stripped.to_string(), sym);
      }
    }
  }

  // Step 3: pre-allocate the merged fragment's vectors.
  let mut merged = Fragment {
    module_id: fragments[0].module_id,
    module_imports: BTreeMap::new(),
    types:   Vec::with_capacity(acc.types as usize),
    funcs:   Vec::with_capacity(acc.funcs as usize),
    globals: Vec::with_capacity(acc.globals as usize),
    data:    Vec::with_capacity(acc.data as usize),
    instrs:  Vec::with_capacity(acc.instrs as usize),
  };

  // Step 4: walk each fragment, append items with remapping.
  // Build a redirect table for cross-fragment user-import resolution.
  // After remap, a placeholder FuncDecl `(import "<canonical_url>"
  // "fink_module" ...)` sits at some merged FuncSym; that FuncSym
  // gets redirected to the producer's actual `<canonical_url>:fink_module`.
  let mut func_redirect: BTreeMap<FuncSym, FuncSym> = BTreeMap::new();

  for (frag_idx, frag) in fragments.iter().enumerate() {
    let off = offsets[frag_idx];
    let is_entry = frag_idx == 0;

    for ty in &frag.types {
      merged.types.push(remap_type_decl(ty, &off));
    }

    for (local_idx, f) in frag.funcs.iter().enumerate() {
      let mut decl = remap_func_decl(f, &off, &producer_fink_module);
      // Only the entry fragment exports `fink_module` under the
      // unqualified name — that's the host's call entry point.
      // Dep fragments' `fink_module`s stay unexported (they're
      // invoked via `std/modules.fnk:import`, not by name from the
      // host).
      if !is_entry && decl.export.as_deref() == Some("fink_module") {
        decl.export = None;
      }
      // Cross-fragment user-import resolution: if this is a
      // placeholder `(import "<canonical_url>" "fink_module" ...)`
      // and the URL matches a producer fragment in the merge set,
      // record the redirect so all FuncSym refs to this slot get
      // rewritten to the producer's real FuncSym below.
      if let Some(import_key) = &decl.import
        && import_key.name == "fink_module"
        && let Some(&real_sym) = producer_fink_module.get(&import_key.module)
      {
        let placeholder_sym = FuncSym(off.funcs + local_idx as u32);
        func_redirect.insert(placeholder_sym, real_sym);
        // Drop the import marker — placeholder is now "shadowed" by
        // the redirect. The FuncDecl stays (it's still in funcs[]),
        // but nothing references it after the redirect pass.
        decl.import = None;
      }
      merged.funcs.push(decl);
    }

    for g in &frag.globals {
      merged.globals.push(remap_global_decl(g, &off));
    }

    for d in &frag.data {
      merged.data.push(d.clone());
    }

    for instr in &frag.instrs {
      merged.instrs.push(Instr {
        kind: remap_instr_kind(&instr.kind, &off),
        origin: instr.origin,
      });
    }
  }

  // Step 5: apply the func-redirect table — walk all funcs + instrs +
  // globals and rewrite FuncSym refs through the redirect.
  if !func_redirect.is_empty() {
    apply_func_redirect(&mut merged, &func_redirect);
  }

  merged
}

/// Walk every FuncSym reference in the merged Fragment and apply the
/// redirect table. Used after multi-fragment merge to rewrite
/// cross-fragment user-import placeholders to point at the producer's
/// actual FuncSym.
fn apply_func_redirect(frag: &mut Fragment, redirect: &BTreeMap<FuncSym, FuncSym>) {
  let lookup = |s: FuncSym| redirect.get(&s).copied().unwrap_or(s);

  for f in &mut frag.funcs {
    f.body = f.body.clone();  // body is Vec<InstrId>, no FuncSym refs
  }

  for g in &mut frag.globals {
    if let GlobalInit::RefFunc(fs) = &mut g.init {
      *fs = lookup(*fs);
    }
  }

  for instr in &mut frag.instrs {
    redirect_instr(&mut instr.kind, &lookup);
  }
}

fn redirect_instr(kind: &mut InstrKind, lookup: &impl Fn(FuncSym) -> FuncSym) {
  match kind {
    InstrKind::RefFunc { func, .. } => *func = lookup(*func),
    InstrKind::Call { target, args, .. } => {
      *target = lookup(*target);
      for a in args { redirect_operand(a, lookup); }
    }
    InstrKind::ReturnCall { target, args } => {
      *target = lookup(*target);
      for a in args { redirect_operand(a, lookup); }
    }
    InstrKind::LocalSet { src, .. }
    | InstrKind::GlobalSet { src, .. }
    | InstrKind::RefI31 { src, .. }
    | InstrKind::I31GetS { src, .. }
    | InstrKind::Drop { src }
    | InstrKind::RefCastNonNull { src, .. }
    | InstrKind::RefCastNullable { src, .. }
    | InstrKind::RefCastNonNullAbs { src, .. } =>
      redirect_operand(src, lookup),
    InstrKind::StructNew { fields, .. } => {
      for f in fields { redirect_operand(f, lookup); }
    }
    InstrKind::ArrayNewFixed { elems, .. } => {
      for e in elems { redirect_operand(e, lookup); }
    }
    InstrKind::ArrayGet { arr, idx, .. } => {
      redirect_operand(arr, lookup);
      redirect_operand(idx, lookup);
    }
    InstrKind::If { cond, .. } => redirect_operand(cond, lookup),
    InstrKind::RefNull { .. }
    | InstrKind::RefNullConcrete { .. }
    | InstrKind::Unreachable => {}
  }
}

fn redirect_operand(op: &mut Operand, lookup: &impl Fn(FuncSym) -> FuncSym) {
  if let Operand::RefFunc(s) = op {
    *s = lookup(*s);
  }
}

// ──────────────────────────────────────────────────────────────────
// Per-item remappers.
// ──────────────────────────────────────────────────────────────────

fn remap_type_decl(ty: &TypeDecl, off: &Offsets) -> TypeDecl {
  TypeDecl {
    kind: remap_type_kind(&ty.kind, off),
    display: ty.display.clone(),
    import: ty.import.clone(),
  }
}

fn remap_type_kind(kind: &TypeKind, off: &Offsets) -> TypeKind {
  match kind {
    TypeKind::Struct { fields } => TypeKind::Struct {
      fields: fields.iter().map(|f| StructField {
        ty: remap_val_type(&f.ty, off),
        mutable: f.mutable,
        display: f.display.clone(),
      }).collect(),
    },
    TypeKind::Array { elem, mutable } => TypeKind::Array {
      elem: remap_val_type(elem, off),
      mutable: *mutable,
    },
    TypeKind::Func { params, results } => TypeKind::Func {
      params: params.iter().map(|v| remap_val_type(v, off)).collect(),
      results: results.iter().map(|v| remap_val_type(v, off)).collect(),
    },
    TypeKind::SubBound { ht } => TypeKind::SubBound { ht: *ht },
  }
}

fn remap_val_type(vt: &ValType, off: &Offsets) -> ValType {
  match vt {
    ValType::I32 | ValType::F64 => vt.clone(),
    ValType::RefConcrete { nullable, ty } => ValType::RefConcrete {
      nullable: *nullable,
      ty: remap_type_sym(*ty, off),
    },
    ValType::RefAbstract { nullable, ht } => ValType::RefAbstract {
      nullable: *nullable,
      ht: *ht,
    },
  }
}

fn remap_type_sym(sym: TypeSym, off: &Offsets) -> TypeSym {
  TypeSym(sym.0 + off.types)
}

fn remap_func_sym(sym: FuncSym, off: &Offsets) -> FuncSym {
  FuncSym(sym.0 + off.funcs)
}

fn remap_global_sym(sym: GlobalSym, off: &Offsets) -> GlobalSym {
  GlobalSym(sym.0 + off.globals)
}

fn remap_data_sym(sym: DataSym, off: &Offsets) -> DataSym {
  DataSym(sym.0 + off.data)
}

fn remap_instr_id(id: InstrId, off: &Offsets) -> InstrId {
  InstrId(id.0 + off.instrs)
}

fn remap_local_decl(l: &LocalDecl, off: &Offsets) -> LocalDecl {
  LocalDecl {
    ty: remap_val_type(&l.ty, off),
    display: l.display.clone(),
  }
}

fn remap_func_decl(
  f: &FuncDecl,
  off: &Offsets,
  producer_fink_module: &BTreeMap<String, FuncSym>,
) -> FuncDecl {
  // Cross-fragment user import resolution: a FuncDecl marked with
  // `import: Some(ImportKey { module: "<canonical_url>", name:
  // "fink_module" })` where `module` matches a producer fragment's
  // canonical URL is rewritten:
  //   - Drop the import marker.
  //   - Mark the FuncDecl as a stub re-export pointing at the
  //     producer's already-merged FuncSym.
  //
  // BUT: we can't change the FuncSym of an existing FuncDecl — the
  // FuncSym IS the index into `frag.funcs`. So instead, we keep the
  // FuncDecl in place (it stays as an unused import slot in the
  // merged fragment) and accept the redundancy. ir_emit's existing
  // ImportKey resolution handles it (looks up by name; cross-frag
  // refs to `<canonical_url>:fink_module` won't be in the runtime
  // export table — so this approach only works if we ALSO export
  // each fragment's `fink_module` under its qualified name in the
  // merged binary).
  //
  // Actually the cleanest approach: ir_lower's call sites use
  // FuncSym pointing at the placeholder import-only FuncDecl. The
  // linker rewrites those Call instrs to point at the producer's
  // real FuncSym. The placeholder FuncDecl can stay (it's just an
  // unused entry in frag.funcs) or get pruned.
  //
  // For simplicity, leave the placeholder in. It serialises as an
  // unused import line. Next pass (post-link cleanup) can prune.
  //
  // The Instr-level rewrite happens in `remap_instr_kind` below,
  // which sees Call/ReturnCall targets and consults
  // `producer_fink_module` to redirect them.
  //
  // For this function we just remap symbols.
  let _ = producer_fink_module; // used at instr level

  FuncDecl {
    sig: remap_type_sym(f.sig, off),
    params: f.params.iter().map(|l| remap_local_decl(l, off)).collect(),
    locals: f.locals.iter().map(|l| remap_local_decl(l, off)).collect(),
    body: f.body.iter().map(|id| remap_instr_id(*id, off)).collect(),
    display: f.display.clone(),
    import: f.import.clone(),
    export: f.export.clone(),
  }
}

fn remap_global_decl(g: &GlobalDecl, off: &Offsets) -> GlobalDecl {
  GlobalDecl {
    ty: remap_val_type(&g.ty, off),
    mutable: g.mutable,
    init: remap_global_init(&g.init, off),
    display: g.display.clone(),
    import: g.import.clone(),
    export: g.export.clone(),
  }
}

fn remap_global_init(init: &GlobalInit, off: &Offsets) -> GlobalInit {
  match init {
    GlobalInit::I32Const(v) => GlobalInit::I32Const(*v),
    GlobalInit::F64Const(v) => GlobalInit::F64Const(*v),
    GlobalInit::RefNull(ht) => GlobalInit::RefNull(*ht),
    GlobalInit::RefNullConcrete(ts) => GlobalInit::RefNullConcrete(remap_type_sym(*ts, off)),
    GlobalInit::RefFunc(fs) => GlobalInit::RefFunc(remap_func_sym(*fs, off)),
  }
}

fn remap_instr_kind(kind: &InstrKind, off: &Offsets) -> InstrKind {
  match kind {
    InstrKind::LocalSet { idx, src } =>
      InstrKind::LocalSet { idx: *idx, src: remap_operand(src, off) },
    InstrKind::GlobalSet { sym, src } =>
      InstrKind::GlobalSet { sym: remap_global_sym(*sym, off), src: remap_operand(src, off) },
    InstrKind::RefNull { ht, into } =>
      InstrKind::RefNull { ht: *ht, into: *into },
    InstrKind::RefNullConcrete { ty, into } =>
      InstrKind::RefNullConcrete { ty: remap_type_sym(*ty, off), into: *into },
    InstrKind::RefI31 { src, into } =>
      InstrKind::RefI31 { src: remap_operand(src, off), into: *into },
    InstrKind::I31GetS { src, into } =>
      InstrKind::I31GetS { src: remap_operand(src, off), into: *into },
    InstrKind::RefFunc { func, into } =>
      InstrKind::RefFunc { func: remap_func_sym(*func, off), into: *into },
    InstrKind::StructNew { ty, fields, into } =>
      InstrKind::StructNew {
        ty: remap_type_sym(*ty, off),
        fields: fields.iter().map(|f| remap_operand(f, off)).collect(),
        into: *into,
      },
    InstrKind::ArrayNewFixed { ty, size, elems, into } =>
      InstrKind::ArrayNewFixed {
        ty: remap_type_sym(*ty, off),
        size: *size,
        elems: elems.iter().map(|f| remap_operand(f, off)).collect(),
        into: *into,
      },
    InstrKind::ArrayGet { ty, arr, idx, into } =>
      InstrKind::ArrayGet {
        ty: remap_type_sym(*ty, off),
        arr: remap_operand(arr, off),
        idx: remap_operand(idx, off),
        into: *into,
      },
    InstrKind::RefCastNonNull { ty, src, into } =>
      InstrKind::RefCastNonNull { ty: remap_type_sym(*ty, off), src: remap_operand(src, off), into: *into },
    InstrKind::RefCastNullable { ty, src, into } =>
      InstrKind::RefCastNullable { ty: remap_type_sym(*ty, off), src: remap_operand(src, off), into: *into },
    InstrKind::RefCastNonNullAbs { ht, src, into } =>
      InstrKind::RefCastNonNullAbs { ht: *ht, src: remap_operand(src, off), into: *into },
    InstrKind::Call { target, args, into } =>
      InstrKind::Call {
        target: remap_func_sym(*target, off),
        args: args.iter().map(|a| remap_operand(a, off)).collect(),
        into: *into,
      },
    InstrKind::ReturnCall { target, args } =>
      InstrKind::ReturnCall {
        target: remap_func_sym(*target, off),
        args: args.iter().map(|a| remap_operand(a, off)).collect(),
      },
    InstrKind::If { cond, then_body, else_body } =>
      InstrKind::If {
        cond: remap_operand(cond, off),
        then_body: then_body.iter().map(|id| remap_instr_id(*id, off)).collect(),
        else_body: else_body.iter().map(|id| remap_instr_id(*id, off)).collect(),
      },
    InstrKind::Unreachable => InstrKind::Unreachable,
    InstrKind::Drop { src } => InstrKind::Drop { src: remap_operand(src, off) },
  }
}

fn remap_operand(op: &Operand, off: &Offsets) -> Operand {
  match op {
    Operand::I32(v) => Operand::I32(*v),
    Operand::F64(v) => Operand::F64(*v),
    Operand::Local(idx) => Operand::Local(*idx),
    Operand::Global(sym) => Operand::Global(remap_global_sym(*sym, off)),
    Operand::RefFunc(sym) => Operand::RefFunc(remap_func_sym(*sym, off)),
    Operand::RefNull(ht) => Operand::RefNull(*ht),
    Operand::DataRef { sym, len } => Operand::DataRef {
      sym: remap_data_sym(*sym, off),
      len: *len,
    },
  }
}
