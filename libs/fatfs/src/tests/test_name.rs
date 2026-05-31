//! FAT 8.3 短文件名生成测试。

extern crate std;

use ktest::ktest;
use crate::name;

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
