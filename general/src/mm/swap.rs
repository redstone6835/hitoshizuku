//! swap 设备表与 `swapon(2)` / `swapoff(2)` 记账。
//!
//! Linux 的 swap 子系统包含三个层次：
//!
//! 1. **设备登记**：`swapon` 校验文件/分区后加入系统 swap 表，`/proc/swaps`、
//!    `sysinfo(2)`、`/proc/meminfo` 的 `SwapTotal`/`SwapFree` 全部来自这张表；
//! 2. **槽位分配**：把 swap 空间切成页槽，记录哪些页已换出；
//! 3. **换出/换入**：内存压力下把 anon 页写入槽位并在 PTE 编码 swap entry，
//!    缺页时读回。
//!
//! 本模块实现第 1 层（登记、优先级、观测输出）与第 2 层的槽位记账骨架
//! （`used_pages`，供 `swapoff` 的 `EBUSY` 判定）。第 3 层需要架构相关的
//! PTE swap 编码与全局压力回收器，属于后续工作；当前 anon 页不换出，
//! `used_pages` 保持 0，`swapoff` 因此总是允许（与"没有实际换出"一致）。
//!
//! 依赖方向：本模块在 `general` 内，可以直接持有 `vfs::File` 保持 swap
//! 文件打开（Linux 同样在 swapoff 前持有引用，防止文件被替换）。

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use errno::Errno;
use sched::sync::Spinlock;

use crate::vfs::file::File;
use crate::vfs::stat::FileType;

/// swap 设备类型（对应 `/proc/swaps` Type 列）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapKind {
    /// 普通文件（`swapon` 指定路径）。
    File,
    /// 块设备分区。
    Partition,
}

impl SwapKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Partition => "partition",
        }
    }
}

/// swap 设备表条目。
pub struct SwapDevice {
    /// 设备路径（`swapoff` 按此匹配）。
    pub name: String,
    pub kind: SwapKind,
    /// swap 空间总页数（文件大小/分区大小按页取整）。
    pub size_pages: u64,
    /// 已占用的槽位数。当前无换出机制，恒为 0。
    used_pages: u64,
    /// swapon 时指定的优先级（`SWAP_FLAG_PREFER` 低 16 位）。
    pub priority: i32,
    /// 保持 swap 文件打开，阻止路径被替换。
    _file: Arc<File>,
}

impl SwapDevice {
    fn snapshot(&self) -> SwapEntrySnapshot {
        SwapEntrySnapshot {
            name: self.name.clone(),
            kind: self.kind,
            size_pages: self.size_pages,
            used_pages: self.used_pages,
            priority: self.priority,
        }
    }
}

/// `/proc/swaps` 渲染用的只读快照。
#[derive(Debug, Clone)]
pub struct SwapEntrySnapshot {
    pub name: String,
    pub kind: SwapKind,
    pub size_pages: u64,
    pub used_pages: u64,
    pub priority: i32,
}

static SWAP_DEVICES: Spinlock<Vec<SwapDevice>> = Spinlock::new(Vec::new());

/// 登记一个 swap 设备。
///
/// 校验：文件必须是普通文件或块设备（否则 `EINVAL`），大小至少一页
/// （否则 `EINVAL`），且未登记过相同路径（否则 `EBUSY`）。
pub fn swapon(file: Arc<File>, path: String, priority: i32) -> Result<(), Errno> {
    let kind = match file.inode().kind() {
        FileType::Regular => SwapKind::File,
        FileType::BlockDevice => SwapKind::Partition,
        _ => return Err(Errno::EINVAL),
    };
    let size_pages =
        (file.inode().size() + crate::mm::page_size() as u64 - 1) / crate::mm::page_size() as u64;
    if size_pages == 0 {
        return Err(Errno::EINVAL);
    }
    let mut devices = SWAP_DEVICES.lock();
    if devices.iter().any(|d| d.name == path) {
        return Err(Errno::EBUSY);
    }
    devices.push(SwapDevice {
        name: path,
        kind,
        size_pages,
        used_pages: 0,
        priority,
        _file: file,
    });
    Ok(())
}

/// 解除登记。已使用的 swap 设备返回 `EBUSY`；路径未登记返回 `EINVAL`。
pub fn swapoff(path: &str) -> Result<(), Errno> {
    let mut devices = SWAP_DEVICES.lock();
    let Some(index) = devices.iter().position(|d| d.name == path) else {
        return Err(Errno::EINVAL);
    };
    if devices[index].used_pages != 0 {
        return Err(Errno::EBUSY);
    }
    devices.remove(index);
    Ok(())
}

/// swap 总量/空闲（页）。供 `sysinfo(2)` 与 `/proc/meminfo`。
pub fn swap_totals() -> (u64, u64) {
    let devices = SWAP_DEVICES.lock();
    let mut total = 0u64;
    let mut free = 0u64;
    for device in devices.iter() {
        total = total.saturating_add(device.size_pages);
        free = free.saturating_add(device.size_pages.saturating_sub(device.used_pages));
    }
    (total, free)
}

/// 全部 swap 设备快照，供 `/proc/swaps` 渲染。
pub fn swap_entries() -> Vec<SwapEntrySnapshot> {
    SWAP_DEVICES
        .lock()
        .iter()
        .map(SwapDevice::snapshot)
        .collect()
}

/// 是否已登记任何 swap 设备。
pub fn has_swap() -> bool {
    !SWAP_DEVICES.lock().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swapoff_unknown_path_is_einval() {
        // 表为空时任何路径都是未登记。
        assert_eq!(swapoff("/nonexistent"), Err(Errno::EINVAL));
    }

    #[test]
    fn swap_kind_names_match_proc_format() {
        assert_eq!(SwapKind::File.as_str(), "file");
        assert_eq!(SwapKind::Partition.as_str(), "partition");
    }
}
