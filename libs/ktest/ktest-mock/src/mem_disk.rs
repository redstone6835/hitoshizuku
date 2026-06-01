//! 基于内存的块设备 mock。

use alloc::vec::Vec;
use spin::Mutex;

/// 基于内存的块设备，扇区数据存放在 `Vec<u8>` 中。
pub struct MemDisk {
    sectors: Mutex<Vec<u8>>,
    sector_size: u32,
    sector_count: u32,
}

impl MemDisk {
    /// 创建全零空盘。
    ///
    /// # Panics
    ///
    /// `sector_size == 0` 时 panic（测试辅助类型，非法参数尽早暴露）。
    pub fn new(sector_count: u32, sector_size: u32) -> Self {
        assert!(sector_size > 0, "MemDisk: sector_size must be > 0");
        let total = (sector_count as usize)
            .checked_mul(sector_size as usize)
            .expect("MemDisk: sector_count * sector_size overflow");
        Self {
            sectors: Mutex::new(alloc::vec![0u8; total]),
            sector_size,
            sector_count,
        }
    }

    /// 从字节数据构造。自动补齐到扇区对齐。
    ///
    /// # Panics
    ///
    /// `sector_size == 0` 或 `data` 为空时 panic。
    pub fn from_bytes(data: alloc::vec::Vec<u8>, sector_size: u32) -> Self {
        assert!(sector_size > 0, "MemDisk: sector_size must be > 0");
        assert!(!data.is_empty(), "MemDisk: data must not be empty");
        let rem = data.len() % sector_size as usize;
        let mut sectors = data;
        if rem != 0 {
            sectors.resize(sectors.len() + sector_size as usize - rem, 0);
        }
        let sector_count = sectors.len() / sector_size as usize;
        Self {
            sectors: Mutex::new(sectors),
            sector_size,
            sector_count: sector_count as u32,
        }
    }

    /// 扇区大小（字节）。
    pub fn sector_size(&self) -> u32 {
        self.sector_size
    }

    /// 扇区总数。
    pub fn sector_count(&self) -> u64 {
        self.sector_count as u64
    }

    /// 读取扇区。成功返回 true，越界或缓冲区长度不足返回 false。
    pub fn read_sectors(&self, lba: u64, count: u32, buf: &mut [u8]) -> bool {
        let sector_size = self.sector_size as u64;
        let needed = sector_size
            .checked_mul(count as u64)
            .unwrap_or(u64::MAX);
        if (buf.len() as u64) < needed {
            return false;
        }
        let start = lba.checked_mul(sector_size).unwrap_or(u64::MAX);
        let end = match start.checked_add(needed) {
            Some(v) => v,
            None => return false,
        };
        let s = self.sectors.lock();
        if end > s.len() as u64 {
            return false;
        }
        let len = needed as usize;
        buf[..len].copy_from_slice(&s[start as usize..end as usize]);
        true
    }

    /// 写入扇区。成功返回 true，越界或缓冲区长度不足返回 false。
    /// 失败时不修改任何数据。
    pub fn write_sectors(&self, lba: u64, count: u32, buf: &[u8]) -> bool {
        let sector_size = self.sector_size as u64;
        let needed = sector_size
            .checked_mul(count as u64)
            .unwrap_or(u64::MAX);
        if (buf.len() as u64) < needed {
            return false;
        }
        let start = lba.checked_mul(sector_size).unwrap_or(u64::MAX);
        let end = match start.checked_add(needed) {
            Some(v) => v,
            None => return false,
        };
        let mut s = self.sectors.lock();
        if end > s.len() as u64 {
            return false;
        }
        let len = needed as usize;
        s[start as usize..end as usize].copy_from_slice(&buf[..len]);
        true
    }

    /// 导出全部扇区数据，用于断言写入结果。
    pub fn dump(&self) -> alloc::vec::Vec<u8> {
        self.sectors.lock().clone()
    }
}
