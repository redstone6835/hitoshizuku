//! FAT 8.3 短文件名生成测试。

extern crate std;

use crate::name;
use ktest::ktest;

/// 简单 ASCII 大写名直接编码为 8.3 SFN，不足部分空格填充。
#[ktest]
fn plain_sfn_simple() {
    let sfn = name::try_plain_sfn("HELLO").expect("plain SFN");
    assert_eq!(&sfn[..5], b"HELLO");
    assert_eq!(sfn[5], b' ');
}

/// 超过 8 字符的 base name 无法用 plain SFN 编码，返回 None。
#[ktest]
fn plain_sfn_too_long() {
    assert!(name::try_plain_sfn("TOOLONGBASENAME").is_none());
}

/// build_tilde_sfn 生成 "BASE~N" 形式的混叠短名，截断基名容纳 ~N 后缀。
#[ktest]
fn tilde_sfn_generation() {
    let sfn = name::build_tilde_sfn("LONGFILE", 1);
    assert_eq!(&sfn[..8], b"LONGFI~1");
}

/// LFN 解码热路径使用固定数组，遇到终止/填充值时不应写入有效字符。
#[ktest]
fn lfn_decode_fixed_skips_terminators() {
    let chars = [
        b'H' as u16,
        b'E' as u16,
        b'L' as u16,
        b'L' as u16,
        b'O' as u16,
        0,
        0xffff,
        0xffff,
        0xffff,
        0xffff,
        0xffff,
        0xffff,
        0xffff,
    ];
    let mut entry = [0u8; crate::dir::DIR_ENTRY_SIZE];
    crate::lfn::encode_lfn_entry(0x41, &chars, 0xaa, &mut entry);

    let mut out = [0u16; 13];
    let (len, terminated) = crate::lfn::decode_lfn_entry_fixed(&entry, &mut out);

    assert!(terminated);
    assert_eq!(len, 5);
    assert_eq!(
        &out[..5],
        &[
            b'H' as u16,
            b'E' as u16,
            b'L' as u16,
            b'L' as u16,
            b'O' as u16
        ]
    );
    assert!(out[5..].iter().all(|&u| u == 0));
}
