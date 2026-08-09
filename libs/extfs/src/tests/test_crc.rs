//! ext4 CRC32C 校验测试。

extern crate std;

use crate::crc;
use ktest::ktest;

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

/// 宽分块实现必须保留 seed 语义，并正确处理任意长度的尾部字节。
#[ktest]
fn accelerated_update_preserves_seed_and_tail_lengths() {
    let mut data = [0u8; 31];
    for (index, byte) in data.iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(37).wrapping_add(11);
    }
    let seed = 0x1357_9bdf;

    for len in 0..=data.len() {
        let expected = reference_update(seed, &data[..len]);
        assert_eq!(
            crc::update_slicing_by_8(seed, &data[..len]),
            expected,
            "len={len}"
        );
    }

    let split = crc::update_slicing_by_8(seed, &data[..13]);
    assert_eq!(
        crc::update_slicing_by_8(split, &data[13..]),
        reference_update(seed, &data),
    );
}

fn reference_update(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82f6_3b78
            } else {
                crc >> 1
            };
        }
    }
    crc
}
