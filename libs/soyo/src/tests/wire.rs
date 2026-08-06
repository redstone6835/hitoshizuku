//! Wire 尺寸测试独立使用规范字面值，避免实现常量同时计算实际值和期望值。

use crate::wire::{
    ABI_IMPORT_SIZE, CAPABILITY_REQUIREMENT_SIZE, DIRECTORY_ENTRY_SIZE, HEADER_SIZE,
    IMAGE_SEGMENT_SIZE, RELOCATION_SIZE, RUNTIME_INFO_SIZE, abi_import, capability_requirement,
    directory, header, image_segment, relocation, runtime_info,
};

#[test]
fn standard_wire_records_have_frozen_sizes() {
    assert_eq!(HEADER_SIZE, 192);
    assert_eq!(DIRECTORY_ENTRY_SIZE, 48);
    assert_eq!(IMAGE_SEGMENT_SIZE, 64);
    assert_eq!(ABI_IMPORT_SIZE, 64);
    assert_eq!(CAPABILITY_REQUIREMENT_SIZE, 64);
    assert_eq!(RELOCATION_SIZE, 48);
    assert_eq!(RUNTIME_INFO_SIZE, 96);
}

#[test]
fn header_offsets_select_every_frozen_field() {
    assert_eq!(
        [
            header::MAGIC,
            header::FORMAT_VERSION,
            header::HEADER_SIZE,
            header::ARTIFACT_KIND,
            header::TARGET_ARCH,
            header::ENDIAN,
            header::POINTER_WIDTH,
            header::ABI_FAMILY,
            header::ABI_EPOCH,
            header::HASH_ALGORITHM,
            header::FLAGS,
            header::REQUIRED_FEATURES,
            header::OPTIONAL_FEATURES,
            header::ENTRY_OFFSET,
            header::TABLE_OFFSET,
            header::TABLE_COUNT,
            header::TABLE_ENTRY_SIZE,
            header::RESERVED0,
            header::FILE_SIZE,
            header::IMAGE_VIRTUAL_SIZE,
            header::BUILD_ID,
            header::CONTENT_HASH,
            header::RESERVED1,
        ],
        [
            0x00, 0x04, 0x06, 0x08, 0x0a, 0x0c, 0x0d, 0x0e, 0x10, 0x12, 0x14, 0x18, 0x20, 0x28,
            0x30, 0x38, 0x3c, 0x3e, 0x40, 0x48, 0x50, 0x70, 0x90,
        ]
    );
}

#[test]
fn table_record_offsets_select_every_frozen_field() {
    assert_eq!(
        [
            directory::TABLE_TYPE,
            directory::FLAGS,
            directory::ENTRY_SIZE,
            directory::ENTRY_COUNT,
            directory::RESERVED0,
            directory::FILE_OFFSET,
            directory::FILE_SIZE,
            directory::ALIGNMENT,
            directory::RESERVED1,
        ],
        [0x00, 0x02, 0x04, 0x08, 0x0c, 0x10, 0x18, 0x20, 0x28]
    );
    assert_eq!(
        [
            image_segment::KIND,
            image_segment::PERMISSIONS,
            image_segment::FLAGS,
            image_segment::VIRTUAL_OFFSET,
            image_segment::FILE_OFFSET,
            image_segment::FILE_SIZE,
            image_segment::MEMORY_SIZE,
            image_segment::ALIGNMENT,
            image_segment::RESERVED0,
            image_segment::RESERVED1,
        ],
        [0x00, 0x02, 0x04, 0x08, 0x10, 0x18, 0x20, 0x28, 0x30, 0x38]
    );
    assert_eq!(
        [
            abi_import::SLOT,
            abi_import::OPERATION_ID,
            abi_import::FLAGS,
            abi_import::DIAGNOSTIC_NAME_OFFSET,
            abi_import::SIGNATURE_HASH,
            abi_import::RESERVED,
        ],
        [0x00, 0x04, 0x08, 0x0c, 0x10, 0x30]
    );
    assert_eq!(
        [
            capability_requirement::REQUIREMENT_ID,
            capability_requirement::OBJECT_INTERFACE,
            capability_requirement::FLAGS,
            capability_requirement::REQUIRED_RIGHTS,
            capability_requirement::DIAGNOSTIC_NAME_OFFSET,
            capability_requirement::RESERVED0,
            capability_requirement::RESERVED1,
        ],
        [0x00, 0x04, 0x06, 0x08, 0x10, 0x14, 0x18]
    );
    assert_eq!(
        [
            relocation::KIND,
            relocation::FLAGS,
            relocation::TARGET_SEGMENT_INDEX,
            relocation::TARGET_OFFSET,
            relocation::SOURCE_SEGMENT_INDEX,
            relocation::RESERVED0,
            relocation::ADDEND,
            relocation::RESERVED1,
            relocation::RESERVED2,
        ],
        [0x00, 0x02, 0x04, 0x08, 0x10, 0x14, 0x18, 0x20, 0x28]
    );
}

#[test]
fn runtime_offsets_select_every_frozen_field() {
    assert_eq!(
        [
            runtime_info::STACK_SIZE,
            runtime_info::STACK_GUARD_SIZE,
            runtime_info::RUNTIME_FLAGS,
            runtime_info::INIT_ARRAY_OFFSET,
            runtime_info::INIT_ARRAY_COUNT,
            runtime_info::INIT_ARRAY_ENTRY_SIZE,
            runtime_info::RESERVED0,
            runtime_info::FINI_ARRAY_OFFSET,
            runtime_info::FINI_ARRAY_COUNT,
            runtime_info::FINI_ARRAY_ENTRY_SIZE,
            runtime_info::RESERVED1,
            runtime_info::STACK_ALIGNMENT,
            runtime_info::START_INFO_MAX_SIZE,
            runtime_info::RESERVED2,
        ],
        [
            0x00, 0x08, 0x10, 0x18, 0x20, 0x24, 0x26, 0x28, 0x30, 0x34, 0x36, 0x38, 0x3c, 0x40,
        ]
    );
}
