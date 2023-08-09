use std::{
    cmp::min,
    collections::{BTreeMap, HashMap, HashSet},
};

use anyhow::{anyhow, bail, ensure, Result};
use itertools::Itertools;
use petgraph::{graph::NodeIndex, Graph};

use crate::{
    obj::{
        ObjArchitecture, ObjInfo, ObjKind, ObjReloc, ObjSection, ObjSectionKind, ObjSplit,
        ObjSymbol, ObjSymbolFlagSet, ObjSymbolFlags, ObjSymbolKind, ObjSymbolScope, ObjUnit,
    },
    util::comment::MWComment,
};

/// Create splits for function pointers in the given section.
fn split_ctors_dtors(obj: &mut ObjInfo, section_start: u32, section_end: u32) -> Result<()> {
    let mut new_splits = BTreeMap::new();
    let mut current_address = section_start;
    let mut referenced_symbols = vec![];

    while current_address < section_end {
        let (section, chunk) = obj.section_data(current_address, current_address + 4)?;
        let function_addr = u32::from_be_bytes(chunk[0..4].try_into().unwrap());
        log::debug!("Found {} entry: {:#010X}", section.name, function_addr);

        let Some((function_symbol_idx, function_symbol)) =
            obj.symbols.kind_at_address(function_addr, ObjSymbolKind::Function)?
        else {
            bail!("Failed to find function symbol @ {:#010X}", function_addr);
        };
        referenced_symbols.push(function_symbol_idx);

        let ctors_split = obj.split_for(current_address);
        let function_split = obj.split_for(function_addr);

        let mut expected_unit = None;
        if let Some((_, ctors_split)) = ctors_split {
            expected_unit = Some(ctors_split.unit.clone());
        }
        if let Some((_, function_split)) = function_split {
            if let Some(unit) = &expected_unit {
                ensure!(
                    unit == &function_split.unit,
                    "Mismatched splits for {} {:#010X} ({}) and function {:#010X} ({})",
                    section.name,
                    current_address,
                    unit,
                    function_addr,
                    function_split.unit
                );
            } else {
                expected_unit = Some(function_split.unit.clone());
            }
        }

        if ctors_split.is_none() || function_split.is_none() {
            let unit = expected_unit.unwrap_or_else(|| {
                let section_name = function_symbol
                    .section
                    .and_then(|idx| obj.sections.get(idx).map(|s| s.name.clone()))
                    .unwrap_or_else(|| "unknown".to_string());
                format!("{}_{}", function_symbol.name, section_name.trim_start_matches('.'))
            });
            log::debug!("Adding splits to unit {}", unit);

            if ctors_split.is_none() {
                log::debug!("Adding split for {} entry @ {:#010X}", section.name, current_address);
                new_splits.insert(current_address, ObjSplit {
                    unit: unit.clone(),
                    end: current_address + 4,
                    align: None,
                    common: false,
                    autogenerated: true,
                });
            }
            if function_split.is_none() {
                log::debug!("Adding split for function @ {:#010X}", function_addr);
                new_splits.insert(function_addr, ObjSplit {
                    unit,
                    end: function_addr + function_symbol.size as u32,
                    align: None,
                    common: false,
                    autogenerated: true,
                });
            }
        }

        current_address += 4;
    }

    for (addr, split) in new_splits {
        obj.add_split(addr, split)?;
    }

    // Hack to avoid deadstripping
    for symbol_idx in referenced_symbols {
        obj.symbols.set_externally_referenced(symbol_idx, true);
    }

    Ok(())
}

/// Create splits for extabindex + extab entries.
fn split_extabindex(obj: &mut ObjInfo, section_index: usize, section_start: u32) -> Result<()> {
    let mut new_splits = BTreeMap::new();
    let (_, eti_init_info) = obj
        .symbols
        .by_name("_eti_init_info")?
        .ok_or_else(|| anyhow!("Failed to find _eti_init_info symbol"))?;
    ensure!(
        eti_init_info.section == Some(section_index),
        "_eti_init_info symbol in the wrong section: {:?} != {}",
        eti_init_info.section,
        section_index
    );
    let mut current_address = section_start;
    let section_end = eti_init_info.address as u32;
    while current_address < section_end {
        let (_eti_section, chunk) = obj.section_data(current_address, current_address + 12)?;
        let function_addr = u32::from_be_bytes(chunk[0..4].try_into().unwrap());
        let function_size = u32::from_be_bytes(chunk[4..8].try_into().unwrap());
        let extab_addr = u32::from_be_bytes(chunk[8..12].try_into().unwrap());
        log::debug!(
            "Found extabindex entry: {:#010X} size {:#010X} extab {:#010X}",
            function_addr,
            function_size,
            extab_addr
        );

        let Some((_, eti_symbol)) =
            obj.symbols.kind_at_address(current_address, ObjSymbolKind::Object)?
        else {
            bail!("Failed to find extabindex symbol @ {:#010X}", current_address);
        };
        ensure!(
            eti_symbol.size_known && eti_symbol.size == 12,
            "extabindex symbol {} has mismatched size ({:#X}, expected {:#X})",
            eti_symbol.name,
            eti_symbol.size,
            12
        );

        let Some((_, function_symbol)) =
            obj.symbols.kind_at_address(function_addr, ObjSymbolKind::Function)?
        else {
            bail!("Failed to find function symbol @ {:#010X}", function_addr);
        };
        ensure!(
            function_symbol.size_known && function_symbol.size == function_size as u64,
            "Function symbol {} has mismatched size ({:#X}, expected {:#X})",
            function_symbol.name,
            function_symbol.size,
            function_size
        );

        let Some((_, extab_symbol)) =
            obj.symbols.kind_at_address(extab_addr, ObjSymbolKind::Object)?
        else {
            bail!("Failed to find extab symbol @ {:#010X}", extab_addr);
        };
        ensure!(
            extab_symbol.size_known && extab_symbol.size > 0,
            "extab symbol {} has unknown size",
            extab_symbol.name
        );

        let extabindex_split = obj.split_for(current_address);
        let extab_split = obj.split_for(extab_addr);
        let function_split = obj.split_for(function_addr);

        let mut expected_unit = None;
        if let Some((_, extabindex_split)) = extabindex_split {
            expected_unit = Some(extabindex_split.unit.clone());
        }
        if let Some((_, extab_split)) = extab_split {
            if let Some(unit) = &expected_unit {
                ensure!(
                    unit == &extab_split.unit,
                    "Mismatched splits for extabindex {:#010X} ({}) and extab {:#010X} ({})",
                    current_address,
                    unit,
                    extab_addr,
                    extab_split.unit
                );
            } else {
                expected_unit = Some(extab_split.unit.clone());
            }
        }
        if let Some((_, function_split)) = function_split {
            if let Some(unit) = &expected_unit {
                ensure!(
                    unit == &function_split.unit,
                    "Mismatched splits for extabindex {:#010X} ({}) and function {:#010X} ({})",
                    current_address,
                    unit,
                    function_addr,
                    function_split.unit
                );
            } else {
                expected_unit = Some(function_split.unit.clone());
            }
        }

        if extabindex_split.is_none() || extab_split.is_none() || function_split.is_none() {
            let unit = expected_unit.unwrap_or_else(|| {
                let section_name = function_symbol
                    .section
                    .and_then(|idx| obj.sections.get(idx).map(|s| s.name.clone()))
                    .unwrap_or_else(|| "unknown".to_string());
                format!("{}_{}", function_symbol.name, section_name.trim_start_matches('.'))
            });
            log::debug!("Adding splits to unit {}", unit);

            if extabindex_split.is_none() {
                let end = current_address + 12;
                log::debug!(
                    "Adding split for extabindex entry @ {:#010X}-{:#010X}",
                    current_address,
                    end
                );
                new_splits.insert(current_address, ObjSplit {
                    unit: unit.clone(),
                    end,
                    align: None,
                    common: false,
                    autogenerated: true,
                });
            }
            if extab_split.is_none() {
                let end = extab_addr + extab_symbol.size as u32;
                log::debug!("Adding split for extab @ {:#010X}-{:#010X}", extab_addr, end);
                new_splits.insert(extab_addr, ObjSplit {
                    unit: unit.clone(),
                    end,
                    align: None,
                    common: false,
                    autogenerated: true,
                });
            }
            if function_split.is_none() {
                let end = function_addr + function_symbol.size as u32;
                log::debug!("Adding split for function @ {:#010X}-{:#010X}", function_addr, end);
                new_splits.insert(function_addr, ObjSplit {
                    unit,
                    end,
                    align: None,
                    common: false,
                    autogenerated: true,
                });
            }
        }

        current_address += 12;
    }

    for (addr, split) in new_splits {
        obj.add_split(addr, split)?;
    }

    Ok(())
}

/// Create splits for gaps between existing splits.
fn create_gap_splits(obj: &mut ObjInfo) -> Result<()> {
    let mut new_splits = BTreeMap::new();

    for (section_idx, section) in obj.sections.iter().enumerate() {
        let mut current_address = section.address as u32;
        let section_end = end_for_section(obj, section_idx)?;
        let mut file_iter = obj.splits_for_range(current_address..section_end).peekable();

        log::debug!(
            "Checking splits for section {} ({:#010X}..{:#010X})",
            section.name,
            current_address,
            section_end
        );
        loop {
            if current_address >= section_end {
                break;
            }

            let (split_start, split_end) = match file_iter.peek() {
                Some(&(addr, split)) => {
                    log::debug!("Found split {} ({:#010X}..{:#010X})", split.unit, addr, split.end);
                    (addr, split.end)
                }
                None => (section_end, 0),
            };
            ensure!(
                split_start >= current_address,
                "Split {:#010X}..{:#010X} overlaps with previous split",
                split_start,
                split_end
            );

            if split_start > current_address {
                // Find any duplicate symbols in this range
                let mut new_split_end = split_start;
                let symbols = obj.symbols.for_range(current_address..split_start).collect_vec();
                let mut existing_symbols = HashSet::new();
                for (_, symbol) in symbols {
                    // Sanity check? Maybe not required?
                    ensure!(
                        symbol.section == Some(section_idx),
                        "Expected symbol {} to be in section {}",
                        symbol.name,
                        section_idx
                    );
                    if !existing_symbols.insert(symbol.name.clone()) {
                        log::debug!(
                            "Found duplicate symbol {} at {:#010X}",
                            symbol.name,
                            symbol.address
                        );
                        new_split_end = symbol.address as u32;
                        break;
                    }
                }

                log::debug!(
                    "Creating split from {:#010X}..{:#010X}",
                    current_address,
                    new_split_end
                );
                let unit =
                    format!("{:08X}_{}", current_address, section.name.trim_start_matches('.'));
                new_splits.insert(current_address, ObjSplit {
                    unit: unit.clone(),
                    end: new_split_end,
                    align: None,
                    common: false,
                    autogenerated: true,
                });
                current_address = new_split_end;
                continue;
            }

            file_iter.next();
            if split_end > 0 {
                current_address = split_end;
            } else {
                let mut file_end = section_end;
                if let Some(&(next_addr, _next_split)) = file_iter.peek() {
                    file_end = min(next_addr, section_end);
                }
                current_address = file_end;
            }
        }
    }

    // Add new splits
    for (addr, split) in new_splits {
        obj.add_split(addr, split)?;
    }

    Ok(())
}

/// Ensures that all .bss splits following a common split are also marked as common.
fn update_common_splits(obj: &mut ObjInfo) -> Result<()> {
    let Some(bss_section) = obj.sections.iter().find(|s| s.name == ".bss") else {
        return Ok(());
    };
    let bss_section_start = bss_section.address as u32;
    let bss_section_end = (bss_section.address + bss_section.size) as u32;
    let Some(common_bss_start) = obj
        .splits_for_range(bss_section_start..bss_section_end)
        .find(|(_, split)| split.common)
        .map(|(addr, _)| addr)
    else {
        return Ok(());
    };
    log::debug!("Found common BSS start at {:#010X}", common_bss_start);
    for (addr, vec) in obj.splits.range_mut(common_bss_start..bss_section_end) {
        for split in vec {
            if !split.common {
                split.common = true;
                log::debug!("Added common flag to split {} at {:#010X}", split.unit, addr);
            }
        }
    }
    Ok(())
}

/// Perform any necessary adjustments to allow relinking.
/// This includes:
/// - Ensuring .ctors & .dtors entries are split with their associated function
/// - Ensuring extab & extabindex entries are split with their associated function
/// - Creating splits for gaps between existing splits
/// - Resolving a new object link order
pub fn update_splits(obj: &mut ObjInfo) -> Result<()> {
    // Create splits for extab and extabindex entries
    if let Some(section) = obj.sections.iter().find(|s| s.name == "extabindex") {
        split_extabindex(obj, section.index, section.address as u32)?;
    }

    // Create splits for .ctors entries
    if let Some(section) = obj.sections.iter().find(|s| s.name == ".ctors") {
        let section_start = section.address as u32;
        let section_end = section.address as u32 + section.size as u32 - 4;
        split_ctors_dtors(obj, section_start, section_end)?;
    }

    // Create splits for .dtors entries
    if let Some(section) = obj.sections.iter().find(|s| s.name == ".dtors") {
        let section_start = section.address as u32 + 4; // skip __destroy_global_chain_reference
        let section_end = section.address as u32 + section.size as u32 - 4;
        split_ctors_dtors(obj, section_start, section_end)?;
    }

    // Create gap splits
    create_gap_splits(obj)?;

    // Update common BSS splits
    update_common_splits(obj)?;

    // Resolve link order
    obj.link_order = resolve_link_order(obj)?;

    Ok(())
}

/// The ordering of TUs inside of each section represents a directed edge in a DAG.
/// We can use a topological sort to determine a valid global TU order.
/// There can be ambiguities, but any solution that satisfies the link order
/// constraints is considered valid.
fn resolve_link_order(obj: &ObjInfo) -> Result<Vec<ObjUnit>> {
    #[allow(dead_code)]
    #[derive(Debug, Copy, Clone)]
    struct SplitEdge {
        from: u32,
        to: u32,
    }

    let mut graph = Graph::<String, SplitEdge>::new();
    let mut unit_to_index_map = BTreeMap::<String, NodeIndex>::new();
    for (_, split) in obj.splits_for_range(..) {
        unit_to_index_map.insert(split.unit.clone(), NodeIndex::new(0));
    }
    for (unit, index) in unit_to_index_map.iter_mut() {
        *index = graph.add_node(unit.clone());
    }

    for section in &obj.sections {
        let mut iter = obj
            .splits_for_range(section.address as u32..(section.address + section.size) as u32)
            .peekable();
        if section.name == ".ctors" || section.name == ".dtors" {
            // Skip __init_cpp_exceptions.o
            let skipped = iter.next();
            log::debug!("Skipping split {:?} (next: {:?})", skipped, iter.peek());
        }
        while let (Some((a_addr, a)), Some(&(b_addr, b))) = (iter.next(), iter.peek()) {
            if !a.common && b.common {
                // This marks the beginning of the common BSS section.
                continue;
            }

            if a.unit != b.unit {
                log::debug!(
                    "Adding dependency {} ({:#010X}) -> {} ({:#010X})",
                    a.unit,
                    a_addr,
                    b.unit,
                    b_addr
                );
                let a_index = *unit_to_index_map.get(&a.unit).unwrap();
                let b_index = *unit_to_index_map.get(&b.unit).unwrap();
                graph.add_edge(a_index, b_index, SplitEdge { from: a_addr, to: b_addr });
            }
        }
    }

    // use petgraph::{
    //     dot::{Config, Dot},
    //     graph::EdgeReference,
    // };
    // let get_edge_attributes = |_, e: EdgeReference<SplitEdge>| {
    //     let &SplitEdge { from, to } = e.weight();
    //     let section_name = &obj.section_at(from).unwrap().name;
    //     format!("label=\"{} {:#010X} -> {:#010X}\"", section_name, from, to)
    // };
    // let dot = Dot::with_attr_getters(
    //     &graph,
    //     &[Config::EdgeNoLabel, Config::NodeNoLabel],
    //     &get_edge_attributes,
    //     &|_, (_, s)| format!("label=\"{}\"", s),
    // );
    // println!("{:?}", dot);

    match petgraph::algo::toposort(&graph, None) {
        Ok(vec) => Ok(vec
            .iter()
            .map(|&idx| {
                let name = &graph[idx];
                if let Some(existing) = obj.link_order.iter().find(|u| &u.name == name) {
                    existing.clone()
                } else {
                    ObjUnit {
                        name: name.clone(),
                        autogenerated: obj.is_unit_autogenerated(name),
                        comment_version: None,
                    }
                }
            })
            .collect_vec()),
        Err(e) => Err(anyhow!(
            "Cyclic dependency (involving {}) encountered while resolving link order",
            graph[e.node_id()]
        )),
    }
}

/// Split an executable object into relocatable objects.
pub fn split_obj(obj: &ObjInfo) -> Result<Vec<ObjInfo>> {
    ensure!(obj.kind == ObjKind::Executable, "Expected executable object");

    let mut objects: Vec<ObjInfo> = vec![];
    let mut object_symbols: Vec<Vec<Option<usize>>> = vec![];
    let mut name_to_obj: HashMap<String, usize> = HashMap::new();
    for unit in &obj.link_order {
        name_to_obj.insert(unit.name.clone(), objects.len());
        object_symbols.push(vec![None; obj.symbols.count()]);
        let mut split_obj = ObjInfo::new(
            ObjKind::Relocatable,
            ObjArchitecture::PowerPc,
            unit.name.clone(),
            vec![],
            vec![],
        );
        if let Some(comment_version) = unit.comment_version {
            split_obj.mw_comment = Some(MWComment::new(comment_version)?);
        } else {
            split_obj.mw_comment = obj.mw_comment.clone();
        }
        objects.push(split_obj);
    }

    for (section_idx, section) in obj.sections.iter().enumerate() {
        let mut current_address = section.address as u32;
        let section_end = end_for_section(obj, section_idx)?;
        let mut file_iter = obj.splits_for_range(current_address..section_end).peekable();

        // Build address to relocation / address to symbol maps
        let relocations = section.build_relocation_map()?;

        loop {
            if current_address >= section_end {
                break;
            }

            let (file_addr, split) = match file_iter.next() {
                Some((addr, split)) => (addr, split),
                None => bail!("No file found"),
            };
            ensure!(
                file_addr <= current_address,
                "Gap in files: {} @ {:#010X}, {} @ {:#010X}",
                section.name,
                section.address,
                split.unit,
                file_addr
            );
            let mut file_end = section_end;
            if let Some(&(next_addr, _next_split)) = file_iter.peek() {
                file_end = min(next_addr, section_end);
            }

            let file = name_to_obj
                .get(&split.unit)
                .and_then(|&idx| objects.get_mut(idx))
                .ok_or_else(|| anyhow!("Unit '{}' not in link order", split.unit))?;
            let symbol_idxs = name_to_obj
                .get(&split.unit)
                .and_then(|&idx| object_symbols.get_mut(idx))
                .ok_or_else(|| anyhow!("Unit '{}' not in link order", split.unit))?;

            // Calculate & verify section alignment
            let mut align =
                split.align.map(u64::from).unwrap_or_else(|| default_section_align(section));
            if current_address & (align as u32 - 1) != 0 {
                log::warn!(
                    "Alignment for {} {} expected {}, but starts at {:#010X}",
                    split.unit,
                    section.name,
                    align,
                    current_address
                );
                while align > 4 {
                    align /= 2;
                    if current_address & (align as u32 - 1) == 0 {
                        break;
                    }
                }
            }
            ensure!(
                current_address & (align as u32 - 1) == 0,
                "Invalid alignment for split: {} {} {:#010X}",
                split.unit,
                section.name,
                current_address
            );

            // Collect relocations; target_symbol will be updated later
            let out_relocations = relocations
                .range(current_address..file_end)
                .map(|(_, &idx)| {
                    let o = &section.relocations[idx];
                    ObjReloc {
                        kind: o.kind,
                        address: o.address - current_address as u64,
                        target_symbol: o.target_symbol,
                        addend: o.addend,
                    }
                })
                .collect();

            // Add section symbols
            let out_section_idx = file.sections.len();
            let mut comm_addr = current_address;
            for (symbol_idx, symbol) in obj.symbols.for_range(current_address..file_end) {
                if symbol_idxs[symbol_idx].is_some() {
                    continue; // should never happen?
                }

                if split.common && symbol.address as u32 > comm_addr {
                    // HACK: Add padding for common bug
                    file.symbols.add_direct(ObjSymbol {
                        name: format!("pad_{:010X}", comm_addr),
                        demangled_name: None,
                        address: 0,
                        section: None,
                        size: symbol.address - comm_addr as u64,
                        size_known: true,
                        flags: ObjSymbolFlagSet(ObjSymbolFlags::Common.into()),
                        kind: ObjSymbolKind::Object,
                        align: Some(4),
                        data_kind: Default::default(),
                    })?;
                }
                comm_addr = (symbol.address + symbol.size) as u32;

                symbol_idxs[symbol_idx] = Some(file.symbols.count());
                file.symbols.add_direct(ObjSymbol {
                    name: symbol.name.clone(),
                    demangled_name: symbol.demangled_name.clone(),
                    address: if split.common { 4 } else { symbol.address - current_address as u64 },
                    section: if split.common { None } else { Some(out_section_idx) },
                    size: symbol.size,
                    size_known: symbol.size_known,
                    flags: if split.common {
                        ObjSymbolFlagSet(ObjSymbolFlags::Common.into())
                    } else {
                        symbol.flags
                    },
                    kind: symbol.kind,
                    align: if split.common { Some(4) } else { symbol.align },
                    data_kind: symbol.data_kind,
                })?;
            }

            // For mwldeppc 2.7 and above, a .comment section is required to link without error
            // when common symbols are present. Automatically add one if needed.
            if split.common && file.mw_comment.is_none() {
                file.mw_comment = Some(MWComment::new(8)?);
            }

            if !split.common {
                let data = match section.kind {
                    ObjSectionKind::Bss => vec![],
                    _ => section.data[(current_address as u64 - section.address) as usize
                        ..(file_end as u64 - section.address) as usize]
                        .to_vec(),
                };
                let name = if let Some(name) = obj.named_sections.get(&current_address) {
                    name.clone()
                } else {
                    section.name.clone()
                };
                file.sections.push(ObjSection {
                    name,
                    kind: section.kind,
                    address: 0,
                    size: file_end as u64 - current_address as u64,
                    data,
                    align,
                    index: out_section_idx,
                    elf_index: out_section_idx + 1,
                    relocations: out_relocations,
                    original_address: current_address as u64,
                    file_offset: section.file_offset + (current_address as u64 - section.address),
                    section_known: true,
                });
            }

            current_address = file_end;
        }
    }

    // Update relocations
    let mut globalize_symbols = vec![];
    for (obj_idx, out_obj) in objects.iter_mut().enumerate() {
        let symbol_idxs = &mut object_symbols[obj_idx];
        for section in &mut out_obj.sections {
            for reloc in &mut section.relocations {
                match symbol_idxs[reloc.target_symbol] {
                    Some(out_sym_idx) => {
                        reloc.target_symbol = out_sym_idx;
                    }
                    None => {
                        // Extern
                        let out_sym_idx = out_obj.symbols.count();
                        let target_sym = obj.symbols.at(reloc.target_symbol);

                        // If the symbol is local, we'll upgrade the scope to global
                        // and rename it to avoid conflicts
                        if target_sym.flags.is_local() {
                            let address_str = format!("{:08X}", target_sym.address);
                            let new_name = if target_sym.name.ends_with(&address_str) {
                                target_sym.name.clone()
                            } else {
                                format!("{}_{}", target_sym.name, address_str)
                            };
                            globalize_symbols.push((reloc.target_symbol, new_name));
                        }

                        symbol_idxs[reloc.target_symbol] = Some(out_sym_idx);
                        out_obj.symbols.add_direct(ObjSymbol {
                            name: target_sym.name.clone(),
                            demangled_name: target_sym.demangled_name.clone(),
                            ..Default::default()
                        })?;
                        reloc.target_symbol = out_sym_idx;

                        if section.name.as_str() == "extabindex" {
                            let Some((target_addr, target_split)) =
                                obj.split_for(target_sym.address as u32)
                            else {
                                bail!(
                                    "Bad extabindex relocation @ {:#010X}",
                                    reloc.address + section.original_address
                                );
                            };
                            let target_section = &obj.section_at(target_addr)?.name;
                            log::error!(
                                "Bad extabindex relocation @ {:#010X}\n\
                                \tSource object: {}:{:#010X} ({})\n\
                                \tTarget object: {}:{:#010X} ({})\n\
                                \tTarget symbol: {:#010X} ({})\n\
                                This will cause the linker to crash.\n",
                                reloc.address + section.original_address,
                                section.name,
                                section.original_address,
                                out_obj.name,
                                target_section,
                                target_addr,
                                target_split.unit,
                                target_sym.address,
                                target_sym.demangled_name.as_deref().unwrap_or(&target_sym.name),
                            );
                        }
                    }
                }
            }
        }
    }

    // Upgrade local symbols to global if necessary
    for (obj, symbol_map) in objects.iter_mut().zip(&object_symbols) {
        for (globalize_idx, new_name) in &globalize_symbols {
            if let Some(symbol_idx) = symbol_map[*globalize_idx] {
                let mut symbol = obj.symbols.at(symbol_idx).clone();
                symbol.name = new_name.clone();
                if symbol.flags.is_local() {
                    log::debug!("Globalizing {} in {}", symbol.name, obj.name);
                    symbol.flags.set_scope(ObjSymbolScope::Global);
                }
                obj.symbols.replace(symbol_idx, symbol)?;
            }
        }
    }

    // Extern linker generated symbols
    for obj in &mut objects {
        let mut replace_symbols = vec![];
        for (symbol_idx, symbol) in obj.symbols.iter().enumerate() {
            if is_linker_generated_label(&symbol.name) && symbol.section.is_some() {
                log::debug!("Externing {:?} in {}", symbol, obj.name);
                replace_symbols.push((symbol_idx, ObjSymbol {
                    name: symbol.name.clone(),
                    demangled_name: symbol.demangled_name.clone(),
                    ..Default::default()
                }));
            }
        }
        for (symbol_idx, symbol) in replace_symbols {
            obj.symbols.replace(symbol_idx, symbol)?;
        }
    }

    Ok(objects)
}

/// mwld doesn't preserve the original section alignment values
pub fn default_section_align(section: &ObjSection) -> u64 {
    match section.kind {
        ObjSectionKind::Code => 4,
        _ => match section.name.as_str() {
            ".ctors" | ".dtors" | "extab" | "extabindex" => 4,
            ".sbss" => 4, // ?
            _ => 8,
        },
    }
}

/// Linker-generated symbols to extern
#[inline]
pub fn is_linker_generated_label(name: &str) -> bool {
    matches!(
        name,
        "_ctors"
            | "_dtors"
            | "_f_init"
            | "_f_init_rom"
            | "_e_init"
            | "_fextab"
            | "_fextab_rom"
            | "_eextab"
            | "_fextabindex"
            | "_fextabindex_rom"
            | "_eextabindex"
            | "_f_text"
            | "_f_text_rom"
            | "_e_text"
            | "_f_ctors"
            | "_f_ctors_rom"
            | "_e_ctors"
            | "_f_dtors"
            | "_f_dtors_rom"
            | "_e_dtors"
            | "_f_rodata"
            | "_f_rodata_rom"
            | "_e_rodata"
            | "_f_data"
            | "_f_data_rom"
            | "_e_data"
            | "_f_sdata"
            | "_f_sdata_rom"
            | "_e_sdata"
            | "_f_sbss"
            | "_f_sbss_rom"
            | "_e_sbss"
            | "_f_sdata2"
            | "_f_sdata2_rom"
            | "_e_sdata2"
            | "_f_sbss2"
            | "_f_sbss2_rom"
            | "_e_sbss2"
            | "_f_bss"
            | "_f_bss_rom"
            | "_e_bss"
            | "_f_stack"
            | "_f_stack_rom"
            | "_e_stack"
            | "_stack_addr"
            | "_stack_end"
            | "_db_stack_addr"
            | "_db_stack_end"
            | "_heap_addr"
            | "_heap_end"
            | "_nbfunctions"
            | "SIZEOF_HEADERS"
            | "_SDA_BASE_"
            | "_SDA2_BASE_"
            | "_ABS_SDA_BASE_"
            | "_ABS_SDA2_BASE_"
    )
}

/// Linker generated objects to strip entirely
#[inline]
pub fn is_linker_generated_object(name: &str) -> bool {
    matches!(
        name,
        "_eti_init_info" | "_rom_copy_info" | "_bss_init_info" | "_ctors$99" | "_dtors$99"
    )
}

/// Locate the end address of a section when excluding linker generated objects
pub fn end_for_section(obj: &ObjInfo, section_index: usize) -> Result<u32> {
    let section = &obj.sections[section_index];
    let section_start = section.address as u32;
    let mut section_end = (section.address + section.size) as u32;
    // .ctors and .dtors end with a linker-generated null pointer,
    // adjust section size appropriately
    if matches!(section.name.as_str(), ".ctors" | ".dtors")
        && section.data[section.data.len() - 4..] == [0u8; 4]
    {
        section_end -= 4;
        return Ok(section_end);
    }
    loop {
        let last_symbol = obj
            .symbols
            .for_range(section_start..section_end)
            .filter(|(_, s)| s.kind == ObjSymbolKind::Object && s.size_known && s.size > 0)
            .last();
        match last_symbol {
            Some((_, symbol)) if is_linker_generated_object(&symbol.name) => {
                log::debug!(
                    "Found {}, adjusting section {} end {:#010X} -> {:#010X}",
                    section.name,
                    symbol.name,
                    section_end,
                    symbol.address
                );
                section_end = symbol.address as u32;
            }
            _ => break,
        }
    }
    Ok(section_end)
}
