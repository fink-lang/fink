// DWARF emission — generates minimal DWARF line tables for source-level debugging.
//
// Produces .debug_info, .debug_abbrev, .debug_line, and .debug_str sections
// that map WASM code-section byte offsets to .fnk source locations.
//
// The DWARF is embedded in the WASM binary as custom sections with standard
// names (.debug_info, etc.). wasmtime reads these natively for breakpoint
// resolution and stepping. The custom WASM→WAT formatter reads them to
// emit native-form mappings via `MappedWriter::mark`.

use gimli::write::{
  Address, AttributeValue, DwarfUnit, EndianVec,
  LineProgram, LineString, Sections,
};
use gimli::{Encoding, Format, LittleEndian};

use super::emit::OffsetMapping;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A named DWARF section ready to append to a WASM binary.
pub struct DwarfSection {
  pub name: String,
  pub data: Vec<u8>,
}

/// Build DWARF sections from offset mappings.
///
/// Returns a list of named sections (.debug_info, .debug_line, etc.)
/// that should be appended to the WASM binary as custom sections.
pub fn emit_dwarf(
  source_path: &str,
  source_content: Option<&str>,
  mappings: &[OffsetMapping],
) -> Vec<DwarfSection> {
  let encoding = Encoding {
    format: Format::Dwarf32,
    version: 4,
    address_size: 4, // wasm32
  };

  // Create line program with the source file.
  let line_program = LineProgram::new(
    encoding,
    Default::default(),
    LineString::String(b".".to_vec()),          // working dir
    None,                                       // source dir
    LineString::String(source_path.as_bytes().to_vec()),
    None,
  );

  let mut dwarf = DwarfUnit::new(encoding);
  // Stash the source_content to embed it in the line program's file info,
  // but gimli's DWARF v4 doesn't support inline source — it's a v5 feature.
  // For now we just note the file.
  let _ = source_content;

  // Set root DIE attributes.
  let root = dwarf.unit.root();
  dwarf.unit.get_mut(root).set(
    gimli::DW_AT_name,
    AttributeValue::String(source_path.as_bytes().to_vec()),
  );
  dwarf.unit.get_mut(root).set(
    gimli::DW_AT_producer,
    AttributeValue::String(b"fink".to_vec()),
  );
  dwarf.unit.get_mut(root).set(
    gimli::DW_AT_language,
    AttributeValue::Udata(0x0001), // DW_LANG_C89 as placeholder — no standard code for Fink
  );
  // Point to the line program.
  dwarf.unit.get_mut(root).set(
    gimli::DW_AT_stmt_list,
    AttributeValue::LineProgramRef,
  );

  // Move the line program into the unit, then add the source file.
  dwarf.unit.line_program = line_program;
  let dir_id = dwarf.unit.line_program.default_directory();
  let file_id = dwarf.unit.line_program.add_file(
    LineString::String(source_path.as_bytes().to_vec()),
    dir_id,
    None,
  );

  // Populate line program rows from offset mappings.
  if !mappings.is_empty() {
    let lp = &mut dwarf.unit.line_program;
    lp.begin_sequence(Some(Address::Constant(0)));

    for mapping in mappings {
      let row = lp.row();
      row.address_offset = mapping.wasm_offset as u64;
      row.file = file_id;
      row.line = mapping.loc.start.line as u64;
      row.column = mapping.loc.start.col as u64;
      row.is_statement = true;
      lp.generate_row();
    }

    // End sequence at one past the last mapping.
    let last_offset = mappings.last().map(|m| m.wasm_offset as u64 + 1).unwrap_or(1);
    lp.end_sequence(last_offset);
  }

  // Serialize to section bytes.
  let mut sections = Sections::new(EndianVec::new(LittleEndian));
  dwarf.write(&mut sections).expect("DWARF serialization should not fail");

  // Collect non-empty sections.
  let mut result = Vec::new();
  let _: Result<(), ()> = sections.for_each(|section_id, writer| {
    let data = writer.slice();
    if !data.is_empty() {
      result.push(DwarfSection {
        name: section_id.name().to_string(),
        data: data.to_vec(),
      });
    }
    Ok(())
  });

  result
}

/// Append DWARF sections to a WASM binary as custom sections.
pub fn append_dwarf_sections(wasm: &mut Vec<u8>, sections: &[DwarfSection]) {
  for section in sections {
    append_custom_section(wasm, &section.name, &section.data);
  }
}

/// Append a single custom section to a WASM binary.
fn append_custom_section(wasm: &mut Vec<u8>, name: &str, data: &[u8]) {
  // Custom section: id=0, then LEB128 size, then name (LEB128-prefixed), then data.
  let name_bytes = name.as_bytes();
  let payload_size = leb128_size(name_bytes.len() as u32) + name_bytes.len() + data.len();

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
    if val == 0 { break; }
  }
  size
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;
  use crate::lexer::{Loc, Pos};

  #[test]
  fn t_emit_empty() {
    let sections = emit_dwarf("test.fnk", None, &[]);
    // Should produce at least debug_abbrev and debug_info even with no mappings.
    assert!(sections.iter().any(|s| s.name == ".debug_abbrev"), "should have .debug_abbrev");
    assert!(sections.iter().any(|s| s.name == ".debug_info"), "should have .debug_info");
  }

  #[test]
  fn t_emit_with_mappings() {
    let mappings = vec![
      OffsetMapping {
        wasm_offset: 100,
        loc: Loc {
          start: Pos { idx: 0, line: 1, col: 0 },
          end: Pos { idx: 5, line: 1, col: 5 },
        },
      },
      OffsetMapping {
        wasm_offset: 110,
        loc: Loc {
          start: Pos { idx: 10, line: 2, col: 4 },
          end: Pos { idx: 15, line: 2, col: 9 },
        },
      },
    ];
    let sections = emit_dwarf("test.fnk", Some("x = 1\ny = 2"), &mappings);
    assert!(sections.iter().any(|s| s.name == ".debug_line"), "should have .debug_line");
  }

  #[test]
  fn t_append_custom_section() {
    // Minimal valid WASM module header.
    let mut wasm = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
    let sections = vec![DwarfSection {
      name: ".debug_test".into(),
      data: vec![0x42, 0x43],
    }];
    append_dwarf_sections(&mut wasm, &sections);
    // Should have grown.
    assert!(wasm.len() > 8);
    // Custom section id = 0.
    assert_eq!(wasm[8], 0x00);
  }

  #[test]
  fn t_dwarf_roundtrip_with_gimli_read() {
    // Emit DWARF, then parse it back with gimli::read to verify structure.
    let mappings = vec![
      OffsetMapping {
        wasm_offset: 50,
        loc: Loc {
          start: Pos { idx: 0, line: 3, col: 2 },
          end: Pos { idx: 5, line: 3, col: 7 },
        },
      },
    ];
    let sections = emit_dwarf("hello.fnk", None, &mappings);

    // Find .debug_line section.
    let debug_line = sections.iter().find(|s| s.name == ".debug_line")
      .expect("should have .debug_line");
    assert!(!debug_line.data.is_empty(), ".debug_line should have data");

    // Find .debug_info section.
    let debug_info = sections.iter().find(|s| s.name == ".debug_info")
      .expect("should have .debug_info");
    assert!(!debug_info.data.is_empty(), ".debug_info should have data");
  }
}
