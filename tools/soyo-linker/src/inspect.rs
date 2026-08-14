//! 已校验 SOYO metadata 的稳定检查输出。

use std::fmt::Write;

use native_abi::TargetArch;
use soyo::registry::{ArtifactKind, PAGE_SIZE, SegmentKind};
use soyo::SoyoMetadata;

pub const TSV_HEADER: &str = "file_size\tartifact_kind\ttarget_arch\tabi_family\tabi_epoch\timage_virtual_size\tsegment_count\tmapped_page_count\tabi_import_count\tcapability_count\trelocation_count\trequired_features\tcontent_hash";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoyoInspection {
    pub file_size: u64,
    pub artifact_kind: &'static str,
    pub target_arch: &'static str,
    pub abi_family: u16,
    pub abi_epoch: u16,
    pub image_virtual_size: u64,
    pub segment_count: usize,
    pub mapped_page_count: u64,
    pub abi_import_count: usize,
    pub capability_count: usize,
    pub relocation_count: usize,
    pub required_features: String,
    pub content_hash: String,
}

impl SoyoInspection {
    pub fn from_metadata(metadata: &SoyoMetadata) -> Self {
        let mapped_page_count = metadata
            .segments
            .iter()
            .filter(|segment| segment.kind != SegmentKind::TlsTemplate)
            .map(|segment| segment.memory_size.div_ceil(PAGE_SIZE))
            .sum();
        let dynamic_relocations = metadata
            .component
            .as_ref()
            .map_or(0, |component| component.dynamic_relocations.len());

        Self {
            file_size: metadata.header.file_size,
            artifact_kind: match metadata.header.artifact_kind {
                ArtifactKind::Executable => "executable",
                ArtifactKind::SharedComponent => "shared-component",
            },
            target_arch: match metadata.header.target_arch {
                TargetArch::Riscv64 => "riscv64",
                TargetArch::LoongArch64 => "loongarch64",
            },
            abi_family: metadata.header.abi_family,
            abi_epoch: metadata.header.abi_epoch,
            image_virtual_size: metadata.header.image_virtual_size,
            segment_count: metadata.segments.len(),
            mapped_page_count,
            abi_import_count: metadata.imports.len(),
            capability_count: metadata.capabilities.len(),
            relocation_count: metadata.relocations.len() + dynamic_relocations,
            required_features: format!("0x{:016x}", metadata.header.required_features),
            content_hash: encode_hex(&metadata.header.content_hash),
        }
    }

    pub fn to_tsv(&self) -> String {
        format!("{TSV_HEADER}\n{}\n", self.tsv_row())
    }

    pub fn to_text(&self) -> String {
        let mut output = String::new();
        for (name, value) in self.fields() {
            writeln!(output, "{name}: {value}").unwrap();
        }
        output
    }

    fn tsv_row(&self) -> String {
        self.fields()
            .into_iter()
            .map(|(_, value)| value)
            .collect::<Vec<_>>()
            .join("\t")
    }

    fn fields(&self) -> [(&'static str, String); 13] {
        [
            ("file_size", self.file_size.to_string()),
            ("artifact_kind", self.artifact_kind.into()),
            ("target_arch", self.target_arch.into()),
            ("abi_family", self.abi_family.to_string()),
            ("abi_epoch", self.abi_epoch.to_string()),
            ("image_virtual_size", self.image_virtual_size.to_string()),
            ("segment_count", self.segment_count.to_string()),
            ("mapped_page_count", self.mapped_page_count.to_string()),
            ("abi_import_count", self.abi_import_count.to_string()),
            ("capability_count", self.capability_count.to_string()),
            ("relocation_count", self.relocation_count.to_string()),
            ("required_features", self.required_features.clone()),
            ("content_hash", self.content_hash.clone()),
        ]
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").unwrap();
    }
    output
}
