//! ext inode 写回 + 位图到 inode 状态 struct 的转换。
//!
//! 本模块持有"可变 inode"的完整 128+ 字节表示,负责 inode 表块的
//! 读-改-写 + 重算 checksum。

use alloc::vec;
use alloc::vec::Vec;

use crate::crc;
use crate::layout::*;
use crate::state::{BlockBackendError, FsState};

/// 一个可修改的 inode 快照(内存中的完整字节副本)。
#[derive(Clone)]
pub(crate) struct RawInode {
    pub ino: u32,
    pub bytes: Vec<u8>,
}

impl RawInode {
    pub fn new(ino: u32, bytes: Vec<u8>) -> Self {
        Self { ino, bytes }
    }

    pub fn mode(&self) -> u16 {
        u16::from_le_bytes([self.bytes[0], self.bytes[1]])
    }
    pub fn set_mode(&mut self, m: u16) {
        self.bytes[0..2].copy_from_slice(&m.to_le_bytes());
    }
    pub fn size(&self) -> u64 {
        let lo = u32::from_le_bytes([self.bytes[4], self.bytes[5], self.bytes[6], self.bytes[7]]);
        let hi = u32::from_le_bytes([
            self.bytes[108],
            self.bytes[109],
            self.bytes[110],
            self.bytes[111],
        ]);
        ((hi as u64) << 32) | lo as u64
    }
    pub fn set_size(&mut self, v: u64) {
        self.bytes[4..8].copy_from_slice(&(v as u32).to_le_bytes());
        self.bytes[108..112].copy_from_slice(&((v >> 32) as u32).to_le_bytes());
    }
    pub fn nlink(&self) -> u16 {
        u16::from_le_bytes([self.bytes[26], self.bytes[27]])
    }
    pub fn set_nlink(&mut self, v: u16) {
        self.bytes[26..28].copy_from_slice(&v.to_le_bytes());
    }
    #[allow(dead_code)]
    pub fn uid(&self) -> u32 {
        let lo = u16::from_le_bytes([self.bytes[2], self.bytes[3]]) as u32;
        let hi = u16::from_le_bytes([self.bytes[0x74], self.bytes[0x75]]) as u32;
        (hi << 16) | lo
    }
    pub fn set_uid(&mut self, v: u32) {
        self.bytes[2..4].copy_from_slice(&(v as u16).to_le_bytes());
        self.bytes[0x74..0x76].copy_from_slice(&((v >> 16) as u16).to_le_bytes());
    }
    #[allow(dead_code)]
    pub fn gid(&self) -> u32 {
        let lo = u16::from_le_bytes([self.bytes[24], self.bytes[25]]) as u32;
        let hi = u16::from_le_bytes([self.bytes[0x72], self.bytes[0x73]]) as u32;
        (hi << 16) | lo
    }
    pub fn set_gid(&mut self, v: u32) {
        self.bytes[24..26].copy_from_slice(&(v as u16).to_le_bytes());
        self.bytes[0x72..0x74].copy_from_slice(&((v >> 16) as u16).to_le_bytes());
    }
    pub fn flags(&self) -> u32 {
        u32::from_le_bytes([
            self.bytes[32],
            self.bytes[33],
            self.bytes[34],
            self.bytes[35],
        ])
    }
    pub fn set_flags(&mut self, f: u32) {
        self.bytes[32..36].copy_from_slice(&f.to_le_bytes());
    }
    #[allow(dead_code)]
    pub fn blocks_lo(&self) -> u32 {
        u32::from_le_bytes([
            self.bytes[28],
            self.bytes[29],
            self.bytes[30],
            self.bytes[31],
        ])
    }
    pub fn set_blocks_lo(&mut self, v: u32) {
        self.bytes[28..32].copy_from_slice(&v.to_le_bytes());
    }
    pub fn generation(&self) -> u32 {
        u32::from_le_bytes([
            self.bytes[100],
            self.bytes[101],
            self.bytes[102],
            self.bytes[103],
        ])
    }
    #[allow(dead_code)]
    pub fn set_generation(&mut self, v: u32) {
        self.bytes[100..104].copy_from_slice(&v.to_le_bytes());
    }

    pub fn i_block_mut(&mut self) -> &mut [u8] {
        &mut self.bytes[0x28..0x28 + 60]
    }
    pub fn i_block(&self) -> &[u8] {
        &self.bytes[0x28..0x28 + 60]
    }
}

/// 从磁盘读一个 inode 的完整字节(不做 metadata_csum 校验,写路径需要原始数据)。
pub(crate) fn read_raw(state: &FsState, ino: u32) -> Result<RawInode, BlockBackendError> {
    let (block, in_block) = state.inode_location(ino)?;
    let inode_size = state.ext_sb.inode_size as usize;
    let block_size = state.ext_sb.block_size as usize;
    let mut blk = vec![0u8; block_size];
    state.read_block(block, &mut blk)?;
    let mut raw = vec![0u8; inode_size];
    if in_block as usize + inode_size <= block_size {
        raw.copy_from_slice(&blk[in_block as usize..in_block as usize + inode_size]);
    } else {
        let first = block_size - in_block as usize;
        raw[..first].copy_from_slice(&blk[in_block as usize..]);
        let mut blk2 = vec![0u8; block_size];
        state.read_block(block + 1, &mut blk2)?;
        raw[first..].copy_from_slice(&blk2[..inode_size - first]);
    }
    Ok(RawInode::new(ino, raw))
}

/// 将 `RawInode` 写回磁盘(含 METADATA_CSUM 重算)。
pub(crate) fn write_raw(state: &FsState, inode: &RawInode) -> Result<(), BlockBackendError> {
    let (block, in_block) = state.inode_location(inode.ino)?;
    let inode_size = state.ext_sb.inode_size as usize;
    let block_size = state.ext_sb.block_size as usize;

    let mut bytes = inode.bytes.clone();
    if state.ext_sb.metadata_csum && inode_size >= 256 {
        // 清零 csum lo/hi 再重算
        bytes[0x7c] = 0;
        bytes[0x7d] = 0;
        let i_extra_isize = u16::from_le_bytes([bytes[0x80], bytes[0x81]]);
        if i_extra_isize >= 4 {
            bytes[0x82] = 0;
            bytes[0x83] = 0;
        }
        let generation = inode.generation();
        let mut seed = state.ext_sb.csum_seed;
        seed = crc::update(seed, &inode.ino.to_le_bytes());
        seed = crc::update(seed, &generation.to_le_bytes());
        let sum = crc::update(seed, &bytes);
        bytes[0x7c..0x7e].copy_from_slice(&(sum as u16).to_le_bytes());
        if i_extra_isize >= 4 {
            bytes[0x82..0x84].copy_from_slice(&((sum >> 16) as u16).to_le_bytes());
        }
    }

    // 写回一个或两个 inode table 块
    let mut blk = vec![0u8; block_size];
    state.read_block(block, &mut blk)?;
    if in_block as usize + inode_size <= block_size {
        blk[in_block as usize..in_block as usize + inode_size].copy_from_slice(&bytes);
        state.write_block(block, &blk)?;
    } else {
        let first = block_size - in_block as usize;
        blk[in_block as usize..].copy_from_slice(&bytes[..first]);
        state.write_block(block, &blk)?;
        let mut blk2 = vec![0u8; block_size];
        state.read_block(block + 1, &mut blk2)?;
        blk2[..inode_size - first].copy_from_slice(&bytes[first..]);
        state.write_block(block + 1, &blk2)?;
    }
    Ok(())
}

/// 清零一个新分配的 inode(EXT2-style),mode/linksize=0,flags 保留为 0。
#[allow(dead_code)]
pub(crate) fn zero_new(state: &FsState, ino: u32) -> Result<RawInode, BlockBackendError> {
    let inode_size = state.ext_sb.inode_size as usize;
    let raw = vec![0u8; inode_size];
    Ok(RawInode::new(ino, raw))
}

/// 工具:依据 `FileType`-like 位构造 i_mode(附权限位)。
#[allow(dead_code)]
pub(crate) fn make_mode(kind: u16, perm: u16) -> u16 {
    (kind & S_IFMT) | (perm & 0o7777)
}
