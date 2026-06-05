//! sysfs：`/sys` 虚拟文件系统。
//!
//! 当前实现以挂载时快照呈现内核对象，设备视图通过 function 注册表的兼容层
//! helper 收集字符/块设备，不向 sysfs 泄露具体 function 类型。

#![allow(dead_code, unused_variables)]

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicU64, Ordering};

use errno::Errno;
use sched::online_cpu_mask;
use vfs::cred::{Credentials, Gid, Uid};
use vfs::dentry::{Dentry, SmallStr};
use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};
use vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use vfs::mount::MountFlags;
use vfs::stat::{DevId, FileMode, FileType, FsId, FsStat, Timespec};
use vfs::superblock::{FsDriver, FsDriverFlags, Superblock, SuperblockOps};

use crate::dev::block::BlockDevice;
use crate::dev::char::CharDevice;
use crate::dev::enumerate::DEVICES;
use crate::dev::function::{active_block_devices, active_char_devices};

// ─── 静态 ino 编号 ──────────────────────────────────────────
const ROOT_INO: u64 = 1;
const BLOCK_DIR_INO: u64 = 2;
const DEVICES_DIR_INO: u64 = 3;
const DEV_DIR_INO: u64 = 4;
const KERNEL_DIR_INO: u64 = 5;
const FS_DIR_INO: u64 = 6;
const BUS_DIR_INO: u64 = 7;
const CLASS_DIR_INO: u64 = 8;
const MODULE_DIR_INO: u64 = 9;
const POWER_DIR_INO: u64 = 10;
const FIRMWARE_DIR_INO: u64 = 11;
const DEVICES_SYSTEM_INO: u64 = 12;
const DEVICES_SYSTEM_CPU_INO: u64 = 13;
const DEVICES_SYSTEM_CPU_ONLINE_INO: u64 = 14;
const DEVICES_SYSTEM_CPU_POSSIBLE_INO: u64 = 15;
const DEVICES_SYSTEM_CPU_PRESENT_INO: u64 = 16;
const DEVICES_VIRTUAL_INO: u64 = 17;
const KERNEL_HOSTNAME_INO: u64 = 18;
const KERNEL_OSTYPE_INO: u64 = 19;
const KERNEL_OSRELEASE_INO: u64 = 20;
const KERNEL_VERSION_INO: u64 = 21;
const KERNEL_CMDLINE_INO: u64 = 22;

const DEV_BLOCK_DIR_INO: u64 = 30;
const DEV_CHAR_DIR_INO: u64 = 31;
const FS_CGROUP_INO: u64 = 40;

const BLOCK_DEV_BASE: u64 = 1_000;
const BLOCK_DEV_SLOTS: u64 = 16;
const BLOCK_QUEUE_BASE: u64 = 2_000;
const BLOCK_QUEUE_SLOTS: u64 = 8;

const DEVICE_BASE: u64 = 100_000;
const DEVICE_SLOTS: u64 = 8;

const DEV_BLOCK_LINK_BASE: u64 = 1_000_000;
const DEV_CHAR_DIR_BASE: u64 = 2_000_000;
const DEV_CHAR_INNER_BASE: u64 = 3_000_000;
const DEV_CHAR_INNER_SLOTS: u64 = 4;

const CPU_BASE: u64 = 10_000_000;
const CPU_SLOTS: u64 = 4;

static SYSFS_INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(1);

const SYSFS_MAGIC: u64 = 0x6265_6572;

// ─── 渲染辅助 ──────────────────────────────────────────────

fn timespec_now() -> Timespec {
    Timespec::now()
}

fn inode_meta(mode: u16, nlink: u32, now: Timespec) -> InodeMeta {
    InodeMeta {
        size: 0,
        nlink,
        mode: FileMode::new(mode),
        uid: Uid::ROOT,
        gid: Gid::ROOT,
        atime: now,
        mtime: now,
        ctime: now,
        blocks: 0,
    }
}

fn mk_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    ino: u64,
    kind: FileType,
    mode: u16,
    nlink: u32,
    ops: Arc<dyn InodeOps + Send + Sync>,
) -> Arc<Inode> {
    Inode::new(
        InodeId { fs_id, ino },
        kind,
        DevId::new(0, 0),
        4096,
        None,
        inode_meta(mode, nlink, timespec_now()),
        ops,
        weak_sb.clone(),
    )
}

fn slice_str(buf: &[u8], offset: usize, len: usize) -> &[u8] {
    let end = (offset + len).min(buf.len());
    if offset >= buf.len() {
        &[]
    } else {
        &buf[offset..end]
    }
}

// ─── 挂载期快照：DEVICES → /sys 树 ──────────────────────────

#[derive(Clone)]
struct CharDevSnapshot {
    /// /sys/devices/ 下的目录名 = `fw_name`（如 "serial@9000000"、"null"）。
    sysfs_name: String,
    dev: CharDevice,
}

#[derive(Clone)]
struct BlockDevSnapshot {
    /// /sys/block/ 与 /sys/dev/block/ 下的目录名 = `dev.name()`（如 "vd0"）。
    sysfs_name: String,
    dev: Arc<BlockDevice>,
}

/// 挂载时从 `DEVICES` 拷贝出的不可变快照；之后 `/sys` 的全部内容都基于此。
#[derive(Clone, Default)]
struct SysSnapshot {
    chars: Vec<CharDevSnapshot>,
    blocks: Vec<BlockDevSnapshot>,
}

impl SysSnapshot {
    fn collect() -> Self {
        let mut snap = SysSnapshot::default();
        for dev in active_block_devices(&DEVICES.functions) {
            let sysfs_name = dev.name().to_string();
            snap.blocks.push(BlockDevSnapshot { sysfs_name, dev });
        }
        for dev in active_char_devices(&DEVICES.functions) {
            let sysfs_name = dev.fw_name().to_string();
            snap.chars.push(CharDevSnapshot { sysfs_name, dev });
        }
        snap
    }
}

// ─── 块设备索引辅助 ──────────────────────────────────────────

fn block_dev_ino(idx: usize) -> u64 {
    BLOCK_DEV_BASE + (idx as u64) * BLOCK_DEV_SLOTS
}
fn block_dev_slot_ino(idx: usize, slot: u64) -> u64 {
    block_dev_ino(idx) + slot
}
fn block_queue_ino(idx: usize) -> u64 {
    BLOCK_QUEUE_BASE + (idx as u64) * BLOCK_QUEUE_SLOTS
}
fn block_queue_slot_ino(idx: usize, slot: u64) -> u64 {
    block_queue_ino(idx) + slot
}
fn device_ino(idx: usize) -> u64 {
    DEVICE_BASE + (idx as u64) * DEVICE_SLOTS
}
fn device_slot_ino(idx: usize, slot: u64) -> u64 {
    device_ino(idx) + slot
}
fn dev_block_link_ino(idx: usize) -> u64 {
    DEV_BLOCK_LINK_BASE + idx as u64
}
fn dev_char_dir_ino(idx: usize) -> u64 {
    DEV_CHAR_DIR_BASE + idx as u64
}
fn dev_char_inner_ino(dir_idx: usize, slot: u64) -> u64 {
    DEV_CHAR_INNER_BASE + (dir_idx as u64) * DEV_CHAR_INNER_SLOTS + slot
}
fn cpu_ino(cpu_id: usize) -> u64 {
    CPU_BASE + (cpu_id as u64) * CPU_SLOTS
}
fn cpu_slot_ino(cpu_id: usize, slot: u64) -> u64 {
    cpu_ino(cpu_id) + slot
}

// ─── 文件 kind 枚举 ──────────────────────────────────────────

#[derive(Clone, Copy)]
enum BlockDevSlot {
    Size,
    Ro,
    Removable,
    Dev,
    Range,
    QueueDir,
    Holders,
    Stat,
    Inflight,
    Periodic,
}

impl BlockDevSlot {
    fn from_u64(v: u64) -> Option<Self> {
        Some(match v {
            0 => Self::Size,
            1 => Self::Ro,
            2 => Self::Removable,
            3 => Self::Dev,
            4 => Self::Range,
            5 => Self::QueueDir,
            6 => Self::Holders,
            7 => Self::Stat,
            8 => Self::Inflight,
            9 => Self::Periodic,
            _ => return None,
        })
    }
    fn to_u64(self) -> u64 {
        match self {
            Self::Size => 0,
            Self::Ro => 1,
            Self::Removable => 2,
            Self::Dev => 3,
            Self::Range => 4,
            Self::QueueDir => 5,
            Self::Holders => 6,
            Self::Stat => 7,
            Self::Inflight => 8,
            Self::Periodic => 9,
        }
    }
}

#[derive(Clone, Copy)]
enum BlockQueueSlot {
    Lbs,
    Pbs,
    Rotational,
    NrRequests,
    HwSectorSize,
    DiscardZeroes,
}

impl BlockQueueSlot {
    fn from_u64(v: u64) -> Option<Self> {
        Some(match v {
            0 => Self::Lbs,
            1 => Self::Pbs,
            2 => Self::Rotational,
            3 => Self::NrRequests,
            4 => Self::HwSectorSize,
            5 => Self::DiscardZeroes,
            _ => return None,
        })
    }
    fn to_u64(self) -> u64 {
        match self {
            Self::Lbs => 0,
            Self::Pbs => 1,
            Self::Rotational => 2,
            Self::NrRequests => 3,
            Self::HwSectorSize => 4,
            Self::DiscardZeroes => 5,
        }
    }
}

#[derive(Clone, Copy)]
enum DeviceSlot {
    Name,
    Dev,
    Driver,
    Subsystem,
    PwrDir,
}

impl DeviceSlot {
    fn from_u64(v: u64) -> Option<Self> {
        Some(match v {
            0 => Self::Name,
            1 => Self::Dev,
            2 => Self::Driver,
            3 => Self::Subsystem,
            4 => Self::PwrDir,
            _ => return None,
        })
    }
    fn to_u64(self) -> u64 {
        match self {
            Self::Name => 0,
            Self::Dev => 1,
            Self::Driver => 2,
            Self::Subsystem => 3,
            Self::PwrDir => 4,
        }
    }
}

#[derive(Clone, Copy)]
enum DevCharInnerSlot {
    Dev,
    DeviceLink,
    SubsystemLink,
    Uevent,
}

impl DevCharInnerSlot {
    fn from_u64(v: u64) -> Option<Self> {
        Some(match v {
            0 => Self::Dev,
            1 => Self::DeviceLink,
            2 => Self::SubsystemLink,
            3 => Self::Uevent,
            _ => return None,
        })
    }
    fn to_u64(self) -> u64 {
        match self {
            Self::Dev => 0,
            Self::DeviceLink => 1,
            Self::SubsystemLink => 2,
            Self::Uevent => 3,
        }
    }
}

#[derive(Clone, Copy)]
enum CpuSlot {
    TopoDir,
    Online,
    Possible,
    Present,
}

impl CpuSlot {
    fn from_u64(v: u64) -> Option<Self> {
        Some(match v {
            0 => Self::TopoDir,
            1 => Self::Online,
            2 => Self::Possible,
            3 => Self::Present,
            _ => return None,
        })
    }
    fn to_u64(self) -> u64 {
        match self {
            Self::TopoDir => 0,
            Self::Online => 1,
            Self::Possible => 2,
            Self::Present => 3,
        }
    }
}

#[derive(Clone, Copy)]
enum SysRegFile {
    BlockDev { idx: usize, slot: BlockDevSlot },
    BlockQueue { idx: usize, slot: BlockQueueSlot },
    Device { idx: usize, slot: DeviceSlot },
    DevCharInner { idx: usize, slot: DevCharInnerSlot },
    Cpu { cpu_id: usize, slot: CpuSlot },
    CpuOnline,
    CpuPossible,
    CpuPresent,
    Hostname,
    Ostype,
    Osrelease,
    Version,
    Cmdline,
    UeventPlaceholder,
}

// ─── 内容渲染 ────────────────────────────────────────────────

fn render_block_dev_file(snap: &SysSnapshot, idx: usize, slot: BlockDevSlot) -> String {
    let dev = &snap.blocks[idx].dev;
    let geom = dev.geometry();
    let features = dev.features();
    match slot {
        BlockDevSlot::Size => {
            let sectors = geom
                .block_count()
                .map(|c| c * (geom.logical_block_size().get() as u64) / 512)
                .unwrap_or(0);
            format!("{}\n", sectors)
        }
        BlockDevSlot::Ro => {
            if features.contains(crate::dev::block::BlockFeatures::READ_ONLY) {
                "1\n".into()
            } else {
                "0\n".into()
            }
        }
        BlockDevSlot::Removable => "0\n".into(),
        // TODO: 当前所有块设备硬编码 major:minor=254:0，需要从兼容层分配的真实编号读取
        BlockDevSlot::Dev => "254:0\n".into(),
        BlockDevSlot::Range => "1\n".into(),
        BlockDevSlot::Holders => String::new(),
        BlockDevSlot::Stat => {
            // TODO: 实现完整 diskstats 统计（reads/writes/sectors/iotime 等 11 字段），
            //       当前新架构不再追踪 in_flight 计数，全填 0
            format!("0 0 0 0 0 0 0 0 0 0 0 0 0\n")
        }
        // TODO: inflight 字段也需要真实统计（目前硬编码 0）
        BlockDevSlot::Inflight => format!(" 0       0\n"),
        BlockDevSlot::Periodic => String::new(),
        BlockDevSlot::QueueDir => String::new(),
    }
}

fn render_block_queue_file(snap: &SysSnapshot, idx: usize, slot: BlockQueueSlot) -> String {
    let dev = &snap.blocks[idx].dev;
    let geom = dev.geometry();
    let features = dev.features();
    match slot {
        BlockQueueSlot::Lbs => format!("{}\n", geom.logical_block_size().get()),
        BlockQueueSlot::Pbs => format!("{}\n", geom.physical_block_size().get()),
        BlockQueueSlot::Rotational => "0\n".into(), // 全部按 SSD 报告：virtio-blk、NVMe 都不是机械盘
        BlockQueueSlot::NrRequests => "64\n".into(),
        BlockQueueSlot::HwSectorSize => format!("{}\n", geom.logical_block_size().get()),
        BlockQueueSlot::DiscardZeroes => {
            if features.contains(crate::dev::block::BlockFeatures::WRITE_ZEROES) {
                "1\n".into()
            } else {
                "0\n".into()
            }
        }
    }
}

fn render_device_file(snap: &SysSnapshot, idx: usize, slot: DeviceSlot) -> String {
    // 注意：idx 跨越 char 与 block。char 排在前、block 排在后。
    let chars_len = snap.chars.len();
    if idx < chars_len {
        let c = &snap.chars[idx];
        match slot {
            DeviceSlot::Name => format!("{}\n", c.sysfs_name),
            // TODO: 字符设备的 dev 字段硬编码 0:0，需要从兼容层读取真实 major:minor
            DeviceSlot::Dev => "0:0\n".into(),
            DeviceSlot::Driver => String::new(),
            // TODO: 实现真正的 subsystem 链接（指向 /sys/class/<class> 或 /sys/bus/<bus>）
            DeviceSlot::Subsystem => "(unimplemented)\n".into(),
            DeviceSlot::PwrDir => String::new(),
        }
    } else {
        let bi = idx - chars_len;
        let b = &snap.blocks[bi];
        match slot {
            DeviceSlot::Name => format!("{}\n", b.sysfs_name),
            // TODO: 块设备的 dev 字段硬编码 0:0，需要从兼容层读取真实 major:minor
            DeviceSlot::Dev => "0:0\n".into(),
            DeviceSlot::Driver => String::new(),
            // TODO: 实现真正的 subsystem 链接（指向 /sys/class/block）
            DeviceSlot::Subsystem => "(unimplemented)\n".into(),
            DeviceSlot::PwrDir => String::new(),
        }
    }
}

fn render_dev_char_inner(_snap: &SysSnapshot, _idx: usize, slot: DevCharInnerSlot) -> String {
    match slot {
        // TODO: 字符设备 /sys/dev/char/<id>/dev 硬编码 0:0，需要兼容层指定的 major:minor
        DevCharInnerSlot::Dev => "0:0\n".into(),
        DevCharInnerSlot::DeviceLink => String::new(), // symlink，不渲染
        DevCharInnerSlot::SubsystemLink => String::new(),
        // TODO: uevent 内容应包含真实的 MAJOR、MINOR、DEVNAME 等字段
        DevCharInnerSlot::Uevent => "MODALIAS=char:0:0\n".into(),
    }
}

fn render_cpu_file(_snap: &SysSnapshot, _cpu_id: usize, slot: CpuSlot) -> String {
    match slot {
        CpuSlot::TopoDir => String::new(),
        CpuSlot::Online => "1\n".into(),
        CpuSlot::Possible => "1\n".into(),
        CpuSlot::Present => "1\n".into(),
    }
}

fn render_reg_file(snap: &SysSnapshot, kind: SysRegFile) -> String {
    match kind {
        SysRegFile::BlockDev { idx, slot } => render_block_dev_file(snap, idx, slot),
        SysRegFile::BlockQueue { idx, slot } => render_block_queue_file(snap, idx, slot),
        SysRegFile::Device { idx, slot } => render_device_file(snap, idx, slot),
        SysRegFile::DevCharInner { idx, slot } => render_dev_char_inner(snap, idx, slot),
        SysRegFile::Cpu { cpu_id, slot } => render_cpu_file(snap, cpu_id, slot),
        // TODO: /sys/devices/system/cpu/{online,possible,present} 应渲染真实 CPU 范围（如 "0-3"），
        //       目前硬编码为 "0\n"
        SysRegFile::CpuOnline => "0\n".into(),
        SysRegFile::CpuPossible => "0\n".into(),
        SysRegFile::CpuPresent => "0\n".into(),
        SysRegFile::Hostname => "mygo\n".into(),
        SysRegFile::Ostype => "MyGO\n".into(),
        SysRegFile::Osrelease => env!("CARGO_PKG_VERSION").to_string() + "\n",
        SysRegFile::Version => format!("mygo {} (mygo-build)\n", env!("CARGO_PKG_VERSION")),
        SysRegFile::Cmdline => {
            // TODO: 应输出内核启动命令行（来自 bootloader/DTB），当前为空字符串
            String::new()
        }
        SysRegFile::UeventPlaceholder => String::new(),
    }
}

// ─── Driver / Superblock ─────────────────────────────────────

pub struct SysFsDriver;

impl FsDriver for SysFsDriver {
    fn name(&self) -> &'static str {
        "sysfs"
    }
    fn flags(&self) -> FsDriverFlags {
        FsDriverFlags::NODEV
            .with(FsDriverFlags::SINGLE)
            .with(FsDriverFlags::RDONLY)
    }

    fn mount(&self, _dev: Option<&str>, _data: &str) -> VfsResult<Arc<Superblock>> {
        let fs_id = FsId::new(SYSFS_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed));
        Ok(Superblock::new(|weak_sb| {
            let snap = Arc::new(SysSnapshot::collect());
            let root_inode = build_root_inode(fs_id, &weak_sb, Arc::clone(&snap));
            let root_dentry = Dentry::new_positive("", None, Arc::clone(&root_inode));
            Superblock {
                fs_type: "sysfs",
                fs_id,
                dev_id: None,
                block_size: 4096,
                name_max: 255,
                root_inode,
                root_dentry,
                inode_cache: vfs::superblock::InodeCache::new(),
                ops: Box::new(SysSuperblockOps),
                self_weak: weak_sb,
            }
        }))
    }

    fn kill_sb(&self, _sb: Arc<Superblock>) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct SysSuperblockOps;
impl SuperblockOps for SysSuperblockOps {
    fn alloc_inode(&self, _: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::ReadOnlyFilesystem)
    }
    fn write_inode(&self, _: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }
    fn statfs(&self, sb: &Arc<Superblock>) -> VfsResult<FsStat> {
        Ok(FsStat {
            fs_type: SYSFS_MAGIC,
            block_size: sb.block_size as u64,
            total_blocks: 0,
            free_blocks: 0,
            avail_blocks: 0,
            total_inodes: 0,
            free_inodes: 0,
            fs_id: sb.fs_id.raw(),
            name_max: sb.name_max,
        })
    }
    fn sync_fs(&self, _: &Arc<Superblock>) -> VfsResult<()> {
        Ok(())
    }
    fn remount(&self, _: &Arc<Superblock>, _: MountFlags) -> VfsResult<()> {
        Ok(())
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ─── File / Dir FileOps ─────────────────────────────────────

struct SysDirFile {
    snapshot: Vec<DirEntry>,
}
struct SysRegFileOps {
    kind: SysRegFile,
    snap: Arc<SysSnapshot>,
}
struct SysEmptyFile; // /sys/dev/char/.../uevent 等目前无可读内容

fn feed_dir_entries(
    snapshot: &[DirEntry],
    pos: u64,
    sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
) -> VfsResult<u64> {
    let start = core::cmp::min(pos as usize, snapshot.len());
    for (i, entry) in snapshot.iter().enumerate().skip(start) {
        if sink(entry.clone()).is_break() {
            return Ok((i + 1) as u64);
        }
    }
    Ok(snapshot.len() as u64)
}

impl FileOps for SysDirFile {
    fn read_at(&self, _: &mut [u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::IsADirectory)
    }
    fn write_at(&self, _: &[u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::IsADirectory)
    }
    fn readdir(
        &self,
        pos: u64,
        sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        feed_dir_entries(&self.snapshot, pos, sink)
    }
    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
    fn poll(&self, _: PollEvents) -> PollEvents {
        PollEvents(0)
    }
    fn ioctl(&self, _: IoctlCmd, _: usize) -> Result<usize, Errno> {
        Err(errno::Errno::ENOTTY)
    }
    fn release(&self) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl FileOps for SysRegFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let s = render_reg_file(&self.snap, self.kind);
        let bytes = s.as_bytes();
        let total = bytes.len();
        let off = offset as usize;
        if off >= total {
            return Ok(0);
        }
        let n = core::cmp::min(buf.len(), total - off);
        buf[..n].copy_from_slice(&bytes[off..off + n]);
        Ok(n)
    }
    fn write_at(&self, _: &[u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::ReadOnlyFilesystem)
    }
    fn readdir(&self, _: u64, _: &mut dyn FnMut(DirEntry) -> ControlFlow<()>) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }
    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
    fn poll(&self, _: PollEvents) -> PollEvents {
        PollEvents(0)
    }
    fn ioctl(&self, _: IoctlCmd, _: usize) -> Result<usize, Errno> {
        Err(errno::Errno::ENOTTY)
    }
    fn release(&self) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl FileOps for SysEmptyFile {
    fn read_at(&self, _: &mut [u8], _: u64) -> VfsResult<usize> {
        Ok(0)
    }
    fn write_at(&self, _: &[u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::ReadOnlyFilesystem)
    }
    fn readdir(&self, _: u64, _: &mut dyn FnMut(DirEntry) -> ControlFlow<()>) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }
    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
    fn poll(&self, _: PollEvents) -> PollEvents {
        PollEvents(0)
    }
    fn ioctl(&self, _: IoctlCmd, _: usize) -> Result<usize, Errno> {
        Err(errno::Errno::ENOTTY)
    }
    fn release(&self) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ─── 目录/文件 InodeOps 统一工厂宏 ───────────────────────────

/// 给定子 ino 与子文件 kind，构造对应文件/目录的 Inode（供目录 lookup 共用）。
fn build_child_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    snap: &Arc<SysSnapshot>,
    ino: u64,
    kind: SysRegFile,
) -> Option<Arc<Inode>> {
    let ops: Arc<dyn InodeOps + Send + Sync> = Arc::new(SysRegInodeOps {
        kind,
        snap: Arc::clone(snap),
    });
    Some(mk_inode(
        fs_id,
        weak_sb,
        ino,
        FileType::Regular,
        0o444,
        1,
        ops,
    ))
}

fn build_dir_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    snap: &Arc<SysSnapshot>,
    ino: u64,
    dir_kind: SysDirKind,
) -> Arc<Inode> {
    let ops: Arc<dyn InodeOps + Send + Sync> = Arc::new(SysDirInodeOps {
        kind: dir_kind,
        fs_id,
        weak_sb: weak_sb.clone(),
        snap: Arc::clone(snap),
    });
    mk_inode(fs_id, weak_sb, ino, FileType::Directory, 0o555, 2, ops)
}

fn build_link_inode(
    fs_id: FsId,
    weak_sb: &Weak<Superblock>,
    ino: u64,
    target: String,
) -> Arc<Inode> {
    let ops: Arc<dyn InodeOps + Send + Sync> = Arc::new(SysLinkInodeOps { target });
    mk_inode(fs_id, weak_sb, ino, FileType::Symlink, 0o777, 1, ops)
}

// ─── 目录类型枚举 ───────────────────────────────────────────

#[derive(Clone, Copy)]
enum SysDirKind {
    Root,
    Block,
    BlockDev { idx: usize },
    BlockQueue { idx: usize },
    Devices,
    Device { idx: usize },
    Dev,
    DevBlock,
    DevChar,
    DevCharInner { idx: usize },
    Kernel,
    Fs,
    FsCgroup,
    Bus,
    Class,
    Module,
    Power,
    Firmware,
    DevicesSystem,
    DevicesSystemCpu,
    Cpu { cpu_id: usize },
}

// ─── InodeOps ────────────────────────────────────────────────

struct SysRegInodeOps {
    kind: SysRegFile,
    snap: Arc<SysSnapshot>,
}
struct SysLinkInodeOps {
    target: String,
}
struct SysDirInodeOps {
    kind: SysDirKind,
    fs_id: FsId,
    weak_sb: Weak<Superblock>,
    snap: Arc<SysSnapshot>,
}

impl InodeOps for SysRegInodeOps {
    fn lookup(&self, _: &Inode, _: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }
    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        if matches!(self.kind, SysRegFile::UeventPlaceholder) {
            return Ok(Box::new(SysEmptyFile));
        }
        Ok(Box::new(SysRegFileOps {
            kind: self.kind,
            snap: Arc::clone(&self.snap),
        }))
    }
    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl InodeOps for SysLinkInodeOps {
    fn lookup(&self, _: &Inode, _: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }
    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Err(VfsError::NotFound)
    }
    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Ok(self.target.clone())
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl InodeOps for SysDirInodeOps {
    fn lookup(&self, _: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        self.lookup_child(name)
    }
    fn open(
        &self,
        _: &Inode,
        _: &OpenOptions,
        _: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(SysDirFile {
            snapshot: self.readdir_entries(),
        }))
    }
    fn readlink(&self, _: &Inode) -> VfsResult<String> {
        Err(VfsError::InvalidArgument)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
impl SysDirInodeOps {
    fn lookup_child(&self, name: &str) -> VfsResult<Arc<Inode>> {
        let fs_id = self.fs_id;
        let weak_sb = &self.weak_sb;
        let snap = &self.snap;
        let mk_reg = |ino: u64, kind: SysRegFile| -> VfsResult<Arc<Inode>> {
            build_child_inode(fs_id, weak_sb, snap, ino, kind).ok_or(VfsError::OutOfMemory)
        };
        let mk_dir = |ino: u64, k: SysDirKind| -> Arc<Inode> {
            build_dir_inode(fs_id, weak_sb, snap, ino, k)
        };
        let mk_link = |ino: u64, target: String| -> Arc<Inode> {
            build_link_inode(fs_id, weak_sb, ino, target)
        };

        match self.kind {
            SysDirKind::Root => match name {
                "block" => Ok(mk_dir(BLOCK_DIR_INO, SysDirKind::Block)),
                "devices" => Ok(mk_dir(DEVICES_DIR_INO, SysDirKind::Devices)),
                "dev" => Ok(mk_dir(DEV_DIR_INO, SysDirKind::Dev)),
                "kernel" => Ok(mk_dir(KERNEL_DIR_INO, SysDirKind::Kernel)),
                "fs" => Ok(mk_dir(FS_DIR_INO, SysDirKind::Fs)),
                "bus" => Ok(mk_dir(BUS_DIR_INO, SysDirKind::Bus)),
                "class" => Ok(mk_dir(CLASS_DIR_INO, SysDirKind::Class)),
                "module" => Ok(mk_dir(MODULE_DIR_INO, SysDirKind::Module)),
                "power" => Ok(mk_dir(POWER_DIR_INO, SysDirKind::Power)),
                "firmware" => Ok(mk_dir(FIRMWARE_DIR_INO, SysDirKind::Firmware)),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::Block => {
                let idx = snap
                    .blocks
                    .iter()
                    .position(|b| b.sysfs_name == name)
                    .ok_or(VfsError::NotFound)?;
                Ok(mk_dir(block_dev_ino(idx), SysDirKind::BlockDev { idx }))
            }
            SysDirKind::BlockDev { idx } => {
                let slot = block_slot_by_name(name).ok_or(VfsError::NotFound)?;
                let ino = block_dev_slot_ino(idx, slot.to_u64());
                if matches!(slot, BlockDevSlot::QueueDir) {
                    Ok(mk_dir(ino, SysDirKind::BlockQueue { idx }))
                } else {
                    mk_reg(ino, SysRegFile::BlockDev { idx, slot })
                }
            }
            SysDirKind::BlockQueue { idx } => {
                let slot = block_queue_slot_by_name(name).ok_or(VfsError::NotFound)?;
                let ino = block_queue_slot_ino(idx, slot.to_u64());
                mk_reg(ino, SysRegFile::BlockQueue { idx, slot })
            }
            SysDirKind::Devices => {
                // char 在前，block 在后，索引与 readdir 顺序一致
                let total_chars = snap.chars.len();
                if let Some(ci) = snap.chars.iter().position(|c| c.sysfs_name == name) {
                    Ok(mk_dir(device_ino(ci), SysDirKind::Device { idx: ci }))
                } else if let Some(bi) = snap.blocks.iter().position(|b| b.sysfs_name == name) {
                    Ok(mk_dir(
                        device_ino(total_chars + bi),
                        SysDirKind::Device {
                            idx: total_chars + bi,
                        },
                    ))
                } else if name == "system" {
                    Ok(mk_dir(DEVICES_SYSTEM_INO, SysDirKind::DevicesSystem))
                } else if name == "virtual" {
                    // TODO: 实现 /sys/devices/virtual/ 目录（包含 tty, mem, misc 等虚拟设备子目录）
                    Ok(mk_dir(
                        DEVICES_VIRTUAL_INO,
                        SysDirKind::Firmware, /* 复用空目录实现 */
                    ))
                } else {
                    Err(VfsError::NotFound)
                }
            }
            SysDirKind::Device { idx } => {
                let slot = device_slot_by_name(name).ok_or(VfsError::NotFound)?;
                let ino = device_slot_ino(idx, slot.to_u64());
                if matches!(slot, DeviceSlot::PwrDir) {
                    // TODO: 实现设备 power 子目录（runtime_status, control, wakeup 等）
                    Ok(mk_dir(ino, SysDirKind::Module))
                } else if matches!(slot, DeviceSlot::Subsystem) {
                    // 子系统链接：直接指向 /sys/<bus|class|...>
                    let target = if idx < snap.chars.len() {
                        "..".to_string()
                    } else {
                        "..".to_string()
                    };
                    Ok(mk_link(ino, target))
                } else {
                    mk_reg(ino, SysRegFile::Device { idx, slot })
                }
            }
            SysDirKind::Dev => match name {
                "block" => Ok(mk_dir(DEV_BLOCK_DIR_INO, SysDirKind::DevBlock)),
                "char" => Ok(mk_dir(DEV_CHAR_DIR_INO, SysDirKind::DevChar)),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::DevBlock => {
                let idx = snap
                    .blocks
                    .iter()
                    .position(|b| b.sysfs_name == name)
                    .ok_or(VfsError::NotFound)?;
                Ok(mk_link(
                    dev_block_link_ino(idx),
                    format!("../../block/{}", snap.blocks[idx].sysfs_name),
                ))
            }
            SysDirKind::DevChar => {
                let idx = snap
                    .chars
                    .iter()
                    .position(|c| c.sysfs_name == name)
                    .ok_or(VfsError::NotFound)?;
                Ok(mk_dir(
                    dev_char_dir_ino(idx),
                    SysDirKind::DevCharInner { idx },
                ))
            }
            SysDirKind::DevCharInner { idx } => {
                let slot = dev_char_inner_slot_by_name(name).ok_or(VfsError::NotFound)?;
                let ino = dev_char_inner_ino(idx, slot.to_u64());
                match slot {
                    DevCharInnerSlot::DeviceLink => Ok(mk_link(
                        ino,
                        format!("../../devices/{}", snap.chars[idx].sysfs_name),
                    )),
                    DevCharInnerSlot::SubsystemLink => Ok(mk_link(ino, "../../class".into())),
                    _ => mk_reg(ino, SysRegFile::DevCharInner { idx, slot }),
                }
            }
            SysDirKind::Kernel => match name {
                "hostname" => mk_reg(KERNEL_HOSTNAME_INO, SysRegFile::Hostname),
                "ostype" => mk_reg(KERNEL_OSTYPE_INO, SysRegFile::Ostype),
                "osrelease" => mk_reg(KERNEL_OSRELEASE_INO, SysRegFile::Osrelease),
                "version" => mk_reg(KERNEL_VERSION_INO, SysRegFile::Version),
                "cmdline" => mk_reg(KERNEL_CMDLINE_INO, SysRegFile::Cmdline),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::Fs => match name {
                // TODO: 实现 /sys/fs/cgroup/ 内容（cgroup 控制器挂载点等）
                "cgroup" => Ok(mk_dir(FS_CGROUP_INO, SysDirKind::FsCgroup)),
                _ => Err(VfsError::NotFound),
            },
            // TODO: /sys/fs/cgroup/ 目录内容为空，需要实现 cgroup 子系统
            SysDirKind::FsCgroup => Err(VfsError::NotFound),
            // TODO: 以下顶层目录均为空占位，需要逐步实现：
            //   Bus: PCI/USB/Platform 等总线设备枚举
            //   Class: 设备分类（net, input, tty 等）
            //   Module: 已加载内核模块列表
            //   Power: 电源管理状态和控制
            //   Firmware: 固件相关（ACPI, DMI 等）
            SysDirKind::Bus
            | SysDirKind::Class
            | SysDirKind::Module
            | SysDirKind::Power
            | SysDirKind::Firmware => Err(VfsError::NotFound),
            SysDirKind::DevicesSystem => match name {
                "cpu" => Ok(mk_dir(DEVICES_SYSTEM_CPU_INO, SysDirKind::DevicesSystemCpu)),
                _ => Err(VfsError::NotFound),
            },
            SysDirKind::DevicesSystemCpu => {
                if name == "online" {
                    mk_reg(DEVICES_SYSTEM_CPU_ONLINE_INO, SysRegFile::CpuOnline)
                } else if name == "possible" {
                    mk_reg(DEVICES_SYSTEM_CPU_POSSIBLE_INO, SysRegFile::CpuPossible)
                } else if name == "present" {
                    mk_reg(DEVICES_SYSTEM_CPU_PRESENT_INO, SysRegFile::CpuPresent)
                } else if let Some(rest) = name.strip_prefix("cpu") {
                    let cpu_id: usize = rest.parse().map_err(|_| VfsError::NotFound)?;
                    let mask = online_cpu_mask();
                    if mask & (1u64 << cpu_id) == 0 {
                        return Err(VfsError::NotFound);
                    }
                    Ok(mk_dir(cpu_ino(cpu_id), SysDirKind::Cpu { cpu_id }))
                } else {
                    Err(VfsError::NotFound)
                }
            }
            SysDirKind::Cpu { cpu_id } => match name {
                "online" => mk_reg(
                    cpu_slot_ino(cpu_id, CpuSlot::Online.to_u64()),
                    SysRegFile::Cpu {
                        cpu_id,
                        slot: CpuSlot::Online,
                    },
                ),
                "possible" => mk_reg(
                    cpu_slot_ino(cpu_id, CpuSlot::Possible.to_u64()),
                    SysRegFile::Cpu {
                        cpu_id,
                        slot: CpuSlot::Possible,
                    },
                ),
                "present" => mk_reg(
                    cpu_slot_ino(cpu_id, CpuSlot::Present.to_u64()),
                    SysRegFile::Cpu {
                        cpu_id,
                        slot: CpuSlot::Present,
                    },
                ),
                "topology" => Ok(mk_dir(
                    cpu_slot_ino(cpu_id, CpuSlot::TopoDir.to_u64()),
                    // TODO: 实现 CPU topology 目录（core_id, physical_package_id 等）
                    SysDirKind::Module, /* 复用空目录 */
                )),
                _ => Err(VfsError::NotFound),
            },
        }
    }

    fn readdir_entries(&self) -> Vec<DirEntry> {
        let fs_id = self.fs_id;
        let weak_sb = &self.weak_sb;
        let snap = &self.snap;
        let mk_dir_entry = |ino: u64, name: &str, kind: FileType| DirEntry {
            ino,
            name: SmallStr::new(name),
            kind,
        };
        match self.kind {
            SysDirKind::Root => vec![
                mk_dir_entry(BLOCK_DIR_INO, "block", FileType::Directory),
                mk_dir_entry(DEVICES_DIR_INO, "devices", FileType::Directory),
                mk_dir_entry(DEV_DIR_INO, "dev", FileType::Directory),
                mk_dir_entry(KERNEL_DIR_INO, "kernel", FileType::Directory),
                mk_dir_entry(FS_DIR_INO, "fs", FileType::Directory),
                mk_dir_entry(BUS_DIR_INO, "bus", FileType::Directory),
                mk_dir_entry(CLASS_DIR_INO, "class", FileType::Directory),
                mk_dir_entry(MODULE_DIR_INO, "module", FileType::Directory),
                mk_dir_entry(POWER_DIR_INO, "power", FileType::Directory),
                mk_dir_entry(FIRMWARE_DIR_INO, "firmware", FileType::Directory),
            ],
            SysDirKind::Block => snap
                .blocks
                .iter()
                .enumerate()
                .map(|(i, b)| mk_dir_entry(block_dev_ino(i), &b.sysfs_name, FileType::Directory))
                .collect(),
            SysDirKind::BlockDev { idx } => vec![
                mk_dir_entry(
                    block_dev_slot_ino(idx, BlockDevSlot::Size.to_u64()),
                    "size",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    block_dev_slot_ino(idx, BlockDevSlot::Ro.to_u64()),
                    "ro",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    block_dev_slot_ino(idx, BlockDevSlot::Removable.to_u64()),
                    "removable",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    block_dev_slot_ino(idx, BlockDevSlot::Dev.to_u64()),
                    "dev",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    block_dev_slot_ino(idx, BlockDevSlot::Range.to_u64()),
                    "range",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    block_dev_slot_ino(idx, BlockDevSlot::QueueDir.to_u64()),
                    "queue",
                    FileType::Directory,
                ),
                mk_dir_entry(
                    block_dev_slot_ino(idx, BlockDevSlot::Holders.to_u64()),
                    "holders",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    block_dev_slot_ino(idx, BlockDevSlot::Stat.to_u64()),
                    "stat",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    block_dev_slot_ino(idx, BlockDevSlot::Inflight.to_u64()),
                    "inflight",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    block_dev_slot_ino(idx, BlockDevSlot::Periodic.to_u64()),
                    "periodic",
                    FileType::Regular,
                ),
            ],
            SysDirKind::BlockQueue { idx } => vec![
                mk_dir_entry(
                    block_queue_slot_ino(idx, BlockQueueSlot::Lbs.to_u64()),
                    "logical_block_size",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    block_queue_slot_ino(idx, BlockQueueSlot::Pbs.to_u64()),
                    "physical_block_size",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    block_queue_slot_ino(idx, BlockQueueSlot::Rotational.to_u64()),
                    "rotational",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    block_queue_slot_ino(idx, BlockQueueSlot::NrRequests.to_u64()),
                    "nr_requests",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    block_queue_slot_ino(idx, BlockQueueSlot::HwSectorSize.to_u64()),
                    "hw_sector_size",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    block_queue_slot_ino(idx, BlockQueueSlot::DiscardZeroes.to_u64()),
                    "discard_zeroes_data",
                    FileType::Regular,
                ),
            ],
            SysDirKind::Devices => {
                let mut v: Vec<DirEntry> = snap
                    .chars
                    .iter()
                    .enumerate()
                    .map(|(i, c)| mk_dir_entry(device_ino(i), &c.sysfs_name, FileType::Directory))
                    .collect();
                let total = snap.chars.len();
                v.extend(snap.blocks.iter().enumerate().map(|(i, b)| {
                    mk_dir_entry(device_ino(total + i), &b.sysfs_name, FileType::Directory)
                }));
                v.push(mk_dir_entry(
                    DEVICES_SYSTEM_INO,
                    "system",
                    FileType::Directory,
                ));
                v.push(mk_dir_entry(
                    DEVICES_VIRTUAL_INO,
                    "virtual",
                    FileType::Directory,
                ));
                v
            }
            SysDirKind::Device { idx } => vec![
                mk_dir_entry(
                    device_slot_ino(idx, DeviceSlot::Name.to_u64()),
                    "name",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    device_slot_ino(idx, DeviceSlot::Dev.to_u64()),
                    "dev",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    device_slot_ino(idx, DeviceSlot::Driver.to_u64()),
                    "driver",
                    FileType::Symlink,
                ),
                mk_dir_entry(
                    device_slot_ino(idx, DeviceSlot::Subsystem.to_u64()),
                    "subsystem",
                    FileType::Symlink,
                ),
                mk_dir_entry(
                    device_slot_ino(idx, DeviceSlot::PwrDir.to_u64()),
                    "power",
                    FileType::Directory,
                ),
            ],
            SysDirKind::Dev => vec![
                mk_dir_entry(DEV_BLOCK_DIR_INO, "block", FileType::Directory),
                mk_dir_entry(DEV_CHAR_DIR_INO, "char", FileType::Directory),
            ],
            SysDirKind::DevBlock => snap
                .blocks
                .iter()
                .enumerate()
                .map(|(i, b)| mk_dir_entry(dev_block_link_ino(i), &b.sysfs_name, FileType::Symlink))
                .collect(),
            SysDirKind::DevChar => snap
                .chars
                .iter()
                .enumerate()
                .map(|(i, c)| mk_dir_entry(dev_char_dir_ino(i), &c.sysfs_name, FileType::Directory))
                .collect(),
            SysDirKind::DevCharInner { idx } => vec![
                mk_dir_entry(
                    dev_char_inner_ino(idx, DevCharInnerSlot::Dev.to_u64()),
                    "dev",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    dev_char_inner_ino(idx, DevCharInnerSlot::DeviceLink.to_u64()),
                    "device",
                    FileType::Symlink,
                ),
                mk_dir_entry(
                    dev_char_inner_ino(idx, DevCharInnerSlot::SubsystemLink.to_u64()),
                    "subsystem",
                    FileType::Symlink,
                ),
                mk_dir_entry(
                    dev_char_inner_ino(idx, DevCharInnerSlot::Uevent.to_u64()),
                    "uevent",
                    FileType::Regular,
                ),
            ],
            SysDirKind::Kernel => vec![
                mk_dir_entry(KERNEL_HOSTNAME_INO, "hostname", FileType::Regular),
                mk_dir_entry(KERNEL_OSTYPE_INO, "ostype", FileType::Regular),
                mk_dir_entry(KERNEL_OSRELEASE_INO, "osrelease", FileType::Regular),
                mk_dir_entry(KERNEL_VERSION_INO, "version", FileType::Regular),
                mk_dir_entry(KERNEL_CMDLINE_INO, "cmdline", FileType::Regular),
            ],
            SysDirKind::Fs => vec![mk_dir_entry(FS_CGROUP_INO, "cgroup", FileType::Directory)],
            SysDirKind::FsCgroup => Vec::new(),
            SysDirKind::Bus
            | SysDirKind::Class
            | SysDirKind::Module
            | SysDirKind::Power
            | SysDirKind::Firmware => Vec::new(),
            SysDirKind::DevicesSystem => vec![mk_dir_entry(
                DEVICES_SYSTEM_CPU_INO,
                "cpu",
                FileType::Directory,
            )],
            SysDirKind::DevicesSystemCpu => {
                let mask = online_cpu_mask();
                let mut v = vec![
                    mk_dir_entry(DEVICES_SYSTEM_CPU_ONLINE_INO, "online", FileType::Regular),
                    mk_dir_entry(
                        DEVICES_SYSTEM_CPU_POSSIBLE_INO,
                        "possible",
                        FileType::Regular,
                    ),
                    mk_dir_entry(DEVICES_SYSTEM_CPU_PRESENT_INO, "present", FileType::Regular),
                ];
                for cpu in 0..64 {
                    if mask & (1u64 << cpu) != 0 {
                        v.push(mk_dir_entry(
                            cpu_ino(cpu),
                            &format!("cpu{}", cpu),
                            FileType::Directory,
                        ));
                    }
                }
                v
            }
            SysDirKind::Cpu { cpu_id } => vec![
                mk_dir_entry(
                    cpu_slot_ino(cpu_id, CpuSlot::Online.to_u64()),
                    "online",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    cpu_slot_ino(cpu_id, CpuSlot::Possible.to_u64()),
                    "possible",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    cpu_slot_ino(cpu_id, CpuSlot::Present.to_u64()),
                    "present",
                    FileType::Regular,
                ),
                mk_dir_entry(
                    cpu_slot_ino(cpu_id, CpuSlot::TopoDir.to_u64()),
                    "topology",
                    FileType::Directory,
                ),
            ],
        }
    }
}
// ─── 名字 → slot 查表 ────────────────────────────────────────

fn block_slot_by_name(name: &str) -> Option<BlockDevSlot> {
    Some(match name {
        "size" => BlockDevSlot::Size,
        "ro" => BlockDevSlot::Ro,
        "removable" => BlockDevSlot::Removable,
        "dev" => BlockDevSlot::Dev,
        "range" => BlockDevSlot::Range,
        "queue" => BlockDevSlot::QueueDir,
        "holders" => BlockDevSlot::Holders,
        "stat" => BlockDevSlot::Stat,
        "inflight" => BlockDevSlot::Inflight,
        "periodic" => BlockDevSlot::Periodic,
        _ => return None,
    })
}

fn block_queue_slot_by_name(name: &str) -> Option<BlockQueueSlot> {
    Some(match name {
        "logical_block_size" => BlockQueueSlot::Lbs,
        "physical_block_size" => BlockQueueSlot::Pbs,
        "rotational" => BlockQueueSlot::Rotational,
        "nr_requests" => BlockQueueSlot::NrRequests,
        "hw_sector_size" => BlockQueueSlot::HwSectorSize,
        "discard_zeroes_data" => BlockQueueSlot::DiscardZeroes,
        _ => return None,
    })
}

fn device_slot_by_name(name: &str) -> Option<DeviceSlot> {
    Some(match name {
        "name" => DeviceSlot::Name,
        "dev" => DeviceSlot::Dev,
        "driver" => DeviceSlot::Driver,
        "subsystem" => DeviceSlot::Subsystem,
        "power" => DeviceSlot::PwrDir,
        _ => return None,
    })
}

fn dev_char_inner_slot_by_name(name: &str) -> Option<DevCharInnerSlot> {
    Some(match name {
        "dev" => DevCharInnerSlot::Dev,
        "device" => DevCharInnerSlot::DeviceLink,
        "subsystem" => DevCharInnerSlot::SubsystemLink,
        "uevent" => DevCharInnerSlot::Uevent,
        _ => return None,
    })
}

// ─── 根 inode 工厂 ───────────────────────────────────────────

fn build_root_inode(fs_id: FsId, weak_sb: &Weak<Superblock>, snap: Arc<SysSnapshot>) -> Arc<Inode> {
    let ops: Arc<dyn InodeOps + Send + Sync> = Arc::new(SysDirInodeOps {
        kind: SysDirKind::Root,
        fs_id,
        weak_sb: weak_sb.clone(),
        snap,
    });
    Inode::new(
        InodeId {
            fs_id,
            ino: ROOT_INO,
        },
        FileType::Directory,
        DevId::new(0, 0),
        4096,
        None,
        inode_meta(0o555, 2, timespec_now()),
        ops,
        weak_sb.clone(),
    )
}

// 显式标注 arc 克隆以保留弱引用别名检查
#[allow(dead_code)]
fn _arc_clone<T>(a: &Arc<T>) -> Arc<T> {
    Arc::clone(a)
}
