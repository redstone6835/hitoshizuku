//! ext 族文件系统的扩展属性存储：ext4 兼容 xattr 块。
//!
//! 磁盘布局（与 Linux ext4 一致）：
//!
//! ```text
//! 块首 32 字节：magic u32 = 0xea020000、refcount u32、blocks u32、hash u32
//! 条目区：{ e_name_index u8, e_name_len u8, e_value_offs u16,
//!          e_value_block u32(=0，内联), e_value_size u32, e_hash u32,
//!          e_name[name_len]（4 字节对齐）}，以 name_len==0 的条目结束
//! 值区：条目之后按 4 字节对齐连续存放各值，e_value_offs 指向块内偏移
//! ```
//!
//! 命名空间索引（`fs/ext4/xattr.c`）：user=1、posix_acl_access=2、
//! posix_acl_default=3、trusted=4、security=6；属性名不含前缀
//! （ACL 的名字为空串）。每次修改重建整个块；超大值返回 `E2BIG`
//! （Linux ext2/3 内联 xattr 同语义）。

use alloc::vec;
use alloc::vec::Vec;

use vfs::error::{VfsError, VfsResult};
use vfs::xattr::{XATTR_CREATE, XATTR_REPLACE};

use crate::state::{BlockBackendError, FsState};

/// ext4 xattr 块魔数（`EXT4_XATTR_MAGIC`）。
const XATTR_MAGIC: u32 = 0xea02_0000;
/// 块首大小（`struct ext4_xattr_header`）。
const XATTR_HEADER_SIZE: usize = 32;
/// 条目头大小（`struct ext4_xattr_entry` 不含 name）。
const XATTR_ENTRY_SIZE: usize = 16;

/// 命名空间 → ext4 name_index。
pub(crate) fn name_index(name: &[u8]) -> VfsResult<(u8, &[u8])> {
    match vfs::xattr::parse_name(name)? {
        vfs::xattr::XattrNamespace::User => Ok((1, &name[b"user.".len()..])),
        vfs::xattr::XattrNamespace::PosixAclAccess => Ok((2, b"")),
        vfs::xattr::XattrNamespace::PosixAclDefault => Ok((3, b"")),
        vfs::xattr::XattrNamespace::Trusted => Ok((4, &name[b"trusted.".len()..])),
        vfs::xattr::XattrNamespace::Security => Ok((6, &name[b"security.".len()..])),
        vfs::xattr::XattrNamespace::OtherSystem => Err(VfsError::NotSupported),
    }
}

/// name_index + 短名 → 完整属性名（listxattr 输出）。
fn full_name(index: u8, short: &[u8]) -> Vec<u8> {
    let prefix: &[u8] = match index {
        1 => b"user.",
        2 => b"system.posix_acl_access",
        3 => b"system.posix_acl_default",
        4 => b"trusted.",
        6 => b"security.",
        _ => b"unknown.",
    };
    let mut out = Vec::with_capacity(prefix.len() + short.len());
    out.extend_from_slice(prefix);
    out.extend_from_slice(short);
    out
}

/// 解析 xattr 块为 `(index, name, value)` 列表。
fn parse_block(block: &[u8]) -> Vec<(u8, Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    if block.len() < XATTR_HEADER_SIZE {
        return out;
    }
    let magic = u32::from_le_bytes(block[0..4].try_into().unwrap());
    if magic != XATTR_MAGIC {
        return out;
    }
    let mut off = XATTR_HEADER_SIZE;
    while off + XATTR_ENTRY_SIZE <= block.len() {
        let name_index = block[off];
        let name_len = block[off + 1] as usize;
        let value_offs = u16::from_le_bytes(block[off + 2..off + 4].try_into().unwrap()) as usize;
        let value_len = u32::from_le_bytes(block[off + 8..off + 12].try_into().unwrap()) as usize;
        if name_len == 0 {
            break; // 结束条目
        }
        let name_start = off + XATTR_ENTRY_SIZE;
        if name_start + name_len > block.len() {
            break;
        }
        let name = block[name_start..name_start + name_len].to_vec();
        let value = if value_offs + value_len <= block.len() {
            block[value_offs..value_offs + value_len].to_vec()
        } else {
            Vec::new()
        };
        out.push((name_index, name, value));
        // 条目按 4 字节对齐。
        off = (name_start + name_len + 3) & !3;
    }
    out
}

/// 重建 xattr 块；超出块容量返回 `E2BIG`。
fn build_block(entries: &[(u8, Vec<u8>, Vec<u8>)], block_size: usize) -> Result<Vec<u8>, VfsError> {
    // 先按 (index, name) 排序，保证磁盘格式稳定。
    let mut sorted: Vec<&(u8, Vec<u8>, Vec<u8>)> = entries.iter().collect();
    sorted.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));

    let mut block = vec![0u8; block_size];
    block[0..4].copy_from_slice(&XATTR_MAGIC.to_le_bytes());
    block[4..8].copy_from_slice(&1u32.to_le_bytes()); // refcount
    block[8..12].copy_from_slice(&1u32.to_le_bytes()); // blocks

    let mut cursor = XATTR_HEADER_SIZE;
    // 第一遍：条目区（值偏移后填）。
    let mut value_cursor = block_size;
    for (index, name, value) in &sorted {
        let name_len = name.len();
        if name_len > 255 {
            return Err(VfsError::InvalidArgument);
        }
        let entry_start = cursor;
        if entry_start + XATTR_ENTRY_SIZE + name_len > block_size {
            return Err(VfsError::FileTooLarge); // E2BIG
        }
        // 值从块尾向前分配（与 ext4 的 tail 分配一致）。
        let value_len = value.len();
        if value_len > 0 {
            let aligned = (value_len + 3) & !3;
            if value_cursor < aligned || entry_start + XATTR_ENTRY_SIZE + name_len > value_cursor - aligned {
                return Err(VfsError::FileTooLarge);
            }
            value_cursor -= aligned;
        }
        block[entry_start] = *index;
        block[entry_start + 1] = name_len as u8;
        block[entry_start + 2..entry_start + 4]
            .copy_from_slice(&(value_cursor as u16).to_le_bytes());
        // e_value_block @+4 = 0（内联）
        block[entry_start + 8..entry_start + 12]
            .copy_from_slice(&(value_len as u32).to_le_bytes());
        // e_hash @+12 = 0（本内核不校验哈希；e2fsck 不强制）
        block[entry_start + 16..entry_start + 16 + name_len].copy_from_slice(name);
        if value_len > 0 {
            block[value_cursor..value_cursor + value_len].copy_from_slice(value);
        }
        cursor = (entry_start + XATTR_ENTRY_SIZE + name_len + 3) & !3;
    }
    Ok(block)
}

/// 读取 inode 的 xattr 块字节；`i_file_acl == 0` 返回 `None`。
fn read_block(state: &FsState, block: u64) -> Result<Vec<u8>, VfsError> {
    let block_size = state.ext_sb.block_size as usize;
    let mut buf = vec![0u8; block_size];
    state
        .read_data_blocks(block, 1, &mut buf)
        .map_err(|_| VfsError::Io)?;
    Ok(buf)
}

fn map_backend_err(err: BlockBackendError) -> VfsError {
    let _ = err;
    VfsError::Io
}

/// 在块上执行 getxattr。
pub(crate) fn get(state: &FsState, acl_block: u64, name: &[u8]) -> VfsResult<Option<Vec<u8>>> {
    let (index, short) = name_index(name)?;
    if acl_block == 0 {
        return Ok(None);
    }
    let block = read_block(state, acl_block)?;
    for (i, n, v) in parse_block(&block) {
        if i == index && n == short {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// 在块上执行 setxattr；返回新的 `i_file_acl` 与块字节（调用方负责写回）。
pub(crate) fn set(
    state: &FsState,
    acl_block: u64,
    name: &[u8],
    value: &[u8],
    flags: u32,
) -> VfsResult<(u64, Vec<u8>)> {
    let (index, short) = name_index(name)?;
    let mut entries: Vec<(u8, Vec<u8>, Vec<u8>)> = if acl_block != 0 {
        parse_block(&read_block(state, acl_block)?)
    } else {
        Vec::new()
    };
    let existing = entries
        .iter()
        .position(|(i, n, _)| *i == index && n == &short);
    match (flags, existing) {
        (XATTR_CREATE, Some(_)) => return Err(VfsError::AlreadyExists),
        (XATTR_REPLACE, None) => return Err(VfsError::NoData),
        _ => {}
    }
    if let Some(pos) = existing {
        entries.remove(pos);
    }
    entries.push((index, short.to_vec(), value.to_vec()));
    let block_size = state.ext_sb.block_size as usize;
    let block = build_block(&entries, block_size)?;
    Ok((acl_block, block))
}

/// 在块上执行 removexattr；返回 `(新的 i_file_acl, 重建后的块字节)`。
/// 属性列表清空时返回 `(0, 空)`（调用方释放块并清 i_file_acl）。
pub(crate) fn remove(
    state: &FsState,
    acl_block: u64,
    name: &[u8],
) -> VfsResult<(u64, Vec<u8>)> {
    let (index, short) = name_index(name)?;
    if acl_block == 0 {
        return Err(VfsError::NoData);
    }
    let block = read_block(state, acl_block)?;
    let mut entries = parse_block(&block);
    let existing = entries
        .iter()
        .position(|(i, n, _)| *i == index && n == &short);
    let Some(pos) = existing else {
        return Err(VfsError::NoData);
    };
    entries.remove(pos);
    if entries.is_empty() {
        return Ok((0, Vec::new()));
    }
    let block_size = state.ext_sb.block_size as usize;
    let block = build_block(&entries, block_size)?;
    Ok((acl_block, block))
}

/// 列出全部属性名。
pub(crate) fn list(state: &FsState, acl_block: u64) -> VfsResult<Vec<Vec<u8>>> {
    if acl_block == 0 {
        return Ok(Vec::new());
    }
    let block = read_block(state, acl_block)?;
    Ok(parse_block(&block)
        .into_iter()
        .map(|(i, n, _)| full_name(i, &n))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_and_parse(entries: &[(u8, Vec<u8>, Vec<u8>)], block_size: usize) -> Vec<(u8, Vec<u8>, Vec<u8>)> {
        let block = build_block(entries, block_size).unwrap();
        assert_eq!(block.len(), block_size);
        parse_block(&block)
    }

    /// 单属性往返。
    #[test]
    fn single_entry_roundtrip() {
        let entries = vec![(1u8, b"key1".to_vec(), b"value1".to_vec())];
        let parsed = build_and_parse(&entries, 4096);
        assert_eq!(parsed, entries);
    }

    /// 多属性 + 排序 + 值在块尾。
    #[test]
    fn multi_entry_roundtrip() {
        let entries = vec![
            (1u8, b"zeta".to_vec(), b"z-value".to_vec()),
            (1u8, b"alpha".to_vec(), b"a".to_vec()),
            (4u8, b"t".to_vec(), b"trusted-value".to_vec()),
            (6u8, b"sec".to_vec(), b"security-value".to_vec()),
        ];
        let parsed = build_and_parse(&entries, 4096);
        // 按 (index, name) 排序后与原始内容一致（无序比较）。
        assert_eq!(parsed.len(), entries.len());
        for (i, n, v) in &parsed {
            assert!(entries.iter().any(|(ei, en, ev)| ei == i && en == n && ev == v));
        }
    }

    /// 值超出块容量 → E2BIG。
    #[test]
    fn oversized_value_rejected() {
        let big = vec![0u8; 4096];
        let entries = vec![(1u8, b"big".to_vec(), big)];
        assert_eq!(build_block(&entries, 4096), Err(VfsError::FileTooLarge));
    }

    /// 空条目块：header + 结束条目。
    #[test]
    fn empty_block_parses() {
        let block = build_block(&[], 4096).unwrap();
        assert!(parse_block(&block).is_empty());
        let magic = u32::from_le_bytes(block[0..4].try_into().unwrap());
        assert_eq!(magic, XATTR_MAGIC);
    }

    /// name_index 映射。
    #[test]
    fn name_index_mapping() {
        assert_eq!(name_index(b"user.foo").unwrap(), (1, b"foo".as_slice()));
        assert_eq!(name_index(b"system.posix_acl_access").unwrap(), (2, b"".as_slice()));
        assert_eq!(name_index(b"system.posix_acl_default").unwrap(), (3, b"".as_slice()));
        assert_eq!(name_index(b"trusted.t").unwrap(), (4, b"t".as_slice()));
        assert_eq!(name_index(b"security.s").unwrap(), (6, b"s".as_slice()));
        assert_eq!(name_index(b"system.other"), Err(VfsError::NotSupported));
    }
}
