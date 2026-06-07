//! ELF 解析错误码。
//!
//! 本 crate 的错误集中在 [`ElfError`]；`impl From<ElfError> for Errno` 让上层
//! syscall / loader 直接把它翻成 POSIX `ENOEXEC`。错误粒度比 Linux 实际接口
//! 细（`TruncatedPhdr` vs `MisalignedPhoff`），方便日志定位；映射到 Errno
//! 时全部归为 `ENOEXEC`，与 Linux 的 `execve(2)` 行为一致。

use errno::Errno;

/// ELF / 未来二进制格式的解析错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// 字节数不够摆下 header / phdr / 某个段数据。
    TooShort,
    /// 前 4 字节不是已知的 magic。
    BadMagic,
    /// 不是 ELFCLASS64。本 crate 目前只处理 64-bit。
    UnsupportedClass,
    /// 不是 ELFDATA2LSB。本 crate 目前只处理 little-endian。
    UnsupportedData,
    /// e_type 非 ET_EXEC / ET_DYN。
    UnsupportedType(u16),
    /// e_machine 未在本 crate 的允许列表里；解析仍然成功，仅做诊断提示——
    /// 本枚举值只在 loader 侧拒绝不兼容架构时触发。
    UnsupportedMachine(u16),
    /// 声明的 Ehdr 字段超过实际字节数。
    TruncatedHeader,
    /// 声明的 phdr 表超过实际字节数。
    TruncatedPhdr,
    /// e_phoff + e_phentsize * e_phnum 算出越界。
    PhdrOffsetOverflow,
    /// p_offset + p_filesz 算出越界。
    SegmentOffsetOverflow,
    /// program header 字段组合不合法（例如 PT_LOAD 对齐/重叠/大小关系错误）。
    InvalidSegment,
    /// PT_PHDR 未覆盖实际 program header table。
    InvalidPhdr,
    /// e_entry 不落在可执行的非空 PT_LOAD 中。
    InvalidEntry,
    /// e_phoff 不符合最小对齐要求。
    MisalignedPhoff,
    /// PT_INTERP 区段不以 NUL 结尾或非 UTF-8。
    InvalidInterp,
}

impl From<ElfError> for Errno {
    fn from(_: ElfError) -> Errno {
        // 所有 ELF 解析失败在 POSIX 语义里都映射为 ENOEXEC。
        Errno::ENOEXEC
    }
}
