use crate::{
    MalformedKind, SliceSoyoReader, SoyoError, SoyoErrorCategory, SoyoReadError, SoyoReadLimits,
    SoyoTargetPolicy, UnsupportedKind, read_soyo, validate_soyo,
};

use native_abi::{NativeAbiError, NativeAbiPolicy, OperationId, TargetArch, bind_native_abi};

use super::fixtures::{
    CAP_FLAGS, CAP_OBJECT_INTERFACE, CAP_REQUIREMENT_ID, CAP_RIGHTS, DIRECTORY_CAPABILITY_COUNT,
    DIRECTORY_CAPABILITY_FILE_SIZE, DIRECTORY_FIRST_TYPE, DIRECTORY_IMPORT_COUNT,
    DIRECTORY_IMPORT_FILE_SIZE, DIRECTORY_RUNTIME_COUNT, DIRECTORY_RUNTIME_ENTRY_SIZE,
    DIRECTORY_RUNTIME_FILE_SIZE, DIRECTORY_RUNTIME_TYPE, DIRECTORY_SECOND_TYPE,
    DIRECTORY_SEGMENT_COUNT, DIRECTORY_SEGMENT_FILE_SIZE, DIRECTORY_STRING_COUNT,
    DIRECTORY_STRING_FILE_OFFSET, DIRECTORY_STRING_FILE_SIZE, EXTENDED_FIRST_SEGMENT_MEMORY_SIZE,
    EXTENDED_OPTIONAL_TABLE_FLAGS, EXTENDED_OPTIONAL_TABLE_TYPE, EXTENDED_RELOCATION_ADDEND,
    EXTENDED_RELOCATION_KIND, EXTENDED_RELOCATION_RESERVED0, EXTENDED_RELOCATION_SOURCE_SEGMENT,
    EXTENDED_RELOCATION_TARGET_OFFSET, EXTENDED_RELOCATION_TARGET_SEGMENT, HEADER_ABI_EPOCH,
    HEADER_BUILD_ID, HEADER_ENTRY_OFFSET, HEADER_FILE_SIZE, HEADER_IMAGE_VIRTUAL_SIZE,
    HEADER_OPTIONAL_FEATURES, HEADER_REQUIRED_FEATURES, HEADER_RESERVED1, HEADER_TABLE_COUNT,
    HEADER_TARGET_ARCH, IMPORT_FLAGS, IMPORT_NAME, IMPORT_OPERATION_ID, IMPORT_SIGNATURE_HASH,
    SEGMENT_ALIGNMENT, SEGMENT_FILE_OFFSET, SEGMENT_KIND, SEGMENT_MEMORY_SIZE, SEGMENT_PERMISSIONS,
    TABLE_PADDING, UNKNOWN_OPTIONAL_TABLE_TYPE, extended_soyo, minimal_soyo, put_u16, put_u32,
    put_u64, rehash,
};

fn parse_category(bytes: &[u8]) -> SoyoErrorCategory {
    let reader = SliceSoyoReader::new(bytes);
    match read_soyo(&reader, SoyoReadLimits::portable()).expect_err("镜像应被拒绝") {
        SoyoReadError::Format(error) => error.category(),
        SoyoReadError::Source(never) => match never {},
        SoyoReadError::ResourceExhausted(_) => SoyoErrorCategory::ResourceExhausted,
        SoyoReadError::AllocationFailed(_) => SoyoErrorCategory::AllocationFailed,
    }
}

fn parse_format_error(bytes: &[u8]) -> SoyoError {
    match read_soyo(&SliceSoyoReader::new(bytes), SoyoReadLimits::portable()) {
        Err(SoyoReadError::Format(error)) => error,
        Err(SoyoReadError::Source(never)) => match never {},
        Err(SoyoReadError::ResourceExhausted(_)) => panic!("预期格式错误，实际为资源耗尽"),
        Err(SoyoReadError::AllocationFailed(_)) => panic!("预期格式错误，实际为分配失败"),
        Ok(_) => panic!("镜像应被拒绝"),
    }
}

#[test]
fn bad_magic_is_malformed() {
    let mut bytes = minimal_soyo();
    bytes[0] = b'x';
    assert_eq!(parse_category(&bytes), SoyoErrorCategory::Malformed);
}

#[test]
fn unknown_non_reserved_abi_family_is_preserved_for_external_binding() {
    let mut bytes = minimal_soyo();
    put_u16(&mut bytes, 0x0e, 2);
    rehash(&mut bytes);

    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable())
        .expect("SOYO 格式层应保留未知但非保留的 ABI family");
    assert_eq!(metadata.header.abi_family, 2);
}

#[test]
fn reserved_abi_family_identities_are_malformed() {
    for family in [0, u16::MAX] {
        let mut bytes = minimal_soyo();
        put_u16(&mut bytes, 0x0e, family);
        rehash(&mut bytes);
        assert_eq!(parse_category(&bytes), SoyoErrorCategory::Malformed);
    }
}

#[test]
fn truncated_header_is_malformed() {
    assert_eq!(
        parse_category(&minimal_soyo()[..100]),
        SoyoErrorCategory::Malformed
    );
}

#[test]
fn configured_limits_cannot_raise_the_wire_directory_limit() {
    let mut bytes = minimal_soyo();
    put_u32(&mut bytes, HEADER_TABLE_COUNT, 65);
    let mut limits = SoyoReadLimits::portable();
    limits.max_directory_entries = 65;

    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), limits),
        Err(SoyoReadError::ResourceExhausted(
            crate::ResourceKind::DirectoryCount
        ))
    );
}

#[test]
fn configured_limits_cannot_raise_the_wire_segment_limit() {
    let mut bytes = minimal_soyo();
    put_u32(&mut bytes, DIRECTORY_SEGMENT_COUNT, 33);
    put_u64(&mut bytes, DIRECTORY_SEGMENT_FILE_SIZE, 33 * 64);
    let mut limits = SoyoReadLimits::portable();
    limits.max_segments = 33;

    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), limits),
        Err(SoyoReadError::ResourceExhausted(
            crate::ResourceKind::Segments
        ))
    );
}

#[test]
fn configured_table_budget_is_enforced_before_allocation() {
    let bytes = minimal_soyo();
    let mut limits = SoyoReadLimits::portable();
    limits.max_table_bytes = 288;

    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), limits),
        Err(SoyoReadError::ResourceExhausted(
            crate::ResourceKind::TableBytes
        ))
    );
}

#[test]
fn configured_limits_cannot_raise_the_wire_string_limit() {
    const COUNT: u32 = 1024 * 1024 + 1;

    let mut bytes = minimal_soyo();
    put_u32(&mut bytes, DIRECTORY_STRING_COUNT, COUNT);
    put_u64(&mut bytes, DIRECTORY_STRING_FILE_SIZE, u64::from(COUNT));
    bytes.resize(432 + COUNT as usize, 0);
    let file_size = bytes.len() as u64;
    put_u64(&mut bytes, HEADER_FILE_SIZE, file_size);
    let mut limits = SoyoReadLimits::portable();
    limits.max_string_bytes = COUNT as usize;

    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), limits),
        Err(SoyoReadError::ResourceExhausted(
            crate::ResourceKind::StringBytes
        ))
    );
}

#[test]
fn configured_limits_cannot_raise_the_wire_import_limit() {
    const COUNT: u32 = 257;
    const TABLE_SIZE: usize = COUNT as usize * 64;

    let mut bytes = minimal_soyo();
    put_u32(&mut bytes, DIRECTORY_IMPORT_COUNT, COUNT);
    put_u64(&mut bytes, DIRECTORY_IMPORT_FILE_SIZE, TABLE_SIZE as u64);
    bytes.resize(504 + TABLE_SIZE, 0);
    let file_size = bytes.len() as u64;
    put_u64(&mut bytes, HEADER_FILE_SIZE, file_size);
    let mut limits = SoyoReadLimits::portable();
    limits.max_imports = COUNT;

    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), limits),
        Err(SoyoReadError::ResourceExhausted(
            crate::ResourceKind::Imports
        ))
    );
}

#[test]
fn configured_limits_cannot_raise_the_wire_capability_limit() {
    const COUNT: u32 = 65;

    let mut bytes = minimal_soyo();
    put_u32(&mut bytes, DIRECTORY_CAPABILITY_COUNT, COUNT);
    put_u64(
        &mut bytes,
        DIRECTORY_CAPABILITY_FILE_SIZE,
        u64::from(COUNT) * 64,
    );
    let mut limits = SoyoReadLimits::portable();
    limits.max_capabilities = COUNT;

    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), limits),
        Err(SoyoReadError::ResourceExhausted(
            crate::ResourceKind::Capabilities
        ))
    );
}

#[test]
fn configured_limits_cannot_raise_the_wire_relocation_limit() {
    const COUNT: u32 = 65_537;
    const TABLE_SIZE: usize = COUNT as usize * 48;

    let mut bytes = minimal_soyo();
    put_u16(&mut bytes, DIRECTORY_RUNTIME_TYPE, 5);
    put_u32(&mut bytes, DIRECTORY_RUNTIME_ENTRY_SIZE, 48);
    put_u32(&mut bytes, DIRECTORY_RUNTIME_COUNT, COUNT);
    put_u64(&mut bytes, DIRECTORY_RUNTIME_FILE_SIZE, TABLE_SIZE as u64);
    bytes.resize(632 + TABLE_SIZE, 0);
    let file_size = bytes.len() as u64;
    put_u64(&mut bytes, HEADER_FILE_SIZE, file_size);
    let mut limits = SoyoReadLimits::portable();
    limits.max_relocations = COUNT;

    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), limits),
        Err(SoyoReadError::ResourceExhausted(
            crate::ResourceKind::Relocations
        ))
    );
}

#[test]
fn non_zero_reserved_header_is_malformed() {
    let mut bytes = minimal_soyo();
    bytes[HEADER_RESERVED1] = 1;
    assert_eq!(parse_category(&bytes), SoyoErrorCategory::Malformed);
}

#[test]
fn duplicate_directory_type_is_malformed() {
    let mut bytes = minimal_soyo();
    put_u16(&mut bytes, DIRECTORY_SECOND_TYPE, 1);
    assert_eq!(parse_category(&bytes), SoyoErrorCategory::Malformed);
}

#[test]
fn out_of_order_directory_types_are_malformed() {
    let mut bytes = minimal_soyo();
    put_u16(&mut bytes, DIRECTORY_FIRST_TYPE, 2);
    put_u16(&mut bytes, DIRECTORY_SECOND_TYPE, 1);
    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()),
        Err(SoyoReadError::Format(SoyoError::Malformed(
            MalformedKind::Ordering
        )))
    );
}

#[test]
fn missing_standard_table_is_malformed() {
    let mut bytes = minimal_soyo();
    put_u16(&mut bytes, DIRECTORY_RUNTIME_TYPE, 7);
    put_u16(&mut bytes, DIRECTORY_RUNTIME_TYPE + 0x02, 0);
    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()),
        Err(SoyoReadError::Format(SoyoError::Malformed(
            MalformedKind::Header
        )))
    );
}

#[test]
fn metadata_range_inside_directory_is_malformed() {
    let mut bytes = minimal_soyo();
    put_u64(&mut bytes, DIRECTORY_STRING_FILE_OFFSET, 400);
    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()),
        Err(SoyoReadError::Format(SoyoError::Malformed(
            MalformedKind::Ordering
        )))
    );
}

#[test]
fn non_zero_canonical_padding_is_malformed() {
    let mut bytes = minimal_soyo();
    bytes[TABLE_PADDING] = 1;
    rehash(&mut bytes);
    assert_eq!(parse_category(&bytes), SoyoErrorCategory::Malformed);
}

#[test]
fn non_zero_segment_tail_padding_is_malformed() {
    let mut bytes = minimal_soyo();
    bytes[4100] = 1;
    rehash(&mut bytes);

    assert_eq!(
        parse_format_error(&bytes),
        SoyoError::Malformed(MalformedKind::Padding)
    );
}

#[test]
fn unknown_required_feature_is_unsupported_before_hash() {
    let mut bytes = minimal_soyo();
    put_u64(&mut bytes, HEADER_REQUIRED_FEATURES, 1 << 63);
    assert_eq!(parse_category(&bytes), SoyoErrorCategory::Unsupported);
}

#[test]
fn unknown_required_table_is_unsupported() {
    let mut bytes = extended_soyo();
    put_u16(&mut bytes, EXTENDED_OPTIONAL_TABLE_FLAGS, 1);
    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()),
        Err(SoyoReadError::Format(SoyoError::Unsupported(
            UnsupportedKind::RequiredTable(UNKNOWN_OPTIONAL_TABLE_TYPE)
        )))
    );
}

#[test]
fn reserved_table_identity_is_malformed() {
    let mut bytes = extended_soyo();
    put_u16(&mut bytes, EXTENDED_OPTIONAL_TABLE_TYPE, u16::MAX);
    rehash(&mut bytes);
    assert_eq!(parse_category(&bytes), SoyoErrorCategory::Malformed);
}

#[test]
fn unknown_optional_feature_is_preserved_but_ignored() {
    let mut bytes = minimal_soyo();
    put_u64(&mut bytes, HEADER_OPTIONAL_FEATURES, 1 << 63);
    rehash(&mut bytes);
    let reader = SliceSoyoReader::new(&bytes);
    let metadata = read_soyo(&reader, SoyoReadLimits::portable()).expect("optional feature 可忽略");
    let plan = validate_soyo(&metadata, SoyoTargetPolicy::for_kernel(TargetArch::Riscv64))
        .expect("未知 optional feature 不改变执行语义");
    assert_eq!(plan.enabled_features, 0);
}

#[test]
fn changed_payload_is_untrusted() {
    let mut bytes = minimal_soyo();
    bytes[4096] ^= 0xff;
    assert_eq!(parse_category(&bytes), SoyoErrorCategory::Untrusted);
}

#[test]
fn unequal_build_and_content_hash_is_untrusted() {
    let mut bytes = minimal_soyo();
    bytes[HEADER_BUILD_ID] ^= 1;
    assert_eq!(parse_category(&bytes), SoyoErrorCategory::Untrusted);
}

#[test]
fn writable_executable_code_is_malformed() {
    let mut bytes = minimal_soyo();
    put_u16(&mut bytes, SEGMENT_PERMISSIONS, 7);
    rehash(&mut bytes);
    assert_eq!(parse_category(&bytes), SoyoErrorCategory::Malformed);
}

#[test]
fn page_expanded_segment_overlap_is_malformed() {
    let mut bytes = extended_soyo();
    put_u64(&mut bytes, EXTENDED_FIRST_SEGMENT_MEMORY_SIZE, 4097);
    rehash(&mut bytes);
    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()),
        Err(SoyoReadError::Format(SoyoError::Malformed(
            MalformedKind::Overlap
        )))
    );
}

#[test]
fn segment_payload_must_follow_metadata_in_canonical_order() {
    let mut bytes = minimal_soyo();
    put_u64(&mut bytes, SEGMENT_FILE_OFFSET, 8192);
    rehash(&mut bytes);

    assert_eq!(
        parse_format_error(&bytes),
        SoyoError::Malformed(MalformedKind::Ordering)
    );
}

#[test]
fn ordinary_segment_alignment_must_be_one_page() {
    let mut bytes = minimal_soyo();
    put_u64(&mut bytes, SEGMENT_ALIGNMENT, 2048);
    rehash(&mut bytes);

    assert_eq!(
        parse_format_error(&bytes),
        SoyoError::Malformed(MalformedKind::Segment)
    );
}

#[test]
fn overflowing_segment_end_is_malformed() {
    let mut bytes = minimal_soyo();
    put_u64(&mut bytes, SEGMENT_MEMORY_SIZE, u64::MAX);
    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()),
        Err(SoyoReadError::Format(SoyoError::Malformed(
            MalformedKind::Range
        )))
    );
}

#[test]
fn relocation_targeting_code_is_malformed() {
    let mut bytes = extended_soyo();
    put_u32(&mut bytes, EXTENDED_RELOCATION_TARGET_SEGMENT, 0);
    rehash(&mut bytes);
    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()),
        Err(SoyoReadError::Format(SoyoError::Malformed(
            MalformedKind::Relocation
        )))
    );
}

#[test]
fn unknown_relocation_kind_is_malformed() {
    let mut bytes = extended_soyo();
    put_u16(&mut bytes, EXTENDED_RELOCATION_KIND, 3);
    rehash(&mut bytes);

    assert_eq!(
        parse_format_error(&bytes),
        SoyoError::Malformed(MalformedKind::Relocation)
    );
}

#[test]
fn image_base_relocation_requires_the_source_sentinel() {
    let mut bytes = extended_soyo();
    put_u32(&mut bytes, EXTENDED_RELOCATION_SOURCE_SEGMENT, 0);
    rehash(&mut bytes);

    assert_eq!(
        parse_format_error(&bytes),
        SoyoError::Malformed(MalformedKind::Relocation)
    );
}

#[test]
fn image_base_relocation_rejects_negative_or_out_of_image_addends() {
    for addend in [u64::MAX, 8193] {
        let mut bytes = extended_soyo();
        put_u64(&mut bytes, EXTENDED_RELOCATION_ADDEND, addend);
        rehash(&mut bytes);

        assert_eq!(
            parse_format_error(&bytes),
            SoyoError::Malformed(MalformedKind::Relocation)
        );
    }
}

#[test]
fn segment_base_relocation_requires_an_existing_source_segment() {
    let mut bytes = extended_soyo();
    put_u16(&mut bytes, EXTENDED_RELOCATION_KIND, 2);
    put_u32(&mut bytes, EXTENDED_RELOCATION_SOURCE_SEGMENT, 2);
    rehash(&mut bytes);

    assert_eq!(
        parse_format_error(&bytes),
        SoyoError::Malformed(MalformedKind::Relocation)
    );
}

#[test]
fn segment_base_relocation_rejects_addends_outside_its_source() {
    for addend in [u64::MAX, 4097] {
        let mut bytes = extended_soyo();
        put_u16(&mut bytes, EXTENDED_RELOCATION_KIND, 2);
        put_u32(&mut bytes, EXTENDED_RELOCATION_SOURCE_SEGMENT, 0);
        put_u64(&mut bytes, EXTENDED_RELOCATION_ADDEND, addend);
        rehash(&mut bytes);

        assert_eq!(
            parse_format_error(&bytes),
            SoyoError::Malformed(MalformedKind::Relocation)
        );
    }
}

#[test]
fn relocation_records_must_be_strictly_ordered_and_unique() {
    let image = extended_soyo();
    let segments = crate::decode::decode_segments(&image[536..664]).expect("规范段表必须合法");
    let canonical = &image[792..840];

    for (first_offset, second_offset) in [(0, 0), (8, 0)] {
        let mut records = alloc::vec![0u8; 96];
        records[..48].copy_from_slice(canonical);
        records[48..].copy_from_slice(canonical);
        put_u64(&mut records, 0x08, first_offset);
        put_u64(&mut records, 48 + 0x08, second_offset);

        assert_eq!(
            crate::decode::decode_relocations(&records, &segments),
            Err(SoyoError::Malformed(MalformedKind::Ordering))
        );
    }
}

#[test]
fn overflowing_relocation_target_is_malformed() {
    let mut bytes = extended_soyo();
    put_u64(&mut bytes, EXTENDED_RELOCATION_TARGET_OFFSET, u64::MAX - 7);
    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()),
        Err(SoyoReadError::Format(SoyoError::Malformed(
            MalformedKind::Relocation
        )))
    );
}

#[test]
fn non_zero_relocation_reserved_field_is_malformed() {
    let mut bytes = extended_soyo();
    put_u32(&mut bytes, EXTENDED_RELOCATION_RESERVED0, 1);
    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()),
        Err(SoyoReadError::Format(SoyoError::Malformed(
            MalformedKind::Reserved
        )))
    );
}

#[test]
fn image_over_wire_limit_is_resource_exhausted() {
    let mut bytes = minimal_soyo();
    put_u64(
        &mut bytes,
        HEADER_IMAGE_VIRTUAL_SIZE,
        1024 * 1024 * 1024 + 4096,
    );
    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()),
        Err(SoyoReadError::ResourceExhausted(
            crate::ResourceKind::ImageSize
        ))
    );
}

#[test]
fn entry_outside_file_backed_code_is_malformed() {
    let mut bytes = minimal_soyo();
    put_u64(&mut bytes, HEADER_ENTRY_OFFSET, 8);
    rehash(&mut bytes);
    assert_eq!(parse_category(&bytes), SoyoErrorCategory::Malformed);
}

#[test]
fn entry_must_follow_the_target_instruction_alignment() {
    for (target_arch, entry_offset) in [(1, 1), (2, 2)] {
        let mut bytes = minimal_soyo();
        put_u16(&mut bytes, HEADER_TARGET_ARCH, target_arch);
        put_u64(&mut bytes, HEADER_ENTRY_OFFSET, entry_offset);
        rehash(&mut bytes);

        assert_eq!(
            parse_format_error(&bytes),
            SoyoError::Malformed(MalformedKind::Alignment)
        );
    }
}

#[test]
fn signature_mismatch_is_incompatible() {
    let mut bytes = minimal_soyo();
    bytes[IMPORT_SIGNATURE_HASH] ^= 1;
    rehash(&mut bytes);
    let reader = SliceSoyoReader::new(&bytes);
    let metadata = read_soyo(&reader, SoyoReadLimits::portable()).expect("结构与 hash 应合法");
    let error = bind_native_abi(
        metadata.header.abi_family,
        metadata.header.abi_epoch,
        &metadata.imports,
        &metadata.capabilities,
        NativeAbiPolicy::for_kernel(),
    )
    .expect_err("signature mismatch 应拒绝绑定");
    assert_eq!(
        error,
        NativeAbiError::Incompatible(native_abi::IncompatibleKind::Signature(1))
    );
}

#[test]
fn unknown_optional_operation_is_ignored_by_native_binding() {
    let mut bytes = minimal_soyo();
    put_u32(&mut bytes, IMPORT_OPERATION_ID, 100);
    put_u32(&mut bytes, IMPORT_FLAGS, 2);
    rehash(&mut bytes);
    let reader = SliceSoyoReader::new(&bytes);
    let metadata =
        read_soyo(&reader, SoyoReadLimits::portable()).expect("optional import 结构合法");
    let plan = bind_native_abi(
        metadata.header.abi_family,
        metadata.header.abi_epoch,
        &metadata.imports,
        &metadata.capabilities,
        NativeAbiPolicy::for_kernel(),
    )
    .expect("未知 optional operation 应保留未绑定 slot");
    assert_eq!(plan.call_slots[0].operation, None);
}

#[test]
fn unknown_required_operation_is_incompatible() {
    let mut bytes = minimal_soyo();
    put_u32(&mut bytes, IMPORT_OPERATION_ID, 100);
    put_u32(&mut bytes, IMPORT_FLAGS, 1);
    rehash(&mut bytes);
    let reader = SliceSoyoReader::new(&bytes);
    let metadata =
        read_soyo(&reader, SoyoReadLimits::portable()).expect("required import 结构合法");
    let error = bind_native_abi(
        metadata.header.abi_family,
        metadata.header.abi_epoch,
        &metadata.imports,
        &metadata.capabilities,
        NativeAbiPolicy::for_kernel(),
    )
    .expect_err("未知 required operation 必须拒绝");
    assert_eq!(
        error.category(),
        native_abi::NativeAbiErrorCategory::Incompatible
    );
}

#[test]
fn invalid_diagnostic_string_offset_is_malformed() {
    let mut bytes = minimal_soyo();
    put_u32(&mut bytes, IMPORT_NAME, 2);
    rehash(&mut bytes);
    assert_eq!(parse_category(&bytes), SoyoErrorCategory::Malformed);
}

#[test]
fn diagnostic_string_over_wire_limit_is_resource_exhausted() {
    let mut strings = alloc::vec![b'a'; 258];
    strings[0] = 0;
    strings[257] = 0;
    let error = crate::format::validate_string_reference(&strings, 1)
        .expect_err("256 字节诊断字符串必须拒绝");
    assert_eq!(
        error,
        SoyoError::ResourceExhausted(crate::ResourceKind::StringLength)
    );
}

#[test]
fn unterminated_short_diagnostic_string_is_malformed() {
    assert_eq!(
        crate::format::validate_string_reference(&[0, b'a'], 1),
        Err(SoyoError::Malformed(MalformedKind::String))
    );
}

#[test]
fn maximum_length_diagnostic_string_is_valid() {
    let mut strings = alloc::vec![b'a'; 257];
    strings[0] = 0;
    strings[256] = 0;
    crate::format::validate_string_reference(&strings, 1)
        .expect("255 字节诊断字符串应位于 Wire 上限内");
}

#[test]
fn diagnostic_string_must_be_valid_utf8() {
    assert_eq!(
        crate::format::validate_string_reference(&[0, 0xff, 0], 1),
        Err(SoyoError::Malformed(MalformedKind::String))
    );
}

#[test]
fn capability_rights_escalation_is_malformed() {
    let mut bytes = minimal_soyo();
    put_u64(&mut bytes, CAP_RIGHTS, (1 << 5) | (1 << 1));
    rehash(&mut bytes);
    let reader = SliceSoyoReader::new(&bytes);
    let metadata = read_soyo(&reader, SoyoReadLimits::portable()).expect("capability 记录结构合法");
    let error = bind_native_abi(
        metadata.header.abi_family,
        metadata.header.abi_epoch,
        &metadata.imports,
        &metadata.capabilities,
        NativeAbiPolicy::for_kernel(),
    )
    .expect_err("requirement 不得请求 interface 最大权限以外的 bit");
    assert_eq!(
        error.category(),
        native_abi::NativeAbiErrorCategory::Malformed
    );
}

#[test]
fn unknown_optional_requirement_does_not_create_authority() {
    let mut bytes = minimal_soyo();
    put_u32(&mut bytes, CAP_REQUIREMENT_ID, 100);
    put_u16(&mut bytes, CAP_FLAGS, 2);
    rehash(&mut bytes);
    let reader = SliceSoyoReader::new(&bytes);
    let metadata =
        read_soyo(&reader, SoyoReadLimits::portable()).expect("optional requirement 结构合法");
    bind_native_abi(
        metadata.header.abi_family,
        metadata.header.abi_epoch,
        &metadata.imports,
        &metadata.capabilities,
        NativeAbiPolicy::for_kernel(),
    )
    .expect("未知 optional requirement 应忽略且不授权");
}

#[test]
fn unknown_optional_requirement_rejects_invalid_object_interface_identity() {
    for object_interface in [0, u16::MAX] {
        let mut bytes = minimal_soyo();
        put_u32(&mut bytes, CAP_REQUIREMENT_ID, 100);
        put_u16(&mut bytes, CAP_OBJECT_INTERFACE, object_interface);
        put_u16(&mut bytes, CAP_FLAGS, 2);
        rehash(&mut bytes);

        assert_eq!(parse_category(&bytes), SoyoErrorCategory::Malformed);
    }
}

#[test]
fn unknown_required_requirement_reports_its_own_identity() {
    let mut bytes = minimal_soyo();
    put_u32(&mut bytes, CAP_REQUIREMENT_ID, 100);
    put_u16(&mut bytes, CAP_FLAGS, 1);
    rehash(&mut bytes);
    let reader = SliceSoyoReader::new(&bytes);
    let metadata =
        read_soyo(&reader, SoyoReadLimits::portable()).expect("required requirement 结构合法");
    assert_eq!(
        bind_native_abi(
            metadata.header.abi_family,
            metadata.header.abi_epoch,
            &metadata.imports,
            &metadata.capabilities,
            NativeAbiPolicy::for_kernel(),
        ),
        Err(NativeAbiError::Unsupported(
            native_abi::UnsupportedKind::RequiredRequirement(100)
        ))
    );
}

#[test]
fn static_tls_feature_without_template_is_malformed() {
    let mut bytes = minimal_soyo();
    put_u64(&mut bytes, HEADER_REQUIRED_FEATURES, 1);
    rehash(&mut bytes);
    assert_eq!(parse_category(&bytes), SoyoErrorCategory::Malformed);
}

#[test]
fn optional_static_tls_without_template_is_malformed() {
    let mut bytes = minimal_soyo();
    put_u64(&mut bytes, HEADER_OPTIONAL_FEATURES, 1);
    rehash(&mut bytes);
    assert_eq!(parse_category(&bytes), SoyoErrorCategory::Malformed);
}

#[test]
fn optional_init_fini_without_arrays_is_malformed() {
    let mut bytes = minimal_soyo();
    put_u64(&mut bytes, HEADER_OPTIONAL_FEATURES, 1 << 1);
    rehash(&mut bytes);
    assert_eq!(parse_category(&bytes), SoyoErrorCategory::Malformed);
}

#[test]
fn tls_template_over_wire_limit_is_resource_exhausted() {
    let mut bytes = minimal_soyo();
    put_u64(&mut bytes, HEADER_REQUIRED_FEATURES, 1);
    put_u16(&mut bytes, SEGMENT_KIND, 5);
    put_u16(&mut bytes, SEGMENT_PERMISSIONS, 3);
    put_u64(&mut bytes, SEGMENT_MEMORY_SIZE, 16 * 1024 * 1024 + 1);
    rehash(&mut bytes);

    assert_eq!(
        read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()),
        Err(SoyoReadError::ResourceExhausted(
            crate::ResourceKind::TlsSize
        ))
    );
}

#[test]
fn valid_static_tls_shape_is_accepted() {
    use crate::metadata::{ImageSegment, SoyoHeader};
    use crate::registry::{FeatureFlags, SegmentKind};
    use native_abi::TargetArch;

    let header = SoyoHeader {
        artifact_kind: crate::registry::ArtifactKind::Executable,
        target_arch: TargetArch::Riscv64,
        abi_family: native_abi::ABI_FAMILY_MYGO_NATIVE,
        abi_epoch: 1,
        required_features: FeatureFlags::STATIC_TLS.bits(),
        optional_features: 0,
        entry_offset: 0,
        file_size: 12296,
        image_virtual_size: 8192,
        build_id: [0; 32],
        content_hash: [0; 32],
    };
    let segments = [
        ImageSegment {
            kind: SegmentKind::Code,
            permissions: 5,
            virtual_offset: 0,
            file_offset: 4096,
            file_size: 4,
            memory_size: 4096,
            alignment: 4096,
        },
        ImageSegment {
            kind: SegmentKind::Data,
            permissions: 3,
            virtual_offset: 4096,
            file_offset: 8192,
            file_size: 8,
            memory_size: 4096,
            alignment: 4096,
        },
        ImageSegment {
            kind: SegmentKind::TlsTemplate,
            permissions: 3,
            virtual_offset: 0,
            file_offset: 12288,
            file_size: 8,
            memory_size: 16,
            alignment: 16,
        },
    ];

    crate::format::validate_segments(&segments, &header).expect("合法 STATIC_TLS 段布局应通过");
}

#[test]
fn segment_base_relocation_cannot_use_tls_as_its_source() {
    use crate::metadata::ImageSegment;
    use crate::registry::{RelocationKind, SegmentKind};

    let segments = [
        ImageSegment {
            kind: SegmentKind::Code,
            permissions: 5,
            virtual_offset: 0,
            file_offset: 4096,
            file_size: 4,
            memory_size: 4096,
            alignment: 4096,
        },
        ImageSegment {
            kind: SegmentKind::Data,
            permissions: 3,
            virtual_offset: 4096,
            file_offset: 8192,
            file_size: 8,
            memory_size: 4096,
            alignment: 4096,
        },
        ImageSegment {
            kind: SegmentKind::TlsTemplate,
            permissions: 3,
            virtual_offset: 0,
            file_offset: 12288,
            file_size: 8,
            memory_size: 16,
            alignment: 16,
        },
    ];

    assert_eq!(
        crate::format::validate_relocation(RelocationKind::SegmentBase64, 1, 0, 2, 0, &segments),
        Err(SoyoError::Malformed(MalformedKind::Relocation))
    );
}

#[test]
fn abi_epoch_mismatch_is_rejected_by_native_binding() {
    let mut bytes = minimal_soyo();
    put_u16(&mut bytes, HEADER_ABI_EPOCH, 2);
    rehash(&mut bytes);
    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable())
        .expect("ABI epoch 属于 Native ABI 兼容性检查");

    assert_eq!(
        bind_native_abi(
            metadata.header.abi_family,
            metadata.header.abi_epoch,
            &metadata.imports,
            &metadata.capabilities,
            NativeAbiPolicy::for_kernel(),
        ),
        Err(NativeAbiError::Incompatible(
            native_abi::IncompatibleKind::AbiEpoch(2)
        ))
    );
}

#[test]
fn load_plan_cannot_bypass_native_abi_binding() {
    let mut bytes = minimal_soyo();
    put_u16(&mut bytes, HEADER_ABI_EPOCH, 2);
    rehash(&mut bytes);
    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable())
        .expect("Wire 结构应先通过格式解析");

    assert_eq!(
        validate_soyo(&metadata, SoyoTargetPolicy::for_kernel(TargetArch::Riscv64)),
        Err(SoyoError::NativeAbi(NativeAbiError::Incompatible(
            native_abi::IncompatibleKind::AbiEpoch(2)
        )))
    );
}

#[test]
fn policy_rejects_unsupported_required_operation() {
    let mut bytes = minimal_soyo();
    let operation = native_abi::operation(OperationId::StreamRead)
        .expect("stream.read 必须在 Native ABI registry 中");
    put_u32(&mut bytes, IMPORT_OPERATION_ID, operation.id as u32);
    bytes[IMPORT_SIGNATURE_HASH..IMPORT_SIGNATURE_HASH + 32]
        .copy_from_slice(&operation.signature_hash);
    rehash(&mut bytes);

    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable())
        .expect("stream.read import 的 Wire 结构应合法");
    assert_eq!(
        bind_native_abi(
            metadata.header.abi_family,
            metadata.header.abi_epoch,
            &metadata.imports,
            &metadata.capabilities,
            NativeAbiPolicy {
                abi_family: native_abi::ABI_FAMILY_MYGO_NATIVE,
                abi_epoch: native_abi::ABI_EPOCH,
                supported_operations: Some(&[OperationId::ProcessExit]),
            },
        ),
        Err(NativeAbiError::Incompatible(
            native_abi::IncompatibleKind::Operation(OperationId::StreamRead as u32,)
        ))
    );
    let host_plan = bind_native_abi(
        metadata.header.abi_family,
        metadata.header.abi_epoch,
        &metadata.imports,
        &metadata.capabilities,
        NativeAbiPolicy::for_host(),
    )
    .expect("host checker 应能校验完整 registry 中的 operation");
    assert_eq!(
        host_plan.call_slots[0].operation,
        Some(OperationId::StreamRead)
    );
}

#[test]
fn policy_leaves_unsupported_optional_operation_unbound() {
    let mut bytes = minimal_soyo();
    let operation = native_abi::operation(OperationId::StreamRead)
        .expect("stream.read 必须在 Native ABI registry 中");
    put_u32(&mut bytes, IMPORT_OPERATION_ID, operation.id as u32);
    put_u32(&mut bytes, IMPORT_FLAGS, 2);
    bytes[IMPORT_SIGNATURE_HASH..IMPORT_SIGNATURE_HASH + 32]
        .copy_from_slice(&operation.signature_hash);
    rehash(&mut bytes);

    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable())
        .expect("optional stream.read import 的 Wire 结构应合法");
    let plan = bind_native_abi(
        metadata.header.abi_family,
        metadata.header.abi_epoch,
        &metadata.imports,
        &metadata.capabilities,
        NativeAbiPolicy {
            abi_family: native_abi::ABI_FAMILY_MYGO_NATIVE,
            abi_epoch: native_abi::ABI_EPOCH,
            supported_operations: Some(&[OperationId::ProcessExit]),
        },
    )
    .expect("policy 不支持的 optional operation 应生成未绑定 slot");
    assert_eq!(plan.call_slots[0].operation, None);
}

#[test]
fn known_optional_operation_still_checks_signature() {
    let mut bytes = minimal_soyo();
    put_u32(
        &mut bytes,
        IMPORT_OPERATION_ID,
        OperationId::StreamRead as u32,
    );
    put_u32(&mut bytes, IMPORT_FLAGS, 2);
    rehash(&mut bytes);

    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable())
        .expect("optional stream.read import 的 Wire 结构应合法");
    assert_eq!(
        bind_native_abi(
            metadata.header.abi_family,
            metadata.header.abi_epoch,
            &metadata.imports,
            &metadata.capabilities,
            NativeAbiPolicy::for_kernel(),
        ),
        Err(NativeAbiError::Incompatible(
            native_abi::IncompatibleKind::Signature(OperationId::StreamRead as u32)
        ))
    );
}

#[test]
fn known_but_wrong_target_is_incompatible() {
    let mut bytes = minimal_soyo();
    put_u16(&mut bytes, HEADER_TARGET_ARCH, 2);
    rehash(&mut bytes);
    let reader = SliceSoyoReader::new(&bytes);
    let metadata = read_soyo(&reader, SoyoReadLimits::portable()).expect("LA64 是已知目标");
    let error = validate_soyo(&metadata, SoyoTargetPolicy::for_kernel(TargetArch::Riscv64))
        .expect_err("当前 RV64 内核必须拒绝 LA64 镜像");
    assert_eq!(error.category(), SoyoErrorCategory::Incompatible);
}
