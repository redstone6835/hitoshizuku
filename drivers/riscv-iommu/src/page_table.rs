//! RISC-V IOMMU S-stage 页表与 IOVA 分配。

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use allocator::PAGE_SIZE;
use general::dev::dma::{DmaBuffer, DmaContext, DmaDirection};

use crate::bits::{
    PTE_ACCESSED, PTE_DIRTY, PTE_PPN_MASK, PTE_READ, PTE_USER, PTE_VALID, PTE_WRITE,
    decode_pte_paddr, encode_ppn, pte_is_leaf,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageTableError {
    Invalid,
    OutOfMemory,
    Conflict,
    Corrupt,
}

struct PageTablePage {
    buffer: DmaBuffer,
}

impl PageTablePage {
    fn new() -> Result<Self, PageTableError> {
        Ok(Self {
            buffer: DmaBuffer::new_in(
                DmaContext::default_coherent(),
                PAGE_SIZE,
                PAGE_SIZE,
                DmaDirection::ToDevice,
            )
            .map_err(|_| PageTableError::OutOfMemory)?,
        })
    }

    fn paddr(&self) -> usize {
        self.buffer.paddr()
    }

    fn read(&self, index: usize) -> u64 {
        let address = self.buffer.vaddr() + index * core::mem::size_of::<u64>();
        // Safety: 页表页始终为 4 KiB，index 由 9-bit VPN 分量产生，地址按 u64 对齐。
        u64::from_le(unsafe { read_volatile(address as *const u64) })
    }

    fn write(&self, index: usize, value: u64) {
        let address = self.buffer.vaddr() + index * core::mem::size_of::<u64>();
        // Safety: 与 `read` 相同，地址位于当前页表页的有效 PTE 槽位中。
        unsafe { write_volatile(address as *mut u64, value.to_le()) };
        self.buffer.sync_for_device();
    }
}

pub struct PageTable {
    mode: u8,
    levels: usize,
    va_bits: u8,
    pas: u8,
    pages: Vec<PageTablePage>,
}

const fn leaf_flags(writable: bool) -> u64 {
    let flags = PTE_VALID | PTE_READ | PTE_USER | PTE_ACCESSED;
    if writable {
        flags | PTE_WRITE | PTE_DIRTY
    } else {
        flags
    }
}

impl PageTable {
    pub fn new(mode: u8, pas: u8) -> Result<Self, PageTableError> {
        let (levels, va_bits) = match mode {
            8 => (3, 39),
            9 => (4, 48),
            10 => (5, 57),
            _ => return Err(PageTableError::Invalid),
        };
        let mut pages = Vec::new();
        pages
            .try_reserve(1)
            .map_err(|_| PageTableError::OutOfMemory)?;
        pages.push(PageTablePage::new()?);
        Ok(Self {
            mode,
            levels,
            va_bits,
            pas,
            pages,
        })
    }

    pub fn mode(&self) -> u8 {
        self.mode
    }

    pub fn root_paddr(&self) -> usize {
        self.pages[0].paddr()
    }

    pub fn max_iova(&self) -> usize {
        let positive_bits = self.va_bits - 1;
        if positive_bits as u32 >= usize::BITS {
            usize::MAX
        } else {
            (1usize << positive_bits) - 1
        }
    }

    fn physical_address_valid(&self, paddr: usize) -> bool {
        self.pas as u32 >= usize::BITS || paddr < (1usize << self.pas)
    }

    fn page_index_by_paddr(&self, paddr: usize) -> Option<usize> {
        self.pages.iter().position(|page| page.paddr() == paddr)
    }

    fn pte_slot(&mut self, iova: usize, create: bool) -> Result<(usize, usize), PageTableError> {
        if iova > self.max_iova() || !iova.is_multiple_of(PAGE_SIZE) {
            return Err(PageTableError::Invalid);
        }
        let mut page_index = 0usize;
        for level in (1..self.levels).rev() {
            let shift = 12 + level * 9;
            let entry_index = (iova >> shift) & 0x1ff;
            let entry = self.pages[page_index].read(entry_index);
            if entry & PTE_VALID != 0 {
                if pte_is_leaf(entry) || entry & !PTE_PPN_MASK != PTE_VALID {
                    return Err(PageTableError::Corrupt);
                }
                page_index = self
                    .page_index_by_paddr(decode_pte_paddr(entry))
                    .ok_or(PageTableError::Corrupt)?;
                continue;
            }
            if entry != 0 {
                return Err(PageTableError::Corrupt);
            }
            if !create {
                return Err(PageTableError::Invalid);
            }
            self.pages
                .try_reserve(1)
                .map_err(|_| PageTableError::OutOfMemory)?;
            let child = PageTablePage::new()?;
            let child_paddr = child.paddr();
            if !self.physical_address_valid(child_paddr) {
                return Err(PageTableError::Invalid);
            }
            let child_index = self.pages.len();
            self.pages.push(child);
            self.pages[page_index].write(entry_index, encode_ppn(child_paddr) | PTE_VALID);
            page_index = child_index;
        }
        Ok((page_index, (iova >> 12) & 0x1ff))
    }

    pub fn map_page(
        &mut self,
        iova: usize,
        paddr: usize,
        writable: bool,
    ) -> Result<(), PageTableError> {
        if !paddr.is_multiple_of(PAGE_SIZE)
            || !self.physical_address_valid(paddr)
            || !self.physical_address_valid(
                paddr
                    .checked_add(PAGE_SIZE - 1)
                    .ok_or(PageTableError::Invalid)?,
            )
        {
            return Err(PageTableError::Invalid);
        }
        let (page, slot) = self.pte_slot(iova, true)?;
        if self.pages[page].read(slot) != 0 {
            return Err(PageTableError::Conflict);
        }
        // RISC-V IOMMU 对没有 process context/PASID 的普通设备请求按 U-mode
        // 权限检查 S-stage PTE；缺少 U 位时 QEMU 与规范实现都会拒绝 PCI DMA。
        let flags = leaf_flags(writable);
        self.pages[page].write(slot, encode_ppn(paddr) | flags);
        Ok(())
    }

    pub fn unmap_page(&mut self, iova: usize) -> Result<(), PageTableError> {
        let (page, slot) = self.pte_slot(iova, false)?;
        let entry = self.pages[page].read(slot);
        // IOTLB invalidate 失败时 domain 会保留 mapping record 并重试整段 unmap。
        // 已经清零的叶槽必须视为成功，避免一次瞬时 CQ 故障永久卡死 teardown。
        if entry == 0 {
            return Ok(());
        }
        if entry & PTE_VALID == 0 || !pte_is_leaf(entry) {
            return Err(PageTableError::Invalid);
        }
        self.pages[page].write(slot, 0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_device_leaf_pte_is_user_accessible() {
        let read_only = leaf_flags(false);
        assert_ne!(read_only & PTE_USER, 0);
        assert_ne!(read_only & PTE_READ, 0);
        assert_eq!(read_only & (PTE_WRITE | PTE_DIRTY), 0);

        let writable = leaf_flags(true);
        assert_ne!(writable & PTE_USER, 0);
        assert_ne!(writable & (PTE_WRITE | PTE_DIRTY), 0);
    }
}
