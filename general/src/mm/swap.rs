//! swap 设备表、槽位分配与换出/换入 I/O。
//!
//! Linux 的 swap 子系统包含三个层次：
//!
//! 1. **设备登记**：`swapon` 校验文件/分区后加入系统 swap 表，`/proc/swaps`、
//!    `sysinfo(2)`、`/proc/meminfo` 的 `SwapTotal`/`SwapFree` 全部来自这张表；
//! 2. **槽位分配**：把 swap 空间切成页槽，记录哪些页已换出；
//! 3. **换出/换入**：内存压力下把 anon 页写入槽位并在 PTE 编码 swap entry，
//!    缺页时读回。
//!
//! 本模块实现全部三层中的"设备登记 + 槽位分配 + 真实换出/换入 I/O"。
//!
//! ## 与 Linux 的差异（重要）
//!
//! Linux 把 swap entry 编码进架构页表项（PTE 中"非 present 但携带
//! `(type, offset)`"的形式），缺页路径据此直接定位槽位。本内核的用户页表由
//! `arch/src/*/mm/user_pgd.rs` 维护，该文件不在本功能块的可修改清单内，因此
//! 换出页改用 **`VmSpace` 侧的槽位表**（`RadixPageMap<SwapSlot>`）跟踪：
//! 换出时写入 swap 文件并登记 `(虚拟页 -> 槽位)`，缺页时按表换入。物理页在
//! 换出后立即归还分配器，`used_pages` 真实计数，`swapoff` 在仍有已换出页时
//! 返回 `EBUSY`。语义上是"真实换出"，仅槽位定位方式与 Linux 不同。
//!
//! 依赖方向：本模块在 `general` 内，可以直接持有 `vfs::File` 保持 swap
//! 文件打开（Linux 同样在 swapoff 前持有引用，防止文件被替换）。

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

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

/// 换出页的槽位句柄。
///
/// `device_id` 由 [`SwapDevice::id`] 单调分配、永不复用；`slot` 是该设备内的
/// 页槽索引。由于 `swapoff` 在仍有已换出页时返回 `EBUSY`，仍被引用的槽位一定
/// 属于尚未解除登记的设备，句柄不会悬空。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapSlot {
    device_id: u64,
    slot: u64,
}

/// swap 设备表条目。
pub struct SwapDevice {
    /// 设备路径（`swapoff` 按此匹配）。
    pub name: String,
    pub kind: SwapKind,
    /// swap 空间总页数（文件大小/分区大小按页取整）。
    pub size_pages: u64,
    /// 已占用的槽位数。
    used_pages: u64,
    /// swapon 时指定的优先级（`SWAP_FLAG_PREFER` 低 16 位）。
    pub priority: i32,
    /// 稳定设备身份（单调递增、永不复用）。
    id: u64,
    /// 下一个尚未分配的连续槽位。
    next_free: u64,
    /// 换入后回收的空闲槽位，优先复用。
    recycled: Vec<u64>,
    /// 保持 swap 文件打开，阻止路径被替换。
    _file: Arc<File>,
}

impl SwapDevice {
    fn has_free_slot(&self) -> bool {
        !self.recycled.is_empty() || self.next_free < self.size_pages
    }

    fn alloc_slot(&mut self) -> Option<u64> {
        if let Some(slot) = self.recycled.pop() {
            self.used_pages += 1;
            return Some(slot);
        }
        if self.next_free < self.size_pages {
            let slot = self.next_free;
            self.next_free += 1;
            self.used_pages += 1;
            return Some(slot);
        }
        None
    }

    fn free_slot(&mut self, slot: u64) {
        // 槽位由调用方保证只释放一次；重复释放只会推入重复槽位导致重复写，
        // 这里仍保守记账并复用（debug 构建由调用方断言保证不重复）。
        if self.used_pages != 0 {
            self.used_pages -= 1;
        }
        self.recycled.push(slot);
    }

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
static NEXT_DEVICE_ID: AtomicU64 = AtomicU64::new(1);

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
        id: NEXT_DEVICE_ID.fetch_add(1, Ordering::Relaxed),
        next_free: 0,
        recycled: Vec::new(),
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

/// 把一页数据写入 swap 并返回槽位句柄。无空闲槽位时返回 `ENOMEM`。
///
/// 调用方必须传入整页数据（长度等于 [`crate::mm::page_size`]）；I/O 在设备表
/// 锁外完成，失败时归还槽位。
pub fn swap_out_page(data: &[u8]) -> Result<SwapSlot, Errno> {
    let page_size = crate::mm::page_size();
    if data.len() != page_size {
        return Err(Errno::EINVAL);
    }
    let (device_id, slot, file) = {
        let mut devices = SWAP_DEVICES.lock();
        let Some(device) = devices
            .iter_mut()
            .filter(|d| d.has_free_slot())
            .max_by_key(|d| d.priority)
        else {
            return Err(Errno::ENOMEM);
        };
        let slot = device.alloc_slot().ok_or(Errno::ENOMEM)?;
        (device.id, slot, Arc::clone(&device._file))
    };
    let offset = slot.checked_mul(page_size as u64).ok_or(Errno::EOVERFLOW)?;
    if write_all(&file, offset, data).is_err() {
        let mut devices = SWAP_DEVICES.lock();
        if let Some(device) = devices.iter_mut().find(|d| d.id == device_id) {
            device.free_slot(slot);
        }
        return Err(Errno::EIO);
    }
    Ok(SwapSlot { device_id, slot })
}

/// 从槽位读回一页数据。
pub fn swap_in_page(slot: SwapSlot, out: &mut [u8]) -> Result<(), Errno> {
    let page_size = crate::mm::page_size();
    if out.len() != page_size {
        return Err(Errno::EINVAL);
    }
    let file = {
        let devices = SWAP_DEVICES.lock();
        devices
            .iter()
            .find(|d| d.id == slot.device_id)
            .map(|d| Arc::clone(&d._file))
            .ok_or(Errno::EINVAL)?
    };
    let offset = slot
        .slot
        .checked_mul(page_size as u64)
        .ok_or(Errno::EOVERFLOW)?;
    read_all(&file, offset, out).map_err(|_| Errno::EIO)
}

/// 归还槽位（页已换入或已丢弃）。设备可能已解除登记时静默忽略。
pub fn swap_free(slot: SwapSlot) {
    let mut devices = SWAP_DEVICES.lock();
    if let Some(device) = devices.iter_mut().find(|d| d.id == slot.device_id) {
        device.free_slot(slot.slot);
    }
}

fn write_all(file: &File, mut offset: u64, mut data: &[u8]) -> Result<(), Errno> {
    while !data.is_empty() {
        let n = file.write_at(data, offset).map_err(|_| Errno::EIO)?;
        if n == 0 {
            return Err(Errno::EIO);
        }
        offset = offset.saturating_add(n as u64);
        data = &data[n..];
    }
    Ok(())
}

fn read_all(file: &File, mut offset: u64, mut out: &mut [u8]) -> Result<(), Errno> {
    while !out.is_empty() {
        let n = file.read_at(out, offset).map_err(|_| Errno::EIO)?;
        if n == 0 {
            return Err(Errno::EIO);
        }
        offset = offset.saturating_add(n as u64);
        out = &mut out[n..];
    }
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
