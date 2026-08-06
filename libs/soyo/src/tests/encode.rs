use crate::{
    SliceSoyoReader, SoyoReadLimits, SoyoTargetPolicy, read_soyo, test_support::SoyoTestEncoder,
    validate_soyo,
};
use native_abi::TargetArch;

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
