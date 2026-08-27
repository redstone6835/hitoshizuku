use crate::{
    ResourceKind, SliceSoyoReader, SoyoError, SoyoReadAt, SoyoReadError, SoyoReadLimits,
    SoyoRuntimeLayoutInput, SoyoTargetPolicy, plan_mapped_segments, plan_runtime_layout, read_soyo,
    registry::RelocationKind, validate_soyo,
};

use core::cell::RefCell;
use core::convert::Infallible;
use native_abi::{OperationId, TargetArch};

use super::fixtures::{
    EXTENDED_FIRST_SEGMENT_FILE_OFFSET, EXTENDED_INIT_ARRAY_COUNT, EXTENDED_INIT_ARRAY_ENTRY_SIZE,
    EXTENDED_INIT_ARRAY_OFFSET, EXTENDED_OPTIONAL_TABLE_COUNT, EXTENDED_OPTIONAL_TABLE_ENTRY_SIZE,
    EXTENDED_OPTIONAL_TABLE_FILE_SIZE, EXTENDED_RELOCATION_ADDEND, EXTENDED_RELOCATION_KIND,
    EXTENDED_RELOCATION_SOURCE_SEGMENT, EXTENDED_RUNTIME_FLAGS,
    EXTENDED_SECOND_SEGMENT_FILE_OFFSET, EXTENDED_SECOND_SEGMENT_KIND,
    EXTENDED_SECOND_SEGMENT_PERMISSIONS, HEADER_FILE_SIZE, HEADER_REQUIRED_FEATURES,
    UNKNOWN_OPTIONAL_TABLE_TYPE, extended_soyo, minimal_soyo, put_u16, put_u32, put_u64, rehash,
};

#[test]
fn valid_minimal_image_produces_a_load_plan() {
    let bytes = minimal_soyo();
    let reader = SliceSoyoReader::new(&bytes);
    let metadata = read_soyo(&reader, SoyoReadLimits::portable()).expect("合法 SOYO 应通过解析");

    assert_eq!(metadata.header.target_arch, TargetArch::Riscv64);
    assert_eq!(metadata.header.abi_family, 1);
    assert_eq!(metadata.header.entry_offset, 0);
    assert_eq!(metadata.header.image_virtual_size, 4096);
    assert_eq!(metadata.segments.len(), 1);
    assert_eq!(metadata.imports.len(), 1);
    assert_eq!(
        metadata.imports[0].operation_id,
        OperationId::ProcessExit as u32
    );
    assert_eq!(metadata.capabilities.len(), 1);

    let plan = validate_soyo(&metadata, SoyoTargetPolicy::for_kernel(TargetArch::Riscv64))
        .expect("合法 SOYO 应产生装载计划");
    assert_eq!(plan.entry_offset, 0);
    assert_eq!(plan.metadata, &metadata);
}

#[test]
fn valid_relocation_is_decoded_from_a_complete_image() {
    let bytes = extended_soyo();
    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable())
        .expect("合法 relocation 应通过完整文件校验");

    assert_eq!(metadata.segments.len(), 2);
    assert_eq!(metadata.relocations.len(), 1);
    assert_eq!(metadata.relocations[0].target_segment_index, 1);
    assert_eq!(metadata.relocations[0].source_segment_index, u32::MAX);
}

#[test]
fn mapped_segments_are_rebased_without_mapping_tls_template() {
    let bytes = extended_soyo();
    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable())
        .expect("合法 SOYO 应通过解析");

    let mapped = plan_mapped_segments(&metadata, 0x0040_0000).expect("应生成映射计划");
    assert_eq!(mapped.len(), 2);
    assert_eq!(mapped[0].virtual_start, 0x0040_0000);
    assert_eq!(mapped[1].virtual_start, 0x0040_1000);
    assert_eq!(mapped[1].memory_size, 4096);
}

#[test]
fn runtime_layout_keeps_start_info_tls_guard_and_stack_disjoint() {
    let layout = plan_runtime_layout(SoyoRuntimeLayoutInput {
        image_base: 0x0040_0000,
        image_virtual_size: 0x2000,
        stack_top: 0x8000_0000,
        stack_size: 0x1_0000,
        stack_guard_size: 0x2000,
        tls_memory_size: 0x1780,
        tls_alignment: 0x100,
        start_info_size: 304,
        user_lower_bound: 0x1000,
    })
    .expect("合法运行时布局应完成规划");

    assert_eq!(layout.image, 0x0040_0000..0x0040_2000);
    assert_eq!(layout.stack, 0x7fff_0000..0x8000_0000);
    assert_eq!(layout.guard, 0x7ffe_e000..0x7fff_0000);
    assert_eq!(layout.tls, Some(0x7ffe_c000..0x7ffe_e000));
    assert_eq!(layout.initial_tls_size, 0x1800);
    assert_eq!(layout.start_info, 0x7ffe_b000..0x7ffe_c000);
}

#[test]
fn runtime_layout_rejects_underflow_and_image_overlap() {
    let base = SoyoRuntimeLayoutInput {
        image_base: 0x0040_0000,
        image_virtual_size: 0x2000,
        stack_top: 0x20_000,
        stack_size: 0x1_0000,
        stack_guard_size: 0x2000,
        tls_memory_size: 0,
        tls_alignment: 0,
        start_info_size: 304,
        user_lower_bound: 0x10_000,
    };
    assert_eq!(
        plan_runtime_layout(base),
        Err(SoyoError::Malformed(crate::MalformedKind::Range))
    );

    assert_eq!(
        plan_runtime_layout(SoyoRuntimeLayoutInput {
            image_base: 0x7ffe_d000,
            image_virtual_size: 0x3000,
            stack_top: 0x8000_0000,
            user_lower_bound: 0x1000,
            ..base
        }),
        Err(SoyoError::Malformed(crate::MalformedKind::Range))
    );
}

#[test]
fn valid_segment_base_relocation_is_decoded_from_a_complete_image() {
    let mut bytes = extended_soyo();
    put_u16(&mut bytes, EXTENDED_RELOCATION_KIND, 2);
    put_u32(&mut bytes, EXTENDED_RELOCATION_SOURCE_SEGMENT, 0);
    put_u64(&mut bytes, EXTENDED_RELOCATION_ADDEND, 4096);
    rehash(&mut bytes);

    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable())
        .expect("合法 SEGMENT_BASE64 relocation 应通过完整文件校验");
    assert_eq!(metadata.relocations[0].kind, RelocationKind::SegmentBase64);
    assert_eq!(metadata.relocations[0].source_segment_index, 0);
    assert_eq!(metadata.relocations[0].addend, 4096);
}

#[test]
fn unknown_optional_table_is_preserved_and_ignored() {
    let bytes = extended_soyo();
    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable())
        .expect("未知 optional 表通过通用校验后应被忽略");

    assert!(
        metadata
            .directory
            .iter()
            .any(|entry| entry.table_type == UNKNOWN_OPTIONAL_TABLE_TYPE && entry.flags == 0)
    );
    validate_soyo(&metadata, SoyoTargetPolicy::for_kernel(TargetArch::Riscv64))
        .expect("未知 optional 表不得改变基础执行语义");
}

#[test]
fn unknown_optional_payload_does_not_consume_parser_allocation_budget() {
    const OPTIONAL_SIZE: usize = 4_216_929;
    const OPTIONAL_OFFSET: usize = 936;

    let mut bytes = extended_soyo();
    let code_offset = (OPTIONAL_OFFSET + OPTIONAL_SIZE + 4095) & !4095;
    let data_offset = code_offset + 4096;
    let file_size = data_offset + 4096;
    bytes.resize(file_size, 0);
    put_u32(&mut bytes, EXTENDED_OPTIONAL_TABLE_ENTRY_SIZE, 1);
    put_u32(
        &mut bytes,
        EXTENDED_OPTIONAL_TABLE_COUNT,
        OPTIONAL_SIZE as u32,
    );
    put_u64(
        &mut bytes,
        EXTENDED_OPTIONAL_TABLE_FILE_SIZE,
        OPTIONAL_SIZE as u64,
    );
    put_u64(
        &mut bytes,
        EXTENDED_FIRST_SEGMENT_FILE_OFFSET,
        code_offset as u64,
    );
    put_u64(
        &mut bytes,
        EXTENDED_SECOND_SEGMENT_FILE_OFFSET,
        data_offset as u64,
    );
    put_u64(&mut bytes, HEADER_FILE_SIZE, file_size as u64);
    bytes[code_offset..code_offset + 4].copy_from_slice(&[0x73, 0, 0, 0]);
    rehash(&mut bytes);

    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable())
        .expect("未知 optional payload 不需要 parser 分配同等大小的缓冲区");
    assert_eq!(metadata.segments[0].file_offset, code_offset as u64);
}

#[test]
fn host_and_kernel_accept_nonempty_init_array() {
    let bytes = soyo_with_init_array(TargetArch::Riscv64, 0);

    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable())
        .expect("合法 init array 应通过格式解析");
    validate_soyo(&metadata, SoyoTargetPolicy::for_host()).expect("host 应接受完整格式语义");
    validate_soyo(&metadata, SoyoTargetPolicy::for_kernel(TargetArch::Riscv64))
        .expect("kernel 应接受完整 init/fini 语义");
}

#[test]
fn init_array_entry_must_be_inside_raw_code_payload() {
    let bytes = soyo_with_init_array(TargetArch::Riscv64, 4);

    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()),
        Err(SoyoReadError::Format(crate::SoyoError::Malformed(
            crate::MalformedKind::Runtime
        )))
    );
}

#[test]
fn init_array_entry_cannot_point_into_rodata() {
    let bytes = soyo_with_init_array(TargetArch::Riscv64, 4096);

    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()),
        Err(SoyoReadError::Format(crate::SoyoError::Malformed(
            crate::MalformedKind::Runtime
        )))
    );
}

#[test]
fn init_array_entry_cannot_point_outside_the_image() {
    let bytes = soyo_with_init_array(TargetArch::Riscv64, 8192);

    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()),
        Err(SoyoReadError::Format(crate::SoyoError::Malformed(
            crate::MalformedKind::Runtime
        )))
    );
}

#[test]
fn rv64_init_array_entry_must_be_two_byte_aligned() {
    let bytes = soyo_with_init_array(TargetArch::Riscv64, 1);

    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()),
        Err(SoyoReadError::Format(crate::SoyoError::Malformed(
            crate::MalformedKind::Runtime
        )))
    );
}

#[test]
fn la64_init_array_entry_must_be_four_byte_aligned() {
    let bytes = soyo_with_init_array(TargetArch::LoongArch64, 2);

    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()),
        Err(SoyoReadError::Format(crate::SoyoError::Malformed(
            crate::MalformedKind::Runtime
        )))
    );
}

#[test]
fn x86_64_init_array_entry_accepts_byte_alignment() {
    let bytes = soyo_with_init_array(TargetArch::X86_64, 1);

    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable())
        .expect("x86_64 原生入口允许字节对齐");
    assert_eq!(metadata.header.target_arch, TargetArch::X86_64);
}

fn soyo_with_init_array(target_arch: TargetArch, entry: u64) -> alloc::vec::Vec<u8> {
    let mut bytes = extended_soyo();
    put_u16(
        &mut bytes,
        super::fixtures::HEADER_TARGET_ARCH,
        target_arch as u16,
    );
    put_u64(&mut bytes, HEADER_REQUIRED_FEATURES, 1 << 1);
    put_u16(&mut bytes, EXTENDED_SECOND_SEGMENT_KIND, 2);
    put_u16(&mut bytes, EXTENDED_SECOND_SEGMENT_PERMISSIONS, 1);
    put_u64(&mut bytes, EXTENDED_RUNTIME_FLAGS, 1);
    put_u64(&mut bytes, EXTENDED_INIT_ARRAY_OFFSET, 4096);
    put_u32(&mut bytes, EXTENDED_INIT_ARRAY_COUNT, 1);
    put_u16(&mut bytes, EXTENDED_INIT_ARRAY_ENTRY_SIZE, 8);
    put_u64(&mut bytes, 8192, entry);
    rehash(&mut bytes);
    bytes
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadFault;

struct RecordingReader<'a> {
    bytes: &'a [u8],
    fail_at: Option<u64>,
    ranges: RefCell<alloc::vec::Vec<(u64, usize)>>,
}

impl SoyoReadAt for RecordingReader<'_> {
    type Error = ReadFault;

    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> Result<(), Self::Error> {
        if self.fail_at.is_some_and(|boundary| offset >= boundary) {
            return Err(ReadFault);
        }
        self.ranges.borrow_mut().push((offset, output.len()));
        let start = offset as usize;
        output.copy_from_slice(&self.bytes[start..start + output.len()]);
        Ok(())
    }
}

#[test]
fn source_failure_is_not_reported_as_a_format_error() {
    let bytes = minimal_soyo();
    let reader = RecordingReader {
        bytes: &bytes,
        fail_at: Some(0),
        ranges: RefCell::new(alloc::vec::Vec::new()),
    };
    assert_eq!(
        read_soyo(&reader, SoyoReadLimits::portable()),
        Err(SoyoReadError::Source(ReadFault))
    );
}

#[test]
fn parser_never_requests_bytes_beyond_the_declared_file() {
    let bytes = minimal_soyo();
    let reader = RecordingReader {
        bytes: &bytes,
        fail_at: None,
        ranges: RefCell::new(alloc::vec::Vec::new()),
    };
    read_soyo(&reader, SoyoReadLimits::portable()).expect("合法镜像应解析");
    assert!(reader.ranges.borrow().iter().all(|(offset, length)| {
        offset
            .checked_add(*length as u64)
            .is_some_and(|end| end <= bytes.len() as u64)
    }));
}

struct OversizedReader;

impl SoyoReadAt for OversizedReader {
    type Error = Infallible;

    fn len(&self) -> u64 {
        256 * 1024 * 1024 + 1
    }

    fn read_exact_at(&self, _offset: u64, _output: &mut [u8]) -> Result<(), Self::Error> {
        unreachable!("文件大小超限必须在首次读取前拒绝")
    }
}

#[test]
fn file_over_wire_limit_is_rejected_before_reading() {
    assert_eq!(
        read_soyo(&OversizedReader, SoyoReadLimits::portable()),
        Err(SoyoReadError::ResourceExhausted(ResourceKind::FileSize))
    );
}

#[test]
fn allocation_failure_keeps_its_resource_kind() {
    let error =
        SoyoReadError::<Infallible>::from(SoyoError::AllocationFailed(ResourceKind::Imports));

    assert_eq!(
        error,
        SoyoReadError::AllocationFailed(ResourceKind::Imports)
    );
}
