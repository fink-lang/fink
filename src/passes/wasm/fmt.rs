// Custom WASM→WAT formatter with source map support.
//
// Reads a WASM binary (with name section and DWARF) and produces
// WAT text + optional Source Map v3. This is the read-side counterpart
// to wasm/emit.rs (the write side).
//
// The formatter reconstructs nested WAT s-expressions from the flat
// WASM stack machine instructions. It uses the name section for
// human-readable identifiers and DWARF .debug_line for source mapping.

use std::collections::{BTreeMap, HashMap};

use wasmparser::{
  ExternalKind, Operator, Parser, Payload,
  SubType, CompositeInnerType,
};

use crate::lexer::{Loc, Pos};
use crate::sourcemap::{MappedWriter, SourceMap};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Format a WASM binary as WAT text.
pub fn format(wasm: &[u8]) -> String {
  let module = parse_module(wasm);
  let mut w = MappedWriter::new();
  emit_wat(&module, &mut w);
  w.finish_string()
}

/// Format a WASM binary as WAT text with source map.
pub fn format_mapped(
  wasm: &[u8],
  source_name: &str,
  source_content: &str,
) -> (String, SourceMap) {
  format_mapped_with_locs(wasm, &[], source_name, source_content)
}

/// Format with structural source locations from the emitter.
pub fn format_mapped_with_locs(
  wasm: &[u8],
  structural_locs: &[super::emit::StructuralLoc],
  source_name: &str,
  source_content: &str,
) -> (String, SourceMap) {
  let mut module = parse_module(wasm);
  // Inject structural locs into the parsed module.
  for sl in structural_locs {
    module.structural_locs.push(sl.clone());
  }
  let mut w = MappedWriter::new();
  emit_wat(&module, &mut w);
  w.finish_with_content(source_name, source_content)
}

// ---------------------------------------------------------------------------
// Parsed module representation
// ---------------------------------------------------------------------------

struct ParsedModule {
  /// Type section entries.
  types: Vec<ParsedType>,
  /// Imported functions: (module, name, type_index).
  imports: Vec<(String, String, u32)>,
  /// Defined functions.
  funcs: Vec<ParsedFunc>,
  /// Globals: (type_index for ref type, init func index).
  globals: Vec<ParsedGlobal>,
  /// Exports: (name, kind, index).
  exports: Vec<(String, ExternalKind, u32)>,
  /// Function names from name section.
  func_names: HashMap<u32, String>,
  /// Local names from name section: (func_idx, local_idx) → name.
  local_names: HashMap<(u32, u32), String>,
  /// Global names from name section.
  global_names: HashMap<u32, String>,
  /// DWARF offset → source location mappings.
  dwarf_locs: BTreeMap<u32, Loc>,
  /// Number of imported functions (offset for defined func indices).
  import_func_count: u32,
  /// Structural source locations from the emitter.
  structural_locs: Vec<super::emit::StructuralLoc>,
}

struct ParsedType {
  kind: ParsedTypeKind,
}

enum ParsedTypeKind {
  Struct { field_count: usize, supertype: Option<u32> },
  Func { param_count: usize },
  Other,
}

struct ParsedFunc {
  type_index: u32,
  /// Parsed instructions with their byte offsets.
  instructions: Vec<(u32, ParsedInstr)>,
  /// Number of local variables (excluding params).
  local_count: u32,
}

struct ParsedGlobal {
  /// Type index of the ref type (for $FnN naming).
  ref_type_index: Option<u32>,
  /// Init expression func index (for ref.func).
  init_func_index: Option<u32>,
}

/// Simplified instruction representation for WAT formatting.
#[derive(Debug, Clone)]
enum ParsedInstr {
  LocalGet(u32),
  LocalSet(u32),
  GlobalGet(u32),
  F64Const(f64),
  StructNew(u32),
  StructGet { struct_type_index: u32, field_index: u32 },
  RefCastNonNull(u32),
  RefNull(u32),
  F64Ne,
  ReturnCallRef(u32),
  ReturnCall(u32),
  Call(u32),
  If,
  Else,
  End,
  Unreachable,
  RefFunc(u32),
  /// Any instruction we don't specifically handle.
  Other(String),
}

// ---------------------------------------------------------------------------
// WASM binary parsing
// ---------------------------------------------------------------------------

fn parse_module(wasm: &[u8]) -> ParsedModule {
  let mut module = ParsedModule {
    types: Vec::new(),
    imports: Vec::new(),
    funcs: Vec::new(),
    globals: Vec::new(),
    exports: Vec::new(),
    func_names: HashMap::new(),
    local_names: HashMap::new(),
    global_names: HashMap::new(),
    dwarf_locs: BTreeMap::new(),
    import_func_count: 0,
    structural_locs: Vec::new(),
  };

  let mut func_type_indices: Vec<u32> = Vec::new();
  let mut func_idx_counter = 0u32;

  for payload in Parser::new(0).parse_all(wasm) {
    let payload = match payload {
      Ok(p) => p,
      Err(_) => continue,
    };
    match payload {
      Payload::TypeSection(reader) => {
        for rec_group in reader {
          let rec_group = match rec_group {
            Ok(rg) => rg,
            Err(_) => continue,
          };
          for sub_type in rec_group.into_types() {
            module.types.push(parse_subtype(&sub_type));
          }
        }
      }

      Payload::ImportSection(reader) => {
        for imports_group in reader {
          let imports_group = match imports_group {
            Ok(g) => g,
            Err(_) => continue,
          };
          // Flatten the group into individual imports.
          match imports_group {
            wasmparser::Imports::Single(_, import) => {
              if let wasmparser::TypeRef::Func(type_idx) = import.ty {
                module.imports.push((
                  import.module.to_string(),
                  import.name.to_string(),
                  type_idx,
                ));
                func_idx_counter += 1;
              }
            }
            wasmparser::Imports::Compact1 { module: mod_name, items } => {
              for item in items {
                if let Ok(item) = item
                  && let wasmparser::TypeRef::Func(type_idx) = item.ty {
                    module.imports.push((
                      mod_name.to_string(),
                      item.name.to_string(),
                      type_idx,
                    ));
                    func_idx_counter += 1;
                  }
              }
            }
            wasmparser::Imports::Compact2 { module: mod_name, ty, names } => {
              if let wasmparser::TypeRef::Func(type_idx) = ty {
                for name in names.into_iter().flatten() {
                  module.imports.push((
                    mod_name.to_string(),
                    name.to_string(),
                    type_idx,
                  ));
                  func_idx_counter += 1;
                }
              }
            }
          }
        }
        module.import_func_count = func_idx_counter;
      }

      Payload::FunctionSection(reader) => {
        for idx in reader.into_iter().flatten() {
          func_type_indices.push(idx);
        }
      }

      Payload::GlobalSection(reader) => {
        for global in reader {
          let global = match global {
            Ok(g) => g,
            Err(_) => continue,
          };
          let ref_type_index = match global.ty.content_type {
            wasmparser::ValType::Ref(rt) => {
              if let wasmparser::HeapType::Concrete(idx) = rt.heap_type() {
                Some(idx.as_module_index().unwrap_or(0))
              } else {
                None
              }
            }
            _ => None,
          };
          // Parse init expr for ref.func index.
          let mut init_func_index = None;
          let mut ops_reader = global.init_expr.get_operators_reader();
          while let Ok(op) = ops_reader.read() {
            if let Operator::RefFunc { function_index } = op {
              init_func_index = Some(function_index);
            }
          }
          module.globals.push(ParsedGlobal { ref_type_index, init_func_index });
        }
      }

      Payload::ExportSection(reader) => {
        for e in reader.into_iter().flatten() {
          module.exports.push((e.name.to_string(), e.kind, e.index));
        }
      }

      Payload::CodeSectionEntry(body) => {
        let def_idx = module.funcs.len() as u32;
        let type_index = func_type_indices.get(def_idx as usize).copied().unwrap_or(0);

        let mut instructions = Vec::new();
        let mut local_count = 0u32;

        // Count locals.
        for (count, _ty) in body.get_locals_reader().unwrap().into_iter().flatten() {
          local_count += count;
        }

        // Parse instructions.
        let ops = body.get_operators_reader().unwrap();
        let base_offset = ops.original_position() as u32;
        let mut ops_iter = ops;
        while let Ok(op) = ops_iter.read() {
          let offset = ops_iter.original_position() as u32;
          // offset points to AFTER the instruction was read, but we want
          // the offset before. We'll use the base tracking approach.
          let instr = parse_operator(&op);
          instructions.push((offset, instr));
        }
        // Adjust offsets: wasmparser gives us position after reading,
        // but we want position before. Use a shifted approach.
        let mut adjusted = Vec::new();
        let mut prev_offset = base_offset;
        for (next_offset, instr) in instructions {
          adjusted.push((prev_offset, instr));
          prev_offset = next_offset;
        }

        module.funcs.push(ParsedFunc {
          type_index,
          instructions: adjusted,
          local_count,
        });
      }

      Payload::CustomSection(reader) => {
        if let wasmparser::KnownCustom::Name(name_reader) = reader.as_known() {
          parse_name_section(name_reader, &mut module);
        }
        // DWARF sections.
        if reader.name().starts_with(".debug_") {
          parse_dwarf_section(reader.name(), reader.data(), wasm, &mut module);
        }
      }

      _ => {}
    }
  }

  module
}

fn parse_subtype(sub_type: &SubType) -> ParsedType {
  let supertype = sub_type.supertype_idx
    .map(|idx| idx.as_module_index().unwrap_or(0));
  match &sub_type.composite_type.inner {
    CompositeInnerType::Struct(s) => ParsedType {
      kind: ParsedTypeKind::Struct {
        field_count: s.fields.len(),
        supertype,
      },
    },
    CompositeInnerType::Func(f) => ParsedType {
      kind: ParsedTypeKind::Func {
        param_count: f.params().len(),
      },
    },
    _ => ParsedType { kind: ParsedTypeKind::Other },
  }
}

fn parse_operator(op: &Operator<'_>) -> ParsedInstr {
  match op {
    Operator::LocalGet { local_index } => ParsedInstr::LocalGet(*local_index),
    Operator::LocalSet { local_index } => ParsedInstr::LocalSet(*local_index),
    Operator::GlobalGet { global_index } => ParsedInstr::GlobalGet(*global_index),
    Operator::F64Const { value } => ParsedInstr::F64Const(f64::from_bits(value.bits())),
    Operator::StructNew { struct_type_index } => ParsedInstr::StructNew(*struct_type_index),
    Operator::StructGet { struct_type_index, field_index } => ParsedInstr::StructGet {
      struct_type_index: *struct_type_index,
      field_index: *field_index,
    },
    Operator::RefCastNonNull { hty } => {
      let idx = match hty {
        wasmparser::HeapType::Concrete(i) => i.as_module_index().unwrap_or(0),
        _ => 0,
      };
      ParsedInstr::RefCastNonNull(idx)
    }
    Operator::RefNull { hty } => {
      let idx = match hty {
        wasmparser::HeapType::Concrete(i) => i.as_module_index().unwrap_or(0),
        _ => 0,
      };
      ParsedInstr::RefNull(idx)
    }
    Operator::F64Ne => ParsedInstr::F64Ne,
    Operator::ReturnCallRef { type_index } => ParsedInstr::ReturnCallRef(*type_index),
    Operator::ReturnCall { function_index } => ParsedInstr::ReturnCall(*function_index),
    Operator::Call { function_index } => ParsedInstr::Call(*function_index),
    Operator::If { .. } => ParsedInstr::If,
    Operator::Else => ParsedInstr::Else,
    Operator::End => ParsedInstr::End,
    Operator::Unreachable => ParsedInstr::Unreachable,
    Operator::RefFunc { function_index } => ParsedInstr::RefFunc(*function_index),
    other => ParsedInstr::Other(format!("{:?}", other)),
  }
}

fn parse_name_section(reader: wasmparser::NameSectionReader<'_>, module: &mut ParsedModule) {
  for name in reader {
    let name = match name {
      Ok(n) => n,
      Err(_) => continue,
    };
    match name {
      wasmparser::Name::Function(map) => {
        for n in map.into_iter().flatten() {
          module.func_names.insert(n.index, n.name.to_string());
        }
      }
      wasmparser::Name::Local(indirect_map) => {
        for ind in indirect_map.into_iter().flatten() {
          for n in ind.names.into_iter().flatten() {
            module.local_names.insert((ind.index, n.index), n.name.to_string());
          }
        }
      }
      wasmparser::Name::Global(map) => {
        for n in map.into_iter().flatten() {
          module.global_names.insert(n.index, n.name.to_string());
        }
      }
      _ => {}
    }
  }
}

fn parse_dwarf_section(
  name: &str,
  _data: &[u8],
  wasm: &[u8],
  module: &mut ParsedModule,
) {
  // We only need .debug_line for source mappings.
  // For a full implementation we'd parse DWARF with gimli::read,
  // but for now we read the offset mappings from the embedded DWARF.
  if name != ".debug_line" {
    return;
  }

  // Use gimli to parse DWARF from the WASM binary.
  // gimli's wasm support reads custom sections directly.
  use gimli::{EndianSlice, LittleEndian};

  // Build a section loader from the WASM binary.
  let mut debug_info_data = &[][..];
  let mut debug_abbrev_data = &[][..];
  let mut debug_line_data = &[][..];
  let mut debug_str_data = &[][..];

  for payload in Parser::new(0).parse_all(wasm) {
    if let Ok(Payload::CustomSection(reader)) = payload {
      match reader.name() {
        ".debug_info" => debug_info_data = reader.data(),
        ".debug_abbrev" => debug_abbrev_data = reader.data(),
        ".debug_line" => debug_line_data = reader.data(),
        ".debug_str" => debug_str_data = reader.data(),
        _ => {}
      }
    }
  }

  let debug_info = gimli::DebugInfo::new(debug_info_data, LittleEndian);
  let debug_abbrev = gimli::DebugAbbrev::new(debug_abbrev_data, LittleEndian);
  let debug_line = gimli::DebugLine::new(debug_line_data, LittleEndian);
  let debug_str = gimli::DebugStr::new(debug_str_data, LittleEndian);

  // Parse the first compilation unit.
  let mut units = debug_info.units();
  let unit_header = match units.next() {
    Ok(Some(header)) => header,
    _ => return,
  };

  let abbrevs = match debug_abbrev.abbreviations(unit_header.debug_abbrev_offset()) {
    Ok(a) => a,
    Err(_) => return,
  };

  // Get the line program offset from root DIE's DW_AT_stmt_list.
  let mut cursor = unit_header.entries(&abbrevs);
  if cursor.next_dfs().is_err() { return; }
  let root = match cursor.current() {
    Some(entry) => entry,
    None => return,
  };

  let stmt_list = match root.attr_value(gimli::DW_AT_stmt_list) {
    Some(gimli::AttributeValue::DebugLineRef(offset)) => offset,
    _ => return,
  };

  let line_program = match debug_line.program(
    stmt_list,
    unit_header.address_size(),
    None::<EndianSlice<'_, LittleEndian>>,
    None::<EndianSlice<'_, LittleEndian>>,
  ) {
    Ok(prog) => prog,
    Err(_) => return,
  };

  let _ = debug_str;

  // Execute the line program to extract rows.
  let mut rows = line_program.rows();
  while let Ok(Some((header, row))) = rows.next_row() {
    if row.end_sequence() {
      continue;
    }
    let address = row.address() as u32;
    let line = row.line().map(|l| l.get() as u32).unwrap_or(0);
    let col = match row.column() {
      gimli::ColumnType::LeftEdge => 0u32,
      gimli::ColumnType::Column(c) => c.get() as u32,
    };

    if line > 0 {
      module.dwarf_locs.insert(address, Loc {
        start: Pos { idx: 0, line, col },
        end: Pos { idx: 0, line, col },
      });
    }
    let _ = header;
  }
}

// ---------------------------------------------------------------------------
// WAT text emission
// ---------------------------------------------------------------------------

fn emit_wat(module: &ParsedModule, w: &mut MappedWriter) {
  w.push_str("(module\n");
  emit_type_section(module, w);
  w.push_str("\n");

  // Emit globals and functions.
  for (def_idx, func) in module.funcs.iter().enumerate() {
    let func_idx = module.import_func_count + def_idx as u32;
    emit_global_for_func(module, func_idx, w);
    emit_func(module, func, func_idx, w);
  }

  emit_exports(module, w);
  w.push_str(")\n");
}

fn emit_exports(module: &ParsedModule, w: &mut MappedWriter) {
  use super::emit::StructuralKind;
  for (name, kind, index) in &module.exports {
    if kind == &ExternalKind::Func {
      let f_name = func_name(module, *index);
      if let Some(loc) = find_structural_loc(module, |k| matches!(k, StructuralKind::Export { name: n } if n == name)) {
        w.mark(loc);
      }
      w.push_str(&format!("  (export {:?} (func {}))\n", name, f_name));
    }
  }
}

fn emit_type_section(module: &ParsedModule, w: &mut MappedWriter) {
  for (idx, ty) in module.types.iter().enumerate() {
    match &ty.kind {
      ParsedTypeKind::Struct { field_count, supertype } => {
        let name = type_name(module, idx as u32);
        if *field_count == 0 && supertype.is_none() {
          w.push_str(&format!("  (type {} (sub (struct)))\n", name));
        } else if let Some(super_idx) = supertype {
          let super_name = type_name(module, *super_idx);
          let fields = if *field_count > 0 { " (field f64)" } else { "" };
          w.push_str(&format!("  (type {} (sub {} (struct{})))\n", name, super_name, fields));
        }
      }
      ParsedTypeKind::Func { param_count } => {
        let name = type_name(module, idx as u32);
        if *param_count == 0 {
          w.push_str(&format!("  (type {} (func))\n", name));
        } else {
          let params: Vec<&str> = (0..*param_count).map(|_| "(ref $Any)").collect();
          w.push_str(&format!("  (type {} (func (param {})))\n", name, params.join(" ")));
        }
      }
      ParsedTypeKind::Other => {}
    }
  }
}

fn emit_global_for_func(module: &ParsedModule, func_idx: u32, w: &mut MappedWriter) {
  use super::emit::StructuralKind;
  for (g_idx, global) in module.globals.iter().enumerate() {
    if global.init_func_index == Some(func_idx) {
      let g_name = global_name(module, g_idx as u32);
      let type_name_str = global.ref_type_index
        .map(|ti| type_name(module, ti))
        .unwrap_or_else(|| "$Any".into());
      let f_name = func_name(module, func_idx);
      // Apply structural source mark for this global.
      if let Some(loc) = find_structural_loc(module, |k| matches!(k, StructuralKind::Global { global_idx } if *global_idx == g_idx as u32)) {
        w.mark(loc);
      }
      w.push_str(&format!("  (global {} (ref {}) (ref.func {}))\n",
        g_name, type_name_str, f_name));
    }
  }
}

fn emit_func(module: &ParsedModule, func: &ParsedFunc, func_idx: u32, w: &mut MappedWriter) {
  let name = func_name(module, func_idx);
  let type_name_str = type_name(module, func.type_index);
  let param_count = module.types.get(func.type_index as usize)
    .map(|t| match &t.kind {
      ParsedTypeKind::Func { param_count } => *param_count,
      _ => 0,
    })
    .unwrap_or(0);

  use super::emit::StructuralKind;

  // Function header — apply structural mark.
  if let Some(loc) = find_structural_loc(module, |k| matches!(k, StructuralKind::FuncHeader { func_idx: fi } if *fi == func_idx)) {
    w.mark(loc);
  }
  w.push_str(&format!("  (func {} (type {})", name, type_name_str));

  // Parameters — each gets its own mark.
  for i in 0..param_count {
    w.push_str(" ");
    if let Some(loc) = find_structural_loc(module, |k| matches!(k, StructuralKind::FuncParam { func_idx: fi, param_idx: pi } if *fi == func_idx && *pi == i as u32)) {
      w.mark(loc);
    }
    let p_name = local_name(module, func_idx, i as u32);
    w.push_str(&format!("(param {} (ref $Any))", p_name));
  }
  w.push_str("\n");

  // Locals.
  for i in 0..func.local_count {
    let l_name = local_name(module, func_idx, param_count as u32 + i);
    w.push_str(&format!("    (local {} (ref $Any))\n", l_name));
  }

  // Body — emit instructions as WAT statements.
  emit_func_body(module, func, w);

  w.push_str("  )\n");
}

fn emit_func_body(module: &ParsedModule, func: &ParsedFunc, w: &mut MappedWriter) {
  // Use a stack-based approach to reconstruct nested s-expressions.
  // Walk instructions, building tree nodes on a stack. Statement-level
  // instructions (local.set, return_call_ref, if, etc.) consume from the
  // stack and emit complete statements.
  let indent = 2usize;
  let instrs = &func.instructions;
  // Stack of (formatted_string, first_offset) — offset tracks where this value started.
  let mut stack: Vec<(String, u32)> = Vec::new();
  let mut i = 0;

  while i < instrs.len() {
    let (offset, instr) = &instrs[i];

    match instr {
      ParsedInstr::End => {
        if i == instrs.len() - 1 { break; } // final End
        w.push_str(&format!("{})\n", ind(indent)));
        i += 1;
      }

      // Value instructions — push onto stack with their offset.
      ParsedInstr::LocalGet(idx) => {
        let name = local_name_by_idx(module, func, *idx);
        stack.push((format!("(local.get {})", name), *offset));
        i += 1;
      }
      ParsedInstr::GlobalGet(idx) => {
        let name = global_name(module, *idx);
        stack.push((format!("(global.get {})", name), *offset));
        i += 1;
      }
      ParsedInstr::F64Const(v) => {
        stack.push((format!("(f64.const {})", format_f64(*v)), *offset));
        i += 1;
      }
      ParsedInstr::StructNew(type_idx) => {
        let name = type_name(module, *type_idx);
        let fields = module.types.get(*type_idx as usize)
          .map(|t| match &t.kind {
            ParsedTypeKind::Struct { field_count, .. } => *field_count,
            _ => 0,
          })
          .unwrap_or(0);
        let popped: Vec<(String, u32)> = stack.split_off(stack.len().saturating_sub(fields));
        let first_offset = popped.first().map(|(_, o)| *o).unwrap_or(*offset);
        let args: Vec<&str> = popped.iter().map(|(s, _)| s.as_str()).collect();
        let args_str = if args.is_empty() { String::new() } else { format!(" {}", args.join(" ")) };
        stack.push((format!("(struct.new {}{})", name, args_str), first_offset));
        i += 1;
      }
      ParsedInstr::StructGet { struct_type_index, field_index } => {
        let name = type_name(module, *struct_type_index);
        let (arg, arg_off) = stack.pop().unwrap_or_default();
        stack.push((format!("(struct.get {} {} {})", name, field_index, arg), arg_off));
        i += 1;
      }
      ParsedInstr::RefCastNonNull(type_idx) => {
        let name = type_name(module, *type_idx);
        let (arg, arg_off) = stack.pop().unwrap_or_default();
        stack.push((format!("(ref.cast (ref {}) {})", name, arg), arg_off));
        i += 1;
      }
      ParsedInstr::RefNull(type_idx) => {
        let name = type_name(module, *type_idx);
        stack.push((format!("(ref.null {})", name), *offset));
        i += 1;
      }
      ParsedInstr::F64Ne => {
        let (b, _) = stack.pop().unwrap_or_default();
        let (a, a_off) = stack.pop().unwrap_or_default();
        stack.push((format!("(f64.ne {} {})", a, b), a_off));
        i += 1;
      }
      ParsedInstr::RefFunc(func_idx) => {
        let name = func_name(module, *func_idx);
        stack.push((format!("(ref.func {})", name), *offset));
        i += 1;
      }
      ParsedInstr::Call(func_idx) => {
        let name = func_name(module, *func_idx);
        let param_count = lookup_func_param_count(module, *func_idx);
        let popped: Vec<(String, u32)> = stack.split_off(stack.len().saturating_sub(param_count));
        let first_offset = popped.first().map(|(_, o)| *o).unwrap_or(*offset);
        let args: Vec<&str> = popped.iter().map(|(s, _)| s.as_str()).collect();
        let args_str = if args.is_empty() { String::new() } else { format!(" {}", args.join(" ")) };
        stack.push((format!("(call {}{})", name, args_str), first_offset));
        i += 1;
      }

      // Statement-level instructions — consume from stack, find DWARF mark, emit.
      ParsedInstr::LocalSet(idx) => {
        let name = local_name_by_idx(module, func, *idx);
        let (val, val_off) = stack.pop().unwrap_or_default();
        // Find DWARF mark in the range [val_off, offset].
        if let Some(loc) = find_dwarf_loc(module, val_off, *offset) { w.mark(loc); }
        w.push_str(&format!("{}(local.set {} {})\n", ind(indent), name, val));
        i += 1;
      }
      ParsedInstr::ReturnCallRef(type_idx) => {
        let type_name_str = type_name(module, *type_idx);
        let param_count = module.types.get(*type_idx as usize)
          .map(|t| match &t.kind {
            ParsedTypeKind::Func { param_count } => *param_count,
            _ => 0,
          })
          .unwrap_or(0);
        let total = param_count + 1;
        let popped: Vec<(String, u32)> = stack.split_off(stack.len().saturating_sub(total));
        let first_offset = popped.first().map(|(_, o)| *o).unwrap_or(*offset);
        let args: Vec<&str> = popped.iter().map(|(s, _)| s.as_str()).collect();
        let args_str = if args.is_empty() { String::new() } else { format!(" {}", args.join(" ")) };
        if let Some(loc) = find_dwarf_loc(module, first_offset, *offset) { w.mark(loc); }
        w.push_str(&format!("{}(return_call_ref {}{})\n", ind(indent), type_name_str, args_str));
        i += 1;
      }
      ParsedInstr::ReturnCall(func_idx_val) => {
        let name = func_name(module, *func_idx_val);
        let param_count = lookup_func_param_count(module, *func_idx_val);
        let popped: Vec<(String, u32)> = stack.split_off(stack.len().saturating_sub(param_count));
        let first_offset = popped.first().map(|(_, o)| *o).unwrap_or(*offset);
        let args: Vec<&str> = popped.iter().map(|(s, _)| s.as_str()).collect();
        let args_str = if args.is_empty() { String::new() } else { format!(" {}", args.join(" ")) };
        if let Some(loc) = find_dwarf_loc(module, first_offset, *offset) { w.mark(loc); }
        w.push_str(&format!("{}(return_call {}{})\n", ind(indent), name, args_str));
        i += 1;
      }

      ParsedInstr::If => {
        let (cond, cond_off) = stack.pop().unwrap_or_default();
        if let Some(loc) = find_dwarf_loc(module, cond_off, *offset) { w.mark(loc); }
        w.push_str(&format!("{}(if {}\n", ind(indent), cond));
        w.push_str(&format!("{}(then\n", ind(indent + 1)));
        i += 1;
      }
      ParsedInstr::Else => {
        w.push_str(&format!("{})\n", ind(indent + 1)));
        w.push_str(&format!("{}(else\n", ind(indent + 1)));
        i += 1;
      }

      ParsedInstr::Unreachable => {
        w.push_str(&format!("{}unreachable\n", ind(indent)));
        i += 1;
      }

      _ => {
        let s = format_instr(module, func, instr);
        w.push_str(&format!("{}{}\n", ind(indent), s));
        i += 1;
      }
    }
  }
}

/// Find the first DWARF source location in the offset range [from..=to].
fn find_dwarf_loc(module: &ParsedModule, from: u32, to: u32) -> Option<Loc> {
  module.dwarf_locs.range(from..=to)
    .next()
    .map(|(_, loc)| *loc)
}

/// Look up param count for a function by its index (imports or defined).
fn lookup_func_param_count(module: &ParsedModule, func_idx: u32) -> usize {
  // Try imports first.
  if (func_idx as usize) < module.imports.len() {
    let (_, _, type_idx) = &module.imports[func_idx as usize];
    return module.types.get(*type_idx as usize)
      .map(|t| match &t.kind {
        ParsedTypeKind::Func { param_count } => *param_count,
        _ => 0,
      })
      .unwrap_or(0);
  }
  // Defined function.
  let def_idx = func_idx as usize - module.imports.len();
  module.funcs.get(def_idx)
    .and_then(|f| module.types.get(f.type_index as usize))
    .map(|t| match &t.kind {
      ParsedTypeKind::Func { param_count } => *param_count,
      _ => 0,
    })
    .unwrap_or(0)
}


/// Find a structural source location by kind predicate.
fn find_structural_loc(module: &ParsedModule, pred: impl Fn(&super::emit::StructuralKind) -> bool) -> Option<Loc> {
  module.structural_locs.iter()
    .find(|sl| pred(&sl.kind))
    .map(|sl| sl.loc)
}

fn format_instr(module: &ParsedModule, func: &ParsedFunc, instr: &ParsedInstr) -> String {
  match instr {
    ParsedInstr::LocalGet(idx) => format!("(local.get {})", local_name_by_idx(module, func, *idx)),
    ParsedInstr::LocalSet(idx) => format!("(local.set {})", local_name_by_idx(module, func, *idx)),
    ParsedInstr::GlobalGet(idx) => format!("(global.get {})", global_name(module, *idx)),
    ParsedInstr::F64Const(v) => format!("(f64.const {})", format_f64(*v)),
    ParsedInstr::StructNew(idx) => format!("(struct.new {})", type_name(module, *idx)),
    ParsedInstr::StructGet { struct_type_index, field_index } =>
      format!("(struct.get {} {})", type_name(module, *struct_type_index), field_index),
    ParsedInstr::RefCastNonNull(idx) => format!("(ref.cast (ref {}))", type_name(module, *idx)),
    ParsedInstr::RefNull(idx) => format!("(ref.null {})", type_name(module, *idx)),
    ParsedInstr::F64Ne => "f64.ne".into(),
    ParsedInstr::ReturnCallRef(idx) => format!("(return_call_ref {})", type_name(module, *idx)),
    ParsedInstr::ReturnCall(idx) => format!("(return_call {})", func_name(module, *idx)),
    ParsedInstr::Call(idx) => format!("(call {})", func_name(module, *idx)),
    ParsedInstr::Unreachable => "unreachable".into(),
    ParsedInstr::RefFunc(idx) => format!("(ref.func {})", func_name(module, *idx)),
    ParsedInstr::If => "(if".into(),
    ParsedInstr::Else => ")(else".into(),
    ParsedInstr::End => ")".into(),
    ParsedInstr::Other(s) => format!(";; {}", s),
  }
}

// ---------------------------------------------------------------------------
// Name resolution helpers
// ---------------------------------------------------------------------------

fn type_name(module: &ParsedModule, idx: u32) -> String {
  // Infer type names from structure: $Any (empty struct), $Num (struct with super),
  // $FnN (func types).
  if let Some(ty) = module.types.get(idx as usize) {
    match &ty.kind {
      ParsedTypeKind::Struct { field_count: 0, supertype: None } => return "$Any".into(),
      ParsedTypeKind::Struct { supertype: Some(_), .. } => return "$Num".into(),
      ParsedTypeKind::Func { param_count } => return format!("$Fn{}", param_count),
      _ => {}
    }
  }
  format!("$type_{}", idx)
}

fn func_name(module: &ParsedModule, idx: u32) -> String {
  module.func_names.get(&idx)
    .map(|n| format!("${}", n))
    .unwrap_or_else(|| format!("$func_{}", idx))
}

fn local_name(module: &ParsedModule, func_idx: u32, local_idx: u32) -> String {
  module.local_names.get(&(func_idx, local_idx))
    .map(|n| format!("${}", n))
    .unwrap_or_else(|| format!("$local_{}", local_idx))
}

fn local_name_by_idx(module: &ParsedModule, func: &ParsedFunc, local_idx: u32) -> String {
  // We need the absolute func index. Find it from the func's type index.
  // This is a bit roundabout — we'd need the func_idx passed in.
  // For now, search func_names for a match.
  for (def_idx, f) in module.funcs.iter().enumerate() {
    if std::ptr::eq(f, func) {
      let func_idx = module.import_func_count + def_idx as u32;
      return local_name(module, func_idx, local_idx);
    }
  }
  format!("$local_{}", local_idx)
}

fn global_name(module: &ParsedModule, idx: u32) -> String {
  module.global_names.get(&idx)
    .map(|n| format!("${}", n))
    .unwrap_or_else(|| format!("$global_{}", idx))
}

fn ind(level: usize) -> String {
  "  ".repeat(level)
}

fn format_f64(v: f64) -> String {
  if v == v.trunc() && v.is_finite() {
    // Integer-valued float: render without decimal for cleaner output.
    format!("{}", v as i64)
  } else {
    format!("{}", v)
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ast::build_index;
  use crate::parser::parse;
  use crate::passes::cps::transform::lower_module;
  use crate::passes::lifting::lift;
  use crate::passes::wasm::{collect, dwarf, emit};

  fn compile_and_format(src: &str) -> String {
    let r = parse(src).unwrap_or_else(|e| panic!("parse error: {}", e.message));
    let (root, node_count) = crate::passes::partial::apply(r.root, r.node_count)
      .unwrap_or_else(|e| panic!("partial error: {:?}", e));
    let r = crate::ast::ParseResult { root, node_count };
    let ast_index = build_index(&r);
    let scope = crate::passes::scopes::analyse(&r.root, r.node_count as usize, &[]);
    let exprs = match &r.root.kind {
      crate::ast::NodeKind::Module(exprs) => &exprs.items,
      _ => panic!("expected module"),
    };
    let cps = lower_module(exprs, &scope);
    let lifted = lift(cps, &ast_index);

    let ir_ctx = collect::IrCtx::new(&lifted.origin, &ast_index);
    let module = collect::collect(&lifted.root, &ir_ctx);
    let ir_ctx = ir_ctx.with_globals(module.globals.clone());
    let mut result = emit::emit(&module, &ir_ctx);

    let dwarf_sections = dwarf::emit_dwarf("test.fnk", Some(src), &result.offset_mappings);
    dwarf::append_dwarf_sections(&mut result.wasm, &dwarf_sections);

    format(&result.wasm)
  }

  #[test]
  fn t_format_simple() {
    let wat = compile_and_format("add = fn a, b: a + b");
    assert!(wat.contains("(module"), "should start with (module");
    assert!(wat.contains("(type $Any"), "should have $Any type");
    assert!(wat.contains("(type $Num"), "should have $Num type");
    assert!(wat.contains("(func"), "should have functions");
    assert!(wat.contains("(export"), "should have exports");
  }

  #[test]
  fn t_format_has_names() {
    let wat = compile_and_format("add = fn a, b: a + b");
    // Should use names from the name section.
    assert!(wat.contains("$add_"), "should have add in names");
  }

  #[test]
  fn t_format_mapped() {
    let src = "add = fn a, b: a + b";
    let r = parse(src).unwrap();
    let (root, node_count) = crate::passes::partial::apply(r.root, r.node_count).unwrap();
    let r = crate::ast::ParseResult { root, node_count };
    let ast_index = build_index(&r);
    let scope = crate::passes::scopes::analyse(&r.root, r.node_count as usize, &[]);
    let exprs = match &r.root.kind {
      crate::ast::NodeKind::Module(exprs) => &exprs.items,
      _ => panic!("expected module"),
    };
    let cps = lower_module(exprs, &scope);
    let lifted = lift(cps, &ast_index);

    let ir_ctx = collect::IrCtx::new(&lifted.origin, &ast_index);
    let module = collect::collect(&lifted.root, &ir_ctx);
    let ir_ctx = ir_ctx.with_globals(module.globals.clone());
    let mut result = emit::emit(&module, &ir_ctx);

    let dwarf_sections = dwarf::emit_dwarf("test.fnk", Some(src), &result.offset_mappings);
    dwarf::append_dwarf_sections(&mut result.wasm, &dwarf_sections);

    let (wat, srcmap) = format_mapped(&result.wasm, "test.fnk", src);
    assert!(wat.contains("(module"), "WAT should have module");
    let json = srcmap.to_json();
    assert!(json.contains("test.fnk"), "source map should reference test.fnk");
  }
}
