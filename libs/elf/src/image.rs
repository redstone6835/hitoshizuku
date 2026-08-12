//! 格式无关的 [`Image`] 契约。
//!
//! 每种二进制格式都 impl 这套方法；loader 只看 trait，不接触格式细节。
//! 保持契约足够窄：loader 真正需要的信息就是"入口、架构、段、解释器"。
//!
//! ## 分派
//!
//! - **静态分派**：已知格式时直接调用 `LinuxElfImage::segments_typed` 之类
//!   inherent 方法，零动态分派开销。
//! - **动态分派**：不知道格式时走 [`crate::parse`]，返回 `Box<dyn Image>`。
//!   动态分派下 `segments` 返回 `Box<dyn Iterator>`——因为 trait-object 不能
//!   用 `impl Iterator` 关联类型。

use alloc::boxed::Box;
use core::ops::Range;

use crate::types::{AddressWidth, Arch, Segment};

/// 一个可装载的二进制镜像视图。
pub trait Image<'a> {
    /// 入口虚地址。`ET_DYN` 时是相对 base 的偏移，loader 决定 base。
    fn entry(&self) -> usize;

    /// 机器类型。
    fn arch(&self) -> Arch;

    /// 地址宽度。
    fn class(&self) -> AddressWidth;

    /// 是否 PIE（Linux ELF: `ET_DYN` + 可执行属性）。
    fn is_pie(&self) -> bool;

    /// 动态链接解释器路径。静态链接 / mygo 原生格式返 `None`。
    fn interpreter(&self) -> Option<&str>;

    /// 遍历全部"需要装入地址空间"的段。
    ///
    /// 返回 `Box<dyn Iterator>` 是动态分派路径下的无奈之选；已知类型的
    /// 调用方请用 `LinuxElfImage::segments_typed` 之类具体方法以消除
    /// 分配与动态调用开销。
    fn segments<'b>(&'b self) -> Box<dyn Iterator<Item = Segment<'a>> + 'b>
    where
        'a: 'b;

    /// 格式名，仅供日志 / 诊断。
    fn format_name(&self) -> &'static str;

    /// Program header 表在镜像虚拟地址空间中的地址。非 ELF 格式可返回 None。
    fn phdr_vaddr(&self) -> Option<usize> {
        None
    }

    /// Program header 单项大小。
    fn phdr_entry_size(&self) -> usize {
        0
    }

    /// Program header 数量。
    fn phdr_count(&self) -> usize {
        0
    }

    /// PT_LOAD 虚拟地址覆盖范围。
    fn load_vaddr_range(&self) -> Option<Range<usize>> {
        None
    }
}
