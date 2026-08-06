//! Registry 测试以规范字面值约束机器身份和 canonical signature。

use crate::SoyoReadLimits;
use crate::registry::{
    CapabilityFlags, DirectoryFlags, FeatureFlags, ImportFlags, RuntimeFlags, SegmentPermissions,
};

#[test]
fn flag_registries_preserve_wire_bits() {
    assert_eq!(FeatureFlags::STATIC_TLS.bits(), 1 << 0);
    assert_eq!(FeatureFlags::INIT_FINI_ARRAY.bits(), 1 << 1);
    assert_eq!(FeatureFlags::KNOWN.bits(), 0b11);
    assert_eq!(DirectoryFlags::REQUIRED.bits(), 1);
    assert_eq!(ImportFlags::REQUIRED.bits(), 1);
    assert_eq!(ImportFlags::OPTIONAL.bits(), 2);
    assert_eq!(CapabilityFlags::REQUIRED.bits(), 1);
    assert_eq!(CapabilityFlags::OPTIONAL.bits(), 2);
    assert_eq!(SegmentPermissions::READ.bits(), 1);
    assert_eq!(SegmentPermissions::WRITE.bits(), 2);
    assert_eq!(SegmentPermissions::EXECUTE.bits(), 4);
    assert_eq!(RuntimeFlags::RUN_INIT_ARRAY.bits(), 1);
    assert_eq!(RuntimeFlags::RUN_FINI_ARRAY.bits(), 2);
}

#[test]
fn wire_registry_generation_is_fresh() {
    assert_eq!(crate::wire::WIRE_ABI_GENERATION, 1);
}

#[test]
fn portable_resource_limits_match_the_wire_abi() {
    assert_eq!(crate::registry::MAX_FILE_SIZE, 256 * 1024 * 1024);
    assert_eq!(crate::registry::MAX_IMAGE_SIZE, 1024 * 1024 * 1024);
    assert_eq!(crate::registry::MAX_DIRECTORY_ENTRIES, 64);
    assert_eq!(crate::registry::MAX_STRING_BYTES, 1024 * 1024);
    assert_eq!(crate::registry::MAX_SEGMENTS, 32);
    assert_eq!(crate::registry::MAX_IMPORTS, 256);
    assert_eq!(crate::registry::MAX_CAPABILITIES, 64);
    assert_eq!(crate::registry::MAX_RELOCATIONS, 65_536);
    assert_eq!(crate::registry::MAX_TLS_SIZE, 16 * 1024 * 1024);
}

#[test]
fn portable_table_budget_covers_all_standard_tables_at_wire_limits() {
    let maximum_standard_tables = 1_048_576 + 32 * 64 + 256 * 64 + 64 * 64 + 65_536 * 48 + 96;
    assert!(SoyoReadLimits::portable().max_table_bytes >= maximum_standard_tables);
}
