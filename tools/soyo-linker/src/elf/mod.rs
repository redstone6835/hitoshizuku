//! ELF64 ET_REL 的严格宿主侧读取器。

mod decode;
mod error;
mod model;

use std::path::PathBuf;

use decode::{bytes, i64_at, string_at, u16_at, u32_at, u64_at};
pub use error::{ElfError, ElfErrorKind};
pub use model::{ElfRelocation, ElfSection, ElfSymbol, ObjectFile, TargetArch};

const ELF_HEADER_SIZE: usize = 64;
const SECTION_HEADER_SIZE: usize = 64;
const SYMBOL_SIZE: u64 = 24;
const RELA_SIZE: u64 = 24;
pub(crate) const MAX_OBJECT_FILE_SIZE: usize = 128 * 1024 * 1024;
const MAX_SECTIONS: usize = 4096;
const MAX_SYMBOLS: usize = 1_048_576;
const MAX_RELOCATIONS: usize = 1_048_576;

const ET_REL: u16 = 1;
const EM_RISCV: u16 = 243;
const EM_LOONGARCH: u16 = 258;
const SHT_NULL: u32 = 0;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_RELA: u32 = 4;
const SHT_NOBITS: u32 = 8;
const SHT_REL: u32 = 9;
const SHN_ABS: u16 = 0xfff1;
const SHN_COMMON: u16 = 0xfff2;

#[derive(Debug, Clone, Copy)]
struct RawSection {
    name_offset: u32,
    section_type: u32,
    flags: u64,
    address: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    alignment: u64,
    entry_size: u64,
}

/// 读取并完整校验一个 ELF64 little-endian ET_REL 对象。
pub fn read_object(path: PathBuf, source: &[u8]) -> Result<ObjectFile<'_>, ElfError> {
    if source.len() > MAX_OBJECT_FILE_SIZE {
        return Err(ElfError::new(&path, ElfErrorKind::FileTooLarge));
    }
    if source.len() < ELF_HEADER_SIZE {
        return Err(ElfError::new(&path, ElfErrorKind::Truncated));
    }
    if source[0..4] != [0x7f, b'E', b'L', b'F'] {
        return Err(ElfError::new(&path, ElfErrorKind::InvalidMagic));
    }
    if source[4] != 2 {
        return Err(ElfError::new(&path, ElfErrorKind::UnsupportedClass));
    }
    if source[5] != 1 {
        return Err(ElfError::new(&path, ElfErrorKind::UnsupportedEndian));
    }
    if source[6] != 1 || u32_at(&path, source, 20)? != 1 {
        return Err(ElfError::new(&path, ElfErrorKind::UnsupportedVersion));
    }
    if u16_at(&path, source, 16)? != ET_REL {
        return Err(ElfError::new(&path, ElfErrorKind::UnsupportedType));
    }
    let target_arch = match u16_at(&path, source, 18)? {
        EM_RISCV => TargetArch::Riscv64,
        EM_LOONGARCH => TargetArch::LoongArch64,
        _ => return Err(ElfError::new(&path, ElfErrorKind::UnsupportedMachine)),
    };

    let section_table_offset = u64_at(&path, source, 40)?;
    let flags = u32_at(&path, source, 48)?;
    let header_size = u16_at(&path, source, 52)?;
    let program_header_size = u16_at(&path, source, 54)?;
    let program_header_count = u16_at(&path, source, 56)?;
    let section_header_size = u16_at(&path, source, 58)?;
    let section_count = usize::from(u16_at(&path, source, 60)?);
    let section_name_index = usize::from(u16_at(&path, source, 62)?);
    if header_size != ELF_HEADER_SIZE as u16
        || u64_at(&path, source, 24)? != 0
        || u64_at(&path, source, 32)? != 0
        || program_header_size != 0
        || program_header_count != 0
        || section_header_size != SECTION_HEADER_SIZE as u16
        || section_table_offset % 8 != 0
        || section_count == 0
        || section_name_index == 0
        || section_name_index >= section_count
    {
        return Err(ElfError::new(&path, ElfErrorKind::InvalidHeader));
    }
    if section_count > MAX_SECTIONS {
        return Err(ElfError::new(&path, ElfErrorKind::TooManySections));
    }
    let section_table_size = u64::try_from(section_count)
        .ok()
        .and_then(|count| count.checked_mul(SECTION_HEADER_SIZE as u64))
        .ok_or_else(|| ElfError::new(&path, ElfErrorKind::InvalidSectionTable))?;
    bytes(
        &path,
        source,
        section_table_offset,
        section_table_size,
        ElfErrorKind::InvalidSectionTable,
    )?;

    let mut raw_sections = Vec::with_capacity(section_count);
    for index in 0..section_count {
        let base = usize::try_from(section_table_offset)
            .ok()
            .and_then(|offset| offset.checked_add(index * SECTION_HEADER_SIZE))
            .ok_or_else(|| ElfError::new(&path, ElfErrorKind::InvalidSectionTable))?;
        raw_sections.push(RawSection {
            name_offset: u32_at(&path, source, base)?,
            section_type: u32_at(&path, source, base + 4)?,
            flags: u64_at(&path, source, base + 8)?,
            address: u64_at(&path, source, base + 16)?,
            offset: u64_at(&path, source, base + 24)?,
            size: u64_at(&path, source, base + 32)?,
            link: u32_at(&path, source, base + 40)?,
            info: u32_at(&path, source, base + 44)?,
            alignment: u64_at(&path, source, base + 48)?,
            entry_size: u64_at(&path, source, base + 56)?,
        });
    }
    validate_null_section(&path, raw_sections[0])?;
    if raw_sections
        .iter()
        .any(|section| section.section_type == SHT_REL)
    {
        return Err(ElfError::new(&path, ElfErrorKind::InvalidRelocation));
    }
    validate_section_storage(
        &path,
        source,
        section_table_offset,
        section_table_size,
        &raw_sections,
    )?;

    let section_name_section = raw_sections[section_name_index];
    if section_name_section.section_type != SHT_STRTAB {
        return Err(ElfError::new(&path, ElfErrorKind::InvalidSectionLink));
    }
    let section_names = bytes(
        &path,
        source,
        section_name_section.offset,
        section_name_section.size,
        ElfErrorKind::InvalidString,
    )?;
    validate_string_table(&path, section_names)?;

    let mut sections = Vec::with_capacity(section_count);
    for (index, raw) in raw_sections.iter().copied().enumerate() {
        let name = string_at(&path, section_names, raw.name_offset)?;
        let data = if raw.section_type == SHT_NOBITS {
            None
        } else {
            Some(bytes(
                &path,
                source,
                raw.offset,
                raw.size,
                ElfErrorKind::InvalidSection,
            )?)
        };
        sections.push(ElfSection {
            index,
            name,
            section_type: raw.section_type,
            flags: raw.flags,
            address: raw.address,
            file_offset: raw.offset,
            size: raw.size,
            link: raw.link,
            info: raw.info,
            alignment: raw.alignment,
            entry_size: raw.entry_size,
            data,
        });
    }

    let symbol_table_indices: Vec<_> = raw_sections
        .iter()
        .enumerate()
        .filter_map(|(index, section)| (section.section_type == SHT_SYMTAB).then_some(index))
        .collect();
    if symbol_table_indices.len() != 1 {
        return Err(ElfError::new(&path, ElfErrorKind::InvalidSymbolTable));
    }
    let symbol_table_index = symbol_table_indices[0];
    let symbols = read_symbols(&path, source, &raw_sections, symbol_table_index)?;
    let relocations = read_relocations(
        &path,
        source,
        &raw_sections,
        symbol_table_index,
        symbols.len(),
    )?;

    Ok(ObjectFile {
        path,
        target_arch,
        flags,
        sections,
        symbols,
        relocations,
    })
}

fn validate_null_section(path: &std::path::Path, section: RawSection) -> Result<(), ElfError> {
    if section.name_offset != 0
        || section.section_type != SHT_NULL
        || section.flags != 0
        || section.address != 0
        || section.offset != 0
        || section.size != 0
        || section.link != 0
        || section.info != 0
        || section.alignment != 0
        || section.entry_size != 0
    {
        return Err(ElfError::new(path, ElfErrorKind::InvalidSectionTable));
    }
    Ok(())
}

fn validate_section_storage(
    path: &std::path::Path,
    source: &[u8],
    section_table_offset: u64,
    section_table_size: u64,
    sections: &[RawSection],
) -> Result<(), ElfError> {
    let mut ranges = Vec::with_capacity(sections.len() + 2);
    ranges.push((0u64, ELF_HEADER_SIZE as u64));
    ranges.push((
        section_table_offset,
        section_table_offset + section_table_size,
    ));
    for section in sections.iter().skip(1) {
        if section.address != 0
            || (section.alignment != 0 && !section.alignment.is_power_of_two())
            || (section.alignment > 1 && section.offset % section.alignment != 0)
        {
            return Err(ElfError::new(path, ElfErrorKind::InvalidSection));
        }
        if section.section_type == SHT_NOBITS {
            if section.offset > source.len() as u64 {
                return Err(ElfError::new(path, ElfErrorKind::InvalidSection));
            }
            continue;
        }
        bytes(
            path,
            source,
            section.offset,
            section.size,
            ElfErrorKind::InvalidSection,
        )?;
        if section.size != 0 {
            ranges.push((section.offset, section.offset + section.size));
        }
    }
    ranges.sort_unstable_by_key(|range| range.0);
    if ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err(ElfError::new(path, ElfErrorKind::InvalidSectionTable));
    }
    Ok(())
}

fn read_symbols<'a>(
    path: &std::path::Path,
    source: &'a [u8],
    sections: &[RawSection],
    table_index: usize,
) -> Result<Vec<ElfSymbol<'a>>, ElfError> {
    let table = sections[table_index];
    let string_index = usize::try_from(table.link)
        .ok()
        .filter(|index| *index < sections.len())
        .ok_or_else(|| ElfError::new(path, ElfErrorKind::InvalidSectionLink))?;
    let strings = sections[string_index];
    if strings.section_type != SHT_STRTAB {
        return Err(ElfError::new(path, ElfErrorKind::InvalidSectionLink));
    }
    if table.entry_size != SYMBOL_SIZE || table.size % SYMBOL_SIZE != 0 {
        return Err(ElfError::new(path, ElfErrorKind::InvalidSymbolTable));
    }
    let count = usize::try_from(table.size / SYMBOL_SIZE)
        .map_err(|_| ElfError::new(path, ElfErrorKind::TooManySymbols))?;
    if count == 0 || count > MAX_SYMBOLS {
        return Err(ElfError::new(path, ElfErrorKind::TooManySymbols));
    }
    let first_global = usize::try_from(table.info)
        .ok()
        .filter(|index| *index <= count)
        .ok_or_else(|| ElfError::new(path, ElfErrorKind::InvalidSymbolTable))?;
    let string_data = bytes(
        path,
        source,
        strings.offset,
        strings.size,
        ElfErrorKind::InvalidString,
    )?;
    validate_string_table(path, string_data)?;
    let data = bytes(
        path,
        source,
        table.offset,
        table.size,
        ElfErrorKind::InvalidSymbolTable,
    )?;

    let mut symbols = Vec::with_capacity(count);
    for index in 0..count {
        let base = index * SYMBOL_SIZE as usize;
        let name = string_at(path, string_data, u32_at(path, data, base)?)?;
        let info = data[base + 4];
        let binding = info >> 4;
        let symbol_type = info & 0xf;
        let visibility = data[base + 5];
        let section_index = u16_at(path, data, base + 6)?;
        let value = u64_at(path, data, base + 8)?;
        let size = u64_at(path, data, base + 16)?;
        if index == 0
            && (!name.is_empty()
                || info != 0
                || visibility != 0
                || section_index != 0
                || value != 0
                || size != 0)
        {
            return Err(ElfError::new(path, ElfErrorKind::InvalidSymbolTable));
        }
        if visibility & !0x3 != 0
            || (index < first_global) != (binding == 0)
            || !valid_symbol_section(section_index, sections.len())
        {
            return Err(ElfError::new(path, ElfErrorKind::InvalidSymbol));
        }
        if usize::from(section_index) < sections.len() && section_index != 0 {
            let section_size = sections[usize::from(section_index)].size;
            if value > section_size || size.checked_add(value).is_none_or(|end| end > section_size)
            {
                return Err(ElfError::new(path, ElfErrorKind::InvalidSymbol));
            }
        }
        symbols.push(ElfSymbol {
            index,
            name,
            binding,
            symbol_type,
            visibility,
            section_index,
            value,
            size,
        });
    }
    Ok(symbols)
}

fn validate_string_table(path: &std::path::Path, table: &[u8]) -> Result<(), ElfError> {
    if table.first() != Some(&0) || table.last() != Some(&0) {
        return Err(ElfError::new(path, ElfErrorKind::InvalidString));
    }
    Ok(())
}

fn valid_symbol_section(section_index: u16, section_count: usize) -> bool {
    usize::from(section_index) < section_count || matches!(section_index, SHN_ABS | SHN_COMMON)
}

fn read_relocations(
    path: &std::path::Path,
    source: &[u8],
    sections: &[RawSection],
    symbol_table_index: usize,
    symbol_count: usize,
) -> Result<Vec<ElfRelocation>, ElfError> {
    let total = sections
        .iter()
        .filter(|section| section.section_type == SHT_RELA)
        .try_fold(0usize, |total, section| {
            if section.entry_size != RELA_SIZE || section.size % RELA_SIZE != 0 {
                return Err(ElfError::new(path, ElfErrorKind::InvalidRelocation));
            }
            let count = usize::try_from(section.size / RELA_SIZE)
                .map_err(|_| ElfError::new(path, ElfErrorKind::TooManyRelocations))?;
            total
                .checked_add(count)
                .ok_or_else(|| ElfError::new(path, ElfErrorKind::TooManyRelocations))
        })?;
    if total > MAX_RELOCATIONS {
        return Err(ElfError::new(path, ElfErrorKind::TooManyRelocations));
    }

    let mut relocations = Vec::with_capacity(total);
    for section in sections
        .iter()
        .filter(|section| section.section_type == SHT_RELA)
    {
        let target_index = usize::try_from(section.info)
            .ok()
            .filter(|index| *index != 0 && *index < sections.len())
            .ok_or_else(|| ElfError::new(path, ElfErrorKind::InvalidSectionLink))?;
        if usize::try_from(section.link).ok() != Some(symbol_table_index) {
            return Err(ElfError::new(path, ElfErrorKind::InvalidSectionLink));
        }
        let target_size = sections[target_index].size;
        let data = bytes(
            path,
            source,
            section.offset,
            section.size,
            ElfErrorKind::InvalidRelocation,
        )?;
        for base in (0..data.len()).step_by(RELA_SIZE as usize) {
            let offset = u64_at(path, data, base)?;
            let info = u64_at(path, data, base + 8)?;
            let symbol_index = usize::try_from(info >> 32)
                .ok()
                .filter(|index| *index < symbol_count)
                .ok_or_else(|| ElfError::new(path, ElfErrorKind::InvalidRelocation))?;
            if offset >= target_size {
                return Err(ElfError::new(path, ElfErrorKind::InvalidRelocation));
            }
            relocations.push(ElfRelocation {
                target_section_index: target_index,
                offset,
                symbol_index,
                kind: info as u32,
                addend: i64_at(path, data, base + 16)?,
            });
        }
    }
    Ok(relocations)
}
