use std::path::{Path, PathBuf};

pub use native_abi::TargetArch;

/// ELF section 的逐字段视图。
#[derive(Debug)]
pub struct ElfSection<'a> {
    pub(crate) index: usize,
    pub(crate) name: &'a str,
    pub(crate) section_type: u32,
    pub(crate) flags: u64,
    pub(crate) address: u64,
    pub(crate) file_offset: u64,
    pub(crate) size: u64,
    pub(crate) link: u32,
    pub(crate) info: u32,
    pub(crate) alignment: u64,
    pub(crate) entry_size: u64,
    pub(crate) data: Option<&'a [u8]>,
}

impl<'a> ElfSection<'a> {
    pub const fn index(&self) -> usize {
        self.index
    }

    pub const fn name(&self) -> &'a str {
        self.name
    }

    pub const fn section_type(&self) -> u32 {
        self.section_type
    }

    pub const fn flags(&self) -> u64 {
        self.flags
    }

    pub const fn address(&self) -> u64 {
        self.address
    }

    pub const fn file_offset(&self) -> u64 {
        self.file_offset
    }

    pub const fn size(&self) -> u64 {
        self.size
    }

    pub const fn link(&self) -> u32 {
        self.link
    }

    pub const fn info(&self) -> u32 {
        self.info
    }

    pub const fn alignment(&self) -> u64 {
        self.alignment
    }

    pub const fn entry_size(&self) -> u64 {
        self.entry_size
    }

    pub const fn data(&self) -> Option<&'a [u8]> {
        self.data
    }

    pub const fn is_allocated(&self) -> bool {
        self.flags & 0x2 != 0
    }

    pub const fn is_writable(&self) -> bool {
        self.flags & 0x1 != 0
    }

    pub const fn is_executable(&self) -> bool {
        self.flags & 0x4 != 0
    }

    pub const fn is_tls(&self) -> bool {
        self.flags & 0x400 != 0
    }

    pub const fn is_nobits(&self) -> bool {
        self.section_type == 8
    }
}

/// ELF symbol 的逐字段视图。
#[derive(Debug)]
pub struct ElfSymbol<'a> {
    pub(crate) index: usize,
    pub(crate) name: &'a str,
    pub(crate) binding: u8,
    pub(crate) symbol_type: u8,
    pub(crate) visibility: u8,
    pub(crate) section_index: u16,
    pub(crate) value: u64,
    pub(crate) size: u64,
}

impl<'a> ElfSymbol<'a> {
    pub const fn index(&self) -> usize {
        self.index
    }

    pub const fn name(&self) -> &'a str {
        self.name
    }

    pub const fn binding(&self) -> u8 {
        self.binding
    }

    pub const fn symbol_type(&self) -> u8 {
        self.symbol_type
    }

    pub const fn visibility(&self) -> u8 {
        self.visibility
    }

    pub const fn section_index(&self) -> u16 {
        self.section_index
    }

    pub const fn value(&self) -> u64 {
        self.value
    }

    pub const fn size(&self) -> u64 {
        self.size
    }

    pub const fn is_undefined(&self) -> bool {
        self.section_index == 0
    }
}

/// ELF64 RELA 记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElfRelocation {
    pub(crate) target_section_index: usize,
    pub(crate) offset: u64,
    pub(crate) symbol_index: usize,
    pub(crate) kind: u32,
    pub(crate) addend: i64,
}

impl ElfRelocation {
    pub const fn target_section_index(self) -> usize {
        self.target_section_index
    }

    pub const fn offset(self) -> u64 {
        self.offset
    }

    pub const fn symbol_index(self) -> usize {
        self.symbol_index
    }

    pub const fn kind(self) -> u32 {
        self.kind
    }

    pub const fn addend(self) -> i64 {
        self.addend
    }
}

/// 已校验的 ELF64 ET_REL 对象。
#[derive(Debug)]
pub struct ObjectFile<'a> {
    pub(crate) path: PathBuf,
    pub(crate) target_arch: TargetArch,
    pub(crate) flags: u32,
    pub(crate) sections: Vec<ElfSection<'a>>,
    pub(crate) symbols: Vec<ElfSymbol<'a>>,
    pub(crate) relocations: Vec<ElfRelocation>,
}

impl<'a> ObjectFile<'a> {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn target_arch(&self) -> TargetArch {
        self.target_arch
    }

    pub const fn flags(&self) -> u32 {
        self.flags
    }

    pub fn sections(&self) -> &[ElfSection<'a>] {
        &self.sections
    }

    pub fn section_by_name(&self, name: &str) -> Option<&ElfSection<'a>> {
        self.sections.iter().find(|section| section.name == name)
    }

    pub fn symbols(&self) -> &[ElfSymbol<'a>] {
        &self.symbols
    }

    pub fn symbol_by_name(&self, name: &str) -> Option<&ElfSymbol<'a>> {
        self.symbols.iter().find(|symbol| symbol.name == name)
    }

    pub fn relocations(&self) -> &[ElfRelocation] {
        &self.relocations
    }
}
