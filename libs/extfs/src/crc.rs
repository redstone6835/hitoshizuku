//! CRC32C(Castagnoli 多项式)软件实现。
//!
//! ext4 METADATA_CSUM 使用 crc32c(CRC-32C / iSCSI),生成多项式 `0x1EDC6F41`;
//! 驱动在读路径用它验证超级块、块组描述符、inode、extent、目录块的校验和。
//!
//! 本模块不依赖任何硬件指令。使用字节查表法,单核、小镜像的性能足够,
//! 未来若需要架构 CRC 指令再替换。

const POLY: u32 = 0x82f63b78; // 0x1EDC6F41 的位反射

/// 静态 CRC32C 查表:256 入口。
static TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut j = 0;
        while j < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            j += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// 以 `seed` 为初值,对 `data` 继续求 CRC32C。
#[inline]
pub(crate) fn update(seed: u32, data: &[u8]) -> u32 {
    let mut c = seed;
    for &b in data {
        c = TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    c
}

/// 从 0xFFFFFFFF 初值起算一次 CRC32C,不对结果取反(保留 ext4 习惯)。
#[inline]
pub(crate) fn crc32c(data: &[u8]) -> u32 {
    update(0xffff_ffff, data)
}

const POLY_BE: u32 = 0x04c1_1db7; // CRC-32 (ITU-T) 正向多项式

/// CRC-32 大端(高位先行)查表:jbd2 v1(COMPAT_CHECKSUM)事务校验使用。
static TABLE_BE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = (i as u32) << 24;
        let mut j = 0;
        while j < 8 {
            c = if c & 0x8000_0000 != 0 {
                (c << 1) ^ POLY_BE
            } else {
                c << 1
            };
            j += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// 以 `seed` 为初值,对 `data` 继续求大端 CRC-32(与内核 `crc32_be` 同约定,
/// 可直接链式调用)。
#[inline]
pub(crate) fn crc32_be_update(seed: u32, data: &[u8]) -> u32 {
    let mut c = seed;
    for &b in data {
        c = (c << 8) ^ TABLE_BE[(((c >> 24) ^ b as u32) & 0xff) as usize];
    }
    c
}
