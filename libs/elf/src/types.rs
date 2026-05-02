//! 格式无关的公共类型。
//!
//! 这里的每个类型都不含 ELF 概念；Linux ELF 与未来自制 "mygo 格式" 都只
//! 在 [`Image`](crate::image::Image) 实现里把自己的内部表示翻成这些类型。
//!
//! `SegmentPerms` 刻意与 Linux `PROT_*` 对齐（README、RWE 同序），便于
//! loader 把它直接 `to_vm_flags` 喂给 `libs/mm::VmFlags`。

use mm::VmFlags;

/// 段的权限位图。与 Linux `PROT_READ / PROT_WRITE / PROT_EXEC` 同序，方便
/// 后续 syscall 层直接透传。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentPerms(pub u8);

impl SegmentPerms {
    pub const EMPTY: Self = Self(0);

    pub const READ: u8 = 1 << 0;
    pub const WRITE: u8 = 1 << 1;
    pub const EXEC: u8 = 1 << 2;

    #[inline]
    pub const fn has(self, flag: u8) -> bool {
        flag != 0 && (self.0 & flag) == flag
    }

    #[inline]
    pub const fn with(self, flag: u8) -> Self {
        Self(self.0 | flag)
    }

    /// 桥到 [`libs/mm`] 的 [`VmFlags`]。loader 装段到 VmSpace 时调用；
    /// 本函数默认附加 `USER` 标志——ELF 段总是用户态的。
    pub fn to_vm_flags(self) -> VmFlags {
        let mut f = VmFlags::EMPTY.with(VmFlags::USER);
        if self.has(Self::READ) {
            f = f.with(VmFlags::READ);
        }
        if self.has(Self::WRITE) {
            f = f.with(VmFlags::WRITE);
        }
        if self.has(Self::EXEC) {
            f = f.with(VmFlags::EXEC);
        }
        f
    }
}

/// 镜像地址宽度。ELF class 映射到这里；未来 mygo 格式可以定义自己的值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressWidth {
    Bits32,
    Bits64,
}

/// 机器类型的格式无关枚举。`Unknown(u16)` 保留原始 e_machine 值便于诊断，
/// loader 根据 crate 目标可自行拒绝不兼容架构。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    LoongArch64,
    Riscv64,
    X86_64,
    Aarch64,
    /// 未在已知清单里的原始 `e_machine` 值；解析仍然成功。
    Unknown(u16),
}

/// 描述"需要被装入地址空间的一段内容"。
///
/// 字段语义：
/// - `vaddr`：段在用户地址空间里的起始虚地址（ET_DYN 时是相对偏移）。
/// - `memsz`：段在内存里占的字节数。可能 `> file_size`（BSS 尾部零填充）。
/// - `file_offset` / `file_size`：段数据在 image 字节流里的位置与长度；
///   `data` 是对应的切片。
/// - `perms`：RWX 组合。
#[derive(Debug, Clone)]
pub struct Segment<'a> {
    pub vaddr: usize,
    pub memsz: usize,
    pub file_offset: u64,
    pub file_size: usize,
    pub perms: SegmentPerms,
    /// 段在原 image 字节流里的只读切片。长度 == `file_size`；可能为空
    /// （纯 BSS 段）。
    pub data: &'a [u8],
}
