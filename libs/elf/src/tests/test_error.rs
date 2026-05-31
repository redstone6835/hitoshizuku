//! ELF 解析错误到 POSIX errno 的映射测试。
//!
//! 验证所有 ElfError 变体统一映射为 ENOEXEC，与 Linux execve(2) 行为一致。

extern crate std;

use crate::ElfError;
use errno::Errno;
use ktest::ktest;

/// 所有 ELF 解析失败变体均应映射为 ENOEXEC，与 Linux execve(2) 语义对齐。
#[ktest]
fn all_elf_errors_map_to_enoexec() {
    let errors: &[ElfError] = &[
        ElfError::TooShort,
        ElfError::BadMagic,
        ElfError::UnsupportedClass,
        ElfError::UnsupportedData,
        ElfError::UnsupportedType(0),
        ElfError::UnsupportedMachine(0),
        ElfError::TruncatedHeader,
        ElfError::TruncatedPhdr,
        ElfError::PhdrOffsetOverflow,
        ElfError::SegmentOffsetOverflow,
        ElfError::MisalignedPhoff,
        ElfError::InvalidInterp,
    ];
    for &err in errors {
        let e: Errno = err.into();
        assert_eq!(
            e, Errno::ENOEXEC,
            "ElfError -> Errno must be ENOEXEC for {:?}", err
        );
    }
}
