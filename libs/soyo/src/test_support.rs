//! 仅供测试构造直接 SOYO 镜像的规范 encoder。

use alloc::vec;
use alloc::vec::Vec;

use native_abi::{
    ABI_EPOCH, ABI_FAMILY_MYGO_NATIVE, OperationId, RequirementId, TargetArch, operation,
    requirement,
};
use sha2::{Digest, Sha256};

use crate::error::{MalformedKind, ResourceKind, SoyoError};
use crate::registry::{
    ArtifactKind, FORMAT_VERSION, FeatureFlags, HashAlgorithm, RuntimeFlags, SOYO_MAGIC,
    SegmentKind, SegmentPermissions, TableType,
};
use crate::wire;

pub struct SoyoTestEncoder<'a> {
    target_arch: TargetArch,
    code: &'a [u8],
}

pub const LOADER_FIXTURE_RODATA_OFFSET: u64 = 4096;
pub const LOADER_FIXTURE_DATA_OFFSET: u64 = 8192;
pub const LOADER_FIXTURE_BSS_OFFSET: u64 = 12288;
pub const LOADER_FIXTURE_RODATA: [u8; 16] = [
    0, 0, 0, 0, 0, 0, 0, 0, b'R', b'O', b'D', b'A', b'T', b'A', b'!', 0,
];
pub const LOADER_FIXTURE_DATA: [u8; 16] = [
    0, 0, 0, 0, 0, 0, 0, 0, b'D', b'A', b'T', b'A', b'!', 0x5a, 0xa5, 0,
];
pub const LOADER_FIXTURE_TLS: [u8; 4] = [0x54, 0x4c, 0x53, 0x21];
pub const LOADER_FIXTURE_TLS_SIZE: usize = 32;

/// 构造覆盖段权限、重定位、零填充与静态 TLS 的直接 SOYO 测试映像。
pub struct SoyoLoaderTestEncoder<'a> {
    target_arch: TargetArch,
    code: &'a [u8],
    init_array: bool,
    start_info_max_size: u32,
}

impl<'a> SoyoLoaderTestEncoder<'a> {
    pub const fn new(target_arch: TargetArch, code: &'a [u8]) -> Self {
        Self {
            target_arch,
            code,
            init_array: false,
            start_info_max_size: 4096,
        }
    }

    pub const fn with_init_array(mut self) -> Self {
        self.init_array = true;
        self
    }

    pub const fn with_start_info_max_size(mut self, max_size: u32) -> Self {
        self.start_info_max_size = max_size;
        self
    }

    pub fn encode(&self) -> Result<Vec<u8>, SoyoError> {
        if self.code.is_empty() || self.code.len() > 4096 {
            return Err(SoyoError::ResourceExhausted(ResourceKind::ImageSize));
        }

        const STRING_OFFSET: usize = 480;
        const SEGMENT_TABLE_OFFSET: usize = 488;
        const IMPORT_OFFSET: usize = 808;
        const CAPABILITY_OFFSET: usize = 872;
        const RELOCATION_OFFSET: usize = 936;
        const RUNTIME_OFFSET: usize = 984;
        const CODE_FILE_OFFSET: usize = 4096;
        const RODATA_FILE_OFFSET: usize = 8192;
        const DATA_FILE_OFFSET: usize = 12288;
        const TLS_FILE_OFFSET: usize = 16384;
        const FILE_SIZE: usize = TLS_FILE_OFFSET + LOADER_FIXTURE_TLS.len();

        let mut bytes = vec![0u8; FILE_SIZE];
        bytes[wire::header::MAGIC..wire::header::MAGIC + 4].copy_from_slice(&SOYO_MAGIC);
        put_u16(&mut bytes, wire::header::FORMAT_VERSION, FORMAT_VERSION);
        put_u16(
            &mut bytes,
            wire::header::HEADER_SIZE,
            wire::HEADER_SIZE as u16,
        );
        put_u16(
            &mut bytes,
            wire::header::ARTIFACT_KIND,
            ArtifactKind::Executable as u16,
        );
        put_u16(
            &mut bytes,
            wire::header::TARGET_ARCH,
            self.target_arch as u16,
        );
        bytes[wire::header::ENDIAN] = 1;
        bytes[wire::header::POINTER_WIDTH] = 64;
        put_u16(&mut bytes, wire::header::ABI_FAMILY, ABI_FAMILY_MYGO_NATIVE);
        put_u16(&mut bytes, wire::header::ABI_EPOCH, ABI_EPOCH);
        put_u16(
            &mut bytes,
            wire::header::HASH_ALGORITHM,
            HashAlgorithm::Sha256 as u16,
        );
        let required_features = FeatureFlags::STATIC_TLS.bits()
            | if self.init_array {
                FeatureFlags::INIT_FINI_ARRAY.bits()
            } else {
                0
            };
        put_u64(
            &mut bytes,
            wire::header::REQUIRED_FEATURES,
            required_features,
        );
        put_u64(
            &mut bytes,
            wire::header::TABLE_OFFSET,
            wire::HEADER_SIZE as u64,
        );
        put_u32(&mut bytes, wire::header::TABLE_COUNT, 6);
        put_u16(
            &mut bytes,
            wire::header::TABLE_ENTRY_SIZE,
            wire::DIRECTORY_ENTRY_SIZE as u16,
        );
        put_u64(&mut bytes, wire::header::FILE_SIZE, FILE_SIZE as u64);
        put_u64(&mut bytes, wire::header::IMAGE_VIRTUAL_SIZE, 4 * 4096);

        put_directory(
            &mut bytes,
            0,
            TableType::String,
            1,
            1,
            STRING_OFFSET as u64,
            1,
            1,
        );
        put_directory(
            &mut bytes,
            1,
            TableType::ImageSegment,
            wire::IMAGE_SEGMENT_SIZE as u32,
            5,
            SEGMENT_TABLE_OFFSET as u64,
            (5 * wire::IMAGE_SEGMENT_SIZE) as u64,
            8,
        );
        put_directory(
            &mut bytes,
            2,
            TableType::AbiImport,
            wire::ABI_IMPORT_SIZE as u32,
            1,
            IMPORT_OFFSET as u64,
            wire::ABI_IMPORT_SIZE as u64,
            8,
        );
        put_directory(
            &mut bytes,
            3,
            TableType::CapabilityRequirement,
            wire::CAPABILITY_REQUIREMENT_SIZE as u32,
            1,
            CAPABILITY_OFFSET as u64,
            wire::CAPABILITY_REQUIREMENT_SIZE as u64,
            8,
        );
        put_directory(
            &mut bytes,
            4,
            TableType::Relocation,
            wire::RELOCATION_SIZE as u32,
            1,
            RELOCATION_OFFSET as u64,
            wire::RELOCATION_SIZE as u64,
            8,
        );
        put_directory(
            &mut bytes,
            5,
            TableType::RuntimeInfo,
            wire::RUNTIME_INFO_SIZE as u32,
            1,
            RUNTIME_OFFSET as u64,
            wire::RUNTIME_INFO_SIZE as u64,
            8,
        );

        bytes[STRING_OFFSET] = 0;
        put_segment(
            &mut bytes,
            SEGMENT_TABLE_OFFSET,
            SegmentKind::Code,
            SegmentPermissions::READ | SegmentPermissions::EXECUTE,
            0,
            CODE_FILE_OFFSET as u64,
            self.code.len() as u64,
            4096,
            4096,
        );
        put_segment(
            &mut bytes,
            SEGMENT_TABLE_OFFSET + wire::IMAGE_SEGMENT_SIZE,
            SegmentKind::Rodata,
            SegmentPermissions::READ,
            LOADER_FIXTURE_RODATA_OFFSET,
            RODATA_FILE_OFFSET as u64,
            LOADER_FIXTURE_RODATA.len() as u64,
            4096,
            4096,
        );
        put_segment(
            &mut bytes,
            SEGMENT_TABLE_OFFSET + 2 * wire::IMAGE_SEGMENT_SIZE,
            SegmentKind::Data,
            SegmentPermissions::READ | SegmentPermissions::WRITE,
            LOADER_FIXTURE_DATA_OFFSET,
            DATA_FILE_OFFSET as u64,
            LOADER_FIXTURE_DATA.len() as u64,
            4096,
            4096,
        );
        put_segment(
            &mut bytes,
            SEGMENT_TABLE_OFFSET + 3 * wire::IMAGE_SEGMENT_SIZE,
            SegmentKind::Bss,
            SegmentPermissions::READ | SegmentPermissions::WRITE,
            LOADER_FIXTURE_BSS_OFFSET,
            0,
            0,
            4096,
            4096,
        );
        put_segment(
            &mut bytes,
            SEGMENT_TABLE_OFFSET + 4 * wire::IMAGE_SEGMENT_SIZE,
            SegmentKind::TlsTemplate,
            SegmentPermissions::READ | SegmentPermissions::WRITE,
            0,
            TLS_FILE_OFFSET as u64,
            LOADER_FIXTURE_TLS.len() as u64,
            LOADER_FIXTURE_TLS_SIZE as u64,
            16,
        );

        let operation = operation(OperationId::ProcessExit)
            .ok_or(SoyoError::Malformed(MalformedKind::Import))?;
        put_u32(&mut bytes, IMPORT_OFFSET + wire::abi_import::SLOT, 0);
        put_u32(
            &mut bytes,
            IMPORT_OFFSET + wire::abi_import::OPERATION_ID,
            operation.id as u32,
        );
        put_u32(&mut bytes, IMPORT_OFFSET + wire::abi_import::FLAGS, 1);
        bytes[IMPORT_OFFSET + wire::abi_import::SIGNATURE_HASH
            ..IMPORT_OFFSET + wire::abi_import::SIGNATURE_HASH + 32]
            .copy_from_slice(&operation.signature_hash);

        let requirement = requirement(RequirementId::SelfProcess)
            .ok_or(SoyoError::Malformed(MalformedKind::Capability))?;
        put_u32(
            &mut bytes,
            CAPABILITY_OFFSET + wire::capability_requirement::REQUIREMENT_ID,
            requirement.id as u32,
        );
        put_u16(
            &mut bytes,
            CAPABILITY_OFFSET + wire::capability_requirement::OBJECT_INTERFACE,
            requirement.interface as u16,
        );
        put_u16(
            &mut bytes,
            CAPABILITY_OFFSET + wire::capability_requirement::FLAGS,
            1,
        );
        put_u64(
            &mut bytes,
            CAPABILITY_OFFSET + wire::capability_requirement::REQUIRED_RIGHTS,
            requirement.max_rights.bits(),
        );

        put_u16(&mut bytes, RELOCATION_OFFSET + wire::relocation::KIND, 1);
        put_u32(
            &mut bytes,
            RELOCATION_OFFSET + wire::relocation::TARGET_SEGMENT_INDEX,
            2,
        );
        put_u32(
            &mut bytes,
            RELOCATION_OFFSET + wire::relocation::SOURCE_SEGMENT_INDEX,
            u32::MAX,
        );

        put_u64(
            &mut bytes,
            RUNTIME_OFFSET + wire::runtime_info::STACK_SIZE,
            64 * 1024,
        );
        put_u64(
            &mut bytes,
            RUNTIME_OFFSET + wire::runtime_info::STACK_GUARD_SIZE,
            4096,
        );
        if self.init_array {
            put_u64(
                &mut bytes,
                RUNTIME_OFFSET + wire::runtime_info::RUNTIME_FLAGS,
                RuntimeFlags::RUN_INIT_ARRAY.bits(),
            );
            put_u64(
                &mut bytes,
                RUNTIME_OFFSET + wire::runtime_info::INIT_ARRAY_OFFSET,
                LOADER_FIXTURE_RODATA_OFFSET,
            );
            put_u32(
                &mut bytes,
                RUNTIME_OFFSET + wire::runtime_info::INIT_ARRAY_COUNT,
                1,
            );
            put_u16(
                &mut bytes,
                RUNTIME_OFFSET + wire::runtime_info::INIT_ARRAY_ENTRY_SIZE,
                8,
            );
        }
        put_u32(
            &mut bytes,
            RUNTIME_OFFSET + wire::runtime_info::STACK_ALIGNMENT,
            16,
        );
        put_u32(
            &mut bytes,
            RUNTIME_OFFSET + wire::runtime_info::START_INFO_MAX_SIZE,
            self.start_info_max_size,
        );

        bytes[CODE_FILE_OFFSET..CODE_FILE_OFFSET + self.code.len()].copy_from_slice(self.code);
        bytes[RODATA_FILE_OFFSET..RODATA_FILE_OFFSET + LOADER_FIXTURE_RODATA.len()]
            .copy_from_slice(&LOADER_FIXTURE_RODATA);
        bytes[DATA_FILE_OFFSET..DATA_FILE_OFFSET + LOADER_FIXTURE_DATA.len()]
            .copy_from_slice(&LOADER_FIXTURE_DATA);
        bytes[TLS_FILE_OFFSET..].copy_from_slice(&LOADER_FIXTURE_TLS);
        rehash(&mut bytes);
        Ok(bytes)
    }
}

impl<'a> SoyoTestEncoder<'a> {
    pub const fn minimal(target_arch: TargetArch, code: &'a [u8]) -> Self {
        Self { target_arch, code }
    }

    pub fn encode(&self) -> Result<Vec<u8>, SoyoError> {
        if self.code.is_empty() || self.code.len() > 4096 {
            return Err(SoyoError::ResourceExhausted(ResourceKind::ImageSize));
        }
        let file_size = 8192usize;
        let mut bytes = vec![0u8; file_size];

        bytes[wire::header::MAGIC..wire::header::MAGIC + 4].copy_from_slice(&SOYO_MAGIC);
        put_u16(&mut bytes, wire::header::FORMAT_VERSION, FORMAT_VERSION);
        put_u16(
            &mut bytes,
            wire::header::HEADER_SIZE,
            wire::HEADER_SIZE as u16,
        );
        put_u16(
            &mut bytes,
            wire::header::ARTIFACT_KIND,
            ArtifactKind::Executable as u16,
        );
        put_u16(
            &mut bytes,
            wire::header::TARGET_ARCH,
            self.target_arch as u16,
        );
        bytes[wire::header::ENDIAN] = 1;
        bytes[wire::header::POINTER_WIDTH] = 64;
        put_u16(&mut bytes, wire::header::ABI_FAMILY, ABI_FAMILY_MYGO_NATIVE);
        put_u16(&mut bytes, wire::header::ABI_EPOCH, ABI_EPOCH);
        put_u16(
            &mut bytes,
            wire::header::HASH_ALGORITHM,
            HashAlgorithm::Sha256 as u16,
        );
        put_u64(
            &mut bytes,
            wire::header::TABLE_OFFSET,
            wire::HEADER_SIZE as u64,
        );
        put_u32(&mut bytes, wire::header::TABLE_COUNT, 5);
        put_u16(
            &mut bytes,
            wire::header::TABLE_ENTRY_SIZE,
            wire::DIRECTORY_ENTRY_SIZE as u16,
        );
        put_u64(&mut bytes, wire::header::FILE_SIZE, file_size as u64);
        put_u64(&mut bytes, wire::header::IMAGE_VIRTUAL_SIZE, 4096);

        put_directory(&mut bytes, 0, TableType::String, 1, 1, 432, 1, 1);
        put_directory(
            &mut bytes,
            1,
            TableType::ImageSegment,
            wire::IMAGE_SEGMENT_SIZE as u32,
            1,
            440,
            wire::IMAGE_SEGMENT_SIZE as u64,
            8,
        );
        put_directory(
            &mut bytes,
            2,
            TableType::AbiImport,
            wire::ABI_IMPORT_SIZE as u32,
            1,
            504,
            wire::ABI_IMPORT_SIZE as u64,
            8,
        );
        put_directory(
            &mut bytes,
            3,
            TableType::CapabilityRequirement,
            wire::CAPABILITY_REQUIREMENT_SIZE as u32,
            1,
            568,
            wire::CAPABILITY_REQUIREMENT_SIZE as u64,
            8,
        );
        put_directory(
            &mut bytes,
            4,
            TableType::RuntimeInfo,
            wire::RUNTIME_INFO_SIZE as u32,
            1,
            632,
            wire::RUNTIME_INFO_SIZE as u64,
            8,
        );

        bytes[432] = 0;
        put_u16(&mut bytes, 440 + wire::image_segment::KIND, 1);
        put_u16(&mut bytes, 440 + wire::image_segment::PERMISSIONS, 5);
        put_u64(&mut bytes, 440 + wire::image_segment::FILE_OFFSET, 4096);
        put_u64(
            &mut bytes,
            440 + wire::image_segment::FILE_SIZE,
            self.code.len() as u64,
        );
        put_u64(&mut bytes, 440 + wire::image_segment::MEMORY_SIZE, 4096);
        put_u64(&mut bytes, 440 + wire::image_segment::ALIGNMENT, 4096);

        let operation = operation(OperationId::ProcessExit)
            .ok_or(SoyoError::Malformed(MalformedKind::Import))?;
        put_u32(&mut bytes, 504 + wire::abi_import::SLOT, 0);
        put_u32(
            &mut bytes,
            504 + wire::abi_import::OPERATION_ID,
            operation.id as u32,
        );
        put_u32(&mut bytes, 504 + wire::abi_import::FLAGS, 1);
        bytes[504 + wire::abi_import::SIGNATURE_HASH..504 + wire::abi_import::SIGNATURE_HASH + 32]
            .copy_from_slice(&operation.signature_hash);

        let requirement = requirement(RequirementId::SelfProcess)
            .ok_or(SoyoError::Malformed(MalformedKind::Capability))?;
        put_u32(
            &mut bytes,
            568 + wire::capability_requirement::REQUIREMENT_ID,
            requirement.id as u32,
        );
        put_u16(
            &mut bytes,
            568 + wire::capability_requirement::OBJECT_INTERFACE,
            requirement.interface as u16,
        );
        put_u16(&mut bytes, 568 + wire::capability_requirement::FLAGS, 1);
        put_u64(
            &mut bytes,
            568 + wire::capability_requirement::REQUIRED_RIGHTS,
            requirement.max_rights.bits(),
        );

        put_u64(&mut bytes, 632 + wire::runtime_info::STACK_SIZE, 64 * 1024);
        put_u64(&mut bytes, 632 + wire::runtime_info::STACK_GUARD_SIZE, 4096);
        put_u32(&mut bytes, 632 + wire::runtime_info::STACK_ALIGNMENT, 16);
        put_u32(
            &mut bytes,
            632 + wire::runtime_info::START_INFO_MAX_SIZE,
            4096,
        );

        bytes[4096..4096 + self.code.len()].copy_from_slice(self.code);
        rehash(&mut bytes);
        Ok(bytes)
    }
}

fn put_segment(
    bytes: &mut [u8],
    offset: usize,
    kind: SegmentKind,
    permissions: SegmentPermissions,
    virtual_offset: u64,
    file_offset: u64,
    file_size: u64,
    memory_size: u64,
    alignment: u64,
) {
    put_u16(bytes, offset + wire::image_segment::KIND, kind as u16);
    put_u16(
        bytes,
        offset + wire::image_segment::PERMISSIONS,
        permissions.bits(),
    );
    put_u64(
        bytes,
        offset + wire::image_segment::VIRTUAL_OFFSET,
        virtual_offset,
    );
    put_u64(
        bytes,
        offset + wire::image_segment::FILE_OFFSET,
        file_offset,
    );
    put_u64(bytes, offset + wire::image_segment::FILE_SIZE, file_size);
    put_u64(
        bytes,
        offset + wire::image_segment::MEMORY_SIZE,
        memory_size,
    );
    put_u64(bytes, offset + wire::image_segment::ALIGNMENT, alignment);
}

fn rehash(bytes: &mut [u8]) {
    bytes[wire::header::BUILD_ID..wire::header::CONTENT_HASH + 32].fill(0);
    let digest: [u8; 32] = Sha256::digest(&*bytes).into();
    bytes[wire::header::BUILD_ID..wire::header::BUILD_ID + 32].copy_from_slice(&digest);
    bytes[wire::header::CONTENT_HASH..wire::header::CONTENT_HASH + 32].copy_from_slice(&digest);
}

fn put_directory(
    bytes: &mut [u8],
    index: usize,
    table_type: TableType,
    entry_size: u32,
    entry_count: u32,
    file_offset: u64,
    file_size: u64,
    alignment: u64,
) {
    let offset = wire::HEADER_SIZE + index * wire::DIRECTORY_ENTRY_SIZE;
    put_u16(
        bytes,
        offset + wire::directory::TABLE_TYPE,
        table_type as u16,
    );
    put_u16(bytes, offset + wire::directory::FLAGS, 1);
    put_u32(bytes, offset + wire::directory::ENTRY_SIZE, entry_size);
    put_u32(bytes, offset + wire::directory::ENTRY_COUNT, entry_count);
    put_u64(bytes, offset + wire::directory::FILE_OFFSET, file_offset);
    put_u64(bytes, offset + wire::directory::FILE_SIZE, file_size);
    put_u64(bytes, offset + wire::directory::ALIGNMENT, alignment);
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
