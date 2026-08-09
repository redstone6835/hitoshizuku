//! 逐字段解码后的 SOYO owned metadata。

use alloc::vec::Vec;

use native_abi::{AbiImportRecord, CapabilityRequirementRecord, TargetArch};

use crate::registry::{ArtifactKind, DynamicRelocationKind, RelocationKind, SegmentKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoyoHeader {
    pub artifact_kind: ArtifactKind,
    pub target_arch: TargetArch,
    pub abi_family: u16,
    pub abi_epoch: u16,
    pub required_features: u64,
    pub optional_features: u64,
    pub entry_offset: u64,
    pub file_size: u64,
    pub image_virtual_size: u64,
    pub build_id: [u8; 32],
    pub content_hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub table_type: u16,
    pub flags: u16,
    pub entry_size: u32,
    pub entry_count: u32,
    pub file_offset: u64,
    pub file_size: u64,
    pub alignment: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSegment {
    pub kind: SegmentKind,
    pub permissions: u16,
    pub virtual_offset: u64,
    pub file_offset: u64,
    pub file_size: u64,
    pub memory_size: u64,
    pub alignment: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbiImport {
    pub slot: u32,
    pub operation_id: u32,
    pub flags: u32,
    pub diagnostic_name_offset: u32,
    pub signature_hash: [u8; 32],
}

impl AbiImport {
    pub const fn required(&self) -> bool {
        self.flags & 1 != 0
    }
}

impl AbiImportRecord for AbiImport {
    fn slot(&self) -> u32 {
        self.slot
    }

    fn operation_id(&self) -> u32 {
        self.operation_id
    }

    fn required(&self) -> bool {
        self.required()
    }

    fn signature_hash(&self) -> &[u8; 32] {
        &self.signature_hash
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRequirement {
    pub requirement_id: u32,
    pub object_interface: u16,
    pub flags: u16,
    pub required_rights: u64,
    pub diagnostic_name_offset: u32,
}

impl CapabilityRequirement {
    pub const fn required(&self) -> bool {
        self.flags & 1 != 0
    }
}

impl CapabilityRequirementRecord for CapabilityRequirement {
    fn requirement_id(&self) -> u32 {
        self.requirement_id
    }

    fn object_interface(&self) -> u16 {
        self.object_interface
    }

    fn required(&self) -> bool {
        self.required()
    }

    fn required_rights(&self) -> u64 {
        self.required_rights
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relocation {
    pub kind: RelocationKind,
    pub target_segment_index: u32,
    pub target_offset: u64,
    pub source_segment_index: u32,
    pub addend: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInfo {
    pub stack_size: u64,
    pub stack_guard_size: u64,
    pub runtime_flags: u64,
    pub init_array_offset: u64,
    pub init_array_count: u32,
    pub init_array_entry_size: u16,
    pub fini_array_offset: u64,
    pub fini_array_count: u32,
    pub fini_array_entry_size: u16,
    pub stack_alignment: u32,
    pub start_info_max_size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentInfo {
    pub component_id: [u8; 16],
    pub abi_id: [u8; 16],
    pub flags: u64,
    pub init_offset: u64,
    pub fini_offset: u64,
    pub interface_count: u32,
    pub call_state_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentDependency {
    pub component_id: [u8; 16],
    pub abi_id: [u8; 16],
    pub content_hash: [u8; 32],
    pub flags: u32,
    pub diagnostic_name_offset: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolImport {
    pub dependency_index: u32,
    pub flags: u32,
    pub interface_id: [u8; 16],
    pub symbol_id: [u8; 16],
    pub signature_hash: [u8; 32],
    pub diagnostic_name_offset: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolExport {
    pub interface_id: [u8; 16],
    pub symbol_id: [u8; 16],
    pub signature_hash: [u8; 32],
    pub entry_offset: u64,
    pub flags: u32,
    pub diagnostic_name_offset: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicRelocation {
    pub kind: DynamicRelocationKind,
    pub target_segment_index: u32,
    pub target_offset: u64,
    pub source_index: u32,
    pub addend: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoyoSignature {
    pub key_id: [u8; 32],
    pub signature: [u8; 64],
    pub flags: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentMetadata {
    pub info: ComponentInfo,
    pub dependencies: Vec<ComponentDependency>,
    pub symbol_imports: Vec<SymbolImport>,
    pub symbol_exports: Vec<SymbolExport>,
    pub dynamic_relocations: Vec<DynamicRelocation>,
    pub signature: Option<SoyoSignature>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoyoMetadata {
    pub header: SoyoHeader,
    pub directory: Vec<DirectoryEntry>,
    pub strings: Vec<u8>,
    pub segments: Vec<ImageSegment>,
    pub imports: Vec<AbiImport>,
    pub capabilities: Vec<CapabilityRequirement>,
    pub relocations: Vec<Relocation>,
    pub runtime: Option<RuntimeInfo>,
    pub component: Option<ComponentMetadata>,
}
