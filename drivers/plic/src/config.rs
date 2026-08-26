//! PLIC DT binding 与寄存器窗口的纯逻辑校验。

extern crate alloc;

use alloc::vec::Vec;

pub(crate) const PLIC_PRIORITY_BASE: usize = 0x000000;
pub(crate) const PLIC_PENDING_BASE: usize = 0x001000;
pub(crate) const PLIC_ENABLE_BASE: usize = 0x002000;
pub(crate) const PLIC_ENABLE_STRIDE: usize = 0x80;
pub(crate) const PLIC_THRESHOLD_BASE: usize = 0x200000;
pub(crate) const PLIC_CLAIM_BASE: usize = 0x200004;
pub(crate) const PLIC_CONTEXT_STRIDE: usize = 0x1000;

const REGISTER_WIDTH: usize = core::mem::size_of::<u32>();
const MAX_NDEV: u32 = (PLIC_ENABLE_STRIDE * u8::BITS as usize - 1) as u32;
const RISCV_SUPERVISOR_EXTERNAL_IRQ: u32 = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlicConfigError {
    MissingNdev,
    MalformedNdev,
    InvalidNdev,
    MissingInterruptContexts,
    MalformedInterruptContext,
    UnknownSupervisorContextCpu,
    MissingBootSupervisorContext,
    DuplicateSupervisorContext,
    OutOfMemory,
    UnalignedMmio,
    AddressOverflow,
    MmioWindowTooSmall,
}

pub(crate) fn parse_ndev(raw: Option<&[u8]>) -> Result<u32, PlicConfigError> {
    let raw = raw.ok_or(PlicConfigError::MissingNdev)?;
    let bytes: [u8; 4] = raw.try_into().map_err(|_| PlicConfigError::MalformedNdev)?;
    let ndev = u32::from_be_bytes(bytes);
    if ndev == 0 || ndev > MAX_NDEV {
        return Err(PlicConfigError::InvalidNdev);
    }
    Ok(ndev)
}

#[derive(Clone, Copy)]
pub(crate) struct PlicInterruptContext<'a> {
    pub(crate) controller: Option<u32>,
    pub(crate) cells: &'a [u32],
}

/// 一个由 `interrupts-extended` tuple 绑定到逻辑 CPU 的 S-mode context。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlicSupervisorContext {
    pub(crate) logical_cpu: usize,
    pub(crate) hart_id: u64,
    pub(crate) context: usize,
}

/// 按 CPU interrupt-controller provider 身份收集全部 S-mode context。
pub(crate) fn select_supervisor_contexts<'a, I, F, G>(
    contexts: I,
    boot_hart_id: u64,
    mut cpu_reg_for_controller: F,
    mut cpu_logical_id_for_controller: G,
) -> Result<Vec<PlicSupervisorContext>, PlicConfigError>
where
    I: IntoIterator<Item = PlicInterruptContext<'a>>,
    F: FnMut(u32) -> Option<u64>,
    G: FnMut(u32) -> Option<usize>,
{
    let mut saw_context = false;
    let mut found_boot = false;
    let mut selected = Vec::new();
    for (index, context) in contexts.into_iter().enumerate() {
        saw_context = true;
        let controller = context
            .controller
            .ok_or(PlicConfigError::MalformedInterruptContext)?;
        let [interrupt] = context.cells else {
            return Err(PlicConfigError::MalformedInterruptContext);
        };
        if *interrupt != RISCV_SUPERVISOR_EXTERNAL_IRQ {
            continue;
        }
        let hart_id = cpu_reg_for_controller(controller)
            .ok_or(PlicConfigError::UnknownSupervisorContextCpu)?;
        let logical_cpu = cpu_logical_id_for_controller(controller)
            .ok_or(PlicConfigError::UnknownSupervisorContextCpu)?;
        if selected
            .iter()
            .any(|entry: &PlicSupervisorContext| entry.logical_cpu == logical_cpu)
        {
            return Err(PlicConfigError::DuplicateSupervisorContext);
        }
        selected
            .try_reserve(1)
            .map_err(|_| PlicConfigError::OutOfMemory)?;
        selected.push(PlicSupervisorContext {
            logical_cpu,
            hart_id,
            context: index,
        });
        found_boot |= hart_id == boot_hart_id;
    }
    if !saw_context {
        return Err(PlicConfigError::MissingInterruptContexts);
    }
    if !found_boot {
        return Err(PlicConfigError::MissingBootSupervisorContext);
    }
    selected.sort_unstable_by_key(|entry| entry.logical_cpu);
    Ok(selected)
}

/// 已验证的 PLIC 寄存器布局。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlicLayout {
    ndev: u32,
    context: usize,
    source_words: usize,
    required_window_size: usize,
}

impl PlicLayout {
    pub(crate) fn new(ndev: u32, context: usize) -> Result<Self, PlicConfigError> {
        if ndev == 0 || ndev > MAX_NDEV {
            return Err(PlicConfigError::InvalidNdev);
        }
        let source_words = (ndev as usize / u32::BITS as usize)
            .checked_add(1)
            .ok_or(PlicConfigError::AddressOverflow)?;
        let priority_end = (ndev as usize)
            .checked_add(1)
            .and_then(|count| count.checked_mul(REGISTER_WIDTH))
            .and_then(|bytes| PLIC_PRIORITY_BASE.checked_add(bytes))
            .ok_or(PlicConfigError::AddressOverflow)?;
        let bitmap_bytes = source_words
            .checked_mul(REGISTER_WIDTH)
            .ok_or(PlicConfigError::AddressOverflow)?;
        if bitmap_bytes > PLIC_ENABLE_STRIDE {
            return Err(PlicConfigError::InvalidNdev);
        }
        let pending_end = PLIC_PENDING_BASE
            .checked_add(bitmap_bytes)
            .ok_or(PlicConfigError::AddressOverflow)?;
        let context_enable = context
            .checked_mul(PLIC_ENABLE_STRIDE)
            .and_then(|offset| PLIC_ENABLE_BASE.checked_add(offset))
            .ok_or(PlicConfigError::AddressOverflow)?;
        let enable_end = context_enable
            .checked_add(bitmap_bytes)
            .ok_or(PlicConfigError::AddressOverflow)?;
        let context_registers = context
            .checked_mul(PLIC_CONTEXT_STRIDE)
            .ok_or(PlicConfigError::AddressOverflow)?;
        let claim_end = PLIC_CLAIM_BASE
            .checked_add(context_registers)
            .and_then(|offset| offset.checked_add(REGISTER_WIDTH))
            .ok_or(PlicConfigError::AddressOverflow)?;
        let required_window_size = priority_end.max(pending_end).max(enable_end).max(claim_end);
        Ok(Self {
            ndev,
            context,
            source_words,
            required_window_size,
        })
    }

    pub(crate) fn validate_window(
        self,
        phys: usize,
        size: usize,
        virt: usize,
    ) -> Result<(), PlicConfigError> {
        if !phys.is_multiple_of(REGISTER_WIDTH) || !virt.is_multiple_of(REGISTER_WIDTH) {
            return Err(PlicConfigError::UnalignedMmio);
        }
        phys.checked_add(size)
            .ok_or(PlicConfigError::AddressOverflow)?;
        if size < self.required_window_size {
            return Err(PlicConfigError::MmioWindowTooSmall);
        }
        virt.checked_add(self.required_window_size)
            .ok_or(PlicConfigError::AddressOverflow)?;
        Ok(())
    }

    pub(crate) const fn source_words(self) -> usize {
        self.source_words
    }

    pub(crate) fn priority_offset(self, hwirq: u32) -> Option<usize> {
        (hwirq <= self.ndev).then(|| PLIC_PRIORITY_BASE + hwirq as usize * REGISTER_WIDTH)
    }

    pub(crate) fn enable_word_offset(self, word: usize) -> Option<usize> {
        (word < self.source_words)
            .then(|| PLIC_ENABLE_BASE + self.context * PLIC_ENABLE_STRIDE + word * REGISTER_WIDTH)
    }

    pub(crate) fn enable_offset(self, hwirq: u32) -> Option<(usize, u32)> {
        if hwirq == 0 || hwirq > self.ndev {
            return None;
        }
        let word = hwirq as usize / u32::BITS as usize;
        Some((self.enable_word_offset(word)?, hwirq % u32::BITS))
    }

    pub(crate) const fn threshold_offset(self) -> usize {
        PLIC_THRESHOLD_BASE + self.context * PLIC_CONTEXT_STRIDE
    }

    pub(crate) const fn claim_offset(self) -> usize {
        PLIC_CLAIM_BASE + self.context * PLIC_CONTEXT_STRIDE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_hart_ids_use_provider_identity_and_tuple_index() {
        let machine = [11];
        let supervisor = [9];
        let contexts = [
            PlicInterruptContext {
                controller: Some(0x10),
                cells: &machine,
            },
            PlicInterruptContext {
                controller: Some(0x10),
                cells: &supervisor,
            },
            PlicInterruptContext {
                controller: Some(0x20),
                cells: &machine,
            },
            PlicInterruptContext {
                controller: Some(0x20),
                cells: &supervisor,
            },
        ];

        let contexts = select_supervisor_contexts(
            contexts,
            42,
            |controller| match controller {
                0x10 => Some(7),
                0x20 => Some(42),
                _ => None,
            },
            |controller| match controller {
                0x10 => Some(4),
                0x20 => Some(1),
                _ => None,
            },
        )
        .unwrap();
        assert_eq!(
            contexts,
            [
                PlicSupervisorContext {
                    logical_cpu: 1,
                    hart_id: 42,
                    context: 3,
                },
                PlicSupervisorContext {
                    logical_cpu: 4,
                    hart_id: 7,
                    context: 1,
                },
            ]
        );
    }

    #[test]
    fn context_selection_rejects_malformed_and_duplicate_cpu_entries() {
        let supervisor = [9];
        assert_eq!(
            select_supervisor_contexts(
                [PlicInterruptContext {
                    controller: None,
                    cells: &supervisor,
                }],
                0,
                |_| Some(0),
                |_| Some(0),
            ),
            Err(PlicConfigError::MalformedInterruptContext)
        );
        let duplicate = [
            PlicInterruptContext {
                controller: Some(1),
                cells: &supervisor,
            },
            PlicInterruptContext {
                controller: Some(2),
                cells: &supervisor,
            },
        ];
        assert_eq!(
            select_supervisor_contexts(duplicate, 7, |_| Some(7), |_| Some(0)),
            Err(PlicConfigError::DuplicateSupervisorContext)
        );
    }

    #[test]
    fn context_selection_requires_known_cpu_and_boot_hart() {
        let supervisor = [9];
        let context = [PlicInterruptContext {
            controller: Some(1),
            cells: &supervisor,
        }];
        assert_eq!(
            select_supervisor_contexts(context, 0, |_| None, |_| Some(0)),
            Err(PlicConfigError::UnknownSupervisorContextCpu)
        );
        assert_eq!(
            select_supervisor_contexts(context, 0, |_| Some(7), |_| Some(0)),
            Err(PlicConfigError::MissingBootSupervisorContext)
        );
    }

    #[test]
    fn ndev_is_required_single_cell_and_bounded_by_enable_stride() {
        assert_eq!(parse_ndev(None), Err(PlicConfigError::MissingNdev));
        assert_eq!(
            parse_ndev(Some(&[0, 1])),
            Err(PlicConfigError::MalformedNdev)
        );
        assert_eq!(
            parse_ndev(Some(&0u32.to_be_bytes())),
            Err(PlicConfigError::InvalidNdev)
        );
        assert_eq!(parse_ndev(Some(&95u32.to_be_bytes())), Ok(95));
        assert_eq!(
            parse_ndev(Some(&1024u32.to_be_bytes())),
            Err(PlicConfigError::InvalidNdev)
        );
    }

    #[test]
    fn mmio_window_covers_priority_pending_enable_and_context_registers() {
        let layout = PlicLayout::new(95, 3).unwrap();
        assert_eq!(layout.priority_offset(95), Some(0x17c));
        assert_eq!(layout.enable_offset(95), Some((0x2188, 31)));
        assert_eq!(layout.threshold_offset(), 0x203000);
        assert_eq!(layout.claim_offset(), 0x203004);
        assert_eq!(
            layout.validate_window(0x0c00_0000, 0x203008, 0x4000_0000),
            Ok(())
        );
        assert_eq!(
            layout.validate_window(0x0c00_0000, 0x203007, 0x4000_0000),
            Err(PlicConfigError::MmioWindowTooSmall)
        );
    }
}
