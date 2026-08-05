//! IMSIC per-hart identity 与通用 hwirq 的稳定映射。

/// 将每个 interrupt file 的本地 identity 空间展开成 controller-wide hwirq。
///
/// `hwirq` 只在当前 IMSIC controller 生命周期内稳定；MSI message data 仍使用
/// interrupt file 的本地 identity，因此不同 hart 可以安全复用相同 identity。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImsicVectorLayout {
    files: usize,
    num_ids: u32,
    stride: usize,
    capacity: usize,
}

impl ImsicVectorLayout {
    pub(crate) fn new(files: usize, num_ids: u32) -> Option<Self> {
        if files == 0 || num_ids == 0 {
            return None;
        }
        let stride = num_ids as usize + 1;
        let slots = files.checked_mul(stride)?;
        u32::try_from(slots.checked_sub(1)?).ok()?;
        Some(Self {
            files,
            num_ids,
            stride,
            capacity: files.checked_mul(num_ids as usize)?,
        })
    }

    pub(crate) const fn capacity(self) -> usize {
        self.capacity
    }

    pub(crate) fn slot(self, file_index: usize, id: u32) -> Option<usize> {
        if file_index >= self.files || id == 0 || id > self.num_ids {
            return None;
        }
        file_index
            .checked_mul(self.stride)?
            .checked_add(id as usize)
    }

    pub(crate) fn hwirq(self, file_index: usize, id: u32) -> Option<u32> {
        u32::try_from(self.slot(file_index, id)?).ok()
    }

    pub(crate) fn decode(self, hwirq: u32) -> Option<(usize, u32, usize)> {
        let slot = hwirq as usize;
        let file_index = slot / self.stride;
        let id = u32::try_from(slot % self.stride).ok()?;
        self.slot(file_index, id)
            .filter(|expected| *expected == slot)
            .map(|slot| (file_index, id, slot))
    }

    pub(crate) fn ordinal(self, ordinal: usize) -> Option<(usize, u32, usize, u32)> {
        if ordinal >= self.capacity {
            return None;
        }
        let file_index = ordinal % self.files;
        let id = u32::try_from(ordinal / self.files).ok()?.checked_add(1)?;
        let slot = self.slot(file_index, id)?;
        Some((file_index, id, slot, u32::try_from(slot).ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_hart_identities_expand_to_unique_hwirqs() {
        let layout = ImsicVectorLayout::new(2, 63).unwrap();
        assert_eq!(layout.capacity(), 126);
        assert_eq!(layout.ordinal(0), Some((0, 1, 1, 1)));
        assert_eq!(layout.ordinal(1), Some((1, 1, 65, 65)));
        assert_eq!(layout.ordinal(2), Some((0, 2, 2, 2)));
        assert_eq!(layout.decode(65), Some((1, 1, 65)));
        assert_eq!(layout.decode(64), None);
    }

    #[test]
    fn namespace_bounds_are_checked_before_allocation() {
        assert!(ImsicVectorLayout::new(0, 63).is_none());
        assert!(ImsicVectorLayout::new(1, 0).is_none());
        assert!(ImsicVectorLayout::new(usize::MAX, 2047).is_none());
    }
}
