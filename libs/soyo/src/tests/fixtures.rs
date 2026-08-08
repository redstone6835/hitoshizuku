use alloc::vec;
use alloc::vec::Vec;
use native_abi::{RequirementId, requirement};
use sha2::{Digest, Sha256};

pub const HEADER_REQUIRED_FEATURES: usize = 0x18;
pub const HEADER_OPTIONAL_FEATURES: usize = 0x20;
pub const HEADER_ENTRY_OFFSET: usize = 0x28;
pub const HEADER_TARGET_ARCH: usize = 0x0a;
pub const HEADER_ABI_EPOCH: usize = 0x10;
pub const HEADER_TABLE_COUNT: usize = 0x38;
pub const HEADER_FILE_SIZE: usize = 0x40;
pub const HEADER_IMAGE_VIRTUAL_SIZE: usize = 0x48;
pub const HEADER_BUILD_ID: usize = 0x50;
pub const HEADER_RESERVED1: usize = 0x90;
pub const DIRECTORY_FIRST_TYPE: usize = 192;
pub const DIRECTORY_STRING_COUNT: usize = 192 + 0x08;
pub const DIRECTORY_STRING_FILE_OFFSET: usize = 192 + 0x10;
pub const DIRECTORY_STRING_FILE_SIZE: usize = 192 + 0x18;
pub const DIRECTORY_SECOND_TYPE: usize = 192 + 48;
pub const DIRECTORY_SEGMENT_COUNT: usize = 192 + 48 + 0x08;
pub const DIRECTORY_SEGMENT_FILE_SIZE: usize = 192 + 48 + 0x18;
pub const DIRECTORY_IMPORT_COUNT: usize = 192 + 2 * 48 + 0x08;
pub const DIRECTORY_IMPORT_FILE_SIZE: usize = 192 + 2 * 48 + 0x18;
pub const DIRECTORY_CAPABILITY_COUNT: usize = 192 + 3 * 48 + 0x08;
pub const DIRECTORY_CAPABILITY_FILE_SIZE: usize = 192 + 3 * 48 + 0x18;
pub const DIRECTORY_RUNTIME_TYPE: usize = 192 + 4 * 48;
pub const DIRECTORY_RUNTIME_ENTRY_SIZE: usize = 192 + 4 * 48 + 0x04;
pub const DIRECTORY_RUNTIME_COUNT: usize = 192 + 4 * 48 + 0x08;
pub const DIRECTORY_RUNTIME_FILE_SIZE: usize = 192 + 4 * 48 + 0x18;
pub const TABLE_PADDING: usize = 433;
pub const SEGMENT_KIND: usize = 440;
pub const SEGMENT_PERMISSIONS: usize = 440 + 0x02;
pub const SEGMENT_FILE_OFFSET: usize = 440 + 0x10;
pub const SEGMENT_MEMORY_SIZE: usize = 440 + 0x20;
pub const SEGMENT_ALIGNMENT: usize = 440 + 0x28;
pub const IMPORT_OPERATION_ID: usize = 504 + 0x04;
pub const IMPORT_FLAGS: usize = 504 + 0x08;
pub const IMPORT_NAME: usize = 504 + 0x0c;
pub const IMPORT_SIGNATURE_HASH: usize = 504 + 0x10;
pub const CAP_REQUIREMENT_ID: usize = 568;
pub const CAP_OBJECT_INTERFACE: usize = 568 + 0x04;
pub const CAP_FLAGS: usize = 568 + 0x06;
pub const CAP_RIGHTS: usize = 568 + 0x08;
pub const EXTENDED_FIRST_SEGMENT_MEMORY_SIZE: usize = 536 + 0x20;
pub const EXTENDED_SECOND_SEGMENT_KIND: usize = 600;
pub const EXTENDED_SECOND_SEGMENT_PERMISSIONS: usize = 600 + 0x02;
pub const EXTENDED_RELOCATION_KIND: usize = 792;
pub const EXTENDED_RELOCATION_TARGET_SEGMENT: usize = 792 + 0x04;
pub const EXTENDED_RELOCATION_TARGET_OFFSET: usize = 792 + 0x08;
pub const EXTENDED_RELOCATION_SOURCE_SEGMENT: usize = 792 + 0x10;
pub const EXTENDED_RELOCATION_RESERVED0: usize = 792 + 0x14;
pub const EXTENDED_RELOCATION_ADDEND: usize = 792 + 0x18;
pub const EXTENDED_OPTIONAL_TABLE_TYPE: usize = 192 + 6 * 48;
pub const EXTENDED_OPTIONAL_TABLE_FLAGS: usize = 192 + 6 * 48 + 0x02;
pub const EXTENDED_OPTIONAL_TABLE_ENTRY_SIZE: usize = 192 + 6 * 48 + 0x04;
pub const EXTENDED_OPTIONAL_TABLE_COUNT: usize = 192 + 6 * 48 + 0x08;
pub const EXTENDED_OPTIONAL_TABLE_FILE_SIZE: usize = 192 + 6 * 48 + 0x18;
pub const EXTENDED_FIRST_SEGMENT_FILE_OFFSET: usize = 536 + 0x10;
pub const EXTENDED_SECOND_SEGMENT_FILE_OFFSET: usize = 600 + 0x10;
pub const EXTENDED_RUNTIME_FLAGS: usize = 840 + 0x10;
pub const EXTENDED_INIT_ARRAY_OFFSET: usize = 840 + 0x18;
pub const EXTENDED_INIT_ARRAY_COUNT: usize = 840 + 0x20;
pub const EXTENDED_INIT_ARRAY_ENTRY_SIZE: usize = 840 + 0x24;

pub const UNKNOWN_OPTIONAL_TABLE_TYPE: u16 = 0x8000;

const FILE_SIZE: usize = 8192;

pub fn minimal_soyo() -> Vec<u8> {
    let mut bytes = vec![0u8; FILE_SIZE];

    bytes[0..4].copy_from_slice(b"soyo");
    put_u16(&mut bytes, 0x04, 1);
    put_u16(&mut bytes, 0x06, 192);
    put_u16(&mut bytes, 0x08, 1);
    put_u16(&mut bytes, HEADER_TARGET_ARCH, 1);
    bytes[0x0c] = 1;
    bytes[0x0d] = 64;
    put_u16(&mut bytes, 0x0e, 1);
    put_u16(&mut bytes, 0x10, 1);
    put_u16(&mut bytes, 0x12, 1);
    put_u64(&mut bytes, HEADER_ENTRY_OFFSET, 0);
    put_u64(&mut bytes, 0x30, 192);
    put_u32(&mut bytes, 0x38, 5);
    put_u16(&mut bytes, 0x3c, 48);
    put_u64(&mut bytes, 0x40, FILE_SIZE as u64);
    put_u64(&mut bytes, 0x48, 4096);

    put_directory(&mut bytes, 0, 1, 1, 1, 1, 432, 1, 1);
    put_directory(&mut bytes, 1, 2, 1, 64, 1, 440, 64, 8);
    put_directory(&mut bytes, 2, 3, 1, 64, 1, 504, 64, 8);
    put_directory(&mut bytes, 3, 4, 1, 64, 1, 568, 64, 8);
    put_directory(&mut bytes, 4, 6, 1, 96, 1, 632, 96, 8);

    bytes[432] = 0;

    put_u16(&mut bytes, 440, 1);
    put_u16(&mut bytes, SEGMENT_PERMISSIONS, 5);
    put_u64(&mut bytes, 440 + 0x08, 0);
    put_u64(&mut bytes, 440 + 0x10, 4096);
    put_u64(&mut bytes, 440 + 0x18, 4);
    put_u64(&mut bytes, 440 + 0x20, 4096);
    put_u64(&mut bytes, 440 + 0x28, 4096);

    put_u32(&mut bytes, 504, 0);
    put_u32(&mut bytes, 504 + 0x04, 1);
    put_u32(&mut bytes, 504 + 0x08, 1);
    bytes[IMPORT_SIGNATURE_HASH..IMPORT_SIGNATURE_HASH + 32].copy_from_slice(&[
        0xa6, 0xc1, 0xfb, 0x70, 0xa1, 0x0c, 0x4b, 0x82, 0xa6, 0x32, 0xa3, 0x02, 0x75, 0xef, 0x98,
        0xd9, 0x62, 0x4e, 0x46, 0xee, 0x8c, 0x3b, 0x2e, 0xdf, 0x02, 0xac, 0xb4, 0xe9, 0xef, 0x83,
        0x44, 0x2b,
    ]);

    put_u32(&mut bytes, 568, 1);
    put_u16(&mut bytes, 568 + 0x04, 1);
    put_u16(&mut bytes, 568 + 0x06, 1);
    let self_process = requirement(RequirementId::SelfProcess).expect("SelfProcess 必须注册");
    put_u64(&mut bytes, 568 + 0x08, self_process.max_rights.bits());

    put_u64(&mut bytes, 632, 64 * 1024);
    put_u64(&mut bytes, 632 + 0x08, 4096);
    put_u32(&mut bytes, 632 + 0x38, 16);
    put_u32(&mut bytes, 632 + 0x3c, 4096);

    bytes[4096..4100].copy_from_slice(&[0x73, 0x00, 0x00, 0x00]);
    rehash(&mut bytes);
    bytes
}

/// 包含 CODE、DATA、Relocation 和未知 optional 表的完整规范镜像。
pub fn extended_soyo() -> Vec<u8> {
    const EXTENDED_FILE_SIZE: usize = 12288;

    let mut bytes = vec![0u8; EXTENDED_FILE_SIZE];

    bytes[0..4].copy_from_slice(b"soyo");
    put_u16(&mut bytes, 0x04, 1);
    put_u16(&mut bytes, 0x06, 192);
    put_u16(&mut bytes, 0x08, 1);
    put_u16(&mut bytes, HEADER_TARGET_ARCH, 1);
    bytes[0x0c] = 1;
    bytes[0x0d] = 64;
    put_u16(&mut bytes, 0x0e, 1);
    put_u16(&mut bytes, 0x10, 1);
    put_u16(&mut bytes, 0x12, 1);
    put_u64(&mut bytes, HEADER_ENTRY_OFFSET, 0);
    put_u64(&mut bytes, 0x30, 192);
    put_u32(&mut bytes, HEADER_TABLE_COUNT, 7);
    put_u16(&mut bytes, 0x3c, 48);
    put_u64(&mut bytes, 0x40, EXTENDED_FILE_SIZE as u64);
    put_u64(&mut bytes, 0x48, 8192);

    put_directory(&mut bytes, 0, 1, 1, 1, 1, 528, 1, 1);
    put_directory(&mut bytes, 1, 2, 1, 64, 2, 536, 128, 8);
    put_directory(&mut bytes, 2, 3, 1, 64, 1, 664, 64, 8);
    put_directory(&mut bytes, 3, 4, 1, 64, 1, 728, 64, 8);
    put_directory(&mut bytes, 4, 5, 1, 48, 1, 792, 48, 8);
    put_directory(&mut bytes, 5, 6, 1, 96, 1, 840, 96, 8);
    put_directory(
        &mut bytes,
        6,
        UNKNOWN_OPTIONAL_TABLE_TYPE,
        0,
        8,
        1,
        936,
        8,
        8,
    );

    bytes[528] = 0;

    put_u16(&mut bytes, 536, 1);
    put_u16(&mut bytes, 536 + 0x02, 5);
    put_u64(&mut bytes, 536 + 0x08, 0);
    put_u64(&mut bytes, 536 + 0x10, 4096);
    put_u64(&mut bytes, 536 + 0x18, 4);
    put_u64(&mut bytes, EXTENDED_FIRST_SEGMENT_MEMORY_SIZE, 4096);
    put_u64(&mut bytes, 536 + 0x28, 4096);

    put_u16(&mut bytes, 600, 3);
    put_u16(&mut bytes, 600 + 0x02, 3);
    put_u64(&mut bytes, 600 + 0x08, 4096);
    put_u64(&mut bytes, 600 + 0x10, 8192);
    put_u64(&mut bytes, 600 + 0x18, 8);
    put_u64(&mut bytes, 600 + 0x20, 4096);
    put_u64(&mut bytes, 600 + 0x28, 4096);

    put_u32(&mut bytes, 664, 0);
    put_u32(&mut bytes, 664 + 0x04, 1);
    put_u32(&mut bytes, 664 + 0x08, 1);
    bytes[664 + 0x10..664 + 0x30].copy_from_slice(&[
        0xa6, 0xc1, 0xfb, 0x70, 0xa1, 0x0c, 0x4b, 0x82, 0xa6, 0x32, 0xa3, 0x02, 0x75, 0xef, 0x98,
        0xd9, 0x62, 0x4e, 0x46, 0xee, 0x8c, 0x3b, 0x2e, 0xdf, 0x02, 0xac, 0xb4, 0xe9, 0xef, 0x83,
        0x44, 0x2b,
    ]);

    put_u32(&mut bytes, 728, 1);
    put_u16(&mut bytes, 728 + 0x04, 1);
    put_u16(&mut bytes, 728 + 0x06, 1);
    put_u64(&mut bytes, 728 + 0x08, 1 << 5);

    put_u16(&mut bytes, 792, 1);
    put_u32(&mut bytes, EXTENDED_RELOCATION_TARGET_SEGMENT, 1);
    put_u64(&mut bytes, 792 + 0x08, 0);
    put_u32(&mut bytes, 792 + 0x10, u32::MAX);
    put_u64(&mut bytes, 792 + 0x18, 0);

    put_u64(&mut bytes, 840, 64 * 1024);
    put_u64(&mut bytes, 840 + 0x08, 4096);
    put_u32(&mut bytes, 840 + 0x38, 16);
    put_u32(&mut bytes, 840 + 0x3c, 192);

    bytes[936..944].copy_from_slice(b"optional");
    bytes[4096..4100].copy_from_slice(&[0x73, 0x00, 0x00, 0x00]);
    bytes[8192..8200].copy_from_slice(&[0; 8]);
    rehash(&mut bytes);
    bytes
}

pub fn rehash(bytes: &mut [u8]) {
    bytes[0x50..0x90].fill(0);
    let digest: [u8; 32] = Sha256::digest(&*bytes).into();
    bytes[0x50..0x70].copy_from_slice(&digest);
    bytes[0x70..0x90].copy_from_slice(&digest);
}

pub fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

pub fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

pub fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_directory(
    bytes: &mut [u8],
    index: usize,
    table_type: u16,
    flags: u16,
    entry_size: u32,
    entry_count: u32,
    file_offset: u64,
    file_size: u64,
    alignment: u64,
) {
    let offset = 192 + index * 48;
    put_u16(bytes, offset, table_type);
    put_u16(bytes, offset + 0x02, flags);
    put_u32(bytes, offset + 0x04, entry_size);
    put_u32(bytes, offset + 0x08, entry_count);
    put_u64(bytes, offset + 0x10, file_offset);
    put_u64(bytes, offset + 0x18, file_size);
    put_u64(bytes, offset + 0x20, alignment);
}
