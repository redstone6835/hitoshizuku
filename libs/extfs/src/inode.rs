//! ext2/3/4 inode 加载/写回 + VFS `InodeOps` 实现。
//!
//! 运行时每个 `Inode` 持有一个 `Spinlock<RawInode>`(完整磁盘字节),所有写
//! 路径先改内存副本,调用 [`inode_wr::write_raw`] 落盘,最后同步到 VFS
//! [`vfs::inode::Inode`] 的镜像字段(`size`/`nlink`/...)。
//!
//! 写路径统一的"降级策略":一旦要改写扩展了 extent 的文件,先走
//! [`extent_wr::demote_if_extent`] 把它转成"空间接布局",之后所有扩容/截断
//! 都走 [`map_wr`]。读路径不受影响 —— 读到的 extent 文件保持原样,直到
//! 第一次写入才改动。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use vfs::cred::{Credentials, Gid, Uid};
use vfs::error::{VfsError, VfsResult};
use vfs::file::{FileOps, OpenOptions};
use vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use vfs::stat::{DevId, FileMode, FileType, Timespec};
use vfs::sync::Spinlock;

use crate::inode_wr::{RawInode, read_raw, write_raw};
use crate::layout::*;
use crate::state::{BlockBackendError, FsState, map_err};
use crate::{alloc_mod, dir_wr, extent_wr, map_wr};

const I_BLOCK_BYTES: usize = 60;

/// on-disk inode 摘要(由 [`load_inode`] 返回)。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct InodeMetaDisk {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub nlink: u16,
    pub flags: u32,
    pub blocks_512: u64,
    pub file_acl_hi: u32,
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

fn parse_inode_meta(raw: &[u8]) -> InodeMetaDisk {
    let mode = u16::from_le_bytes([raw[0], raw[1]]);
    let uid_lo = u16::from_le_bytes([raw[2], raw[3]]) as u32;
    let size_lo = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
    let gid_lo = u16::from_le_bytes([raw[24], raw[25]]) as u32;
    let nlink = u16::from_le_bytes([raw[26], raw[27]]);
    let blocks_lo = u32::from_le_bytes([raw[28], raw[29], raw[30], raw[31]]);
    let flags = u32::from_le_bytes([raw[32], raw[33], raw[34], raw[35]]);
    let size_hi = u32::from_le_bytes([raw[108], raw[109], raw[110], raw[111]]);
    let file_acl_hi = u16::from_le_bytes([raw[120], raw[121]]) as u32;
    let uid_hi = u16::from_le_bytes([raw[0x74], raw[0x75]]) as u32;
    let gid_hi = u16::from_le_bytes([raw[0x72], raw[0x73]]) as u32;
    InodeMetaDisk {
        mode,
        uid: (uid_hi << 16) | uid_lo,
        gid: (gid_hi << 16) | gid_lo,
        size: ((size_hi as u64) << 32) | size_lo as u64,
        nlink,
        flags,
        blocks_512: blocks_lo as u64,
        file_acl_hi,
    }
}

/// 旧接口 —— mount 入口仍在用;只读 mount 的路径。
pub(crate) fn load_inode(
    state: &FsState,
    ino: u32,
) -> Result<(InodeMetaDisk, Vec<u8>), BlockBackendError> {
    let raw = read_raw(state, ino)?;
    Ok((parse_inode_meta(&raw.bytes), raw.bytes))
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
    pub(crate) raw: Spinlock<RawInode>,
}

impl ExtInodeOps {
    pub(crate) fn new(state: Arc<FsState>, ino: u32, bytes: Vec<u8>) -> Self {
        Self {
            state,
            ino,
            raw: Spinlock::new(RawInode::new(ino, bytes)),
        }
    }

    fn snapshot_meta(&self) -> InodeMetaDisk {
        let g = self.raw.lock();
        parse_inode_meta(&g.bytes)
    }

    fn snapshot_i_block(&self) -> [u8; I_BLOCK_BYTES] {
        let g = self.raw.lock();
        copy_i_block(i_block_slice(&g.bytes))
    }

    #[allow(dead_code)]
    fn snapshot_flags(&self) -> u32 {
        self.raw.lock().flags()
    }

    fn snapshot_all(&self) -> (u32, u64, [u8; I_BLOCK_BYTES]) {
        let g = self.raw.lock();
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
    let mode = FileMode::new((meta.mode & 0o7777) as u16);
    let vmeta = InodeMeta {
        size: meta.size,
        nlink: meta.nlink as u32,
        mode,
        uid: Uid(meta.uid),
        gid: Gid(meta.gid),
        atime: Timespec::ZERO,
        mtime: Timespec::ZERO,
        ctime: Timespec::ZERO,
        blocks: meta.blocks_512,
    };
    let block_size = state.ext_sb.block_size;
    let ops = ExtInodeOps::new(Arc::clone(state), ino, raw);
    let inode = Inode::new(
        InodeId {
            fs_id: sb.fs_id,
            ino: ino as u64,
        },
        kind,
        DevId::new(0, 0),
        block_size,
        None,
        vmeta,
        Arc::new(ops) as Arc<dyn InodeOps + Send + Sync>,
        sb.self_weak.clone(),
    );
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
    // 若 inode_size >= 256,要设好 i_extra_isize(否则 csum 不对)
    if state.ext_sb.inode_size >= 256 {
        // linux 默认 32(至少覆盖 atime_extra/ctime_extra/...);我们用 32
        raw.bytes[0x80..0x82].copy_from_slice(&32u16.to_le_bytes());
    }
    write_raw(state, &raw)?;
    Ok(raw)
}

fn sync_vfs_meta(inode: &Inode, raw: &RawInode) {
    inode.set_size(raw.size());
    inode.set_nlink(raw.nlink() as u32);
    // mode 改动不经 VFS 镜像(vfs::Inode 没暴露公开 setter),
    // 下次 stat 读不到新 mode 直到 Inode 被重新创建 —— 只读 FS 不关心,
    // R/W 场景里 chmod 的常见路径是 open + 用新 handle 再 stat。
}

impl InodeOps for ExtInodeOps {
    fn lookup(&self, inode: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        let meta = self.snapshot_meta();
        if file_type_from_mode(meta.mode) != FileType::Directory {
            return Err(VfsError::NotADirectory);
        }
        let i_block = self.snapshot_i_block();
        if let Some(e) = crate::dir::find_entry(&self.state, &i_block, meta.flags, meta.size, name)
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
        let new_raw = create_disk_inode(&self.state, full_mode, cred.uid.0, cred.gid.0, false, 1)
            .map_err(map_err)?;

        // 在父目录插 entry
        let mut parent = self.raw.lock();
        let mut i_block = copy_i_block(parent.i_block());
        let mut pflags = parent.flags();
        let new_size = dir_wr::insert_entry(
            &self.state,
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
        write_raw(&self.state, &parent).map_err(map_err)?;
        sync_vfs_meta(inode, &parent);
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
        // 新目录:S_IFDIR | perm,nlink=2("." 指自己),parent 的 nlink += 1
        let full_mode = S_IFDIR | (mode.bits() & 0o7777);
        let mut new_raw =
            create_disk_inode(&self.state, full_mode, cred.uid.0, cred.gid.0, true, 2)
                .map_err(map_err)?;

        // 给新目录分配第一个块,写入 "." / ".."
        let block = alloc_mod::alloc_block(&self.state).map_err(map_err)?;
        let init_blk = dir_wr::make_init_dir_block(
            self.state.ext_sb.block_size,
            new_raw.ino,
            self.ino,
            self.state.ext_sb.feature_incompat & INCOMPAT_FILETYPE != 0,
        );
        self.state.write_block(block, &init_blk).map_err(map_err)?;
        // 把 block 指针写到 new_raw.i_block[0]
        new_raw.i_block_mut()[0..4].copy_from_slice(&(block as u32).to_le_bytes());
        new_raw.set_size(self.state.ext_sb.block_size as u64);
        new_raw.set_blocks_lo((self.state.ext_sb.block_size / 512) as u32);
        write_raw(&self.state, &new_raw).map_err(map_err)?;

        // 父目录:插 entry + nlink++
        let mut parent = self.raw.lock();
        let mut pi_block = copy_i_block(parent.i_block());
        let mut pflags = parent.flags();
        let new_size = dir_wr::insert_entry(
            &self.state,
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
        let new_nl = parent.nlink() + 1;
        parent.set_nlink(new_nl);
        write_raw(&self.state, &parent).map_err(map_err)?;
        sync_vfs_meta(inode, &parent);
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
        let mut parent = self.raw.lock();
        let pi_block = copy_i_block(parent.i_block());
        let pflags = parent.flags();
        let ok = dir_wr::remove_entry(&self.state, &pi_block, pflags, parent.size(), name)
            .map_err(map_err)?;
        if !ok {
            return Err(VfsError::NotFound);
        }
        parent.i_block_mut().copy_from_slice(&pi_block);
        parent.set_flags(pflags);
        write_raw(&self.state, &parent).map_err(map_err)?;
        sync_vfs_meta(inode, &parent);
        drop(parent);

        // 2) 目标 inode 的 nlink--,到 0 则释放数据
        let t_ops = target
            .downcast_ops::<ExtInodeOps>()
            .ok_or(VfsError::InvalidArgument)?;
        let mut traw = t_ops.raw.lock();
        let nl = traw.nlink().saturating_sub(1);
        traw.set_nlink(nl);
        if nl == 0 {
            let mut ib = copy_i_block(traw.i_block());
            let mut flags = traw.flags();
            extent_wr::demote_if_extent(&self.state, &mut flags, &mut ib).map_err(map_err)?;
            map_wr::free_all_blocks(&self.state, &mut ib).map_err(map_err)?;
            traw.i_block_mut().copy_from_slice(&ib);
            traw.set_flags(flags);
            traw.set_size(0);
            traw.set_blocks_lo(0);
            write_raw(&self.state, &traw).map_err(map_err)?;
            let ino_to_free = traw.ino;
            drop(traw);
            alloc_mod::free_inode(&self.state, ino_to_free, false).map_err(map_err)?;
        } else {
            write_raw(&self.state, &traw).map_err(map_err)?;
            drop(traw);
        }
        sync_vfs_meta(&target, &t_ops.raw.lock());
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
            let traw = t_ops.raw.lock();
            let tib = copy_i_block(i_block_slice(&traw.bytes));
            let entries =
                crate::dir::read_all_entries(&self.state, &tib, traw.flags(), traw.size())
                    .map_err(map_err)?;
            for e in &entries {
                if e.name != "." && e.name != ".." {
                    return Err(VfsError::DirectoryNotEmpty);
                }
            }
        }
        // 从父目录移除
        let mut parent = self.raw.lock();
        let pib = copy_i_block(parent.i_block());
        let pflags = parent.flags();
        dir_wr::remove_entry(&self.state, &pib, pflags, parent.size(), name).map_err(map_err)?;
        parent.i_block_mut().copy_from_slice(&pib);
        parent.set_flags(pflags);
        let pn = parent.nlink().saturating_sub(1);
        parent.set_nlink(pn);
        write_raw(&self.state, &parent).map_err(map_err)?;
        sync_vfs_meta(inode, &parent);
        drop(parent);

        // 释放目标目录的所有数据块 + inode
        let mut traw = t_ops.raw.lock();
        let mut ib = copy_i_block(traw.i_block());
        let mut flags = traw.flags();
        extent_wr::demote_if_extent(&self.state, &mut flags, &mut ib).map_err(map_err)?;
        map_wr::free_all_blocks(&self.state, &mut ib).map_err(map_err)?;
        traw.i_block_mut().copy_from_slice(&ib);
        traw.set_flags(flags);
        traw.set_nlink(0);
        traw.set_size(0);
        traw.set_blocks_lo(0);
        write_raw(&self.state, &traw).map_err(map_err)?;
        let ino = traw.ino;
        drop(traw);
        alloc_mod::free_inode(&self.state, ino, true).map_err(map_err)?;
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
            create_disk_inode(&self.state, full_mode, cred.uid.0, cred.gid.0, false, 1)
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
        write_raw(&self.state, &new_raw).map_err(map_err)?;

        // 父目录插 entry
        let mut parent = self.raw.lock();
        let mut pib = copy_i_block(parent.i_block());
        let mut pflags = parent.flags();
        let new_size = dir_wr::insert_entry(
            &self.state,
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
        write_raw(&self.state, &parent).map_err(map_err)?;
        sync_vfs_meta(inode, &parent);
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
        // nlink++
        {
            let mut traw = t_ops.raw.lock();
            let new_nl = traw.nlink() + 1;
            traw.set_nlink(new_nl);
            write_raw(&self.state, &traw).map_err(map_err)?;
        }
        sync_vfs_meta(target, &t_ops.raw.lock());

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
        let mut parent = self.raw.lock();
        let mut pib = copy_i_block(parent.i_block());
        let mut pflags = parent.flags();
        let new_size = dir_wr::insert_entry(
            &self.state,
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
        write_raw(&self.state, &parent).map_err(map_err)?;
        sync_vfs_meta(inode, &parent);
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
                let traw = eops.raw.lock();
                let tib = copy_i_block(i_block_slice(&traw.bytes));
                let entries =
                    crate::dir::read_all_entries(&self.state, &tib, traw.flags(), traw.size())
                        .map_err(map_err)?;
                drop(traw);
                for e in &entries {
                    if e.name != "." && e.name != ".." {
                        return Err(VfsError::DirectoryNotEmpty);
                    }
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
            let mut ndir = new_dir_ops.raw.lock();
            let mut ib = copy_i_block(ndir.i_block());
            let mut flags = ndir.flags();
            let new_size = dir_wr::insert_entry(
                &self.state,
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
                let nl = ndir.nlink() + 1;
                ndir.set_nlink(nl);
            }
            write_raw(&self.state, &ndir).map_err(map_err)?;
            sync_vfs_meta(new_dir, &ndir);
        }
        // 从源目录移除 old_name 条目;跨目录时顺手减 nlink
        {
            let mut parent = self.raw.lock();
            let mut ib = copy_i_block(parent.i_block());
            let mut flags = parent.flags();
            dir_wr::remove_entry(&self.state, &ib, flags, parent.size(), old_name)
                .map_err(map_err)?;
            // 同目录 rename:就地改名,需要新增 new_name 条目(上面跨目录分支已插过)
            if !cross_dir {
                let new_size = dir_wr::insert_entry(
                    &self.state,
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
                let nl = parent.nlink().saturating_sub(1);
                parent.set_nlink(nl);
            }
            parent.i_block_mut().copy_from_slice(&ib);
            parent.set_flags(flags);
            write_raw(&self.state, &parent).map_err(map_err)?;
            sync_vfs_meta(inode, &parent);
        }

        // 跨目录移动目录:同步子目录的 ".." 到新父目录
        if cross_dir && target_is_dir {
            let tops = entry_target
                .downcast_ops::<ExtInodeOps>()
                .ok_or(VfsError::InvalidArgument)?;
            let tib = {
                let g = tops.raw.lock();
                (copy_i_block(i_block_slice(&g.bytes)), g.flags())
            };
            dir_wr::update_dotdot(&self.state, &tib.0, tib.1, new_dir_ops.ino).map_err(map_err)?;
        }
        Ok(())
    }

    fn truncate(&self, inode: &Inode, new_size: u64) -> VfsResult<()> {
        self.check_writable()?;
        let meta = self.snapshot_meta();
        if file_type_from_mode(meta.mode) != FileType::Regular {
            return Err(VfsError::InvalidArgument);
        }
        let mut raw = self.raw.lock();
        let cur_size = raw.size();
        if new_size == cur_size {
            return Ok(());
        }
        let block_size = self.state.ext_sb.block_size as u64;
        if new_size < cur_size {
            let mut ib = copy_i_block(raw.i_block());
            let mut flags = raw.flags();
            // extent 文件先降级成间接布局再做精确释放
            extent_wr::demote_if_extent(&self.state, &mut flags, &mut ib).map_err(map_err)?;

            if new_size == 0 {
                map_wr::free_all_blocks(&self.state, &mut ib).map_err(map_err)?;
                raw.set_blocks_lo(0);
            } else {
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
                let blocks512 = (new_size + 511) / 512;
                raw.set_blocks_lo(blocks512 as u32);
            }
            raw.i_block_mut().copy_from_slice(&ib);
            raw.set_flags(flags);
        }
        // new_size > cur_size:不分配新块,读路径返回零;下次 write 触及再补。
        raw.set_size(new_size);
        write_raw(&self.state, &raw).map_err(map_err)?;
        sync_vfs_meta(inode, &raw);
        Ok(())
    }

    fn chmod(&self, inode: &Inode, mode: FileMode) -> VfsResult<()> {
        self.check_writable()?;
        let mut raw = self.raw.lock();
        let cur = raw.mode();
        raw.set_mode((cur & S_IFMT) | (mode.bits() & 0o7777));
        write_raw(&self.state, &raw).map_err(map_err)?;
        sync_vfs_meta(inode, &raw);
        Ok(())
    }

    fn chown(&self, inode: &Inode, uid: Option<Uid>, gid: Option<Gid>) -> VfsResult<()> {
        self.check_writable()?;
        let mut raw = self.raw.lock();
        if let Some(u) = uid {
            raw.set_uid(u.0);
        }
        if let Some(g) = gid {
            raw.set_gid(g.0);
        }
        write_raw(&self.state, &raw).map_err(map_err)?;
        sync_vfs_meta(inode, &raw);
        Ok(())
    }

    fn readlink(&self, _inode: &Inode) -> VfsResult<String> {
        let meta = self.snapshot_meta();
        if file_type_from_mode(meta.mode) != FileType::Symlink {
            return Err(VfsError::InvalidArgument);
        }
        crate::file::symlink_target(&self.state, meta.flags, meta.size, &self.snapshot_i_block())
    }

    fn open(
        &self,
        inode: &Inode,
        opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        if opts.truncate {
            self.truncate(inode, 0)?;
        }
        let (flags, size, i_block_owned) = self.snapshot_all();
        let meta = self.snapshot_meta();
        let kind = file_type_from_mode(meta.mode);
        match kind {
            FileType::Directory => {
                let entries =
                    crate::dir::read_all_entries(&self.state, &i_block_owned, flags, size)
                        .map_err(map_err)?;
                Ok(Box::new(crate::file::ExtDirFileOps::new_with_state(
                    entries,
                    &self.state,
                )))
            }
            FileType::Regular => {
                let sb = inode.superblock().ok_or(VfsError::InvalidArgument)?;
                Ok(Box::new(crate::file::ExtRegFileOps::new(
                    Arc::clone(&self.state),
                    sb,
                    self.ino,
                )))
            }
            FileType::Symlink => Ok(Box::new(crate::file::ExtRegFileOps::new_empty(
                Arc::clone(&self.state),
                inode.superblock().ok_or(VfsError::InvalidArgument)?,
                self.ino,
            ))),
            _ => Err(VfsError::NotSupported),
        }
    }

    fn evict(&self, _i: &Inode) {}
    fn sync_metadata(&self, _i: &Inode) -> VfsResult<()> {
        Ok(())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// (inline helper 已通过 crate::file::ExtRegFileOps 的 read_at 路径按需装载)
