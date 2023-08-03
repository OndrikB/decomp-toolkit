use std::{
    cmp::{min, Ordering},
    collections::{btree_map, BTreeMap},
    io::Write,
};

use anyhow::{anyhow, bail, ensure, Result};
use ppc750cl::{disasm_iter, Argument, Ins, Opcode};

use crate::{
    obj::{
        ObjDataKind, ObjInfo, ObjReloc, ObjRelocKind, ObjSection, ObjSectionKind, ObjSymbol,
        ObjSymbolFlags, ObjSymbolKind,
    },
    util::nested::NestedVec,
};

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum SymbolEntryKind {
    Start,
    End,
    Label,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
struct SymbolEntry {
    index: usize,
    kind: SymbolEntryKind,
}

pub fn write_asm<W: Write>(w: &mut W, obj: &ObjInfo) -> Result<()> {
    writeln!(w, ".include \"macros.inc\"")?;
    if !obj.name.is_empty() {
        let name = obj
            .name
            .rsplit_once('/')
            .or_else(|| obj.name.rsplit_once('\\'))
            .or_else(|| obj.name.rsplit_once(' '))
            .map(|(_, b)| b)
            .unwrap_or(&obj.name);
        writeln!(w, ".file \"{}\"", name.replace('\\', "\\\\"))?;
    }

    // We'll append generated symbols to the end
    let mut symbols: Vec<ObjSymbol> = obj.symbols.iter().cloned().collect();
    let mut section_entries: Vec<BTreeMap<u32, Vec<SymbolEntry>>> = vec![];
    let mut section_relocations: Vec<BTreeMap<u32, ObjReloc>> = vec![];
    for (section_idx, section) in obj.sections.iter().enumerate() {
        // Build symbol start/end entries
        let mut entries = BTreeMap::<u32, Vec<SymbolEntry>>::new();
        for (symbol_index, symbol) in obj.symbols.for_section(section) {
            entries.nested_push(symbol.address as u32, SymbolEntry {
                index: symbol_index,
                kind: SymbolEntryKind::Start,
            });
            if symbol.size > 0 {
                entries.nested_push((symbol.address + symbol.size) as u32, SymbolEntry {
                    index: symbol_index,
                    kind: SymbolEntryKind::End,
                });
            }
        }

        let mut relocations = section.build_relocation_map_cloned()?;

        // Generate local jump labels
        if section.kind == ObjSectionKind::Code {
            for ins in disasm_iter(&section.data, section.address as u32) {
                if let Some(address) = ins.branch_dest() {
                    if ins.field_AA() || !section.contains(address) {
                        continue;
                    }

                    // Replace section-relative jump relocations (generated by GCC)
                    // These aren't always possible to express accurately in GNU assembler
                    if matches!(relocations.get(&ins.addr), Some(reloc) if reloc.addend == 0) {
                        continue;
                    }

                    let vec = match entries.entry(address) {
                        btree_map::Entry::Occupied(e) => e.into_mut(),
                        btree_map::Entry::Vacant(e) => e.insert(vec![]),
                    };
                    let mut target_symbol_idx = vec
                        .iter()
                        .find(|e| e.kind == SymbolEntryKind::Label)
                        .or_else(|| vec.iter().find(|e| e.kind == SymbolEntryKind::Start))
                        .map(|e| e.index);
                    if target_symbol_idx.is_none() {
                        let display_address = address as u64 + section.original_address;
                        let symbol_idx = symbols.len();
                        symbols.push(ObjSymbol {
                            name: format!(".L_{display_address:08X}"),
                            address: display_address,
                            section: Some(section_idx),
                            size_known: true,
                            ..Default::default()
                        });
                        vec.push(SymbolEntry { index: symbol_idx, kind: SymbolEntryKind::Label });
                        target_symbol_idx = Some(symbol_idx);
                    }
                    if let Some(symbol_idx) = target_symbol_idx {
                        relocations.insert(ins.addr, ObjReloc {
                            kind: match ins.op {
                                Opcode::B => ObjRelocKind::PpcRel24,
                                Opcode::Bc => ObjRelocKind::PpcRel14,
                                _ => unreachable!(),
                            },
                            address: ins.addr as u64,
                            target_symbol: symbol_idx,
                            addend: 0,
                        });
                    }
                }
            }
        }

        section_entries.push(entries);
        section_relocations.push(relocations);
    }

    // Generate labels for jump tables & relative data relocations
    for section in &obj.sections {
        if !matches!(section.kind, ObjSectionKind::Data | ObjSectionKind::ReadOnlyData) {
            continue;
        }

        for reloc in &section.relocations {
            if reloc.addend == 0 {
                continue;
            }
            let target = &symbols[reloc.target_symbol];
            let target_section_idx = match target.section {
                Some(v) => v,
                None => continue,
            };
            let target_section = &obj.sections[target_section_idx];
            let address = (target.address as i64 + reloc.addend) as u64;
            let vec = match section_entries[target_section_idx].entry(address as u32) {
                btree_map::Entry::Occupied(e) => e.into_mut(),
                btree_map::Entry::Vacant(e) => e.insert(vec![]),
            };
            if !vec
                .iter()
                .any(|e| e.kind == SymbolEntryKind::Label || e.kind == SymbolEntryKind::Start)
            {
                let display_address = address + target_section.original_address;
                let symbol_idx = symbols.len();
                symbols.push(ObjSymbol {
                    name: format!(".L_{display_address:08X}"),
                    address: display_address,
                    section: Some(target_section_idx),
                    size_known: true,
                    ..Default::default()
                });
                vec.push(SymbolEntry { index: symbol_idx, kind: SymbolEntryKind::Label });
            }
        }
    }

    // Write common symbols
    let mut common_symbols = Vec::new();
    for symbol in symbols.iter().filter(|s| s.flags.is_common()) {
        ensure!(symbol.section.is_none(), "Invalid: common symbol with section {:?}", symbol);
        common_symbols.push(symbol);
    }
    if !common_symbols.is_empty() {
        writeln!(w)?;
        for symbol in common_symbols {
            if let Some(name) = &symbol.demangled_name {
                writeln!(w, "# {name}")?;
            }
            write!(w, ".comm ")?;
            write_symbol_name(w, &symbol.name)?;
            writeln!(w, ", {:#X}, 4", symbol.size)?;
        }
    }

    for section in &obj.sections {
        let entries = &section_entries[section.index];
        let relocations = &section_relocations[section.index];

        let mut current_address = section.address as u32;
        let section_end = (section.address + section.size) as u32;
        let subsection =
            obj.sections.iter().take(section.index).filter(|s| s.name == section.name).count();

        loop {
            if current_address >= section_end {
                break;
            }

            write_section_header(w, section, subsection, current_address, section_end)?;
            match section.kind {
                ObjSectionKind::Code | ObjSectionKind::Data | ObjSectionKind::ReadOnlyData => {
                    write_data(
                        w,
                        &symbols,
                        entries,
                        relocations,
                        section,
                        current_address,
                        section_end,
                        &section_entries,
                    )?;
                }
                ObjSectionKind::Bss => {
                    write_bss(w, &symbols, entries, current_address, section_end)?;
                }
            }

            // Write end of symbols
            if let Some(entries) = entries.get(&section_end) {
                for entry in entries {
                    if entry.kind != SymbolEntryKind::End {
                        continue;
                    }
                    write_symbol_entry(w, &symbols, entry)?;
                }
            }

            current_address = section_end;
        }
    }

    w.flush()?;
    Ok(())
}

fn write_code_chunk<W: Write>(
    w: &mut W,
    symbols: &[ObjSymbol],
    _entries: &BTreeMap<u32, Vec<SymbolEntry>>,
    relocations: &BTreeMap<u32, ObjReloc>,
    section: &ObjSection,
    address: u32,
    data: &[u8],
) -> Result<()> {
    for ins in disasm_iter(data, address) {
        let reloc = relocations.get(&ins.addr);
        let file_offset = section.file_offset + (ins.addr as u64 - section.address);
        write_ins(w, symbols, ins, reloc, file_offset, section.original_address)?;
    }
    Ok(())
}

fn write_ins<W: Write>(
    w: &mut W,
    symbols: &[ObjSymbol],
    ins: Ins,
    reloc: Option<&ObjReloc>,
    file_offset: u64,
    section_address: u64,
) -> Result<()> {
    write!(
        w,
        "/* {:08X} {:08X}  {:02X} {:02X} {:02X} {:02X} */\t",
        ins.addr as u64 + section_address,
        file_offset,
        (ins.code >> 24) & 0xFF,
        (ins.code >> 16) & 0xFF,
        (ins.code >> 8) & 0xFF,
        ins.code & 0xFF
    )?;

    if ins.op == Opcode::Illegal {
        write!(w, ".4byte {:#010X} /* invalid */", ins.code)?;
    } else if is_illegal_instruction(ins.code) {
        let sins = ins.simplified();
        write!(w, ".4byte {:#010X} /* illegal: {} */", sins.ins.code, sins)?;
    } else {
        let sins = ins.simplified();
        write!(w, "{}{}", sins.mnemonic, sins.ins.suffix())?;

        let mut writing_offset = false;
        for (i, arg) in sins.args.iter().enumerate() {
            if !writing_offset {
                if i == 0 {
                    write!(w, " ")?;
                } else {
                    write!(w, ", ")?;
                }
            }
            match arg {
                Argument::Uimm(_) | Argument::Simm(_) | Argument::BranchDest(_) => {
                    if let Some(reloc) = reloc {
                        write_reloc(w, symbols, reloc)?;
                    } else {
                        write!(w, "{arg}")?;
                    }
                }
                Argument::Offset(_) => {
                    if let Some(reloc) = reloc {
                        write_reloc(w, symbols, reloc)?;
                    } else {
                        write!(w, "{arg}")?;
                    }
                    write!(w, "(")?;
                    writing_offset = true;
                    continue;
                }
                _ => {
                    write!(w, "{arg}")?;
                }
            }
            if writing_offset {
                write!(w, ")")?;
                writing_offset = false;
            }
        }
    }
    writeln!(w)?;
    Ok(())
}

fn write_reloc<W: Write>(w: &mut W, symbols: &[ObjSymbol], reloc: &ObjReloc) -> Result<()> {
    write_reloc_symbol(w, symbols, reloc)?;
    match reloc.kind {
        ObjRelocKind::Absolute | ObjRelocKind::PpcRel24 | ObjRelocKind::PpcRel14 => {
            // pass
        }
        ObjRelocKind::PpcAddr16Hi => {
            write!(w, "@h")?;
        }
        ObjRelocKind::PpcAddr16Ha => {
            write!(w, "@ha")?;
        }
        ObjRelocKind::PpcAddr16Lo => {
            write!(w, "@l")?;
        }
        ObjRelocKind::PpcEmbSda21 => {
            write!(w, "@sda21")?;
        }
    }
    Ok(())
}

fn write_symbol_entry<W: Write>(
    w: &mut W,
    symbols: &[ObjSymbol],
    entry: &SymbolEntry,
) -> Result<()> {
    let symbol = &symbols[entry.index];

    // Skip writing certain symbols
    if symbol.kind == ObjSymbolKind::Section {
        return Ok(());
    }

    let symbol_kind = match symbol.kind {
        ObjSymbolKind::Function => "fn",
        ObjSymbolKind::Object => "obj",
        ObjSymbolKind::Unknown => "sym",
        ObjSymbolKind::Section => bail!("Attempted to write section symbol: {symbol:?}"),
    };
    let scope = if symbol.flags.is_weak() {
        "weak"
    } else if symbol.flags.is_local() {
        "local"
    } else {
        // Default to global
        "global"
    };

    match entry.kind {
        SymbolEntryKind::Label => {
            if symbol.name.starts_with(".L") {
                write_symbol_name(w, &symbol.name)?;
                writeln!(w, ":")?;
            } else {
                write!(w, ".sym ")?;
                write_symbol_name(w, &symbol.name)?;
                writeln!(w, ", {scope}")?;
            }
        }
        SymbolEntryKind::Start => {
            if symbol.kind != ObjSymbolKind::Unknown {
                writeln!(w)?;
            }
            if let Some(name) = &symbol.demangled_name {
                writeln!(w, "# {name}")?;
            }
            write!(w, ".{symbol_kind} ")?;
            write_symbol_name(w, &symbol.name)?;
            writeln!(w, ", {scope}")?;
        }
        SymbolEntryKind::End => {
            write!(w, ".end{symbol_kind} ")?;
            write_symbol_name(w, &symbol.name)?;
            writeln!(w)?;
        }
    }

    if matches!(entry.kind, SymbolEntryKind::Start | SymbolEntryKind::Label)
        && symbol.flags.is_hidden()
    {
        write!(w, ".hidden ")?;
        write_symbol_name(w, &symbol.name)?;
        writeln!(w)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_data<W: Write>(
    w: &mut W,
    symbols: &[ObjSymbol],
    entries: &BTreeMap<u32, Vec<SymbolEntry>>,
    relocations: &BTreeMap<u32, ObjReloc>,
    section: &ObjSection,
    start: u32,
    end: u32,
    section_entries: &[BTreeMap<u32, Vec<SymbolEntry>>],
) -> Result<()> {
    let mut entry_iter = entries.range(start..end);
    let mut reloc_iter = relocations.range(start..end);

    let mut current_address = start;
    let mut current_symbol_kind = ObjSymbolKind::Unknown;
    let mut current_data_kind = ObjDataKind::Unknown;
    let mut entry = entry_iter.next();
    let mut reloc = reloc_iter.next();
    let mut begin = true;
    loop {
        if current_address == end {
            break;
        }
        if let Some((sym_addr, vec)) = entry {
            if current_address == *sym_addr {
                for entry in vec {
                    if entry.kind == SymbolEntryKind::End && begin {
                        continue;
                    }
                    write_symbol_entry(w, symbols, entry)?;
                }
                current_symbol_kind = find_symbol_kind(current_symbol_kind, symbols, vec)?;
                current_data_kind = find_data_kind(current_data_kind, symbols, vec)?;
                entry = entry_iter.next();
            }
        }
        begin = false;

        let symbol_kind = if current_symbol_kind == ObjSymbolKind::Unknown {
            match section.kind {
                ObjSectionKind::Code => ObjSymbolKind::Function,
                ObjSectionKind::Data | ObjSectionKind::ReadOnlyData | ObjSectionKind::Bss => {
                    ObjSymbolKind::Object
                }
            }
        } else {
            current_symbol_kind
        };
        if let Some((reloc_addr, r)) = reloc {
            if current_address == *reloc_addr {
                reloc = reloc_iter.next();
                match symbol_kind {
                    ObjSymbolKind::Object => {
                        current_address =
                            write_data_reloc(w, symbols, entries, r, section_entries)?;
                        continue;
                    }
                    ObjSymbolKind::Function => {
                        // handled in write_code_chunk
                    }
                    ObjSymbolKind::Unknown | ObjSymbolKind::Section => unreachable!(),
                }
            }
        }

        let until = match (entry, reloc) {
            (Some((sym_addr, _)), Some((reloc_addr, _))) => min(*reloc_addr, *sym_addr),
            (Some((addr, _)), None) | (None, Some((addr, _))) => *addr,
            (None, None) => end,
        };
        let data = &section.data[(current_address - section.address as u32) as usize
            ..(until - section.address as u32) as usize];
        if symbol_kind == ObjSymbolKind::Function {
            ensure!(
                current_address & 3 == 0 && data.len() & 3 == 0,
                "Unaligned code write @ {} {:#010X} size {:#X} (next entry: {:?}, reloc: {:?})",
                section.name,
                current_address,
                data.len(),
                entry,
                reloc,
            );
            write_code_chunk(w, symbols, entries, relocations, section, current_address, data)?;
        } else {
            write_data_chunk(w, data, current_data_kind)?;
        }
        current_address = until;
    }
    Ok(())
}

fn find_symbol_kind(
    current: ObjSymbolKind,
    symbols: &[ObjSymbol],
    entries: &Vec<SymbolEntry>,
) -> Result<ObjSymbolKind> {
    let mut kind = current;
    let mut found = false;
    for entry in entries {
        match entry.kind {
            SymbolEntryKind::Start => {
                let new_kind = symbols[entry.index].kind;
                if !matches!(new_kind, ObjSymbolKind::Unknown | ObjSymbolKind::Section) {
                    ensure!(
                        !found || new_kind == kind,
                        "Conflicting symbol kinds found: {kind:?} and {new_kind:?}"
                    );
                    kind = new_kind;
                    found = true;
                }
            }
            _ => continue,
        }
    }
    Ok(kind)
}

fn find_data_kind(
    current_data_kind: ObjDataKind,
    symbols: &[ObjSymbol],
    entries: &Vec<SymbolEntry>,
) -> Result<ObjDataKind> {
    let mut kind = ObjDataKind::Unknown;
    let mut found = false;
    for entry in entries {
        match entry.kind {
            SymbolEntryKind::Start => {
                let new_kind = symbols[entry.index].data_kind;
                if !matches!(new_kind, ObjDataKind::Unknown) {
                    ensure!(
                        !found || new_kind == kind,
                        "Conflicting data kinds found: {kind:?} and {new_kind:?}"
                    );
                    found = true;
                    kind = new_kind;
                }
            }
            SymbolEntryKind::Label => {
                // If type is a local label, don't change data types
                if !found {
                    kind = current_data_kind;
                }
            }
            _ => continue,
        }
    }
    Ok(kind)
}

fn write_string<W: Write>(w: &mut W, data: &[u8]) -> Result<()> {
    let terminated = matches!(data.last(), Some(&b) if b == 0);
    if terminated {
        write!(w, "\t.string \"")?;
    } else {
        write!(w, "\t.ascii \"")?;
    }
    for &b in &data[..data.len() - if terminated { 1 } else { 0 }] {
        match b as char {
            '\x08' => write!(w, "\\b")?,
            '\x09' => write!(w, "\\t")?,
            '\x0A' => write!(w, "\\n")?,
            '\x0C' => write!(w, "\\f")?,
            '\x0D' => write!(w, "\\r")?,
            '\\' => write!(w, "\\\\")?,
            '"' => write!(w, "\\\"")?,
            c if c.is_ascii_graphic() || c.is_ascii_whitespace() => write!(w, "{}", c)?,
            _ => write!(w, "\\{:03o}", b)?,
        }
    }
    writeln!(w, "\"")?;
    Ok(())
}

fn write_string16<W: Write>(w: &mut W, data: &[u16]) -> Result<()> {
    if matches!(data.last(), Some(&b) if b == 0) {
        write!(w, "\t.string16 \"")?;
    } else {
        bail!("Non-terminated UTF-16 string");
    }
    if data.len() > 1 {
        for result in std::char::decode_utf16(data[..data.len() - 1].iter().cloned()) {
            let c = match result {
                Ok(c) => c,
                Err(_) => bail!("Failed to decode UTF-16"),
            };
            match c {
                '\x08' => write!(w, "\\b")?,
                '\x09' => write!(w, "\\t")?,
                '\x0A' => write!(w, "\\n")?,
                '\x0C' => write!(w, "\\f")?,
                '\x0D' => write!(w, "\\r")?,
                '\\' => write!(w, "\\\\")?,
                '"' => write!(w, "\\\"")?,
                c if c.is_ascii_graphic() || c.is_ascii_whitespace() => write!(w, "{}", c)?,
                _ => write!(w, "\\{:#X}", c as u32)?,
            }
        }
    }
    writeln!(w, "\"")?;
    Ok(())
}

fn write_data_chunk<W: Write>(w: &mut W, data: &[u8], data_kind: ObjDataKind) -> Result<()> {
    let remain = data;
    match data_kind {
        ObjDataKind::String => {
            return write_string(w, data);
        }
        ObjDataKind::String16 => {
            if data.len() % 2 != 0 {
                bail!("Attempted to write wstring with length {:#X}", data.len());
            }
            let data = data
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes(c.try_into().unwrap()))
                .collect::<Vec<u16>>();
            return write_string16(w, &data);
        }
        ObjDataKind::StringTable => {
            for slice in data.split_inclusive(|&b| b == 0) {
                write_string(w, slice)?;
            }
            return Ok(());
        }
        ObjDataKind::String16Table => {
            if data.len() % 2 != 0 {
                bail!("Attempted to write wstring_table with length {:#X}", data.len());
            }
            let data = data
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes(c.try_into().unwrap()))
                .collect::<Vec<u16>>();
            for slice in data.split_inclusive(|&b| b == 0) {
                write_string16(w, slice)?;
            }
            return Ok(());
        }
        _ => {}
    }
    let chunk_size = match data_kind {
        ObjDataKind::Byte2 => 2,
        ObjDataKind::Unknown | ObjDataKind::Byte4 | ObjDataKind::Float => 4,
        ObjDataKind::Byte | ObjDataKind::Byte8 | ObjDataKind::Double => 8,
        ObjDataKind::String
        | ObjDataKind::String16
        | ObjDataKind::StringTable
        | ObjDataKind::String16Table => unreachable!(),
    };
    for chunk in remain.chunks(chunk_size) {
        if data_kind == ObjDataKind::Byte || matches!(chunk.len(), 1 | 3 | 5..=7) {
            let bytes = chunk.iter().map(|c| format!("{:#04X}", c)).collect::<Vec<String>>();
            writeln!(w, "\t.byte {}", bytes.join(", "))?;
        } else {
            match chunk.len() {
                8 if data_kind == ObjDataKind::Double => {
                    let data = f64::from_be_bytes(chunk.try_into().unwrap());
                    if data.is_nan() {
                        let int_data = u64::from_be_bytes(chunk.try_into().unwrap());
                        writeln!(w, "\t.8byte {int_data:#018X} # {data}")?;
                    } else {
                        writeln!(w, "\t.double {data}")?;
                    }
                }
                8 => {
                    let data = u64::from_be_bytes(chunk.try_into().unwrap());
                    writeln!(w, "\t.8byte {data:#018X}")?;
                }
                4 if data_kind == ObjDataKind::Float => {
                    let data = f32::from_be_bytes(chunk.try_into().unwrap());
                    if data.is_nan() {
                        let int_data = u32::from_be_bytes(chunk.try_into().unwrap());
                        writeln!(w, "\t.4byte {int_data:#010X} # {data}")?;
                    } else {
                        writeln!(w, "\t.float {data}")?;
                    }
                }
                4 => {
                    let data = u32::from_be_bytes(chunk.try_into().unwrap());
                    writeln!(w, "\t.4byte {data:#010X}")?;
                }
                2 => {
                    writeln!(w, "\t.2byte {:#06X}", u16::from_be_bytes(chunk.try_into().unwrap()))?;
                }
                _ => unreachable!(),
            }
        }
    }
    Ok(())
}

fn write_data_reloc<W: Write>(
    w: &mut W,
    symbols: &[ObjSymbol],
    _entries: &BTreeMap<u32, Vec<SymbolEntry>>,
    reloc: &ObjReloc,
    section_entries: &[BTreeMap<u32, Vec<SymbolEntry>>],
) -> Result<u32> {
    match reloc.kind {
        ObjRelocKind::Absolute => {
            // Attempt to use .rel macro for relative relocations
            if reloc.addend != 0 {
                let target = &symbols[reloc.target_symbol];
                let target_addr = (target.address as i64 + reloc.addend) as u32;
                if let Some(entry) = target
                    .section
                    .and_then(|section_idx| section_entries[section_idx].get(&target_addr))
                    .and_then(|entries| entries.iter().find(|e| e.kind == SymbolEntryKind::Label))
                {
                    let symbol = &symbols[entry.index];
                    write!(w, "\t.rel ")?;
                    write_symbol_name(w, &target.name)?;
                    write!(w, ", ")?;
                    write_symbol_name(w, &symbol.name)?;
                    writeln!(w)?;
                    return Ok((reloc.address + 4) as u32);
                }
            }
            write!(w, "\t.4byte ")?;
            write_reloc_symbol(w, symbols, reloc)?;
            writeln!(w)?;
            Ok((reloc.address + 4) as u32)
        }
        _ => Err(anyhow!(
            "Unsupported data relocation type {:?} @ {:#010X}",
            reloc.kind,
            reloc.address
        )),
    }
}

fn write_bss<W: Write>(
    w: &mut W,
    symbols: &[ObjSymbol],
    entries: &BTreeMap<u32, Vec<SymbolEntry>>,
    start: u32,
    end: u32,
) -> Result<()> {
    let mut entry_iter = entries.range(start..end);

    let mut current_address = start;
    let mut entry = entry_iter.next();
    let mut begin = true;
    loop {
        if current_address == end {
            break;
        }
        if let Some((sym_addr, vec)) = entry {
            if current_address == *sym_addr {
                for entry in vec {
                    if entry.kind == SymbolEntryKind::End && begin {
                        continue;
                    }
                    write_symbol_entry(w, symbols, entry)?;
                }
                entry = entry_iter.next();
            }
        }
        begin = false;

        let until = entry.map(|(addr, _)| *addr).unwrap_or(end);
        let size = until - current_address;
        if size > 0 {
            writeln!(w, "\t.skip {size:#X}")?;
        }
        current_address = until;
    }
    Ok(())
}

fn write_section_header<W: Write>(
    w: &mut W,
    section: &ObjSection,
    subsection: usize,
    start: u32,
    end: u32,
) -> Result<()> {
    writeln!(
        w,
        "\n# {:#010X} - {:#010X}",
        start as u64 + section.original_address,
        end as u64 + section.original_address
    )?;
    match section.name.as_str() {
        ".text" if subsection == 0 => {
            write!(w, "{}", section.name)?;
        }
        // .bss excluded to support < r40 devkitPro
        ".data" | ".rodata" if subsection == 0 => {
            write!(w, "{}", section.name)?;
        }
        ".text" | ".init" => {
            write!(w, ".section {}", section.name)?;
            write!(w, ", \"ax\"")?;
        }
        ".data" | ".sdata" => {
            write!(w, ".section {}", section.name)?;
            write!(w, ", \"wa\"")?;
        }
        ".rodata" | ".sdata2" => {
            write!(w, ".section {}", section.name)?;
            write!(w, ", \"a\"")?;
        }
        ".bss" | ".sbss" => {
            write!(w, ".section {}", section.name)?;
            write!(w, ", \"wa\", @nobits")?;
        }
        ".sbss2" => {
            write!(w, ".section {}", section.name)?;
            write!(w, ", \"a\", @nobits")?;
        }
        ".ctors" | ".dtors" | ".ctors$10" | ".dtors$10" | ".dtors$15" | "extab" | "extabindex" => {
            write!(w, ".section {}", section.name)?;
            write!(w, ", \"a\"")?;
        }
        ".comment" => {
            write!(w, ".section {}", section.name)?;
            write!(w, ", \"\"")?;
        }
        name => {
            log::warn!("Unknown section {name}");
            write!(w, ".section {}", section.name)?;
            if section.kind == ObjSectionKind::Bss {
                write!(w, ", \"\", @nobits")?;
            }
        }
    };
    if subsection != 0 {
        write!(w, ", unique, {subsection}")?;
    }
    writeln!(w)?;
    if section.align != 0 {
        writeln!(w, ".balign {}", section.align)?;
    }
    Ok(())
}

fn write_reloc_symbol<W: Write>(
    w: &mut W,
    symbols: &[ObjSymbol],
    reloc: &ObjReloc,
) -> std::io::Result<()> {
    write_symbol_name(w, &symbols[reloc.target_symbol].name)?;
    match reloc.addend.cmp(&0i64) {
        Ordering::Greater => write!(w, "+{:#X}", reloc.addend),
        Ordering::Less => write!(w, "-{:#X}", -reloc.addend),
        Ordering::Equal => Ok(()),
    }
}

fn write_symbol_name<W: Write>(w: &mut W, name: &str) -> std::io::Result<()> {
    if name.contains('@')
        || name.contains('<')
        || name.contains('\\')
        || name.contains('-')
        || name.contains('+')
    {
        write!(w, "\"{name}\"")?;
    } else {
        write!(w, "{name}")?;
    }
    Ok(())
}

#[inline]
fn is_illegal_instruction(code: u32) -> bool {
    matches!(code, 0x43000000 /* bc 24, lt, 0x0 */ | 0xB8030000 /* lmw r0, 0(r3) */)
}
