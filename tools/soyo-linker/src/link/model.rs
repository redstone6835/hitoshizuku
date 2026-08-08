use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use native_abi::TargetArch;
use soyo::Relocation;
use soyo::registry::SegmentKind;

/// 一个待链接的 ELF ET_REL 输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputObject {
    path: PathBuf,
    bytes: Vec<u8>,
}

impl InputObject {
    pub fn new(path: PathBuf, bytes: Vec<u8>) -> Self {
        Self { path, bytes }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// 构造一次静态映像所需的输入。
#[derive(Debug, Clone, Copy)]
pub struct LinkRequest<'a> {
    pub target_arch: TargetArch,
    pub entry_symbol: &'a str,
    pub objects: &'a [InputObject],
}

/// 链接后符号值的地址域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolValue {
    Image(u64),
    Tls(u64),
    Absolute(u64),
}

/// 链接器合成的构造器与析构器数组位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeArrays {
    pub(crate) init_offset: u64,
    pub(crate) init_count: u32,
    pub(crate) fini_offset: u64,
    pub(crate) fini_count: u32,
    pub(crate) segment_index: Option<usize>,
}

impl RuntimeArrays {
    pub(crate) const EMPTY: Self = Self {
        init_offset: 0,
        init_count: 0,
        fini_offset: 0,
        fini_count: 0,
        segment_index: None,
    };

    pub const fn init_offset(self) -> u64 {
        self.init_offset
    }

    pub const fn init_count(self) -> u32 {
        self.init_count
    }

    pub const fn fini_offset(self) -> u64 {
        self.fini_offset
    }

    pub const fn fini_count(self) -> u32 {
        self.fini_count
    }

    pub const fn is_empty(self) -> bool {
        self.init_count == 0 && self.fini_count == 0
    }
}

/// 已解析的全局符号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSymbol {
    pub(crate) value: SymbolValue,
    pub(crate) segment_index: Option<usize>,
    pub(crate) size: u64,
}

impl LinkSymbol {
    pub const fn value(&self) -> SymbolValue {
        self.value
    }

    pub const fn segment_index(&self) -> Option<usize> {
        self.segment_index
    }

    pub const fn size(&self) -> u64 {
        self.size
    }
}

/// 一个可直接投影为 SOYO ImageSegment 的链接段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSegment {
    pub(crate) kind: SegmentKind,
    pub(crate) virtual_offset: u64,
    pub(crate) payload: Vec<u8>,
    pub(crate) memory_size: u64,
    pub(crate) alignment: u64,
}

impl LinkSegment {
    pub const fn kind(&self) -> SegmentKind {
        self.kind
    }

    pub const fn virtual_offset(&self) -> u64 {
        self.virtual_offset
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub(crate) fn payload_mut(&mut self) -> &mut [u8] {
        &mut self.payload
    }

    pub const fn memory_size(&self) -> u64 {
        self.memory_size
    }

    pub const fn alignment(&self) -> u64 {
        self.alignment
    }
}

/// 尚未应用的目标架构 relocation。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRelocation {
    pub(crate) input_path: PathBuf,
    pub(crate) target_segment_index: usize,
    pub(crate) target_offset: u64,
    pub(crate) place_offset: u64,
    pub(crate) kind: u32,
    pub(crate) addend: i64,
    pub(crate) symbol_name: String,
    pub(crate) symbol_value: SymbolValue,
}

impl PendingRelocation {
    pub const fn target_segment_index(&self) -> usize {
        self.target_segment_index
    }

    pub const fn target_offset(&self) -> u64 {
        self.target_offset
    }

    pub const fn place_offset(&self) -> u64 {
        self.place_offset
    }

    pub const fn kind(&self) -> u32 {
        self.kind
    }

    pub const fn addend(&self) -> i64 {
        self.addend
    }

    pub fn symbol_name(&self) -> &str {
        &self.symbol_name
    }

    pub const fn symbol_value(&self) -> SymbolValue {
        self.symbol_value
    }
}

/// 架构 relocation 应用前的确定性链接映像。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkImage {
    pub(crate) target_arch: TargetArch,
    pub(crate) entry_offset: u64,
    pub(crate) image_virtual_size: u64,
    pub(crate) segments: Vec<LinkSegment>,
    pub(crate) symbols: BTreeMap<String, LinkSymbol>,
    pub(crate) pending_relocations: Vec<PendingRelocation>,
    pub(crate) runtime_arrays: RuntimeArrays,
}

impl LinkImage {
    pub const fn target_arch(&self) -> TargetArch {
        self.target_arch
    }

    pub const fn entry_offset(&self) -> u64 {
        self.entry_offset
    }

    pub const fn image_virtual_size(&self) -> u64 {
        self.image_virtual_size
    }

    pub fn segments(&self) -> &[LinkSegment] {
        &self.segments
    }

    pub fn symbol(&self, name: &str) -> Option<&LinkSymbol> {
        self.symbols.get(name)
    }

    pub fn pending_relocations(&self) -> &[PendingRelocation] {
        &self.pending_relocations
    }

    pub const fn runtime_arrays(&self) -> RuntimeArrays {
        self.runtime_arrays
    }
}

/// 已完成架构 relocation、可交给 SOYO writer 的链接映像。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedImage {
    pub(crate) target_arch: TargetArch,
    pub(crate) entry_offset: u64,
    pub(crate) image_virtual_size: u64,
    pub(crate) segments: Vec<LinkSegment>,
    pub(crate) symbols: BTreeMap<String, LinkSymbol>,
    pub(crate) runtime_relocations: Vec<Relocation>,
    pub(crate) runtime_arrays: RuntimeArrays,
}

impl LinkedImage {
    pub const fn target_arch(&self) -> TargetArch {
        self.target_arch
    }

    pub const fn entry_offset(&self) -> u64 {
        self.entry_offset
    }

    pub const fn image_virtual_size(&self) -> u64 {
        self.image_virtual_size
    }

    pub fn segments(&self) -> &[LinkSegment] {
        &self.segments
    }

    pub fn symbol(&self, name: &str) -> Option<&LinkSymbol> {
        self.symbols.get(name)
    }

    pub fn runtime_relocations(&self) -> &[Relocation] {
        &self.runtime_relocations
    }

    pub const fn runtime_arrays(&self) -> RuntimeArrays {
        self.runtime_arrays
    }
}
