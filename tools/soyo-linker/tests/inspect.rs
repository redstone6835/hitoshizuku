use native_abi::TargetArch;
use soyo::registry::{ArtifactKind, SegmentKind, SegmentPermissions};
use soyo::{ImageSegment, SoyoHeader, SoyoMetadata};
use soyo_linker::inspect::SoyoInspection;

fn metadata() -> SoyoMetadata {
    SoyoMetadata {
        header: SoyoHeader {
            artifact_kind: ArtifactKind::Executable,
            target_arch: TargetArch::Riscv64,
            abi_family: 1,
            abi_epoch: 1,
            required_features: 5,
            optional_features: 2,
            entry_offset: 0x1000,
            file_size: 12_345,
            image_virtual_size: 0x5000,
            build_id: [0x11; 32],
            content_hash: [0xab; 32],
        },
        directory: Vec::new(),
        strings: Vec::new(),
        segments: vec![
            ImageSegment {
                kind: SegmentKind::Code,
                permissions: (SegmentPermissions::READ | SegmentPermissions::EXECUTE).bits(),
                virtual_offset: 0,
                file_offset: 0x1000,
                file_size: 0x800,
                memory_size: 0x1000,
                alignment: 0x1000,
            },
            ImageSegment {
                kind: SegmentKind::Data,
                permissions: (SegmentPermissions::READ | SegmentPermissions::WRITE).bits(),
                virtual_offset: 0x2000,
                file_offset: 0x2000,
                file_size: 0x1000,
                memory_size: 0x3000,
                alignment: 0x1000,
            },
            ImageSegment {
                kind: SegmentKind::TlsTemplate,
                permissions: SegmentPermissions::READ.bits(),
                virtual_offset: 0,
                file_offset: 0x3000,
                file_size: 32,
                memory_size: 64,
                alignment: 16,
            },
        ],
        imports: Vec::new(),
        capabilities: Vec::new(),
        relocations: Vec::new(),
        runtime: None,
        component: None,
    }
}

#[test]
fn inspection_reports_stable_format_metrics() {
    let inspection = SoyoInspection::from_metadata(&metadata());

    assert_eq!(inspection.file_size, 12_345);
    assert_eq!(inspection.artifact_kind, "executable");
    assert_eq!(inspection.target_arch, "riscv64");
    assert_eq!(inspection.segment_count, 3);
    assert_eq!(inspection.mapped_page_count, 4);
    assert_eq!(inspection.required_features, "0x0000000000000005");
    assert_eq!(inspection.content_hash, "ab".repeat(32));
}

#[test]
fn tsv_has_a_fixed_machine_readable_schema() {
    let inspection = SoyoInspection::from_metadata(&metadata());
    let output = inspection.to_tsv();
    let mut lines = output.lines();

    assert_eq!(
        lines.next(),
        Some("file_size\tartifact_kind\ttarget_arch\tabi_family\tabi_epoch\timage_virtual_size\tsegment_count\tmapped_page_count\tabi_import_count\tcapability_count\trelocation_count\trequired_features\tcontent_hash")
    );
    assert_eq!(lines.count(), 1);
}
