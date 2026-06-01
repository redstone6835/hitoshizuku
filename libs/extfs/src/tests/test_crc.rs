//! ext4 CRC32C 校验测试。

extern crate std;

use ktest::ktest;
use crate::crc;

/// 空输入返回初值 0xFFFFFFFF（无字节处理，无 final XOR）。
#[ktest]
fn empty_buffer() {
    assert_eq!(crc::crc32c(&[]), 0xFFFFFFFF);
}

/// crc32c(b"123456789") == 0x1CF96D7C（标准 iSCSI 值 0xE3069283 ^ 0xFFFFFFFF）。
#[ktest]
fn known_vector() {
    assert_eq!(crc::crc32c(b"123456789"), 0x1CF96D7C);
}
