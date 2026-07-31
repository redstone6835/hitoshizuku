//! ext 符号链接目标读取(fast + slow)。
//!
//! - Fast symlink:目标长度 ≤ 60 字节,直接存在 `i_block[0..60]`。
//! - Slow symlink:目标放在数据块里,走标准块映射。

use alloc::string::String;
use alloc::vec;

use crate::layout::FAST_SYMLINK_MAX;
use crate::state::{BlockBackendError, FsState};

/// 读取一个符号链接的目标。
///
/// `size` 是 i_size,`i_block` 是 60 字节 i_block 区。
/// `csum_ctx = Some((ino, generation))` 时对 extent 节点块做 METADATA_CSUM 校验。
pub(crate) fn read_link(
    state: &FsState,
    flags: u32,
    size: u64,
    i_block: &[u8],
    csum_ctx: Option<(u32, u32)>,
) -> Result<String, BlockBackendError> {
    if size == 0 {
        return Ok(String::new());
    }
    if size <= FAST_SYMLINK_MAX as u64 {
        let target = &i_block[..size as usize];
        return Ok(bytes_to_string(target));
    }
    // slow symlink:读足够多的块把 size 字节装出来
    let block_size = state.ext_sb.block_size as u64;
    let total_blocks = (size + block_size - 1) / block_size;
    let mut buf = vec![0u8; (total_blocks * block_size) as usize];
    for lb in 0..total_blocks {
        let phys = if flags & crate::layout::EXT4_EXTENTS_FL != 0 {
            crate::extent::map_block(state, i_block, lb as u32, csum_ctx)?
        } else {
            crate::map::map_block(state, i_block, lb as u32)?
        };
        let dst = &mut buf[(lb * block_size) as usize..((lb + 1) * block_size) as usize];
        match phys {
            Some(p) => state.read_block(p, dst)?,
            None => {
                for b in dst.iter_mut() {
                    *b = 0;
                }
            }
        }
    }
    buf.truncate(size as usize);
    Ok(bytes_to_string(&buf))
}

fn bytes_to_string(b: &[u8]) -> String {
    match core::str::from_utf8(b) {
        Ok(s) => String::from(s),
        Err(_) => {
            let mut s = String::with_capacity(b.len());
            for &x in b {
                if x.is_ascii() {
                    s.push(x as char);
                } else {
                    s.push('\u{fffd}');
                }
            }
            s
        }
    }
}
