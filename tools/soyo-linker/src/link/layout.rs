use std::collections::BTreeMap;

use native_abi::TargetArch;
use soyo::registry::{MAX_IMAGE_SIZE, SegmentKind};

use crate::elf::{ElfSection, ElfSymbol, ObjectFile, read_object};

use super::error::{LinkError, LinkErrorKind};
use super::model::{
    InputObject, LinkImage, LinkRequest, LinkSegment, LinkSymbol, PendingRelocation, SymbolValue,
};

const PAGE_SIZE: u64 = 4096;
const SHN_ABS: u16 = 0xfff1;
const SHN_COMMON: u16 = 0xfff2;
const SHT_GROUP: u32 = 17;
const SHF_GROUP: u64 = 0x200;
const EF_RISCV_ABI_MASK: u32 = 0x000e;
const EF_LARCH_ABI_MASK: u32 = 0x0047;
const STB_LOCAL: u8 = 0;
const STB_GLOBAL: u8 = 1;
const STB_WEAK: u8 = 2;

#[derive(Debug, Clone, Copy)]
struct SectionPlacement {
    kind: SegmentKind,
    offset: u64,
}

#[derive(Debug)]
struct SegmentBuilder {
    kind: SegmentKind,
    payload: Vec<u8>,
    memory_size: u64,
    alignment: u64,
}

impl SegmentBuilder {
    fn new(kind: SegmentKind) -> Self {
        Self {
            kind,
            payload: Vec::new(),
            memory_size: 0,
            alignment: if kind == SegmentKind::TlsTemplate {
                16
            } else {
                PAGE_SIZE
            },
        }
    }

    fn append(&mut self, section: &ElfSection<'_>) -> Result<u64, LinkError> {
        let alignment = section.alignment();
        let offset = align_up(self.memory_size, alignment).ok_or_else(|| {
            LinkError::new(LinkErrorKind::ImageTooLarge, "section 布局发生整数溢出")
        })?;
        if section.data().is_some() {
            let start = usize::try_from(offset).map_err(|_| {
                LinkError::new(LinkErrorKind::ImageTooLarge, "section 偏移超过宿主范围")
            })?;
            self.payload.resize(start, 0);
            self.payload.extend_from_slice(section.data().unwrap());
        }
        self.memory_size = offset
            .checked_add(section.size())
            .ok_or_else(|| LinkError::new(LinkErrorKind::ImageTooLarge, "section 大小溢出"))?;
        self.alignment = self.alignment.max(alignment);
        Ok(offset)
    }
}

pub fn build_link_image(request: LinkRequest<'_>) -> Result<LinkImage, LinkError> {
    if request.objects.is_empty() {
        return Err(LinkError::new(
            LinkErrorKind::EntryNotFound,
            "链接输入不能为空",
        ));
    }
    let objects = request
        .objects
        .iter()
        .map(parse_input)
        .collect::<Result<Vec<_>, _>>()?;
    for (input, object) in request.objects.iter().zip(&objects) {
        if object.target_arch() != request.target_arch {
            return Err(LinkError::in_object(
                LinkErrorKind::TargetMismatch,
                input.path(),
                "对象架构与 --target 不一致",
            ));
        }
    }
    validate_object_abis(request.objects, &objects, request.target_arch)?;

    let (mut segments, placements, image_virtual_size) =
        layout_sections(request.objects, &objects)?;
    let globals = collect_global_symbols(request.objects, &objects, &placements, &segments)?;
    reject_unresolved_globals(request.objects, &objects, &globals)?;
    let pending_relocations =
        collect_relocations(request.objects, &objects, &placements, &segments, &globals)?;

    let entry = globals.get(request.entry_symbol).ok_or_else(|| {
        LinkError::new(
            LinkErrorKind::EntryNotFound,
            format!("找不到入口符号 {}", request.entry_symbol),
        )
    })?;
    let entry_offset = match (entry.value, entry.segment_index) {
        (SymbolValue::Image(value), Some(index)) if segments[index].kind == SegmentKind::Code => {
            value
        }
        _ => {
            return Err(LinkError::new(
                LinkErrorKind::EntryNotCode,
                format!("入口符号 {} 不在 CODE 中", request.entry_symbol),
            ));
        }
    };
    let instruction_alignment = match request.target_arch {
        TargetArch::Riscv64 => 2,
        TargetArch::LoongArch64 => 4,
    };
    if entry_offset % instruction_alignment != 0 {
        return Err(LinkError::new(
            LinkErrorKind::EntryNotCode,
            format!("入口符号 {} 未满足指令对齐", request.entry_symbol),
        ));
    }

    // 保留可变段所有权，以便在同一映像上应用架构 relocation。
    segments.shrink_to_fit();
    Ok(LinkImage {
        target_arch: request.target_arch,
        entry_offset,
        image_virtual_size,
        segments,
        symbols: globals,
        pending_relocations,
    })
}

fn parse_input(input: &InputObject) -> Result<ObjectFile<'_>, LinkError> {
    read_object(input.path().to_path_buf(), input.bytes()).map_err(LinkError::from_elf)
}

fn validate_object_abis(
    inputs: &[InputObject],
    objects: &[ObjectFile<'_>],
    target_arch: TargetArch,
) -> Result<(), LinkError> {
    let mask = match target_arch {
        TargetArch::Riscv64 => EF_RISCV_ABI_MASK,
        TargetArch::LoongArch64 => EF_LARCH_ABI_MASK,
    };
    let expected = objects[0].flags() & mask;
    for (input, object) in inputs.iter().zip(objects).skip(1) {
        let actual = object.flags() & mask;
        if actual != expected {
            return Err(LinkError::in_object(
                LinkErrorKind::TargetMismatch,
                input.path(),
                format!("对象 ABI flags 0x{actual:x} 与首个输入 0x{expected:x} 不兼容"),
            ));
        }
    }
    Ok(())
}

fn layout_sections(
    inputs: &[InputObject],
    objects: &[ObjectFile<'_>],
) -> Result<(Vec<LinkSegment>, Vec<Vec<Option<SectionPlacement>>>, u64), LinkError> {
    let mut builders = [
        SegmentBuilder::new(SegmentKind::Code),
        SegmentBuilder::new(SegmentKind::Rodata),
        SegmentBuilder::new(SegmentKind::Data),
        SegmentBuilder::new(SegmentKind::Bss),
        SegmentBuilder::new(SegmentKind::TlsTemplate),
    ];
    let mut placements = objects
        .iter()
        .map(|object| vec![None; object.sections().len()])
        .collect::<Vec<_>>();

    for (object_index, object) in objects.iter().enumerate() {
        for section in object.sections().iter().skip(1) {
            if section.section_type() == SHT_GROUP || section.flags() & SHF_GROUP != 0 {
                return Err(LinkError::in_object(
                    LinkErrorKind::UnsupportedSection,
                    inputs[object_index].path(),
                    format!("不支持 COMDAT/section group {}", section.name()),
                ));
            }
            if !section.is_allocated() || section.size() == 0 {
                continue;
            }
            let kind = classify_section(inputs[object_index].path(), section)?;
            let builder = &mut builders[builder_index(kind)];
            let offset = builder.append(section)?;
            placements[object_index][section.index()] = Some(SectionPlacement { kind, offset });
        }
    }

    let mut segments = Vec::new();
    let mut cursor = 0u64;
    for builder in builders {
        if builder.memory_size == 0 {
            continue;
        }
        let virtual_offset = if builder.kind == SegmentKind::TlsTemplate {
            0
        } else {
            let offset = align_up(cursor, PAGE_SIZE)
                .ok_or_else(|| LinkError::new(LinkErrorKind::ImageTooLarge, "映像虚拟地址溢出"))?;
            cursor =
                offset
                    .checked_add(align_up(builder.memory_size, PAGE_SIZE).ok_or_else(|| {
                        LinkError::new(LinkErrorKind::ImageTooLarge, "映像大小溢出")
                    })?)
                    .ok_or_else(|| LinkError::new(LinkErrorKind::ImageTooLarge, "映像大小溢出"))?;
            offset
        };
        segments.push(LinkSegment {
            kind: builder.kind,
            virtual_offset,
            payload: builder.payload,
            memory_size: builder.memory_size,
            alignment: builder.alignment,
        });
    }
    if segments
        .first()
        .is_none_or(|segment| segment.kind != SegmentKind::Code)
    {
        return Err(LinkError::new(
            LinkErrorKind::InvalidSection,
            "链接映像缺少非空 CODE",
        ));
    }
    if cursor > MAX_IMAGE_SIZE {
        return Err(LinkError::new(
            LinkErrorKind::ImageTooLarge,
            "链接映像超过 SOYO 上限",
        ));
    }
    Ok((segments, placements, cursor))
}

fn classify_section(
    path: &std::path::Path,
    section: &ElfSection<'_>,
) -> Result<SegmentKind, LinkError> {
    let alignment = section.alignment();
    if alignment == 0 || !alignment.is_power_of_two() || alignment > PAGE_SIZE {
        return Err(LinkError::in_object(
            LinkErrorKind::InvalidSectionAlignment,
            path,
            format!("section {} 对齐无效", section.name()),
        ));
    }
    if section.is_writable() && section.is_executable() {
        return Err(LinkError::in_object(
            LinkErrorKind::WritableExecutableSection,
            path,
            format!("section {} 同时可写和可执行", section.name()),
        ));
    }
    let known_name =
        |base: &str| section.name() == base || section.name().starts_with(&format!("{base}."));
    let kind = if section.is_tls()
        && section.is_writable()
        && !section.is_executable()
        && (known_name(".tdata") || known_name(".tbss"))
    {
        SegmentKind::TlsTemplate
    } else if section.is_executable()
        && !section.is_writable()
        && !section.is_tls()
        && known_name(".text")
        && !section.is_nobits()
    {
        SegmentKind::Code
    } else if !section.is_writable()
        && !section.is_executable()
        && !section.is_tls()
        && (known_name(".rodata") || known_name(".srodata"))
        && !section.is_nobits()
    {
        SegmentKind::Rodata
    } else if section.is_writable()
        && !section.is_executable()
        && !section.is_tls()
        && (known_name(".data") || known_name(".sdata"))
        && !section.is_nobits()
    {
        SegmentKind::Data
    } else if section.is_writable()
        && !section.is_executable()
        && !section.is_tls()
        && (known_name(".bss") || known_name(".sbss"))
        && section.is_nobits()
    {
        SegmentKind::Bss
    } else {
        return Err(LinkError::in_object(
            LinkErrorKind::UnsupportedSection,
            path,
            format!("不支持 alloc section {}", section.name()),
        ));
    };
    Ok(kind)
}

fn collect_global_symbols(
    inputs: &[InputObject],
    objects: &[ObjectFile<'_>],
    placements: &[Vec<Option<SectionPlacement>>],
    segments: &[LinkSegment],
) -> Result<BTreeMap<String, LinkSymbol>, LinkError> {
    let mut globals = BTreeMap::new();
    for (object_index, object) in objects.iter().enumerate() {
        for symbol in object.symbols().iter().skip(1) {
            if symbol.binding() == STB_WEAK {
                return Err(symbol_error(
                    LinkErrorKind::WeakSymbol,
                    &inputs[object_index],
                    symbol,
                    "首版不支持 weak symbol",
                ));
            }
            if symbol.section_index() == SHN_COMMON {
                return Err(symbol_error(
                    LinkErrorKind::CommonSymbol,
                    &inputs[object_index],
                    symbol,
                    "首版不支持 COMMON symbol",
                ));
            }
            if symbol.binding() != STB_GLOBAL || symbol.is_undefined() {
                continue;
            }
            if symbol.name().is_empty() {
                return Err(symbol_error(
                    LinkErrorKind::InvalidSymbol,
                    &inputs[object_index],
                    symbol,
                    "全局定义缺少名称",
                ));
            }
            let resolved = resolve_defined_symbol(symbol, object_index, placements, segments)
                .ok_or_else(|| {
                    symbol_error(
                        LinkErrorKind::InvalidSymbol,
                        &inputs[object_index],
                        symbol,
                        "全局定义不在可链接 section 中",
                    )
                })?;
            if globals.insert(symbol.name().to_owned(), resolved).is_some() {
                return Err(symbol_error(
                    LinkErrorKind::DuplicateSymbol,
                    &inputs[object_index],
                    symbol,
                    "重复的全局强符号",
                ));
            }
        }
    }
    Ok(globals)
}

fn reject_unresolved_globals(
    inputs: &[InputObject],
    objects: &[ObjectFile<'_>],
    globals: &BTreeMap<String, LinkSymbol>,
) -> Result<(), LinkError> {
    for (object_index, object) in objects.iter().enumerate() {
        for symbol in object.symbols().iter().skip(1) {
            if symbol.binding() == STB_GLOBAL
                && symbol.is_undefined()
                && !symbol.name().is_empty()
                && !globals.contains_key(symbol.name())
            {
                return Err(symbol_error(
                    LinkErrorKind::UndefinedSymbol,
                    &inputs[object_index],
                    symbol,
                    "未解析的全局符号",
                ));
            }
        }
    }
    Ok(())
}

fn collect_relocations(
    inputs: &[InputObject],
    objects: &[ObjectFile<'_>],
    placements: &[Vec<Option<SectionPlacement>>],
    segments: &[LinkSegment],
    globals: &BTreeMap<String, LinkSymbol>,
) -> Result<Vec<PendingRelocation>, LinkError> {
    let mut pending = Vec::new();
    for (object_index, object) in objects.iter().enumerate() {
        for relocation in object.relocations() {
            let Some(target_placement) =
                placements[object_index][relocation.target_section_index()]
            else {
                continue;
            };
            let target_segment_index =
                segment_index(segments, target_placement.kind).ok_or_else(|| {
                    LinkError::new(LinkErrorKind::InvalidRelocation, "relocation 目标段不存在")
                })?;
            let target_offset = target_placement
                .offset
                .checked_add(relocation.offset())
                .ok_or_else(|| {
                    LinkError::new(LinkErrorKind::InvalidRelocation, "relocation 目标溢出")
                })?;
            let place_offset = if target_placement.kind == SegmentKind::TlsTemplate {
                target_offset
            } else {
                segments[target_segment_index]
                    .virtual_offset
                    .checked_add(target_offset)
                    .ok_or_else(|| {
                        LinkError::new(LinkErrorKind::InvalidRelocation, "relocation 地址溢出")
                    })?
            };
            let symbol = object
                .symbols()
                .get(relocation.symbol_index())
                .ok_or_else(|| {
                    LinkError::new(LinkErrorKind::InvalidRelocation, "relocation 符号越界")
                })?;
            let resolved =
                resolve_relocation_symbol(symbol, object_index, placements, segments, globals)
                    .ok_or_else(|| {
                        symbol_error(
                            LinkErrorKind::InvalidSymbol,
                            &inputs[object_index],
                            symbol,
                            "relocation 符号不可解析",
                        )
                    })?;
            pending.push(PendingRelocation {
                input_path: inputs[object_index].path().to_path_buf(),
                target_segment_index,
                target_offset,
                place_offset,
                kind: relocation.kind(),
                addend: relocation.addend(),
                symbol_name: symbol.name().to_owned(),
                symbol_value: resolved.value,
            });
        }
    }
    Ok(pending)
}

fn resolve_relocation_symbol(
    symbol: &ElfSymbol<'_>,
    object_index: usize,
    placements: &[Vec<Option<SectionPlacement>>],
    segments: &[LinkSegment],
    globals: &BTreeMap<String, LinkSymbol>,
) -> Option<LinkSymbol> {
    match symbol.binding() {
        STB_LOCAL => resolve_defined_symbol(symbol, object_index, placements, segments),
        STB_GLOBAL => globals.get(symbol.name()).cloned(),
        _ => None,
    }
}

fn resolve_defined_symbol(
    symbol: &ElfSymbol<'_>,
    object_index: usize,
    placements: &[Vec<Option<SectionPlacement>>],
    segments: &[LinkSegment],
) -> Option<LinkSymbol> {
    if symbol.section_index() == SHN_ABS {
        return Some(LinkSymbol {
            value: SymbolValue::Absolute(symbol.value()),
            segment_index: None,
            size: symbol.size(),
        });
    }
    let section_index = usize::from(symbol.section_index());
    let placement = placements.get(object_index)?.get(section_index)?.as_ref()?;
    let segment_index = segment_index(segments, placement.kind)?;
    let offset = placement.offset.checked_add(symbol.value())?;
    let value = if placement.kind == SegmentKind::TlsTemplate {
        SymbolValue::Tls(offset)
    } else {
        SymbolValue::Image(segments[segment_index].virtual_offset.checked_add(offset)?)
    };
    Some(LinkSymbol {
        value,
        segment_index: Some(segment_index),
        size: symbol.size(),
    })
}

fn symbol_error(
    kind: LinkErrorKind,
    input: &InputObject,
    symbol: &ElfSymbol<'_>,
    message: &str,
) -> LinkError {
    LinkError::in_object(kind, input.path(), format!("{message}: {}", symbol.name()))
}

fn builder_index(kind: SegmentKind) -> usize {
    match kind {
        SegmentKind::Code => 0,
        SegmentKind::Rodata => 1,
        SegmentKind::Data => 2,
        SegmentKind::Bss => 3,
        SegmentKind::TlsTemplate => 4,
    }
}

fn segment_index(segments: &[LinkSegment], kind: SegmentKind) -> Option<usize> {
    segments.iter().position(|segment| segment.kind == kind)
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    value
        .checked_add(alignment.checked_sub(1)?)
        .map(|sum| sum & !(alignment - 1))
}
