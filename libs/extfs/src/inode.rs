//! ext2/3/4 inode 加载/写回 + VFS `InodeOps` 实现。
//!
//! 运行时每个 `Inode` 持有一个 `Spinlock<RawInode>`(完整磁盘字节),所有写
//! 路径先改内存副本,调用 [`FsState::publish_inode_write`] 发布,最后同步到 VFS
//! [`vfs::inode::Inode`] 的镜像字段(`size`/`nlink`/...)。
//!
//! 写路径统一的"降级策略":改写 extent 文件时必须保留仍在文件尺寸内的
//! 数据映射，再转换成间接块布局；只有截断到零时才允许释放整棵 extent 树。
//! 转换完成后的扩容和部分截断统一走 [`map_wr`]。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use vfs::cred::{Credentials, Gid, Uid};
use vfs::error::{VfsError, VfsResult};
use vfs::file::{FileOps, OpenOptions};
use vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use vfs::stat::{DevId, FileMode, FileType, Timespec};
use vfs::sync::{Spinlock, SpinlockGuard};

use crate::inode_wr::{RawInode, read_raw};
use crate::layout::*;
use crate::state::{BlockBackendError, FsState, map_err};
use crate::{alloc_mod, dir_wr, extent_wr, map_wr};

const I_BLOCK_BYTES: usize = 60;

fn inode_has_extra_time_field(raw: &[u8], extra_offset: usize) -> bool {
    if raw.len() < 0x82 || raw.len() < extra_offset + 4 {
        return false;
    }
    let extra_isize = u16::from_le_bytes([raw[0x80], raw[0x81]]) as usize;
    extra_offset + 4 <= 0x80 + extra_isize
}

fn parse_inode_time(raw: &[u8], base_offset: usize, extra_offset: usize) -> Timespec {
    let base = i32::from_le_bytes(raw[base_offset..base_offset + 4].try_into().unwrap()) as i64;
    if !inode_has_extra_time_field(raw, extra_offset) {
        return Timespec {
            secs: base,
            nsecs: 0,
        };
    }
    let extra = u32::from_le_bytes(raw[extra_offset..extra_offset + 4].try_into().unwrap());
    Timespec {
        secs: base + (((extra & 0x3) as i64) << 32),
        nsecs: (extra >> 2).min(999_999_999),
    }
}

/// on-disk inode 摘要(由 [`load_inode`] 返回)。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct InodeMetaDisk {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub nlink: u16,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub flags: u32,
    pub blocks_512: u64,
    pub file_acl_hi: u32,
    /// `i_file_acl` 低 32 位（xattr 块号）。
    pub file_acl_lo: u32,
    pub generation: u32,
}

pub(crate) fn file_type_from_mode(mode: u16) -> FileType {
    match mode & S_IFMT {
        S_IFDIR => FileType::Directory,
        S_IFREG => FileType::Regular,
        S_IFLNK => FileType::Symlink,
        S_IFCHR => FileType::CharDevice,
        S_IFBLK => FileType::BlockDevice,
        S_IFIFO => FileType::Fifo,
        S_IFSOCK => FileType::Socket,
        _ => FileType::Regular,
    }
}

/// 返回 ext 族文件系统中特殊文件对应的 inode 类型位和目录项类型。
pub(crate) fn special_file_layout(kind: FileType) -> Option<(u16, u8)> {
    match kind {
        FileType::CharDevice => Some((S_IFCHR, DT_CHR)),
        FileType::BlockDevice => Some((S_IFBLK, DT_BLK)),
        FileType::Fifo => Some((S_IFIFO, DT_FIFO)),
        FileType::Socket => Some((S_IFSOCK, DT_SOCK)),
        FileType::Regular | FileType::Directory | FileType::Symlink => None,
    }
}

pub(crate) fn should_truncate_on_open(kind: FileType, opts: &OpenOptions) -> bool {
    opts.truncate && kind == FileType::Regular
}

/// 按 ext2/3/4 的 `i_block[0..2]` 约定编码设备号。
pub(crate) fn encode_special_device(dev: DevId) -> VfsResult<(u32, u32)> {
    if dev.major > 0x0fff || dev.minor > 0x0f_ffff {
        return Err(VfsError::InvalidArgument);
    }
    if dev.major < 256 && dev.minor < 256 {
        return Ok(((dev.major << 8) | dev.minor, 0));
    }
    let encoded = (dev.minor & 0xff) | (dev.major << 8) | ((dev.minor & !0xff) << 12);
    Ok((0, encoded))
}

/// 解码 ext2/3/4 存储在 `i_block[0..2]` 中的设备号。
pub(crate) fn decode_special_device(old: u32, new: u32) -> DevId {
    if old != 0 {
        return DevId::new((old >> 8) & 0xff, old & 0xff);
    }
    DevId::new(
        (new & 0x000f_ff00) >> 8,
        (new & 0xff) | ((new >> 12) & 0x000f_ff00),
    )
}

fn parse_inode_meta(raw: &[u8], huge_file: bool, block_size: u32) -> InodeMetaDisk {
    let mode = u16::from_le_bytes([raw[0], raw[1]]);
    let uid_lo = u16::from_le_bytes([raw[2], raw[3]]) as u32;
    let size_lo = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
    let gid_lo = u16::from_le_bytes([raw[24], raw[25]]) as u32;
    let nlink = u16::from_le_bytes([raw[26], raw[27]]);
    let blocks_lo = u32::from_le_bytes([raw[28], raw[29], raw[30], raw[31]]);
    let flags = u32::from_le_bytes([raw[32], raw[33], raw[34], raw[35]]);
    let size_hi = u32::from_le_bytes([raw[108], raw[109], raw[110], raw[111]]);
    let file_acl_lo = u32::from_le_bytes([raw[104], raw[105], raw[106], raw[107]]);
    let file_acl_hi = u16::from_le_bytes([raw[120], raw[121]]) as u32;
    let uid_hi = u16::from_le_bytes([raw[0x74], raw[0x75]]) as u32;
    let gid_hi = u16::from_le_bytes([raw[0x72], raw[0x73]]) as u32;
    // HUGE_FILE:带 EXT4_HUGE_FILE_FL 的 inode,其 i_blocks 以文件系统块(而
    // 非 512 字节扇区)计数(对齐 Linux `ext4_inode_blocks`)。
    let blocks_512 = if huge_file && flags & EXT4_HUGE_FILE_FL != 0 {
        blocks_lo as u64 * (block_size as u64 / 512)
    } else {
        blocks_lo as u64
    };
    InodeMetaDisk {
        mode,
        uid: (uid_hi << 16) | uid_lo,
        gid: (gid_hi << 16) | gid_lo,
        size: ((size_hi as u64) << 32) | size_lo as u64,
        nlink,
        atime: parse_inode_time(raw, 8, 0x8c),
        mtime: parse_inode_time(raw, 16, 0x88),
        ctime: parse_inode_time(raw, 12, 0x84),
        flags,
        blocks_512,
        file_acl_hi,
        file_acl_lo,
        generation: u32::from_le_bytes([raw[100], raw[101], raw[102], raw[103]]),
    }
}

/// inode 读侧 METADATA_CSUM 校验(Linux `ext4_inode_csum_verify`)。
///
/// 与写回路径(`inode_wr::write_raw`)的算法完全镜像:seed 链
/// `csum_seed ‖ ino ‖ generation`,校验域(0x7c 及可选的 0x82)清零后
/// 对整个 inode 求 crc32c。128 字节 inode 只有低位 16 位。
fn verify_inode_csum(state: &FsState, raw: &[u8], ino: u32) -> Result<(), BlockBackendError> {
    if !state.ext_sb.metadata_csum {
        return Ok(());
    }
    let inode_size = state.ext_sb.inode_size as usize;
    if raw.len() < inode_size || inode_size < 128 {
        return Err(BlockBackendError::Io);
    }
    let has_hi = inode_size >= 0x84 && (u16::from_le_bytes([raw[0x80], raw[0x81]]) as usize) >= 4;
    let provided_lo = u16::from_le_bytes([raw[0x7c], raw[0x7d]]);
    let provided_hi = if has_hi {
        u16::from_le_bytes([raw[0x82], raw[0x83]])
    } else {
        0
    };
    let mut bytes = alloc::vec::Vec::from(&raw[..inode_size]);
    bytes[0x7c] = 0;
    bytes[0x7d] = 0;
    if has_hi {
        bytes[0x82] = 0;
        bytes[0x83] = 0;
    }
    let generation = u32::from_le_bytes([raw[100], raw[101], raw[102], raw[103]]);
    let mut seed = state.ext_sb.csum_seed;
    seed = crate::crc::update(seed, &ino.to_le_bytes());
    seed = crate::crc::update(seed, &generation.to_le_bytes());
    let sum = crate::crc::update(seed, &bytes);
    if (sum & 0xffff) as u16 != provided_lo {
        return Err(BlockBackendError::Io);
    }
    if has_hi && ((sum >> 16) & 0xffff) as u16 != provided_hi {
        return Err(BlockBackendError::Io);
    }
    Ok(())
}

/// 旧接口 —— mount 入口仍在用;只读 mount 的路径。
pub(crate) fn load_inode(
    state: &FsState,
    ino: u32,
) -> Result<(InodeMetaDisk, Vec<u8>), BlockBackendError> {
    let raw = read_raw(state, ino)?;
    verify_inode_csum(state, &raw.bytes, ino)?;
    let huge_file = state.ext_sb.feature_ro_compat & RO_COMPAT_HUGE_FILE != 0;
    Ok((
        parse_inode_meta(&raw.bytes, huge_file, state.ext_sb.block_size),
        raw.bytes,
    ))
}

#[inline]
pub(crate) fn i_block_slice(raw: &[u8]) -> &[u8] {
    &raw[0x28..0x28 + I_BLOCK_BYTES]
}

/// 复制 inode 的 `i_block[0..60]` 到栈上固定数组,避免元数据热路径反复分配小 `Vec`。
#[inline]
fn copy_i_block(i_block: &[u8]) -> [u8; I_BLOCK_BYTES] {
    let mut out = [0u8; I_BLOCK_BYTES];
    out.copy_from_slice(i_block);
    out
}

/// 获取 inode 原始状态锁；等待时让出 CPU，避免单核上纯自旋饿死持锁 I/O 任务。
pub(crate) fn lock_raw(raw: &Spinlock<RawInode>) -> SpinlockGuard<'_, RawInode> {
    loop {
        if let Some(guard) = raw.try_lock() {
            return guard;
        }
        if sched::is_ready() {
            sched::poll_urgent_work();
            sched::schedule_once(sched::now_ns_public());
        } else {
            core::hint::spin_loop();
        }
    }
}

/// 当 `EXT4_INLINE_DATA_FL` 启用时,尝试读出内联数据。
pub(crate) fn try_inline_data(
    state: &FsState,
    raw: &[u8],
    size: u64,
    _flags: u32,
) -> Option<Vec<u8>> {
    let ib = i_block_slice(raw);
    let head_len = (size.min(60)) as usize;
    let mut out = alloc::vec::Vec::with_capacity(size as usize);
    out.extend_from_slice(&ib[..head_len]);
    if size <= 60 {
        return Some(out);
    }
    let inode_size = state.ext_sb.inode_size as usize;
    if raw.len() < inode_size || inode_size < 128 {
        return Some(out);
    }
    let i_extra_isize = u16::from_le_bytes([raw[0x80], raw[0x81]]) as usize;
    let xattr_start = 128 + i_extra_isize;
    if xattr_start + 4 > inode_size {
        return Some(out);
    }
    let magic = u32::from_le_bytes([
        raw[xattr_start],
        raw[xattr_start + 1],
        raw[xattr_start + 2],
        raw[xattr_start + 3],
    ]);
    if magic != 0xea020000 {
        return Some(out);
    }
    let mut pos = xattr_start + 4;
    while pos + 16 <= inode_size {
        let name_len = raw[pos] as usize;
        if name_len == 0 {
            break;
        }
        let name_index = raw[pos + 1];
        let value_offs = u16::from_le_bytes([raw[pos + 2], raw[pos + 3]]) as usize;
        let value_size =
            u32::from_le_bytes([raw[pos + 8], raw[pos + 9], raw[pos + 10], raw[pos + 11]]) as usize;
        let name_off = pos + 16;
        if name_off + name_len > inode_size {
            break;
        }
        let name = &raw[name_off..name_off + name_len];
        if name_index == 7 && name == b"data" {
            let v_start = xattr_start + value_offs;
            let v_end = v_start + value_size;
            if v_end <= inode_size {
                let remain = (size as usize).saturating_sub(60);
                let take = remain.min(value_size);
                out.extend_from_slice(&raw[v_start..v_start + take]);
            }
            break;
        }
        let step = 16 + ((name_len + 3) & !3);
        pos += step;
    }
    Some(out)
}

// ── ExtInodeOps ─────────────────────────────────────────────────────────

/// 所有类型 inode 共用一个 `ExtInodeOps`。
pub struct ExtInodeOps {
    pub(crate) state: Arc<FsState>,
    pub(crate) ino: u32,
    /// 原始 inode 字节 + 运行时修改的 Spinlock。写路径先改这里再写回磁盘。
    ///
    /// 这里必须是 `Arc`：普通文件打开后会生成 `ExtRegFileOps`，该打开文件
    /// 可能在 `unlink()` 后继续读写。VFS 会把 nlink=0 的 inode 从
    /// superblock cache 中摘掉，因此打开文件不能再依赖 `sb.find_inode()` 找回
    /// 状态，否则会破坏 Linux 的 unlink-but-open 语义。
    pub(crate) raw: Arc<Spinlock<RawInode>>,
    /// 普通文件块映射的 inode-local 代际。
    ///
    /// 多个打开句柄各自持有映射缓存；间接块内补洞时 `flags/size/i_block`
    /// 可能都不变化，因此需要共享代际通知其它句柄丢弃旧映射。该代际只由
    /// 本 inode 的映射修改推进，不受其它 inode 元数据写入影响。
    mapping_generation: Arc<AtomicU64>,
    /// 命名 FIFO 的运行时数据通道。
    ///
    /// ext 磁盘格式只保存 FIFO inode 类型，缓冲区和打开端点属于内存态；同一
    /// inode 的所有打开文件共享此对象，最后一个 inode 引用释放后自动销毁。
    fifo: Option<Arc<vfs::pipe::Pipe>>,
}

impl ExtInodeOps {
    pub(crate) fn new(state: Arc<FsState>, ino: u32, bytes: Vec<u8>) -> Self {
        let kind = file_type_from_mode(parse_inode_meta(&bytes, false, 4096).mode);
        Self {
            state,
            ino,
            raw: Arc::new(Spinlock::new(RawInode::new(ino, bytes))),
            mapping_generation: Arc::new(AtomicU64::new(0)),
            fifo: (kind == FileType::Fifo).then(vfs::pipe::new_fifo),
        }
    }

    #[inline]
    fn bump_mapping_generation(&self) {
        self.mapping_generation.fetch_add(1, Ordering::AcqRel);
    }

    fn snapshot_meta(&self) -> InodeMetaDisk {
        let g = lock_raw(&self.raw);
        let huge_file = self.state.ext_sb.feature_ro_compat & RO_COMPAT_HUGE_FILE != 0;
        parse_inode_meta(&g.bytes, huge_file, self.state.ext_sb.block_size)
    }

    fn snapshot_i_block(&self) -> [u8; I_BLOCK_BYTES] {
        let g = lock_raw(&self.raw);
        copy_i_block(i_block_slice(&g.bytes))
    }

    #[allow(dead_code)]
    fn snapshot_flags(&self) -> u32 {
        lock_raw(&self.raw).flags()
    }

    fn snapshot_all(&self) -> (u32, u64, [u8; I_BLOCK_BYTES]) {
        let g = lock_raw(&self.raw);
        (g.flags(), g.size(), copy_i_block(i_block_slice(&g.bytes)))
    }

    fn check_writable(&self) -> VfsResult<()> {
        if self.state.is_read_only() {
            Err(VfsError::ReadOnlyFilesystem)
        } else {
            Ok(())
        }
    }
}

// ── 核心:构造/查找内存 Inode ───────────────────────────────────────────

fn build_inode_for(
    state: &Arc<FsState>,
    sb: &Arc<vfs::superblock::Superblock>,
    ino: u32,
) -> VfsResult<Arc<Inode>> {
    if let Some(existing) = sb.find_inode(ino as u64) {
        return Ok(existing);
    }
    let (meta, raw) = load_inode(state, ino).map_err(map_err)?;
    let kind = file_type_from_mode(meta.mode);
    let rdev = if matches!(kind, FileType::CharDevice | FileType::BlockDevice) {
        let block = i_block_slice(&raw);
        let old = u32::from_le_bytes(block[0..4].try_into().unwrap());
        let new = u32::from_le_bytes(block[4..8].try_into().unwrap());
        decode_special_device(old, new)
    } else {
        DevId::new(0, 0)
    };
    let mode = FileMode::new((meta.mode & 0o7777) as u16);
    let vmeta = InodeMeta {
        size: meta.size,
        nlink: meta.nlink as u32,
        mode,
        uid: Uid(meta.uid),
        gid: Gid(meta.gid),
        atime: meta.atime,
        mtime: meta.mtime,
        ctime: meta.ctime,
        blocks: meta.blocks_512,
    };
    let block_size = state.ext_sb.block_size;
    let has_xattr_block = meta.file_acl_lo != 0 || meta.file_acl_hi != 0;
    let ops = ExtInodeOps::new(Arc::clone(state), ino, raw);
    let inode = Inode::new(
        InodeId {
            fs_id: sb.fs_id,
            ino: ino as u64,
        },
        kind,
        rdev,
        block_size,
        None,
        vmeta,
        Arc::new(ops) as Arc<dyn InodeOps + Send + Sync>,
        sb.self_weak.clone(),
    );
    if has_xattr_block {
        // xattr 快速路径提示：ACL 强制需要知道 inode 可能携带属性。
        inode.mark_has_xattrs();
    }
    Ok(sb.insert_inode(inode))
}

/// 新建一个 inode 记录(分配 ino + 初始化 mode/uid/gid/nlink + 写回磁盘),
/// 返回 `(ino, RawInode 副本)`。不管父目录的 entry 插入 —— 由调用方处理。
fn create_disk_inode(
    state: &FsState,
    mode: u16,
    uid: u32,
    gid: u32,
    is_dir: bool,
    initial_nlink: u16,
) -> Result<RawInode, BlockBackendError> {
    let ino = alloc_mod::alloc_inode(state, is_dir)?;
    let mut raw = RawInode::new(ino, alloc::vec![0u8; state.ext_sb.inode_size as usize]);
    // 若 inode_size >= 256，要先声明 extra 区域，再编码纳秒时间字段。
    if state.ext_sb.inode_size >= 256 {
        raw.bytes[0x80..0x82].copy_from_slice(&32u16.to_le_bytes());
    }
    raw.set_mode(mode);
    raw.set_uid(uid);
    raw.set_gid(gid);
    raw.set_nlink(initial_nlink);
    raw.set_size(0);
    raw.set_blocks_lo(0);
    if mode & S_IFMT == S_IFREG && state.ext_sb.feature_incompat & INCOMPAT_EXTENTS != 0 {
        raw.set_flags(EXT4_EXTENTS_FL);
        extent_wr::init_empty_root(raw.i_block_mut());
    } else {
        raw.set_flags(0);
    }
    let now = Timespec::now();
    raw.set_atime(now);
    raw.set_mtime(now);
    raw.set_ctime(now);
    state.publish_inode_write(&raw)?;
    Ok(raw)
}

pub(crate) fn touch_content_times(raw: &mut RawInode, now: Timespec) {
    raw.set_mtime(now);
    raw.set_ctime(now);
}

fn touch_change_time(raw: &mut RawInode, now: Timespec) {
    raw.set_ctime(now);
}

pub(crate) fn sync_vfs_meta(state: &FsState, inode: &Inode, raw: &RawInode) {
    let huge_file = state.ext_sb.feature_ro_compat & RO_COMPAT_HUGE_FILE != 0;
    let meta = parse_inode_meta(&raw.bytes, huge_file, state.ext_sb.block_size);
    inode.refresh_meta_from_fs(InodeMeta {
        size: meta.size,
        nlink: meta.nlink as u32,
        mode: FileMode::new((meta.mode & 0o7777) as u16),
        uid: Uid(meta.uid),
        gid: Gid(meta.gid),
        atime: meta.atime,
        mtime: meta.mtime,
        ctime: meta.ctime,
        blocks: meta.blocks_512,
    });
}

/// ext4_inc_count 的等价物:目录 nlink 达到 [`EXT4_LINK_MAX`] 且启用
/// DIR_NLINK 时固定为 1(表示"真实计数未知");曾经为 1 的保持为 1。
fn inc_nlink(state: &FsState, raw: &mut RawInode, is_dir: bool) {
    let dir_nlink = state.ext_sb.feature_ro_compat & RO_COMPAT_DIR_NLINK != 0;
    let cur = raw.nlink();
    let next = cur as u32 + 1;
    if is_dir && dir_nlink && (next > EXT4_LINK_MAX as u32 || (cur == 1 && next == 2)) {
        raw.set_nlink(1);
    } else {
        raw.set_nlink(next as u16);
    }
}

/// ext4_dec_count 的等价物:目录 nlink 为 1(溢出态)或 2(最小值)时不动。
fn dec_nlink(raw: &mut RawInode, is_dir: bool) {
    let cur = raw.nlink();
    if !is_dir || cur > 2 {
        raw.set_nlink(cur.saturating_sub(1));
    }
}

pub(crate) fn blocks_lo_from_mapping(
    state: &FsState,
    flags: u32,
    i_block: &[u8],
) -> Result<u32, BlockBackendError> {
    let fs_blocks = if flags & EXT4_EXTENTS_FL != 0 {
        extent_wr::count_tree_blocks(state, i_block)?
    } else {
        map_wr::count_all_blocks(state, i_block)?
    };
    let sectors_per_block = (state.ext_sb.block_size / 512) as u64;
    let sectors = fs_blocks
        .checked_mul(sectors_per_block)
        .ok_or(BlockBackendError::OutOfRange)?;
    if sectors > u32::MAX as u64 {
        return Err(BlockBackendError::OutOfRange);
    }
    Ok(sectors as u32)
}

fn refresh_blocks_lo(state: &FsState, raw: &mut RawInode) -> Result<(), BlockBackendError> {
    let blocks = blocks_lo_from_mapping(state, raw.flags(), raw.i_block())?;
    raw.set_blocks_lo(blocks);
    Ok(())
}

fn clear_deleted_inode(raw: &mut RawInode) {
    // inode bitmap 清掉后,宿主 e2fsck 仍会扫描 inode 表。若保留 regular/dir 的
    // i_mode,会被当成残留 orphan inode;清整 inode 只留下非零 dtime 表示已删除。
    //
    // dtime 不能写 1: e2fsck 会把过小的删除时间解释成损坏的 orphan 链痕迹。
    // extfs 作为独立库无法直接访问内核 realtime,这里写一个稳定的合法 epoch 秒。
    raw.bytes.fill(0);
    raw.set_dtime(1_700_000_000);
}

impl InodeOps for ExtInodeOps {
    fn getxattr(&self, name: &[u8]) -> VfsResult<Option<Vec<u8>>> {
        let acl_block = lock_raw(&self.raw).file_acl();
        crate::xattr::get(&self.state, acl_block, name)
    }

    fn setxattr(&self, name: &[u8], value: &[u8], flags: u32) -> VfsResult<()> {
        let mut raw = lock_raw(&self.raw);
        let acl_block = raw.file_acl();
        let (new_block, block) = crate::xattr::set(&self.state, acl_block, name, value, flags)?;
        if new_block == 0 {
            // 首个属性：分配 xattr 块。
            let block_no = crate::alloc_mod::alloc_block(&self.state).map_err(|_| VfsError::Io)?;
            raw.set_file_acl(block_no);
            self.state
                .write_data_blocks(block_no, 1, &block)
                .map_err(|_| VfsError::Io)?;
        } else {
            self.state
                .write_data_blocks(new_block, 1, &block)
                .map_err(|_| VfsError::Io)?;
        }
        self.state
            .publish_inode_write(&raw)
            .map_err(|_| VfsError::Io)?;
        Ok(())
    }

    fn listxattr(&self) -> VfsResult<Vec<Vec<u8>>> {
        let acl_block = lock_raw(&self.raw).file_acl();
        crate::xattr::list(&self.state, acl_block)
    }

    fn removexattr(&self, name: &[u8]) -> VfsResult<()> {
        let mut raw = lock_raw(&self.raw);
        let acl_block = raw.file_acl();
        let (new_block, block) = crate::xattr::remove(&self.state, acl_block, name)?;
        if new_block == 0 {
            // 最后一个属性被删除：释放 xattr 块并清 i_file_acl。
            if acl_block != 0 {
                crate::alloc_mod::free_block(&self.state, acl_block).map_err(|_| VfsError::Io)?;
            }
            raw.set_file_acl(0);
            self.state
                .publish_inode_write(&raw)
                .map_err(|_| VfsError::Io)?;
        } else {
            self.state
                .write_data_blocks(new_block, 1, &block)
                .map_err(|_| VfsError::Io)?;
        }
        Ok(())
    }
    fn supports_private_page_cache(&self) -> bool {
        true
    }

    fn lookup(&self, inode: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        let meta = self.snapshot_meta();
        if file_type_from_mode(meta.mode) != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        let i_block = self.snapshot_i_block();
        let casefold = meta.flags & EXT4_CASEFOLD_FL != 0;
        let csum_ctx = Some((self.ino, meta.generation));
        if let Some(e) = crate::dir::find_entry(
            &self.state,
            &i_block,
            meta.flags,
            meta.size,
            name,
            csum_ctx,
            casefold,
        )
        .map_err(map_err)?
        {
            let sb = inode.superblock().ok_or(VfsError::InvalidArgument)?;
            return build_inode_for(&self.state, &sb, e.ino);
        }
        Err(VfsError::NotFound)
    }

    fn create(
        &self,
        inode: &Inode,
        name: &str,
        mode: FileMode,
        cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        self.check_writable()?;
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(VfsError::InvalidArgument);
        }
        // 先 lookup 确认未重名
        if self.lookup(inode, name).is_ok() {
            return Err(VfsError::AlreadyExists);
        }
        // 新 inode:S_IFREG | perm
        let full_mode = (S_IFREG) | (mode.bits() & 0o7777);
        let new_raw =
            create_disk_inode(&self.state, full_mode, cred.fsuid.0, cred.fsgid.0, false, 1)
                .map_err(map_err)?;

        // 在父目录插 entry
        let mut parent = lock_raw(&self.raw);
        let mut i_block = copy_i_block(parent.i_block());
        let mut pflags = parent.flags();
        let new_size = dir_wr::insert_entry(
            &self.state,
            self.ino,
            parent.generation(),
            &mut i_block,
            &mut pflags,
            parent.size(),
            new_raw.ino,
            DT_REG,
            name,
        )
        .map_err(map_err)?;
        parent.i_block_mut().copy_from_slice(&i_block);
        parent.set_flags(pflags);
        parent.set_size(new_size);
        touch_content_times(&mut parent, Timespec::now());
        refresh_blocks_lo(&self.state, &mut parent).map_err(map_err)?;
        self.state.publish_inode_write(&parent).map_err(map_err)?;
        sync_vfs_meta(&self.state, inode, &parent);
        drop(parent);

        let sb = inode.superblock().ok_or(VfsError::InvalidArgument)?;
        build_inode_for(&self.state, &sb, new_raw.ino)
    }

    fn mkdir(
        &self,
        inode: &Inode,
        name: &str,
        mode: FileMode,
        cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        self.check_writable()?;
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(VfsError::InvalidArgument);
        }
        if self.lookup(inode, name).is_ok() {
            return Err(VfsError::AlreadyExists);
        }
        // 父目录 nlink 溢出检查(无 DIR_NLINK 特性时上限 65000)。
        {
            let parent = lock_raw(&self.raw);
            let dir_nlink = self.state.ext_sb.feature_ro_compat & RO_COMPAT_DIR_NLINK != 0;
            if !dir_nlink && parent.nlink() >= EXT4_LINK_MAX {
                return Err(VfsError::TooManyLinks);
            }
            drop(parent);
        }
        // 新目录:S_IFDIR | perm,nlink=2("." 指自己),parent 的 nlink += 1
        let full_mode = S_IFDIR | (mode.bits() & 0o7777);
        let mut new_raw =
            create_disk_inode(&self.state, full_mode, cred.fsuid.0, cred.fsgid.0, true, 2)
                .map_err(map_err)?;

        // 给新目录分配第一个块,写入 "." / ".."
        let block = alloc_mod::alloc_block(&self.state).map_err(map_err)?;
        let has_dir_tail = self.state.ext_sb.metadata_csum
            && self.state.ext_sb.feature_incompat & INCOMPAT_FILETYPE != 0;
        let mut init_blk = dir_wr::make_init_dir_block(
            self.state.ext_sb.block_size,
            new_raw.ino,
            self.ino,
            self.state.ext_sb.feature_incompat & INCOMPAT_FILETYPE != 0,
            has_dir_tail,
        );
        dir_wr::finish_dir_block(
            &self.state,
            new_raw.ino,
            new_raw.generation(),
            &mut init_blk,
        )
        .map_err(map_err)?;
        self.state.write_block(block, &init_blk).map_err(map_err)?;
        // 把 block 指针写到 new_raw.i_block[0]
        new_raw.i_block_mut()[0..4].copy_from_slice(&(block as u32).to_le_bytes());
        new_raw.set_size(self.state.ext_sb.block_size as u64);
        new_raw.set_blocks_lo((self.state.ext_sb.block_size / 512) as u32);
        self.state.publish_inode_write(&new_raw).map_err(map_err)?;

        // 父目录:插 entry + nlink++
        let mut parent = lock_raw(&self.raw);
        let mut pi_block = copy_i_block(parent.i_block());
        let mut pflags = parent.flags();
        let new_size = dir_wr::insert_entry(
            &self.state,
            self.ino,
            parent.generation(),
            &mut pi_block,
            &mut pflags,
            parent.size(),
            new_raw.ino,
            DT_DIR,
            name,
        )
        .map_err(map_err)?;
        parent.i_block_mut().copy_from_slice(&pi_block);
        parent.set_flags(pflags);
        parent.set_size(new_size);
        inc_nlink(&self.state, &mut parent, true);
        touch_content_times(&mut parent, Timespec::now());
        refresh_blocks_lo(&self.state, &mut parent).map_err(map_err)?;
        self.state.publish_inode_write(&parent).map_err(map_err)?;
        sync_vfs_meta(&self.state, inode, &parent);
        drop(parent);

        let sb = inode.superblock().ok_or(VfsError::InvalidArgument)?;
        build_inode_for(&self.state, &sb, new_raw.ino)
    }

    fn unlink(&self, inode: &Inode, name: &str, child: &Inode) -> VfsResult<()> {
        self.check_writable()?;
        let _ = child;
        let target = self.lookup(inode, name)?;
        if target.kind() == FileType::Directory {
            return Err(VfsError::IsADirectory);
        }
        // 1) 从父目录移除 entry
        let mut parent = lock_raw(&self.raw);
        let pi_block = copy_i_block(parent.i_block());
        let mut pflags = parent.flags();
        let ok = dir_wr::remove_entry(
            &self.state,
            self.ino,
            parent.generation(),
            &pi_block,
            &mut pflags,
            parent.size(),
            name,
        )
        .map_err(map_err)?;
        if !ok {
            return Err(VfsError::NotFound);
        }
        parent.i_block_mut().copy_from_slice(&pi_block);
        parent.set_flags(pflags);
        touch_content_times(&mut parent, Timespec::now());
        refresh_blocks_lo(&self.state, &mut parent).map_err(map_err)?;
        self.state.publish_inode_write(&parent).map_err(map_err)?;
        sync_vfs_meta(&self.state, inode, &parent);
        drop(parent);

        // 2) 目标 inode 的 nlink--。nlink 到 0 时只写回元数据，不在 unlink
        // 现场释放块和 inode；真正回收由 VFS 在最后一个引用释放时调用 evict()。
        // 这样打开但已 unlink 的文件仍可继续 I/O，符合 POSIX/Linux 语义。
        let t_ops = target
            .downcast_ops::<ExtInodeOps>()
            .ok_or(VfsError::InvalidArgument)?;
        let mut traw = lock_raw(&t_ops.raw);
        dec_nlink(&mut traw, false);
        touch_change_time(&mut traw, Timespec::now());
        self.state.publish_inode_write(&traw).map_err(map_err)?;
        sync_vfs_meta(&self.state, &target, &traw);
        drop(traw);
        Ok(())
    }

    fn rmdir(&self, inode: &Inode, name: &str, child: &Inode) -> VfsResult<()> {
        self.check_writable()?;
        let _ = child;
        let target = self.lookup(inode, name)?;
        if target.kind() != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        // 必须为空目录(除 "." ".." 外没别的 entry)
        let t_ops = target
            .downcast_ops::<ExtInodeOps>()
            .ok_or(VfsError::InvalidArgument)?;
        {
            let traw = lock_raw(&t_ops.raw);
            let tib = copy_i_block(i_block_slice(&traw.bytes));
            if !crate::dir::is_dir_empty(
                &self.state,
                &tib,
                traw.flags(),
                traw.size(),
                Some((t_ops.ino, traw.generation())),
            )
            .map_err(map_err)?
            {
                return Err(VfsError::DirectoryNotEmpty);
            }
        }
        // 从父目录移除
        let mut parent = lock_raw(&self.raw);
        let pib = copy_i_block(parent.i_block());
        let mut pflags = parent.flags();
        dir_wr::remove_entry(
            &self.state,
            self.ino,
            parent.generation(),
            &pib,
            &mut pflags,
            parent.size(),
            name,
        )
        .map_err(map_err)?;
        parent.i_block_mut().copy_from_slice(&pib);
        parent.set_flags(pflags);
        dec_nlink(&mut parent, true);
        touch_content_times(&mut parent, Timespec::now());
        refresh_blocks_lo(&self.state, &mut parent).map_err(map_err)?;
        self.state.publish_inode_write(&parent).map_err(map_err)?;
        sync_vfs_meta(&self.state, inode, &parent);
        drop(parent);

        // 目标目录已从父目录摘除。这里只把目标目录 nlink 置 0 并写回；
        // 数据块和 inode 位图必须等最后一个打开引用释放时由 evict() 回收。
        let mut traw = lock_raw(&t_ops.raw);
        traw.set_nlink(0);
        touch_change_time(&mut traw, Timespec::now());
        self.state.publish_inode_write(&traw).map_err(map_err)?;
        sync_vfs_meta(&self.state, &target, &traw);
        drop(traw);
        Ok(())
    }

    fn symlink(
        &self,
        inode: &Inode,
        name: &str,
        target: &str,
        cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        self.check_writable()?;
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(VfsError::InvalidArgument);
        }
        if self.lookup(inode, name).is_ok() {
            return Err(VfsError::AlreadyExists);
        }
        let full_mode = S_IFLNK | 0o777;
        let mut new_raw =
            create_disk_inode(&self.state, full_mode, cred.fsuid.0, cred.fsgid.0, false, 1)
                .map_err(map_err)?;

        let tbytes = target.as_bytes();
        new_raw.set_size(tbytes.len() as u64);
        if tbytes.len() <= FAST_SYMLINK_MAX {
            // fast symlink
            let ib = new_raw.i_block_mut();
            for b in ib.iter_mut() {
                *b = 0;
            }
            ib[..tbytes.len()].copy_from_slice(tbytes);
        } else {
            // 分配 slow symlink 数据块
            let block = alloc_mod::alloc_block(&self.state).map_err(map_err)?;
            let bs = self.state.ext_sb.block_size as usize;
            let mut blk = alloc::vec![0u8; bs];
            let take = tbytes.len().min(bs);
            blk[..take].copy_from_slice(&tbytes[..take]);
            self.state.write_block(block, &blk).map_err(map_err)?;
            new_raw.i_block_mut()[0..4].copy_from_slice(&(block as u32).to_le_bytes());
            new_raw.set_blocks_lo((self.state.ext_sb.block_size / 512) as u32);
        }
        self.state.publish_inode_write(&new_raw).map_err(map_err)?;

        // 父目录插 entry
        let mut parent = lock_raw(&self.raw);
        let mut pib = copy_i_block(parent.i_block());
        let mut pflags = parent.flags();
        let new_size = dir_wr::insert_entry(
            &self.state,
            self.ino,
            parent.generation(),
            &mut pib,
            &mut pflags,
            parent.size(),
            new_raw.ino,
            DT_LNK,
            name,
        )
        .map_err(map_err)?;
        parent.i_block_mut().copy_from_slice(&pib);
        parent.set_flags(pflags);
        parent.set_size(new_size);
        touch_content_times(&mut parent, Timespec::now());
        refresh_blocks_lo(&self.state, &mut parent).map_err(map_err)?;
        self.state.publish_inode_write(&parent).map_err(map_err)?;
        sync_vfs_meta(&self.state, inode, &parent);
        drop(parent);

        let sb = inode.superblock().ok_or(VfsError::InvalidArgument)?;
        build_inode_for(&self.state, &sb, new_raw.ino)
    }

    fn mknod(
        &self,
        inode: &Inode,
        name: &str,
        kind: FileType,
        mode: FileMode,
        dev: DevId,
        cred: &Credentials,
    ) -> VfsResult<Arc<Inode>> {
        self.check_writable()?;
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(VfsError::InvalidArgument);
        }
        let (type_mode, dir_type) = special_file_layout(kind).ok_or(VfsError::InvalidArgument)?;
        if self.lookup(inode, name).is_ok() {
            return Err(VfsError::AlreadyExists);
        }
        let encoded_device = if matches!(kind, FileType::CharDevice | FileType::BlockDevice) {
            Some(encode_special_device(dev)?)
        } else {
            None
        };

        let full_mode = type_mode | (mode.bits() & 0o7777);
        let mut new_raw =
            create_disk_inode(&self.state, full_mode, cred.fsuid.0, cred.fsgid.0, false, 1)
                .map_err(map_err)?;
        if let Some((old, new)) = encoded_device {
            new_raw.i_block_mut()[0..4].copy_from_slice(&old.to_le_bytes());
            new_raw.i_block_mut()[4..8].copy_from_slice(&new.to_le_bytes());
            self.state.publish_inode_write(&new_raw).map_err(map_err)?;
        }

        let mut parent = lock_raw(&self.raw);
        let mut parent_block = copy_i_block(parent.i_block());
        let mut parent_flags = parent.flags();
        let new_size = dir_wr::insert_entry(
            &self.state,
            self.ino,
            parent.generation(),
            &mut parent_block,
            &mut parent_flags,
            parent.size(),
            new_raw.ino,
            dir_type,
            name,
        )
        .map_err(map_err)?;
        parent.i_block_mut().copy_from_slice(&parent_block);
        parent.set_flags(parent_flags);
        parent.set_size(new_size);
        touch_content_times(&mut parent, Timespec::now());
        refresh_blocks_lo(&self.state, &mut parent).map_err(map_err)?;
        self.state.publish_inode_write(&parent).map_err(map_err)?;
        sync_vfs_meta(&self.state, inode, &parent);
        drop(parent);

        let sb = inode.superblock().ok_or(VfsError::InvalidArgument)?;
        build_inode_for(&self.state, &sb, new_raw.ino)
    }

    fn link(&self, inode: &Inode, target: &Inode, name: &str) -> VfsResult<()> {
        self.check_writable()?;
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(VfsError::InvalidArgument);
        }
        if self.lookup(inode, name).is_ok() {
            return Err(VfsError::AlreadyExists);
        }
        if target.kind() == FileType::Directory {
            return Err(VfsError::IsADirectory);
        }
        let t_ops = target
            .downcast_ops::<ExtInodeOps>()
            .ok_or(VfsError::InvalidArgument)?;
        // nlink++(硬链接数上限 65000,对应 Linux -EMLINK)
        {
            let mut traw = lock_raw(&t_ops.raw);
            if traw.nlink() >= EXT4_LINK_MAX {
                return Err(VfsError::TooManyLinks);
            }
            inc_nlink(&self.state, &mut traw, false);
            touch_change_time(&mut traw, Timespec::now());
            self.state.publish_inode_write(&traw).map_err(map_err)?;
        }
        sync_vfs_meta(&self.state, target, &lock_raw(&t_ops.raw));

        // 父目录插 entry
        let file_type_val = match target.kind() {
            FileType::Regular => DT_REG,
            FileType::Symlink => DT_LNK,
            FileType::CharDevice => DT_CHR,
            FileType::BlockDevice => DT_BLK,
            FileType::Fifo => DT_FIFO,
            FileType::Socket => DT_SOCK,
            FileType::Directory => unreachable!(),
        };
        let mut parent = lock_raw(&self.raw);
        let mut pib = copy_i_block(parent.i_block());
        let mut pflags = parent.flags();
        let new_size = dir_wr::insert_entry(
            &self.state,
            self.ino,
            parent.generation(),
            &mut pib,
            &mut pflags,
            parent.size(),
            t_ops.ino,
            file_type_val,
            name,
        )
        .map_err(map_err)?;
        parent.i_block_mut().copy_from_slice(&pib);
        parent.set_flags(pflags);
        parent.set_size(new_size);
        touch_content_times(&mut parent, Timespec::now());
        refresh_blocks_lo(&self.state, &mut parent).map_err(map_err)?;
        self.state.publish_inode_write(&parent).map_err(map_err)?;
        sync_vfs_meta(&self.state, inode, &parent);
        Ok(())
    }

    fn rename(
        &self,
        inode: &Inode,
        old_name: &str,
        _old_inode: &Inode,
        new_dir: &Inode,
        new_name: &str,
    ) -> VfsResult<()> {
        self.check_writable()?;
        if new_name.is_empty() || new_name == "." || new_name == ".." || new_name.contains('/') {
            return Err(VfsError::InvalidArgument);
        }
        // 原条目
        let entry_target = self.lookup(inode, old_name)?;
        let target_is_dir = entry_target.kind() == FileType::Directory;
        let new_dir_ops = new_dir
            .downcast_ops::<ExtInodeOps>()
            .ok_or(VfsError::CrossDevice)?;
        if !Arc::ptr_eq(&new_dir_ops.state, &self.state) {
            return Err(VfsError::CrossDevice);
        }
        let cross_dir = !core::ptr::eq(self as *const _, new_dir_ops as *const _);

        // 目标名若已存在:非目录覆盖,目录则必须为空
        if let Ok(existing) = new_dir_ops.lookup(new_dir, new_name) {
            if existing.kind() == FileType::Directory {
                // 空目录才允许覆盖
                let eops = existing
                    .downcast_ops::<ExtInodeOps>()
                    .ok_or(VfsError::InvalidArgument)?;
                let traw = lock_raw(&eops.raw);
                let tib = copy_i_block(i_block_slice(&traw.bytes));
                let empty = crate::dir::is_dir_empty(
                    &self.state,
                    &tib,
                    traw.flags(),
                    traw.size(),
                    Some((eops.ino, traw.generation())),
                )
                .map_err(map_err)?;
                drop(traw);
                if !empty {
                    return Err(VfsError::DirectoryNotEmpty);
                }
                if !target_is_dir {
                    // 源不是目录、目标是目录:禁止
                    return Err(VfsError::IsADirectory);
                }
                new_dir_ops.rmdir(new_dir, new_name, &existing)?;
            } else {
                if target_is_dir {
                    return Err(VfsError::NotADirectory);
                }
                new_dir_ops.unlink(new_dir, new_name, &existing)?;
            }
        }

        // 插入新 entry
        let ft = match entry_target.kind() {
            FileType::Regular => DT_REG,
            FileType::Directory => DT_DIR,
            FileType::Symlink => DT_LNK,
            FileType::CharDevice => DT_CHR,
            FileType::BlockDevice => DT_BLK,
            FileType::Fifo => DT_FIFO,
            FileType::Socket => DT_SOCK,
        };
        if cross_dir {
            let mut ndir = lock_raw(&new_dir_ops.raw);
            let mut ib = copy_i_block(ndir.i_block());
            let mut flags = ndir.flags();
            let new_size = dir_wr::insert_entry(
                &self.state,
                new_dir_ops.ino,
                ndir.generation(),
                &mut ib,
                &mut flags,
                ndir.size(),
                entry_target.ino() as u32,
                ft,
                new_name,
            )
            .map_err(map_err)?;
            ndir.i_block_mut().copy_from_slice(&ib);
            ndir.set_flags(flags);
            ndir.set_size(new_size);
            // 目录迁进来 → nlink++(新父目录被子目录 .. 引用)
            if target_is_dir {
                inc_nlink(&self.state, &mut ndir, true);
            }
            touch_content_times(&mut ndir, Timespec::now());
            refresh_blocks_lo(&self.state, &mut ndir).map_err(map_err)?;
            self.state.publish_inode_write(&ndir).map_err(map_err)?;
            sync_vfs_meta(&self.state, new_dir, &ndir);
        }
        // 从源目录移除 old_name 条目;跨目录时顺手减 nlink
        {
            let mut parent = lock_raw(&self.raw);
            let mut ib = copy_i_block(parent.i_block());
            let mut flags = parent.flags();
            dir_wr::remove_entry(
                &self.state,
                self.ino,
                parent.generation(),
                &ib,
                &mut flags,
                parent.size(),
                old_name,
            )
            .map_err(map_err)?;
            // 同目录 rename:就地改名,需要新增 new_name 条目(上面跨目录分支已插过)
            if !cross_dir {
                let new_size = dir_wr::insert_entry(
                    &self.state,
                    self.ino,
                    parent.generation(),
                    &mut ib,
                    &mut flags,
                    parent.size(),
                    entry_target.ino() as u32,
                    ft,
                    new_name,
                )
                .map_err(map_err)?;
                parent.set_size(new_size);
            } else if target_is_dir {
                // 跨目录时,源父目录失去一个子目录 → nlink--
                dec_nlink(&mut parent, true);
            }
            parent.i_block_mut().copy_from_slice(&ib);
            parent.set_flags(flags);
            touch_content_times(&mut parent, Timespec::now());
            refresh_blocks_lo(&self.state, &mut parent).map_err(map_err)?;
            self.state.publish_inode_write(&parent).map_err(map_err)?;
            sync_vfs_meta(&self.state, inode, &parent);
        }

        // 跨目录移动目录:同步子目录的 ".." 到新父目录
        if cross_dir && target_is_dir {
            let tops = entry_target
                .downcast_ops::<ExtInodeOps>()
                .ok_or(VfsError::InvalidArgument)?;
            let mut target_raw = lock_raw(&tops.raw);
            let tib = copy_i_block(target_raw.i_block());
            let mut flags = target_raw.flags();
            dir_wr::update_dotdot(
                &self.state,
                tops.ino,
                target_raw.generation(),
                &tib,
                &mut flags,
                new_dir_ops.ino,
            )
            .map_err(map_err)?;
            target_raw.set_flags(flags);
            self.state
                .publish_inode_write(&target_raw)
                .map_err(map_err)?;
        }
        let target_ops = entry_target
            .downcast_ops::<ExtInodeOps>()
            .ok_or(VfsError::InvalidArgument)?;
        let mut target_raw = lock_raw(&target_ops.raw);
        touch_change_time(&mut target_raw, Timespec::now());
        self.state
            .publish_inode_write(&target_raw)
            .map_err(map_err)?;
        sync_vfs_meta(&self.state, &entry_target, &target_raw);
        Ok(())
    }

    fn truncate(&self, inode: &Inode, new_size: u64) -> VfsResult<()> {
        self.check_writable()?;
        let meta = self.snapshot_meta();
        if file_type_from_mode(meta.mode) != FileType::Regular {
            return Err(VfsError::InvalidArgument);
        }
        // fscrypt 无密钥不可写;fs-verity 已启用校验的文件不可变。
        if meta.flags & EXT4_ENCRYPT_FL != 0 {
            return Err(VfsError::NotSupported);
        }
        if meta.flags & EXT4_VERITY_FL != 0 {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        let mut raw = lock_raw(&self.raw);
        let cur_size = raw.size();
        if new_size == cur_size {
            return Ok(());
        }
        let block_size = self.state.ext_sb.block_size as u64;
        if new_size < cur_size {
            let mut ib = copy_i_block(raw.i_block());
            let mut flags = raw.flags();

            if new_size == 0 {
                if flags & EXT4_EXTENTS_FL != 0 {
                    extent_wr::demote_if_extent(&self.state, &mut flags, &mut ib)
                        .map_err(map_err)?;
                } else {
                    map_wr::free_all_blocks(&self.state, &mut ib).map_err(map_err)?;
                }
                raw.set_blocks_lo(0);
            } else {
                // 部分截断必须保留前缀数据，禁止使用会释放整棵 extent 树的降级路径。
                if !extent_wr::demote_preserve_if_extent(&self.state, &mut flags, &mut ib)
                    .map_err(map_err)?
                {
                    return Err(VfsError::NotSupported);
                }
                // 从第一个超出 new_size 的逻辑块开始全部释放
                let first_free_lb = ((new_size + block_size - 1) / block_size) as u32;
                map_wr::free_blocks_from(&self.state, &mut ib, first_free_lb).map_err(map_err)?;
                // new_size 不是块边界时清零最后保留块的尾部,保证洞读出 0
                let tail = (new_size % block_size) as usize;
                if tail != 0 {
                    let lb = (new_size / block_size) as u32;
                    if let Some(phys) =
                        crate::map::map_block(&self.state, &ib, lb).map_err(map_err)?
                    {
                        let bs = block_size as usize;
                        let mut blk = alloc::vec![0u8; bs];
                        self.state.read_block(phys, &mut blk).map_err(map_err)?;
                        for b in &mut blk[tail..] {
                            *b = 0;
                        }
                        self.state.write_block(phys, &blk).map_err(map_err)?;
                    }
                }
                raw.i_block_mut().copy_from_slice(&ib);
                raw.set_flags(flags);
                refresh_blocks_lo(&self.state, &mut raw).map_err(map_err)?;
            }
            raw.i_block_mut().copy_from_slice(&ib);
            raw.set_flags(flags);
            self.bump_mapping_generation();
        }
        // new_size > cur_size:不分配新块,读路径返回零;下次 write 触及再补。
        raw.set_size(new_size);
        touch_content_times(&mut raw, Timespec::now());
        self.state.publish_inode_write(&raw).map_err(map_err)?;
        sync_vfs_meta(&self.state, inode, &raw);
        Ok(())
    }

    fn utimes(
        &self,
        inode: &Inode,
        atime: Option<Timespec>,
        mtime: Option<Timespec>,
    ) -> VfsResult<()> {
        self.check_writable()?;
        let mut raw = lock_raw(&self.raw);
        if let Some(ts) = atime {
            raw.set_atime(ts);
        }
        if let Some(ts) = mtime {
            raw.set_mtime(ts);
        }
        touch_change_time(&mut raw, Timespec::now());
        self.state.publish_inode_write(&raw).map_err(map_err)?;
        sync_vfs_meta(&self.state, inode, &raw);
        Ok(())
    }

    fn chmod(&self, inode: &Inode, mode: FileMode) -> VfsResult<()> {
        self.check_writable()?;
        let mut raw = lock_raw(&self.raw);
        let cur = raw.mode();
        raw.set_mode((cur & S_IFMT) | (mode.bits() & 0o7777));
        touch_change_time(&mut raw, Timespec::now());
        self.state.publish_inode_write(&raw).map_err(map_err)?;
        sync_vfs_meta(&self.state, inode, &raw);
        Ok(())
    }

    fn chown(&self, inode: &Inode, uid: Option<Uid>, gid: Option<Gid>) -> VfsResult<()> {
        self.check_writable()?;
        let mut raw = lock_raw(&self.raw);
        if let Some(u) = uid {
            raw.set_uid(u.0);
        }
        if let Some(g) = gid {
            raw.set_gid(g.0);
        }
        touch_change_time(&mut raw, Timespec::now());
        self.state.publish_inode_write(&raw).map_err(map_err)?;
        sync_vfs_meta(&self.state, inode, &raw);
        Ok(())
    }

    fn readlink(&self, _inode: &Inode) -> VfsResult<String> {
        let meta = self.snapshot_meta();
        if file_type_from_mode(meta.mode) != FileType::Symlink {
            return Err(VfsError::InvalidArgument);
        }
        crate::file::symlink_target(
            &self.state,
            meta.flags,
            meta.size,
            &self.snapshot_i_block(),
            Some((self.ino, meta.generation)),
        )
    }

    fn open(
        &self,
        inode: &Inode,
        opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        let initial_kind = file_type_from_mode(self.snapshot_meta().mode);
        if should_truncate_on_open(initial_kind, opts) {
            self.truncate(inode, 0)?;
        }
        let (flags, size, i_block_owned) = self.snapshot_all();
        let meta = self.snapshot_meta();
        let kind = file_type_from_mode(meta.mode);
        match kind {
            FileType::Directory => Ok(Box::new(crate::file::ExtDirFileOps::new_with_state(
                &self.state,
                &i_block_owned,
                flags,
                size,
                Some((self.ino, meta.generation)),
            )?)),
            FileType::Regular => {
                let sb = inode.superblock().ok_or(VfsError::InvalidArgument)?;
                Ok(Box::new(crate::file::ExtRegFileOps::new(
                    Arc::clone(&self.state),
                    sb,
                    self.ino,
                    Arc::clone(&self.raw),
                    Arc::clone(&self.mapping_generation),
                )))
            }
            FileType::Symlink => Ok(Box::new(crate::file::ExtRegFileOps::new_empty(
                Arc::clone(&self.state),
                inode.superblock().ok_or(VfsError::InvalidArgument)?,
                self.ino,
                Arc::clone(&self.raw),
                Arc::clone(&self.mapping_generation),
            ))),
            FileType::Fifo => vfs::pipe::open_fifo(
                Arc::clone(self.fifo.as_ref().ok_or(VfsError::InvalidArgument)?),
                opts,
            ),
            _ => Err(VfsError::NotSupported),
        }
    }

    fn evict(&self, inode: &Inode) {
        if self.state.is_read_only() {
            return;
        }

        let is_dir = inode.kind() == FileType::Directory;
        let mut raw = lock_raw(&self.raw);
        if raw.nlink() != 0 || raw.mode() == 0 {
            return;
        }

        let ino = raw.ino;
        let mut ib = copy_i_block(raw.i_block());
        let mut flags = raw.flags();
        if extent_wr::demote_if_extent(&self.state, &mut flags, &mut ib).is_err() {
            return;
        }
        if map_wr::free_all_blocks(&self.state, &mut ib).is_err() {
            return;
        }
        clear_deleted_inode(&mut raw);
        if self.state.publish_inode_write(&raw).is_err() {
            return;
        }
        drop(raw);
        let _ = alloc_mod::free_inode(&self.state, ino, is_dir);
    }
    fn sync_metadata(&self, _i: &Inode) -> VfsResult<()> {
        // 把本 inode 的待写回快照同步到块缓存(与其它元数据路径一致)。
        self.state.flush_inode_write(self.ino).map_err(map_err)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// (inline helper 已通过 crate::file::ExtRegFileOps 的 read_at 路径按需装载)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inode_times_round_trip_extra_epoch_and_nanoseconds() {
        let mut raw = RawInode::new(7, alloc::vec![0u8; 256]);
        raw.bytes[0x80..0x82].copy_from_slice(&32u16.to_le_bytes());
        let atime = Timespec {
            secs: 2_500_000_000,
            nsecs: 123_456_789,
        };
        let mtime = Timespec {
            secs: (1i64 << 32) + 123,
            nsecs: 987_654_321,
        };
        let ctime = Timespec {
            secs: 2_000_000_000,
            nsecs: 42,
        };
        raw.set_atime(atime);
        raw.set_mtime(mtime);
        raw.set_ctime(ctime);

        let parsed = parse_inode_meta(&raw.bytes, false, 4096);
        assert_eq!(parsed.atime, atime);
        assert_eq!(parsed.mtime, mtime);
        assert_eq!(parsed.ctime, ctime);
    }
}
