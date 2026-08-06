//! 仅供测试构造直接 SOYO 镜像的规范 encoder。

use alloc::vec;
use alloc::vec::Vec;

use native_abi::{
    ABI_EPOCH, ABI_FAMILY_MYGO_NATIVE, OperationId, RequirementId, TargetArch, operation,
    requirement, wire as native_wire,
};
use sha2::{Digest, Sha256};

use crate::error::{MalformedKind, ResourceKind, SoyoError};
use crate::registry::{ArtifactKind, FORMAT_VERSION, HashAlgorithm, SOYO_MAGIC, TableType};
use crate::wire;

pub struct SoyoTestEncoder<'a> {
    target_arch: TargetArch,
    code: &'a [u8],
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
            native_wire::START_INFO_SIZE as u32,
        );

        bytes[4096..4096 + self.code.len()].copy_from_slice(self.code);
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        bytes[wire::header::BUILD_ID..wire::header::BUILD_ID + 32].copy_from_slice(&digest);
        bytes[wire::header::CONTENT_HASH..wire::header::CONTENT_HASH + 32].copy_from_slice(&digest);
        Ok(bytes)
    }
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
