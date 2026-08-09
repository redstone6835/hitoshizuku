use crate::{
    SliceSoyoReader, SoyoReadLimits, SoyoTargetPolicy, read_soyo, test_support::SoyoTestEncoder,
    validate_soyo,
};
use native_abi::TargetArch;

use crate::test_support::{
    LOADER_FIXTURE_DATA, LOADER_FIXTURE_RODATA, LOADER_FIXTURE_TLS, SoyoLoaderTestEncoder,
};

use super::fixtures::minimal_soyo;

#[test]
fn test_encoder_matches_the_hand_derived_canonical_image() {
    let encoded = SoyoTestEncoder::minimal(TargetArch::Riscv64, &[0x73, 0x00, 0x00, 0x00])
        .encode()
        .expect("测试 encoder 应生成最小镜像");

    assert_eq!(encoded, minimal_soyo());
    let metadata = read_soyo(&SliceSoyoReader::new(&encoded), SoyoReadLimits::portable())
        .expect("encoder 输出应能被共享 parser 接受");
    validate_soyo(&metadata, SoyoTargetPolicy::for_kernel(TargetArch::Riscv64))
        .expect("encoder 输出应能通过格式策略校验");
}

#[test]
fn test_encoder_is_byte_deterministic() {
    let first = SoyoTestEncoder::minimal(TargetArch::Riscv64, &[0x73, 0, 0, 0])
        .encode()
        .expect("第一次编码");
    let second = SoyoTestEncoder::minimal(TargetArch::Riscv64, &[0x73, 0, 0, 0])
        .encode()
        .expect("第二次编码");
    assert_eq!(first, second);
}

#[test]
fn loader_encoder_covers_the_complete_mapping_shape() {
    let encoded = SoyoLoaderTestEncoder::new(TargetArch::Riscv64, &[0x73, 0, 0, 0])
        .encode()
        .expect("完整测试映像应可编码");
    let metadata = read_soyo(&SliceSoyoReader::new(&encoded), SoyoReadLimits::portable())
        .expect("完整测试映像应通过共享 parser");
    validate_soyo(&metadata, SoyoTargetPolicy::for_kernel(TargetArch::Riscv64))
        .expect("完整测试映像应通过内核策略");
    assert_eq!(metadata.segments.len(), 5);
    assert_eq!(metadata.relocations.len(), 1);
    assert_eq!(
        &encoded[8192..8192 + LOADER_FIXTURE_RODATA.len()],
        &LOADER_FIXTURE_RODATA
    );
    assert_eq!(
        &encoded[12288..12288 + LOADER_FIXTURE_DATA.len()],
        &LOADER_FIXTURE_DATA
    );
    assert_eq!(
        &encoded[16384..16384 + LOADER_FIXTURE_TLS.len()],
        &LOADER_FIXTURE_TLS
    );
}

#[test]
fn loader_encoder_can_request_nonempty_init_array() {
    let encoded = SoyoLoaderTestEncoder::new(TargetArch::Riscv64, &[0x73, 0, 0, 0])
        .with_init_array()
        .encode()
        .expect("init array 测试映像应可编码");
    let metadata = read_soyo(&SliceSoyoReader::new(&encoded), SoyoReadLimits::portable())
        .expect("host parser 应接受 init array");
    validate_soyo(&metadata, SoyoTargetPolicy::for_host()).expect("host 策略应接受 init array");
    assert_eq!(
        metadata
            .runtime
            .as_ref()
            .expect("executable 必须携带 RuntimeInfo")
            .init_array_count,
        1
    );
}
