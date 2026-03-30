// Static linker — merges pre-compiled runtime WASM fragments into the
// compiler's emitted WASM output, producing a single standalone module.
//
// ## Design
//
// The fink compiler emits a WASM fragment for user code (via emit.rs).
// Runtime data structures (list, hamt, set, strings) are implemented as
// standalone WAT files compiled to WASM once. The linker merges these
// fragments into one self-contained WASM binary — no runtime imports,
// no component model, runs on any current WASM engine.
//
// ## Pipeline position
//
//   CPS → lift → collect → emit → **link** → DWARF → CompileResult
//                                    ↑
//                            runtime .wasm fragments
//                            (pre-compiled from WAT)
//
// ## Type unification
//
// WASM GC uses nominal typing within rec groups — structurally identical
// types from different modules are distinct. All shared types are defined
// in `runtime/types.wat` as the single canonical source.
//
// The universal value type is `(ref any)` — WASM GC's built-in top type.
// No custom $Any supertype. See `runtime/types.wat` for the full hierarchy.
//
// Each runtime WAT module and the compiler's emitted fragment reference
// shared types by name. The linker:
//   1. Emits canonical types (from types.wat) once in the output type section
//   2. Assigns each module's internal types with namespaced names (no clashes)
//   3. Remaps all type index references in merged code
//
// ## Import convention
//
// Dependencies between fragments are declared as WASM imports using
// the `@fink/` module namespace:
//
//   ;; "I depend on the entire list module"
//   (import "@fink/runtime/list" "*" (func (param anyref)))
//
//   ;; "I depend on a specific function from hamt"
//   (import "@fink/runtime/hamt" "rec_pop" (func (param anyref)))
//
// The `(func (param anyref))` descriptor is a dummy — cheapest valid
// import to keep the WASM validator happy. The linker strips all `@fink/`
// imports and resolves them from the link set.
//
// Granular imports (naming specific functions) enable selective linking:
// if user code only uses `seq_pop`, only `list.wasm` is pulled in, not
// `hamt.wasm` or `set.wasm`. For now the linker pulls the entire module
// for any import from it. Future: trace internal call graph for finer
// tree shaking, or defer to the WASM runtime's optimizer.
//
// ## Linking steps
//
//   1. Parse all WASM fragments (wasmparser)
//   2. Scan imports — identify `@fink/` dependencies, build link set
//   3. Unify type sections:
//      - Canonical types (from types.wat) → emitted once
//      - Module-internal types → namespaced (e.g. `@fink/runtime/list:Cons`)
//      - Build old-index → new-index remap tables per fragment
//   4. Merge function sections:
//      - Resolve import references → defined function indices
//      - Namespace internal function names per module
//      - Build function index remap tables per fragment
//   5. Merge code sections:
//      - Rewrite type and function index references in instructions
//   6. Merge name sections:
//      - Combine debug names from all fragments, preserving namespaces
//      - Name format: `@fink/runtime/list:list_append` (free-form UTF-8)
//   7. Adjust DWARF:
//      - Runtime fragments (hand-written WAT) carry no DWARF
//      - User code DWARF offsets adjusted by prepended runtime code size
//   8. Emit single WASM binary (wasm-encoder)
//
// ## Name section conventions
//
// WASM name section entries are free-form UTF-8 strings. The linker uses
// module-qualified names for all merged items:
//
//   @fink/runtime/types:Num        — shared type
//   @fink/runtime/list:list_append — runtime function
//   @fink/runtime/hamt:_hash       — runtime internal function
//
// These names appear in WAT disassembly and debug tools. User-defined
// functions keep their original names without a module prefix.

use std::collections::BTreeMap;

use gimli::LittleEndian;
use wasmparser::{Payload, Parser};

// -- Public API ---------------------------------------------------------------

/// A WASM fragment to be linked.
pub struct LinkInput {
    /// Module name for namespacing (e.g. `"@fink/runtime/list"`).
    /// Empty string for user code (no prefix applied).
    pub module_name: String,
    /// Raw WASM bytes.
    pub wasm: Vec<u8>,
}

/// Result of linking.
pub struct LinkResult {
    /// The merged WASM binary.
    pub wasm: Vec<u8>,
}

/// Link a set of WASM fragments into a single standalone module.
///
/// Fragments are processed in order: earlier fragments' types and functions
/// get lower indices. The convention is types.wat first, then runtime modules,
/// then user code last (so DWARF adjustment is straightforward).
pub fn link(inputs: &[LinkInput]) -> LinkResult {
    let fragments: Vec<ParsedFragment> = inputs
        .iter()
        .map(|input| parse_fragment(&input.module_name, &input.wasm))
        .collect();

    let mut ctx = LinkContext::new();

    // Step 1: Merge types from all fragments.
    merge_types(&mut ctx, &fragments);

    // Step 2: Merge functions — resolve @fink/ imports, assign indices.
    merge_functions(&mut ctx, &fragments);

    // Step 3: Merge code — rewrite index references.
    merge_code(&mut ctx, &fragments);

    // Step 4: Merge globals.
    merge_globals(&mut ctx, &fragments);

    // Step 5: Merge exports (strip @fink/ internal imports from output).
    merge_exports(&mut ctx, &fragments);

    // Step 6: Emit final module.
    emit_module(&ctx)
}


// -- Parsed representation ----------------------------------------------------

/// Import entry from a fragment.
struct ParsedImport {
    module: String,
    name: String,
    /// True if module starts with `@fink/` — resolved by linker, not kept.
    is_fink: bool,
}

/// Exported item from a fragment.
struct ParsedExport {
    name: String,
    kind: wasmparser::ExternalKind,
    /// Index in the fragment's local index space.
    index: u32,
}

/// A parsed global from a fragment.
struct ParsedGlobal {
    ty: wasmparser::GlobalType,
}

/// Parsed name section data.
struct ParsedNames {
    func_names: BTreeMap<u32, String>,
    local_names: BTreeMap<u32, BTreeMap<u32, String>>,
    global_names: BTreeMap<u32, String>,
    type_names: BTreeMap<u32, String>,
}

/// A fully parsed WASM fragment ready for linking.
struct ParsedFragment {
    module_name: String,
    /// The original WASM bytes (needed for code body re-parsing).
    wasm: Vec<u8>,
    /// Type section entries (raw rec group bytes for re-encoding).
    type_count: u32,
    /// Raw type section bytes for re-encoding via RawSection.
    type_section_bytes: Option<Vec<u8>>,
    /// Imports (both @fink/ and external).
    imports: Vec<ParsedImport>,
    /// Number of imported functions (offsets defined function indices).
    import_func_count: u32,
    /// Function declarations: type index per defined function.
    func_type_indices: Vec<u32>,
    /// Code body ranges into `wasm` — (start, end) per defined function.
    code_body_ranges: Vec<(usize, usize)>,
    /// Exports.
    exports: Vec<ParsedExport>,
    /// Globals.
    globals: Vec<ParsedGlobal>,
    /// Name section data.
    names: ParsedNames,
    /// DWARF custom sections: (section_name, data).
    dwarf_sections: Vec<(String, Vec<u8>)>,
    /// Element section entries (declarative ref.func indices).
    elem_func_indices: Vec<u32>,
}


// -- Parsing ------------------------------------------------------------------

fn parse_fragment(module_name: &str, wasm: &[u8]) -> ParsedFragment {
    let mut frag = ParsedFragment {
        module_name: module_name.to_string(),
        wasm: wasm.to_vec(),
        type_count: 0,
        type_section_bytes: None,
        imports: Vec::new(),
        import_func_count: 0,
        func_type_indices: Vec::new(),
        code_body_ranges: Vec::new(),
        exports: Vec::new(),
        globals: Vec::new(),
        names: ParsedNames {
            func_names: BTreeMap::new(),
            local_names: BTreeMap::new(),
            global_names: BTreeMap::new(),
            type_names: BTreeMap::new(),
        },
        dwarf_sections: Vec::new(),
        elem_func_indices: Vec::new(),
    };

    for payload in Parser::new(0).parse_all(wasm) {
        let payload = match payload {
            Ok(p) => p,
            Err(_) => continue,
        };
        match payload {
            Payload::TypeSection(reader) => {
                // Count types and store raw section bytes for re-encoding.
                let range = reader.range();
                frag.type_section_bytes =
                    Some(wasm[range.start..range.end].to_vec());

                let mut count = 0u32;
                for rg in reader.into_iter().flatten() {
                    for _ in rg.into_types() {
                        count += 1;
                    }
                }
                frag.type_count = count;
            }

            Payload::ImportSection(reader) => {
                for imports_group in reader {
                    let imports_group = match imports_group {
                        Ok(g) => g,
                        Err(_) => continue,
                    };
                    match imports_group {
                        wasmparser::Imports::Single(_, import) => {
                            let is_fink =
                                import.module.starts_with("@fink/");
                            if matches!(
                                import.ty,
                                wasmparser::TypeRef::Func(_)
                            ) {
                                frag.import_func_count += 1;
                            }
                            frag.imports.push(ParsedImport {
                                module: import.module.to_string(),
                                name: import.name.to_string(),
                                is_fink,
                            });
                        }
                        wasmparser::Imports::Compact1 {
                            module: mod_name,
                            items,
                        } => {
                            for item in items.into_iter().flatten() {
                                let is_fink =
                                    mod_name.starts_with("@fink/");
                                if matches!(
                                    item.ty,
                                    wasmparser::TypeRef::Func(_)
                                ) {
                                    frag.import_func_count += 1;
                                }
                                frag.imports.push(ParsedImport {
                                    module: mod_name.to_string(),
                                    name: item.name.to_string(),
                                    is_fink,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }

            Payload::FunctionSection(reader) => {
                for type_idx in reader.into_iter().flatten() {
                    frag.func_type_indices.push(type_idx);
                }
            }

            Payload::CodeSectionEntry(body) => {
                // Store the range into the original WASM bytes.
                let range = body.range();
                frag.code_body_ranges.push((range.start, range.end));
            }

            Payload::ExportSection(reader) => {
                for export in reader.into_iter().flatten() {
                    frag.exports.push(ParsedExport {
                        name: export.name.to_string(),
                        kind: export.kind,
                        index: export.index,
                    });
                }
            }

            Payload::GlobalSection(reader) => {
                for global in reader.into_iter().flatten() {
                    frag.globals.push(ParsedGlobal {
                        ty: global.ty,
                    });
                }
            }

            Payload::ElementSection(reader) => {
                for elem in reader.into_iter().flatten() {
                    if let wasmparser::ElementItems::Functions(funcs) = elem.items {
                        for func_idx in funcs.into_iter().flatten() {
                            frag.elem_func_indices.push(func_idx);
                        }
                    }
                }
            }

            Payload::CustomSection(reader) => {
                let name = reader.name();
                if let wasmparser::KnownCustom::Name(name_reader) =
                    reader.as_known()
                {
                    parse_name_section(&mut frag.names, name_reader);
                } else if name.starts_with(".debug_") {
                    frag.dwarf_sections
                        .push((name.to_string(), reader.data().to_vec()));
                }
            }

            _ => {}
        }
    }

    frag
}

fn parse_name_section(
    names: &mut ParsedNames,
    reader: wasmparser::NameSectionReader,
) {
    for name in reader.into_iter().flatten() {
        match name {
            wasmparser::Name::Function(map) => {
                for n in map.into_iter().flatten() {
                    names.func_names.insert(n.index, n.name.to_string());
                }
            }
            wasmparser::Name::Local(indirect) => {
                for ind in indirect.into_iter().flatten() {
                    let locals: BTreeMap<u32, String> = ind
                        .names
                        .into_iter()
                        .flatten()
                        .map(|n| (n.index, n.name.to_string()))
                        .collect();
                    names.local_names.insert(ind.index, locals);
                }
            }
            wasmparser::Name::Global(map) => {
                for n in map.into_iter().flatten() {
                    names.global_names.insert(n.index, n.name.to_string());
                }
            }
            wasmparser::Name::Type(map) => {
                for n in map.into_iter().flatten() {
                    names.type_names.insert(n.index, n.name.to_string());
                }
            }
            _ => {}
        }
    }
}


// -- Link context -------------------------------------------------------------

/// Per-fragment index remap table.
struct RemapTable {
    types: BTreeMap<u32, u32>,
    funcs: BTreeMap<u32, u32>,
    globals: BTreeMap<u32, u32>,
}

/// Accumulates merged output during linking.
struct LinkContext {
    /// Remap tables, one per input fragment (same order as inputs).
    remaps: Vec<RemapTable>,

    // Counters for assigning new indices.
    next_type_idx: u32,
    next_func_idx: u32,
    next_global_idx: u32,

    // Accumulated type section bytes (from all fragments).
    // We collect raw type section content and re-emit.
    type_sections: Vec<Vec<u8>>,

    // Accumulated function declarations: new_type_idx per defined function.
    func_decls: Vec<u32>,

    // Accumulated code bodies (raw, to be rewritten).
    // (fragment_index, is_user_code, raw_body)
    code_bodies: Vec<(usize, bool, Vec<u8>)>,

    // Merged exports.
    exports: Vec<(String, wasmparser::ExternalKind, u32)>,

    // Merged globals (simplified).
    globals: Vec<(wasmparser::GlobalType, Vec<u8>)>, // (type, raw_init_expr)

    // Merged element section func indices.
    elem_func_indices: Vec<u32>,

    // Merged name maps.
    func_names: BTreeMap<u32, String>,
    local_names: BTreeMap<u32, BTreeMap<u32, String>>,
    global_names: BTreeMap<u32, String>,
    type_names: BTreeMap<u32, String>,

    // DWARF sections from user code fragment (adjusted before emit).
    dwarf_sections: Vec<(String, Vec<u8>)>,

    // Build an export lookup: module_name → export_name → new_func_idx.
    // Populated during merge_functions for import resolution.
    export_map: BTreeMap<String, BTreeMap<String, u32>>,
}

impl LinkContext {
    fn new() -> Self {
        Self {
            remaps: Vec::new(),
            next_type_idx: 0,
            next_func_idx: 0,
            next_global_idx: 0,
            type_sections: Vec::new(),
            func_decls: Vec::new(),
            code_bodies: Vec::new(),
            exports: Vec::new(),
            globals: Vec::new(),
            elem_func_indices: Vec::new(),
            func_names: BTreeMap::new(),
            local_names: BTreeMap::new(),
            global_names: BTreeMap::new(),
            type_names: BTreeMap::new(),
            dwarf_sections: Vec::new(),
            export_map: BTreeMap::new(),
        }
    }
}


// -- Merge steps --------------------------------------------------------------

fn merge_types(ctx: &mut LinkContext, fragments: &[ParsedFragment]) {
    for (i, frag) in fragments.iter().enumerate() {
        // Ensure we have a remap table for this fragment.
        while ctx.remaps.len() <= i {
            ctx.remaps.push(RemapTable {
                types: BTreeMap::new(),
                funcs: BTreeMap::new(),
                globals: BTreeMap::new(),
            });
        }

        let base = ctx.next_type_idx;

        // Map each type in this fragment to a new index.
        for old_idx in 0..frag.type_count {
            let new_idx = base + old_idx;
            ctx.remaps[i].types.insert(old_idx, new_idx);
        }
        ctx.next_type_idx += frag.type_count;

        // Collect raw type section bytes for re-encoding.
        if let Some(ref bytes) = frag.type_section_bytes {
            ctx.type_sections.push(bytes.clone());
        }

        // Merge type names with namespace prefix.
        for (&old_idx, name) in &frag.names.type_names {
            let new_idx = ctx.remaps[i].types[&old_idx];
            let prefixed = if frag.module_name.is_empty() {
                name.clone()
            } else {
                format!("{}:{}", frag.module_name, name)
            };
            ctx.type_names.insert(new_idx, prefixed);
        }
    }
}

fn merge_functions(ctx: &mut LinkContext, fragments: &[ParsedFragment]) {
    // First pass: assign indices to all defined functions and build export map.
    for (i, frag) in fragments.iter().enumerate() {
        let base = ctx.next_func_idx;

        for (local_idx, &type_idx) in frag.func_type_indices.iter().enumerate() {
            let old_func_idx = frag.import_func_count + local_idx as u32;
            let new_func_idx = base + local_idx as u32;
            let new_type_idx = ctx.remaps[i].types[&type_idx];

            ctx.remaps[i].funcs.insert(old_func_idx, new_func_idx);
            ctx.func_decls.push(new_type_idx);

            // Merge function names.
            if let Some(name) = frag.names.func_names.get(&old_func_idx) {
                let prefixed = if frag.module_name.is_empty() {
                    name.clone()
                } else {
                    format!("{}:{}", frag.module_name, name)
                };
                ctx.func_names.insert(new_func_idx, prefixed);
            }

            // Merge local names.
            if let Some(locals) = frag.names.local_names.get(&old_func_idx) {
                ctx.local_names.insert(new_func_idx, locals.clone());
            }
        }

        ctx.next_func_idx += frag.func_type_indices.len() as u32;

        // Build export map for this fragment (for import resolution).
        let mut module_exports = BTreeMap::new();
        for export in &frag.exports {
            if matches!(export.kind, wasmparser::ExternalKind::Func) {
                let new_idx = ctx.remaps[i].funcs[&export.index];
                module_exports.insert(export.name.clone(), new_idx);
            }
        }
        if !frag.module_name.is_empty() {
            ctx.export_map
                .insert(frag.module_name.clone(), module_exports);
        }
    }

    // Second pass: resolve @fink/ imports.
    for (i, frag) in fragments.iter().enumerate() {
        for (import_idx, import) in frag.imports.iter().enumerate() {
            if !import.is_fink {
                continue;
            }
            // Look up the target module's exports.
            if let Some(module_exports) = ctx.export_map.get(&import.module) {
                // Wildcard import ("*") — skip resolution, the import was
                // just a dependency marker. Specific imports resolve by name.
                if import.name != "*"
                    && let Some(&new_func_idx) =
                        module_exports.get(&import.name)
                {
                    let old_import_idx = import_idx as u32;
                    ctx.remaps[i]
                        .funcs
                        .insert(old_import_idx, new_func_idx);
                }
            }
        }
    }
}

fn merge_code(ctx: &mut LinkContext, fragments: &[ParsedFragment]) {
    for (i, frag) in fragments.iter().enumerate() {
        let is_user = frag.module_name.is_empty();
        for &(start, end) in &frag.code_body_ranges {
            ctx.code_bodies
                .push((i, is_user, frag.wasm[start..end].to_vec()));
        }
    }
}

fn merge_globals(ctx: &mut LinkContext, fragments: &[ParsedFragment]) {
    for (i, frag) in fragments.iter().enumerate() {
        for (local_idx, global) in frag.globals.iter().enumerate() {
            let old_idx = local_idx as u32;
            let new_idx = ctx.next_global_idx;
            ctx.remaps[i].globals.insert(old_idx, new_idx);
            ctx.next_global_idx += 1;

            // We'll handle init expression encoding during emit.
            // For now store the raw type.
            ctx.globals.push((global.ty, Vec::new()));

            // Merge global names.
            if let Some(name) = frag.names.global_names.get(&old_idx) {
                let prefixed = if frag.module_name.is_empty() {
                    name.clone()
                } else {
                    format!("{}:{}", frag.module_name, name)
                };
                ctx.global_names.insert(new_idx, prefixed);
            }
        }

        // Merge element indices.
        for &func_idx in &frag.elem_func_indices {
            if let Some(&new_idx) = ctx.remaps[i].funcs.get(&func_idx) {
                ctx.elem_func_indices.push(new_idx);
            }
        }

        // Collect DWARF from user code fragment (module_name is empty).
        if frag.module_name.is_empty() {
            ctx.dwarf_sections.extend(frag.dwarf_sections.clone());
        }
    }
}

fn merge_exports(ctx: &mut LinkContext, fragments: &[ParsedFragment]) {
    for (i, frag) in fragments.iter().enumerate() {
        for export in &frag.exports {
            let new_idx = match export.kind {
                wasmparser::ExternalKind::Func => {
                    ctx.remaps[i].funcs.get(&export.index).copied()
                }
                wasmparser::ExternalKind::Global => {
                    ctx.remaps[i].globals.get(&export.index).copied()
                }
                _ => Some(export.index),
            };
            if let Some(idx) = new_idx {
                ctx.exports.push((export.name.clone(), export.kind, idx));
            }
        }
    }
}


// -- Code rewriting -----------------------------------------------------------

/// Rewrite a single function body, remapping type and function indices.
///
/// Strategy: use wasmparser's offset tracking to identify instruction
/// boundaries. For operators that carry indices, parse and re-encode
/// with remapped values. For all others, copy raw bytes verbatim.
/// This handles any WASM instruction without needing an exhaustive match.
fn rewrite_body(raw: &[u8], remap: &RemapTable) -> Vec<u8> {
    use wasmparser::{BinaryReader, FunctionBody, Operator};

    let reader = BinaryReader::new(raw, 0);
    let body = FunctionBody::new(reader);

    let locals: Vec<wasm_encoder::ValType> = parse_locals(&body)
        .into_iter()
        .map(|vt| convert_val_type_remapped(vt, remap))
        .collect();
    let mut func = wasm_encoder::Function::new_with_locals_types(locals);

    let ops = body.get_operators_reader().unwrap();
    let mut prev_pos = ops.original_position();
    let mut ops_iter = ops;

    while !ops_iter.eof() {
        let op = match ops_iter.read() {
            Ok(op) => op,
            Err(_) => break,
        };
        let cur_pos = ops_iter.original_position();
        let instr_start = prev_pos;
        let instr_end = cur_pos;

        let needs_rewrite = matches!(
            op,
            // Function index
            Operator::Call { .. }
            | Operator::ReturnCall { .. }
            | Operator::RefFunc { .. }
            // Type index
            | Operator::ReturnCallRef { .. }
            | Operator::CallRef { .. }
            | Operator::StructNew { .. }
            | Operator::StructNewDefault { .. }
            | Operator::StructGet { .. }
            | Operator::StructSet { .. }
            | Operator::ArrayNew { .. }
            | Operator::ArrayNewDefault { .. }
            | Operator::ArrayNewFixed { .. }
            | Operator::ArrayGet { .. }
            | Operator::ArrayGetS { .. }
            | Operator::ArrayGetU { .. }
            | Operator::ArraySet { .. }
            | Operator::ArrayCopy { .. }
            // Heap type
            | Operator::RefNull { .. }
            | Operator::RefCastNonNull { .. }
            | Operator::RefCastNullable { .. }
            | Operator::RefTestNonNull { .. }
            | Operator::RefTestNullable { .. }
            // Cast with ref types
            | Operator::BrOnCast { .. }
            | Operator::BrOnCastFail { .. }
            // Global index
            | Operator::GlobalGet { .. }
            | Operator::GlobalSet { .. }
            // Block types (may reference type indices)
            | Operator::Block { .. }
            | Operator::Loop { .. }
            | Operator::If { .. }
            // Call indirect
            | Operator::CallIndirect { .. }
            | Operator::ReturnCallIndirect { .. }
        );

        if !needs_rewrite {
            // Copy raw bytes verbatim — no index references to remap.
            func.raw(raw[instr_start..instr_end].iter().copied());
        } else {
            rewrite_indexed_op(&mut func, op, remap);
        }

        prev_pos = cur_pos;
    }

    func.into_raw_body()
}

/// Re-encode a single operator that carries index references.
fn rewrite_indexed_op(
    func: &mut wasm_encoder::Function,
    op: wasmparser::Operator,
    remap: &RemapTable,
) {
    use wasm_encoder::Instruction as I;
    use wasmparser::Operator as O;

    match op {
        // -- Function index --
        O::Call { function_index } => {
            func.instruction(&I::Call(remap_func(remap, function_index)));
        }
        O::ReturnCall { function_index } => {
            func.instruction(&I::ReturnCall(remap_func(remap, function_index)));
        }
        O::RefFunc { function_index } => {
            func.instruction(&I::RefFunc(remap_func(remap, function_index)));
        }

        // -- Type index --
        O::ReturnCallRef { type_index } => {
            func.instruction(&I::ReturnCallRef(remap_type(remap, type_index)));
        }
        O::CallRef { type_index } => {
            func.instruction(&I::CallRef(remap_type(remap, type_index)));
        }
        O::StructNew { struct_type_index } => {
            func.instruction(&I::StructNew(remap_type(remap, struct_type_index)));
        }
        O::StructNewDefault { struct_type_index } => {
            func.instruction(&I::StructNewDefault(remap_type(remap, struct_type_index)));
        }
        O::StructGet { struct_type_index, field_index } => {
            func.instruction(&I::StructGet {
                struct_type_index: remap_type(remap, struct_type_index),
                field_index,
            });
        }
        O::StructSet { struct_type_index, field_index } => {
            func.instruction(&I::StructSet {
                struct_type_index: remap_type(remap, struct_type_index),
                field_index,
            });
        }
        O::ArrayNew { array_type_index } => {
            func.instruction(&I::ArrayNew(remap_type(remap, array_type_index)));
        }
        O::ArrayNewDefault { array_type_index } => {
            func.instruction(&I::ArrayNewDefault(remap_type(remap, array_type_index)));
        }
        O::ArrayNewFixed { array_type_index, array_size } => {
            func.instruction(&I::ArrayNewFixed {
                array_type_index: remap_type(remap, array_type_index),
                array_size,
            });
        }
        O::ArrayGet { array_type_index } => {
            func.instruction(&I::ArrayGet(remap_type(remap, array_type_index)));
        }
        O::ArrayGetS { array_type_index } => {
            func.instruction(&I::ArrayGetS(remap_type(remap, array_type_index)));
        }
        O::ArrayGetU { array_type_index } => {
            func.instruction(&I::ArrayGetU(remap_type(remap, array_type_index)));
        }
        O::ArraySet { array_type_index } => {
            func.instruction(&I::ArraySet(remap_type(remap, array_type_index)));
        }
        O::ArrayCopy { array_type_index_dst, array_type_index_src } => {
            func.instruction(&I::ArrayCopy {
                array_type_index_dst: remap_type(remap, array_type_index_dst),
                array_type_index_src: remap_type(remap, array_type_index_src),
            });
        }

        // -- Heap type --
        O::RefNull { hty } => {
            func.instruction(&I::RefNull(remap_heap_type(remap, hty)));
        }
        O::RefCastNonNull { hty } => {
            func.instruction(&I::RefCastNonNull(remap_heap_type(remap, hty)));
        }
        O::RefCastNullable { hty } => {
            func.instruction(&I::RefCastNullable(remap_heap_type(remap, hty)));
        }
        O::RefTestNonNull { hty } => {
            func.instruction(&I::RefTestNonNull(remap_heap_type(remap, hty)));
        }
        O::RefTestNullable { hty } => {
            func.instruction(&I::RefTestNullable(remap_heap_type(remap, hty)));
        }

        // -- Cast with ref types --
        O::BrOnCast { relative_depth, from_ref_type, to_ref_type } => {
            func.instruction(&I::BrOnCast {
                relative_depth,
                from_ref_type: remap_ref_type(remap, from_ref_type),
                to_ref_type: remap_ref_type(remap, to_ref_type),
            });
        }
        O::BrOnCastFail { relative_depth, from_ref_type, to_ref_type } => {
            func.instruction(&I::BrOnCastFail {
                relative_depth,
                from_ref_type: remap_ref_type(remap, from_ref_type),
                to_ref_type: remap_ref_type(remap, to_ref_type),
            });
        }

        // -- Global index --
        O::GlobalGet { global_index } => {
            func.instruction(&I::GlobalGet(remap_global(remap, global_index)));
        }
        O::GlobalSet { global_index } => {
            func.instruction(&I::GlobalSet(remap_global(remap, global_index)));
        }

        // -- Block types --
        O::Block { blockty } => {
            func.instruction(&I::Block(remap_block_type(remap, blockty)));
        }
        O::Loop { blockty } => {
            func.instruction(&I::Loop(remap_block_type(remap, blockty)));
        }
        O::If { blockty } => {
            func.instruction(&I::If(remap_block_type(remap, blockty)));
        }

        // -- Call indirect --
        O::CallIndirect { type_index, table_index, .. } => {
            func.instruction(&I::CallIndirect {
                type_index: remap_type(remap, type_index),
                table_index,
            });
        }
        O::ReturnCallIndirect { type_index, table_index } => {
            func.instruction(&I::ReturnCallIndirect {
                type_index: remap_type(remap, type_index),
                table_index,
            });
        }

        _ => unreachable!("needs_rewrite guard should prevent this"),
    };
}

fn parse_locals(body: &wasmparser::FunctionBody) -> Vec<wasmparser::ValType> {
    let mut locals = Vec::new();
    if let Ok(reader) = body.get_locals_reader() {
        for local in reader.into_iter().flatten() {
            let (count, ty) = local;
            for _ in 0..count {
                locals.push(ty);
            }
        }
    }
    locals
}


// -- Index remapping helpers --------------------------------------------------

fn remap_type(remap: &RemapTable, old: u32) -> u32 {
    remap.types.get(&old).copied().unwrap_or(old)
}

fn remap_func(remap: &RemapTable, old: u32) -> u32 {
    remap.funcs.get(&old).copied().unwrap_or(old)
}

fn remap_global(remap: &RemapTable, old: u32) -> u32 {
    remap.globals.get(&old).copied().unwrap_or(old)
}

fn remap_heap_type(
    remap: &RemapTable,
    hty: wasmparser::HeapType,
) -> wasm_encoder::HeapType {
    match hty {
        wasmparser::HeapType::Concrete(idx) | wasmparser::HeapType::Exact(idx) => {
            let old = idx.as_module_index().unwrap_or(0);
            wasm_encoder::HeapType::Concrete(remap_type(remap, old))
        }
        wasmparser::HeapType::Abstract { shared, ty } => {
            wasm_encoder::HeapType::Abstract {
                shared,
                ty: convert_abstract_heap_type(ty),
            }
        }
    }
}

fn remap_ref_type(
    remap: &RemapTable,
    rt: wasmparser::RefType,
) -> wasm_encoder::RefType {
    wasm_encoder::RefType {
        nullable: rt.is_nullable(),
        heap_type: remap_heap_type(remap, rt.heap_type()),
    }
}

fn remap_block_type(
    remap: &RemapTable,
    bt: wasmparser::BlockType,
) -> wasm_encoder::BlockType {
    match bt {
        wasmparser::BlockType::Empty => wasm_encoder::BlockType::Empty,
        wasmparser::BlockType::Type(vt) => {
            wasm_encoder::BlockType::Result(convert_val_type(vt))
        }
        wasmparser::BlockType::FuncType(idx) => {
            wasm_encoder::BlockType::FunctionType(remap_type(remap, idx))
        }
    }
}

fn convert_val_type(vt: wasmparser::ValType) -> wasm_encoder::ValType {
    match vt {
        wasmparser::ValType::I32 => wasm_encoder::ValType::I32,
        wasmparser::ValType::I64 => wasm_encoder::ValType::I64,
        wasmparser::ValType::F32 => wasm_encoder::ValType::F32,
        wasmparser::ValType::F64 => wasm_encoder::ValType::F64,
        wasmparser::ValType::V128 => wasm_encoder::ValType::V128,
        wasmparser::ValType::Ref(rt) => {
            wasm_encoder::ValType::Ref(wasm_encoder::RefType {
                nullable: rt.is_nullable(),
                heap_type: match rt.heap_type() {
                    wasmparser::HeapType::Abstract { shared, ty } => {
                        wasm_encoder::HeapType::Abstract {
                            shared,
                            ty: convert_abstract_heap_type(ty),
                        }
                    }
                    wasmparser::HeapType::Concrete(idx)
                    | wasmparser::HeapType::Exact(idx) => {
                        wasm_encoder::HeapType::Concrete(
                            idx.as_module_index().unwrap_or(0),
                        )
                    }
                },
            })
        }
    }
}

/// Like convert_val_type but applies remap to concrete type indices.
fn convert_val_type_remapped(
    vt: wasmparser::ValType,
    remap: &RemapTable,
) -> wasm_encoder::ValType {
    match vt {
        wasmparser::ValType::Ref(rt) => {
            wasm_encoder::ValType::Ref(remap_ref_type(remap, rt))
        }
        other => convert_val_type(other),
    }
}

fn convert_abstract_heap_type(
    ty: wasmparser::AbstractHeapType,
) -> wasm_encoder::AbstractHeapType {
    match ty {
        wasmparser::AbstractHeapType::Func => {
            wasm_encoder::AbstractHeapType::Func
        }
        wasmparser::AbstractHeapType::Extern => {
            wasm_encoder::AbstractHeapType::Extern
        }
        wasmparser::AbstractHeapType::Any => {
            wasm_encoder::AbstractHeapType::Any
        }
        wasmparser::AbstractHeapType::None => {
            wasm_encoder::AbstractHeapType::None
        }
        wasmparser::AbstractHeapType::NoExtern => {
            wasm_encoder::AbstractHeapType::NoExtern
        }
        wasmparser::AbstractHeapType::NoFunc => {
            wasm_encoder::AbstractHeapType::NoFunc
        }
        wasmparser::AbstractHeapType::Eq => {
            wasm_encoder::AbstractHeapType::Eq
        }
        wasmparser::AbstractHeapType::Struct => {
            wasm_encoder::AbstractHeapType::Struct
        }
        wasmparser::AbstractHeapType::Array => {
            wasm_encoder::AbstractHeapType::Array
        }
        wasmparser::AbstractHeapType::I31 => {
            wasm_encoder::AbstractHeapType::I31
        }
        wasmparser::AbstractHeapType::Exn => {
            wasm_encoder::AbstractHeapType::Exn
        }
        wasmparser::AbstractHeapType::NoExn => {
            wasm_encoder::AbstractHeapType::NoExn
        }
        wasmparser::AbstractHeapType::Cont => {
            wasm_encoder::AbstractHeapType::Cont
        }
        wasmparser::AbstractHeapType::NoCont => {
            wasm_encoder::AbstractHeapType::NoCont
        }
    }
}


// -- Emit final module --------------------------------------------------------

fn emit_module(ctx: &LinkContext) -> LinkResult {
    let mut module = wasm_encoder::Module::new();

    // 1. Type section — re-encode all collected type sections.
    //    We use RawSection to pass through the already-encoded type bytes.
    //    Each fragment's type section is valid WASM encoding; we concatenate
    //    the type entries by re-parsing and re-encoding with remapped
    //    supertype indices.
    //
    //    For now, we use a simpler approach: emit each fragment's type section
    //    as a raw section. This works when there are no cross-fragment
    //    supertype references (which is our current case — subtypes reference
    //    types within their own fragment's rec group).
    if !ctx.type_sections.is_empty() {
        let mut combined = wasm_encoder::TypeSection::new();
        for (frag_idx, raw_bytes) in ctx.type_sections.iter().enumerate() {
            let br = wasmparser::BinaryReader::new(raw_bytes, 0);
            let reader =
                wasmparser::TypeSectionReader::new(br).unwrap();
            let remap = &ctx.remaps[frag_idx];
            reencode_type_section(&mut combined, reader, remap);
        }
        module.section(&combined);
    }

    // 2. Function section.
    if !ctx.func_decls.is_empty() {
        let mut funcs = wasm_encoder::FunctionSection::new();
        for &type_idx in &ctx.func_decls {
            funcs.function(type_idx);
        }
        module.section(&funcs);
    }

    // 3. Global section.
    // (Simplified — only supports ref.func and simple const init exprs.)

    // 4. Export section.
    if !ctx.exports.is_empty() {
        let mut exports = wasm_encoder::ExportSection::new();
        for (name, kind, idx) in &ctx.exports {
            let ek = match kind {
                wasmparser::ExternalKind::Func => {
                    wasm_encoder::ExportKind::Func
                }
                wasmparser::ExternalKind::Global => {
                    wasm_encoder::ExportKind::Global
                }
                wasmparser::ExternalKind::Table => {
                    wasm_encoder::ExportKind::Table
                }
                wasmparser::ExternalKind::Memory => {
                    wasm_encoder::ExportKind::Memory
                }
                wasmparser::ExternalKind::Tag => {
                    wasm_encoder::ExportKind::Tag
                }
                wasmparser::ExternalKind::FuncExact => {
                    wasm_encoder::ExportKind::Func
                }
            };
            exports.export(name, ek, *idx);
        }
        module.section(&exports);
    }

    // 5. Element section (declarative, for ref.func).
    if !ctx.elem_func_indices.is_empty() {
        let mut elems = wasm_encoder::ElementSection::new();
        let indices: Vec<u32> = ctx.elem_func_indices.clone();
        elems.declared(wasm_encoder::Elements::Functions(
            indices.into(),
        ));
        module.section(&elems);
    }

    // 6. Code section — rewrite bodies with remapped indices.
    // Track runtime code byte size for DWARF offset adjustment.
    let mut runtime_code_size = 0u32;
    if !ctx.code_bodies.is_empty() {
        let mut code = wasm_encoder::CodeSection::new();
        for (frag_idx, is_user, raw_body) in &ctx.code_bodies {
            let remap = &ctx.remaps[*frag_idx];
            let rewritten = rewrite_body(raw_body, remap);
            if !is_user {
                runtime_code_size += rewritten.len() as u32;
            }
            code.raw(&rewritten);
        }
        module.section(&code);
    }

    // 7. Name section.
    let has_names = !ctx.func_names.is_empty()
        || !ctx.local_names.is_empty()
        || !ctx.global_names.is_empty()
        || !ctx.type_names.is_empty();
    if has_names {
        let mut names = wasm_encoder::NameSection::new();

        if !ctx.func_names.is_empty() {
            let mut map = wasm_encoder::NameMap::new();
            for (&idx, name) in &ctx.func_names {
                map.append(idx, name);
            }
            names.functions(&map);
        }

        if !ctx.local_names.is_empty() {
            let mut indirect = wasm_encoder::IndirectNameMap::new();
            for (&func_idx, locals) in &ctx.local_names {
                let mut map = wasm_encoder::NameMap::new();
                for (&local_idx, name) in locals {
                    map.append(local_idx, name);
                }
                indirect.append(func_idx, &map);
            }
            names.locals(&indirect);
        }

        if !ctx.global_names.is_empty() {
            let mut map = wasm_encoder::NameMap::new();
            for (&idx, name) in &ctx.global_names {
                map.append(idx, name);
            }
            names.globals(&map);
        }

        if !ctx.type_names.is_empty() {
            let mut map = wasm_encoder::NameMap::new();
            for (&idx, name) in &ctx.type_names {
                map.append(idx, name);
            }
            names.types(&map);
        }

        module.section(&names);
    }

    // 8. DWARF custom sections — adjust addresses by runtime code offset.
    if runtime_code_size > 0 && !ctx.dwarf_sections.is_empty() {
        let adjusted = adjust_dwarf(&ctx.dwarf_sections, runtime_code_size);
        for (name, data) in &adjusted {
            module.section(&wasm_encoder::CustomSection {
                name: std::borrow::Cow::Borrowed(name),
                data: std::borrow::Cow::Borrowed(data),
            });
        }
    } else {
        // No offset needed — pass through unchanged.
        for (name, data) in &ctx.dwarf_sections {
            module.section(&wasm_encoder::CustomSection {
                name: std::borrow::Cow::Borrowed(name),
                data: std::borrow::Cow::Borrowed(data),
            });
        }
    }

    LinkResult {
        wasm: module.finish(),
    }
}


/// Adjust DWARF section addresses by a byte offset.
///
/// Reads the user code's DWARF line program, adds `offset` to every
/// address, and re-serializes all DWARF sections. Non-line sections
/// (.debug_info, .debug_abbrev, .debug_str) are rebuilt from scratch
/// since gimli::write produces a self-consistent set.
fn adjust_dwarf(
    sections: &[(String, Vec<u8>)],
    offset: u32,
) -> Vec<(String, Vec<u8>)> {
    use gimli::EndianSlice;

    // Collect raw section data by name.
    let mut debug_info_data = &[][..];
    let mut debug_abbrev_data = &[][..];
    let mut debug_line_data = &[][..];
    let mut debug_str_data = &[][..];

    for (name, data) in sections {
        match name.as_str() {
            ".debug_info" => debug_info_data = data,
            ".debug_abbrev" => debug_abbrev_data = data,
            ".debug_line" => debug_line_data = data,
            ".debug_str" => debug_str_data = data,
            _ => {}
        }
    }

    // If no line data, just pass through.
    if debug_line_data.is_empty() || debug_info_data.is_empty() {
        return sections.to_vec();
    }

    // Parse DWARF to extract line program rows.
    let debug_info = gimli::DebugInfo::new(debug_info_data, LittleEndian);
    let debug_abbrev =
        gimli::DebugAbbrev::new(debug_abbrev_data, LittleEndian);
    let debug_line = gimli::DebugLine::new(debug_line_data, LittleEndian);
    let _debug_str = gimli::DebugStr::new(debug_str_data, LittleEndian);

    let mut units = debug_info.units();
    let unit_header = match units.next() {
        Ok(Some(h)) => h,
        _ => return sections.to_vec(),
    };
    let abbrevs = match debug_abbrev
        .abbreviations(unit_header.debug_abbrev_offset())
    {
        Ok(a) => a,
        Err(_) => return sections.to_vec(),
    };

    // Extract source file name and producer from root DIE.
    let mut cursor = unit_header.entries(&abbrevs);
    if cursor.next_dfs().is_err() {
        return sections.to_vec();
    }
    let root = match cursor.current() {
        Some(e) => e,
        None => return sections.to_vec(),
    };

    let source_name = root
        .attr_value(gimli::DW_AT_name)
        .and_then(|v| {
            if let gimli::AttributeValue::String(s) = v {
                Some(s.to_string_lossy().to_string())
            } else {
                None
            }
        })
        .unwrap_or_default();

    let stmt_list = match root.attr_value(gimli::DW_AT_stmt_list) {
        Some(gimli::AttributeValue::DebugLineRef(o)) => o,
        _ => return sections.to_vec(),
    };

    let line_program = match debug_line.program(
        stmt_list,
        unit_header.address_size(),
        None::<EndianSlice<'_, LittleEndian>>,
        None::<EndianSlice<'_, LittleEndian>>,
    ) {
        Ok(p) => p,
        Err(_) => return sections.to_vec(),
    };

    // Execute line program, collecting rows with adjusted addresses.
    struct Row {
        address: u64,
        line: u64,
        col: u64,
    }

    let mut rows_data = Vec::new();
    let mut rows = line_program.rows();
    while let Ok(Some((_header, row))) = rows.next_row() {
        if row.end_sequence() {
            continue;
        }
        let line = row.line().map(|l| l.get()).unwrap_or(0);
        if line == 0 {
            continue;
        }
        let col = match row.column() {
            gimli::ColumnType::LeftEdge => 0,
            gimli::ColumnType::Column(c) => c.get(),
        };
        rows_data.push(Row {
            address: row.address() + offset as u64,
            line,
            col,
        });
    }

    // Re-emit DWARF with adjusted addresses using gimli::write.
    let encoding = gimli::Encoding {
        format: gimli::Format::Dwarf32,
        version: 4,
        address_size: 4,
    };

    let line_program_w = gimli::write::LineProgram::new(
        encoding,
        Default::default(),
        gimli::write::LineString::String(b".".to_vec()),
        None,
        gimli::write::LineString::String(source_name.as_bytes().to_vec()),
        None,
    );

    let mut dwarf = gimli::write::DwarfUnit::new(encoding);
    let root_id = dwarf.unit.root();
    dwarf.unit.get_mut(root_id).set(
        gimli::DW_AT_name,
        gimli::write::AttributeValue::String(
            source_name.as_bytes().to_vec(),
        ),
    );
    dwarf.unit.get_mut(root_id).set(
        gimli::DW_AT_producer,
        gimli::write::AttributeValue::String(b"fink".to_vec()),
    );
    dwarf.unit.get_mut(root_id).set(
        gimli::DW_AT_language,
        gimli::write::AttributeValue::Udata(0x0001),
    );
    dwarf.unit.get_mut(root_id).set(
        gimli::DW_AT_stmt_list,
        gimli::write::AttributeValue::LineProgramRef,
    );

    dwarf.unit.line_program = line_program_w;
    let dir_id = dwarf.unit.line_program.default_directory();
    let file_id = dwarf.unit.line_program.add_file(
        gimli::write::LineString::String(
            source_name.as_bytes().to_vec(),
        ),
        dir_id,
        None,
    );

    if !rows_data.is_empty() {
        let lp = &mut dwarf.unit.line_program;
        lp.begin_sequence(Some(gimli::write::Address::Constant(0)));

        for r in &rows_data {
            let row = lp.row();
            row.address_offset = r.address;
            row.file = file_id;
            row.line = r.line;
            row.column = r.col;
            row.is_statement = true;
            lp.generate_row();
        }

        let last_addr = rows_data.last().map(|r| r.address + 1).unwrap_or(1);
        lp.end_sequence(last_addr);
    }

    // Serialize.
    let mut out_sections =
        gimli::write::Sections::new(gimli::write::EndianVec::new(LittleEndian));
    if dwarf.write(&mut out_sections).is_err() {
        return sections.to_vec();
    }

    let mut result = Vec::new();
    let _: Result<(), ()> =
        out_sections.for_each(|section_id, writer| {
            let data = writer.slice();
            if !data.is_empty() {
                result.push((section_id.name().to_string(), data.to_vec()));
            }
            Ok(())
        });

    result
}

/// Re-encode a type section from wasmparser types into wasm-encoder,
/// applying the remap table to all concrete type index references.
fn reencode_type_section(
    types: &mut wasm_encoder::TypeSection,
    reader: wasmparser::TypeSectionReader,
    remap: &RemapTable,
) {
    for rec_group in reader.into_iter().flatten() {
        let sub_types: Vec<_> = rec_group.into_types().collect();
        if sub_types.len() == 1 {
            let st = &sub_types[0];
            types.ty().subtype(&convert_sub_type(st, remap));
        } else {
            let converted: Vec<_> =
                sub_types.iter().map(|st| convert_sub_type(st, remap)).collect();
            types.ty().rec(converted);
        }
    }
}

fn convert_sub_type(
    st: &wasmparser::SubType,
    remap: &RemapTable,
) -> wasm_encoder::SubType {
    wasm_encoder::SubType {
        is_final: st.is_final,
        supertype_idx: st
            .supertype_idx
            .map(|idx| remap_type(remap, idx.as_module_index().unwrap_or(0))),
        composite_type: convert_composite_type(&st.composite_type, remap),
    }
}

fn convert_composite_type(
    ct: &wasmparser::CompositeType,
    remap: &RemapTable,
) -> wasm_encoder::CompositeType {
    wasm_encoder::CompositeType {
        inner: match &ct.inner {
            wasmparser::CompositeInnerType::Func(f) => {
                wasm_encoder::CompositeInnerType::Func(
                    wasm_encoder::FuncType::new(
                        f.params()
                            .iter()
                            .map(|vt| convert_val_type_remapped(*vt, remap))
                            .collect::<Vec<_>>(),
                        f.results()
                            .iter()
                            .map(|vt| convert_val_type_remapped(*vt, remap))
                            .collect::<Vec<_>>(),
                    ),
                )
            }
            wasmparser::CompositeInnerType::Struct(s) => {
                wasm_encoder::CompositeInnerType::Struct(
                    wasm_encoder::StructType {
                        fields: s
                            .fields
                            .iter()
                            .map(|f| convert_field_type(f, remap))
                            .collect(),
                    },
                )
            }
            wasmparser::CompositeInnerType::Array(a) => {
                wasm_encoder::CompositeInnerType::Array(
                    wasm_encoder::ArrayType(convert_field_type(&a.0, remap)),
                )
            }
            wasmparser::CompositeInnerType::Cont(_) => {
                panic!("link: continuation types not supported");
            }
        },
        shared: ct.shared,
        descriptor: None,
        describes: None,
    }
}

fn convert_field_type(
    f: &wasmparser::FieldType,
    remap: &RemapTable,
) -> wasm_encoder::FieldType {
    wasm_encoder::FieldType {
        element_type: match f.element_type {
            wasmparser::StorageType::I8 => wasm_encoder::StorageType::I8,
            wasmparser::StorageType::I16 => wasm_encoder::StorageType::I16,
            wasmparser::StorageType::Val(vt) => {
                wasm_encoder::StorageType::Val(
                    convert_val_type_remapped(vt, remap),
                )
            }
        },
        mutable: f.mutable,
    }
}


// -- Tests --------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile WAT text to WASM bytes.
    fn wat(source: &str) -> Vec<u8> {
        wat_crate::parse_str(source).expect("WAT parse failed")
    }

    /// Validate that WASM bytes are a valid module.
    fn validate(wasm: &[u8]) {
        let mut validator = wasmparser::Validator::new_with_features(
            wasmparser::WasmFeatures::all(),
        );
        validator
            .validate_all(wasm)
            .expect("validation failed");
    }

    /// Extract function names from a WASM module's name section.
    fn get_func_names(wasm: &[u8]) -> BTreeMap<u32, String> {
        let mut names = BTreeMap::new();
        for payload in Parser::new(0).parse_all(wasm) {
            if let Ok(Payload::CustomSection(reader)) = payload {
                if let wasmparser::KnownCustom::Name(name_reader) =
                    reader.as_known()
                {
                    for name in name_reader.into_iter().flatten() {
                        if let wasmparser::Name::Function(map) = name {
                            for n in map.into_iter().flatten() {
                                names
                                    .insert(n.index, n.name.to_string());
                            }
                        }
                    }
                }
            }
        }
        names
    }

    /// Extract type names from a WASM module's name section.
    fn get_type_names(wasm: &[u8]) -> BTreeMap<u32, String> {
        let mut names = BTreeMap::new();
        for payload in Parser::new(0).parse_all(wasm) {
            if let Ok(Payload::CustomSection(reader)) = payload {
                if let wasmparser::KnownCustom::Name(name_reader) =
                    reader.as_known()
                {
                    for name in name_reader.into_iter().flatten() {
                        if let wasmparser::Name::Type(map) = name {
                            for n in map.into_iter().flatten() {
                                names
                                    .insert(n.index, n.name.to_string());
                            }
                        }
                    }
                }
            }
        }
        names
    }

    /// Extract exports from a WASM module.
    fn get_exports(wasm: &[u8]) -> Vec<(String, u32)> {
        let mut exports = Vec::new();
        for payload in Parser::new(0).parse_all(wasm) {
            if let Ok(Payload::ExportSection(reader)) = payload {
                for export in reader.into_iter().flatten() {
                    exports.push((
                        export.name.to_string(),
                        export.index,
                    ));
                }
            }
        }
        exports
    }

    /// Count types in a WASM module.
    fn count_types(wasm: &[u8]) -> u32 {
        let mut count = 0;
        for payload in Parser::new(0).parse_all(wasm) {
            if let Ok(Payload::TypeSection(reader)) = payload {
                for rg in reader.into_iter().flatten() {
                    for _ in rg.into_types() {
                        count += 1;
                    }
                }
            }
        }
        count
    }

    /// Count functions in a WASM module.
    fn count_funcs(wasm: &[u8]) -> u32 {
        let mut count = 0;
        for payload in Parser::new(0).parse_all(wasm) {
            if let Ok(Payload::FunctionSection(reader)) = payload {
                for _ in reader.into_iter().flatten() {
                    count += 1;
                }
            }
        }
        count
    }

    /// Check if DWARF sections exist in a WASM module.
    fn has_dwarf(wasm: &[u8]) -> bool {
        for payload in Parser::new(0).parse_all(wasm) {
            if let Ok(Payload::CustomSection(reader)) = payload {
                if reader.name().starts_with(".debug_") {
                    return true;
                }
            }
        }
        false
    }

    // -- Test cases -----------------------------------------------------------

    #[test]
    fn t_link_single_module_passthrough() {
        // A single module with no imports — output should be valid and
        // functionally identical.
        let wasm_a = wat(
            r#"(module
                (type $f (func (param i32) (result i32)))
                (func $identity (type $f) (param i32) (result i32)
                    local.get 0)
                (export "identity" (func $identity))
            )"#,
        );

        let result = link(&[LinkInput {
            module_name: String::new(),
            wasm: wasm_a,
        }]);

        validate(&result.wasm);
        assert_eq!(count_types(&result.wasm), 1);
        assert_eq!(count_funcs(&result.wasm), 1);

        let exports = get_exports(&result.wasm);
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].0, "identity");
    }

    #[test]
    fn t_link_two_modules_type_merge() {
        // Two modules each define their own types. After linking, all
        // types should be present and indices should be remapped.
        let wasm_a = wat(
            r#"(module
                (type $T0 (func (param i32) (result i32)))
                (func $f (type $T0) (param i32) (result i32) local.get 0)
                (export "f" (func $f))
            )"#,
        );
        let wasm_b = wat(
            r#"(module
                (type $U0 (func (param i32 i32) (result i32)))
                (func $g (type $U0) (param i32) (param i32) (result i32)
                    local.get 0
                    local.get 1
                    i32.add)
                (export "g" (func $g))
            )"#,
        );

        let result = link(&[
            LinkInput {
                module_name: "@fink/runtime/a".into(),
                wasm: wasm_a,
            },
            LinkInput {
                module_name: String::new(),
                wasm: wasm_b,
            },
        ]);

        validate(&result.wasm);
        assert_eq!(count_types(&result.wasm), 2);
        assert_eq!(count_funcs(&result.wasm), 2);

        let exports = get_exports(&result.wasm);
        assert_eq!(exports.len(), 2);
    }

    #[test]
    fn t_link_name_section_preserved() {
        // Function names should survive linking with module prefixes.
        let wasm_a = wat(
            r#"(module
                (func $helper (result i32) i32.const 42)
                (export "helper" (func $helper))
            )"#,
        );
        let wasm_b = wat(
            r#"(module
                (func $main (result i32) i32.const 1)
                (export "main" (func $main))
            )"#,
        );

        let result = link(&[
            LinkInput {
                module_name: "@fink/runtime/lib".into(),
                wasm: wasm_a,
            },
            LinkInput {
                module_name: String::new(),
                wasm: wasm_b,
            },
        ]);

        validate(&result.wasm);
        let names = get_func_names(&result.wasm);

        // Runtime function should be prefixed.
        assert_eq!(
            names.get(&0),
            Some(&"@fink/runtime/lib:helper".to_string())
        );
        // User function should keep its original name.
        assert_eq!(names.get(&1), Some(&"main".to_string()));
    }

    #[test]
    fn t_link_import_resolution() {
        // Module B imports a function from module A via @fink/ convention.
        // The linker should resolve the import and produce a valid module
        // with no imports.
        let wasm_a = wat(
            r#"(module
                (func $add (param i32) (param i32) (result i32)
                    local.get 0
                    local.get 1
                    i32.add)
                (export "add" (func $add))
            )"#,
        );
        let wasm_b = wat(
            r#"(module
                (import "@fink/runtime/math" "add"
                    (func $add (param i32) (param i32) (result i32)))
                (func $main (result i32)
                    i32.const 3
                    i32.const 4
                    call $add)
                (export "main" (func $main))
            )"#,
        );

        let result = link(&[
            LinkInput {
                module_name: "@fink/runtime/math".into(),
                wasm: wasm_a,
            },
            LinkInput {
                module_name: String::new(),
                wasm: wasm_b,
            },
        ]);

        validate(&result.wasm);

        // No imports should remain.
        let mut has_imports = false;
        for payload in Parser::new(0).parse_all(&result.wasm) {
            if let Ok(Payload::ImportSection(_)) = payload {
                has_imports = true;
            }
        }
        assert!(!has_imports, "linked module should have no imports");

        // Should have 2 functions: add + main.
        assert_eq!(count_funcs(&result.wasm), 2);
    }

    #[test]
    fn t_link_gc_types_merged() {
        // Two modules using WasmGC struct types. After linking, types from
        // both should be present and struct.new indices should be correct.
        let wasm_a = wat(
            r#"(module
                (type $Num (struct (field f64)))
                (func $make_num (param f64) (result (ref $Num))
                    local.get 0
                    struct.new $Num)
                (export "make_num" (func $make_num))
            )"#,
        );
        let wasm_b = wat(
            r#"(module
                (type $Pair (struct (field i32) (field i32)))
                (func $make_pair (param i32) (param i32) (result (ref $Pair))
                    local.get 0
                    local.get 1
                    struct.new $Pair)
                (export "make_pair" (func $make_pair))
            )"#,
        );

        let result = link(&[
            LinkInput {
                module_name: "@fink/runtime/types".into(),
                wasm: wasm_a,
            },
            LinkInput {
                module_name: String::new(),
                wasm: wasm_b,
            },
        ]);

        validate(&result.wasm);
        assert_eq!(count_types(&result.wasm), 4); // $Num, make_num_type, $Pair, make_pair_type
        assert_eq!(count_funcs(&result.wasm), 2);
    }

    #[test]
    fn t_link_dwarf_preserved() {
        // DWARF sections from user code should survive linking.
        // We create a module with a fake .debug_info section and verify
        // it appears in the output.
        let wasm_rt = wat(
            r#"(module
                (func $helper (result i32) i32.const 1)
                (export "helper" (func $helper))
            )"#,
        );

        // Build user module with an embedded DWARF section.
        let mut user_wasm = wat(
            r#"(module
                (func $main (result i32) i32.const 42)
                (export "main" (func $main))
            )"#,
        );
        // Manually append a .debug_info custom section.
        append_custom_section(&mut user_wasm, ".debug_info", b"fake_dwarf");

        let result = link(&[
            LinkInput {
                module_name: "@fink/runtime/lib".into(),
                wasm: wasm_rt,
            },
            LinkInput {
                module_name: String::new(),
                wasm: user_wasm,
            },
        ]);

        validate(&result.wasm);
        assert!(has_dwarf(&result.wasm), "DWARF should survive linking");
    }

    #[test]
    fn t_link_element_section_merged() {
        // ref.func requires a declarative element segment. After linking,
        // element indices should be remapped.
        let wasm_a = wat(
            r#"(module
                (type $fn0 (func (result i32)))
                (func $target (type $fn0) (result i32) i32.const 99)
                (func $get_ref (result funcref)
                    ref.func $target)
                (elem declare func $target)
                (export "get_ref" (func $get_ref))
            )"#,
        );

        let result = link(&[LinkInput {
            module_name: String::new(),
            wasm: wasm_a,
        }]);

        validate(&result.wasm);
    }

    /// Append a custom section to WASM bytes.
    fn append_custom_section(wasm: &mut Vec<u8>, name: &str, data: &[u8]) {
        let name_bytes = name.as_bytes();
        let payload_size =
            leb128_size(name_bytes.len() as u32)
            + name_bytes.len()
            + data.len();

        wasm.push(0x00); // custom section id
        leb128_encode(wasm, payload_size as u32);
        leb128_encode(wasm, name_bytes.len() as u32);
        wasm.extend_from_slice(name_bytes);
        wasm.extend_from_slice(data);
    }

    fn leb128_encode(out: &mut Vec<u8>, mut val: u32) {
        loop {
            let byte = (val & 0x7f) as u8;
            val >>= 7;
            if val == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
    }

    fn leb128_size(mut val: u32) -> usize {
        let mut size = 0;
        loop {
            val >>= 7;
            size += 1;
            if val == 0 {
                break;
            }
        }
        size
    }

    /// Extract DWARF line program addresses from a WASM module.
    fn get_dwarf_addresses(wasm: &[u8]) -> Vec<u32> {
        use gimli::EndianSlice;

        let mut debug_info_data = &[][..];
        let mut debug_abbrev_data = &[][..];
        let mut debug_line_data = &[][..];

        for payload in Parser::new(0).parse_all(wasm) {
            if let Ok(Payload::CustomSection(reader)) = payload {
                match reader.name() {
                    ".debug_info" => debug_info_data = reader.data(),
                    ".debug_abbrev" => debug_abbrev_data = reader.data(),
                    ".debug_line" => debug_line_data = reader.data(),
                    _ => {}
                }
            }
        }

        if debug_info_data.is_empty() || debug_line_data.is_empty() {
            return Vec::new();
        }

        let debug_info =
            gimli::DebugInfo::new(debug_info_data, LittleEndian);
        let debug_abbrev =
            gimli::DebugAbbrev::new(debug_abbrev_data, LittleEndian);
        let debug_line =
            gimli::DebugLine::new(debug_line_data, LittleEndian);

        let mut units = debug_info.units();
        let unit_header = match units.next() {
            Ok(Some(h)) => h,
            _ => return Vec::new(),
        };
        let abbrevs = match debug_abbrev
            .abbreviations(unit_header.debug_abbrev_offset())
        {
            Ok(a) => a,
            Err(_) => return Vec::new(),
        };

        let mut cursor = unit_header.entries(&abbrevs);
        if cursor.next_dfs().is_err() {
            return Vec::new();
        }
        let root = match cursor.current() {
            Some(e) => e,
            None => return Vec::new(),
        };

        let stmt_list = match root.attr_value(gimli::DW_AT_stmt_list) {
            Some(gimli::AttributeValue::DebugLineRef(o)) => o,
            _ => return Vec::new(),
        };

        let line_program = match debug_line.program(
            stmt_list,
            unit_header.address_size(),
            None::<EndianSlice<'_, LittleEndian>>,
            None::<EndianSlice<'_, LittleEndian>>,
        ) {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };

        let mut addrs = Vec::new();
        let mut rows = line_program.rows();
        while let Ok(Some((_header, row))) = rows.next_row() {
            if !row.end_sequence() {
                addrs.push(row.address() as u32);
            }
        }
        addrs
    }

    #[test]
    fn t_link_dwarf_offset_adjusted() {
        // DWARF addresses should be shifted by the size of prepended
        // runtime code bodies.
        use crate::passes::wasm::dwarf;
        use crate::passes::wasm::emit::OffsetMapping;
        use crate::passes::ast::lexer::{Loc, Pos};

        let wasm_rt = wat(
            r#"(module
                (func $helper (result i32) i32.const 1)
                (func $helper2 (result i32) i32.const 2)
                (export "helper" (func $helper))
            )"#,
        );

        // Build user module with real DWARF.
        let mut user_wasm = wat(
            r#"(module
                (func $main (result i32) i32.const 42)
                (export "main" (func $main))
            )"#,
        );

        // Create DWARF with known addresses.
        let mappings = vec![
            OffsetMapping {
                wasm_offset: 10,
                loc: Loc {
                    start: Pos {
                        idx: 0,
                        line: 1,
                        col: 1,
                    },
                    end: Pos {
                        idx: 0,
                        line: 1,
                        col: 5,
                    },
                },
            },
            OffsetMapping {
                wasm_offset: 20,
                loc: Loc {
                    start: Pos {
                        idx: 0,
                        line: 2,
                        col: 1,
                    },
                    end: Pos {
                        idx: 0,
                        line: 2,
                        col: 10,
                    },
                },
            },
        ];

        let dwarf_sections = dwarf::emit_dwarf("test.fnk", None, &mappings);
        for section in &dwarf_sections {
            append_custom_section(
                &mut user_wasm,
                &section.name,
                &section.data,
            );
        }

        // Get original addresses.
        let original_addrs = get_dwarf_addresses(&user_wasm);
        assert_eq!(original_addrs, vec![10, 20]);

        // Link with runtime prepended.
        let result = link(&[
            LinkInput {
                module_name: "@fink/runtime/lib".into(),
                wasm: wasm_rt,
            },
            LinkInput {
                module_name: String::new(),
                wasm: user_wasm,
            },
        ]);

        validate(&result.wasm);

        // DWARF addresses should be shifted by runtime code size.
        let adjusted_addrs = get_dwarf_addresses(&result.wasm);
        assert!(
            !adjusted_addrs.is_empty(),
            "should have DWARF addresses"
        );
        // All addresses should be greater than originals.
        assert!(
            adjusted_addrs[0] > 10,
            "first address {} should be shifted past 10",
            adjusted_addrs[0]
        );
        assert!(
            adjusted_addrs[1] > 20,
            "second address {} should be shifted past 20",
            adjusted_addrs[1]
        );
        // The offset between the two should be preserved.
        assert_eq!(
            adjusted_addrs[1] - adjusted_addrs[0],
            10,
            "relative offset between addresses should be preserved"
        );
    }
}
