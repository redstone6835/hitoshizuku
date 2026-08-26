//! Loongson EIOINTC 的 DT `reg` 与 IOCSR 寄存器布局。
//!
//! `loongson,ls2k2000-eiointc` 通过 LoongArch IOCSR 指令访问寄存器；DT `reg`
//! 描述的是 IOCSR 寄存器编号窗口，不是应交给 `phys_to_virt` 的普通 MMIO 窗口。
//! 本模块把架构手册中的绝对编号改写为相对 DT base 的偏移，使驱动能够真正消费
//! 固件资源，同时集中校验窗口长度与地址溢出。

const IOCSR_ALIGNMENT: usize = 4;

const NODEMAP_OFFSET: usize = 0x0a0;
const IPMAP_OFFSET: usize = 0x0c0;
const ENABLE_OFFSET: usize = 0x200;
const BOUNCE_OFFSET: usize = 0x280;
const ISR_OFFSET: usize = 0x400;
const ROUTE_OFFSET: usize = 0x800;

const ROUTE_TABLE_BYTES: usize = 0x100;
const REQUIRED_WINDOW_SIZE: usize = ROUTE_OFFSET + ROUTE_TABLE_BYTES;

// QEMU LoongArch virt 长期发布 `reg = <0x1400 0x800>`，但同一硬件模型仍在
// 紧邻窗口的 0x1c00..0x1cff 提供 route table。只对这一精确布局扩展有效范围，
// 避免把任意截断固件都静默当成可访问。
const QEMU_LEGACY_BASE: usize = 0x1400;
const QEMU_LEGACY_WINDOW_SIZE: usize = ROUTE_OFFSET;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EioIntcIocsrWindowError {
    UnalignedBase,
    AddressOverflow,
    WindowTooSmall { actual: usize, required: usize },
}

/// 一段已经校验的 EIOINTC IOCSR 编号窗口。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EioIntcIocsrWindow {
    base: usize,
    declared_size: usize,
    qemu_legacy_route_extension: bool,
}

impl EioIntcIocsrWindow {
    pub(crate) fn new(base: usize, declared_size: usize) -> Result<Self, EioIntcIocsrWindowError> {
        if !base.is_multiple_of(IOCSR_ALIGNMENT) {
            return Err(EioIntcIocsrWindowError::UnalignedBase);
        }
        base.checked_add(declared_size)
            .ok_or(EioIntcIocsrWindowError::AddressOverflow)?;

        let qemu_legacy_route_extension =
            base == QEMU_LEGACY_BASE && declared_size == QEMU_LEGACY_WINDOW_SIZE;
        if declared_size < REQUIRED_WINDOW_SIZE && !qemu_legacy_route_extension {
            return Err(EioIntcIocsrWindowError::WindowTooSmall {
                actual: declared_size,
                required: REQUIRED_WINDOW_SIZE,
            });
        }
        base.checked_add(REQUIRED_WINDOW_SIZE)
            .ok_or(EioIntcIocsrWindowError::AddressOverflow)?;

        Ok(Self {
            base,
            declared_size,
            qemu_legacy_route_extension,
        })
    }

    pub(crate) const fn base(self) -> usize {
        self.base
    }

    pub(crate) const fn declared_size(self) -> usize {
        self.declared_size
    }

    pub(crate) const fn uses_qemu_legacy_route_extension(self) -> bool {
        self.qemu_legacy_route_extension
    }

    pub(crate) fn nodemap(self, register: u32) -> usize {
        self.register32(NODEMAP_OFFSET, register)
    }

    pub(crate) fn ipmap(self, register: u32) -> usize {
        self.register32(IPMAP_OFFSET, register)
    }

    pub(crate) fn enable(self, register: u32) -> usize {
        self.register32(ENABLE_OFFSET, register)
    }

    pub(crate) fn bounce(self, register: u32) -> usize {
        self.register32(BOUNCE_OFFSET, register)
    }

    pub(crate) fn isr64(self, register: u32) -> usize {
        self.register64(ISR_OFFSET, register)
    }

    pub(crate) fn route(self, register: u32) -> usize {
        self.register32(ROUTE_OFFSET, register)
    }

    fn register32(self, offset: usize, register: u32) -> usize {
        self.register(offset, register, 4)
    }

    fn register64(self, offset: usize, register: u32) -> usize {
        self.register(offset, register, 8)
    }

    fn register(self, offset: usize, register: u32, width: usize) -> usize {
        let register_offset = (register as usize)
            .checked_mul(width)
            .expect("validated EIOINTC register index must fit usize");
        let relative = offset
            .checked_add(register_offset)
            .expect("validated EIOINTC register index must fit usize");
        let end = relative
            .checked_add(width)
            .expect("validated EIOINTC register width must fit usize");
        debug_assert!(end <= REQUIRED_WINDOW_SIZE);
        self.base
            .checked_add(relative)
            .expect("EIOINTC IOCSR window was checked at construction")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_window_relocates_every_register_family_from_dtb_base() {
        let window = EioIntcIocsrWindow::new(0x2400, 0x900).unwrap();

        assert_eq!(window.nodemap(0), 0x24a0);
        assert_eq!(window.ipmap(1), 0x24c4);
        assert_eq!(window.enable(7), 0x261c);
        assert_eq!(window.bounce(7), 0x269c);
        assert_eq!(window.isr64(3), 0x2818);
        assert_eq!(window.route(63), 0x2cfc);
        assert!(!window.uses_qemu_legacy_route_extension());
    }

    #[test]
    fn qemu_legacy_window_is_the_only_truncated_layout_accepted() {
        let qemu = EioIntcIocsrWindow::new(0x1400, 0x800).unwrap();
        assert_eq!(qemu.route(63), 0x1cfc);
        assert!(qemu.uses_qemu_legacy_route_extension());

        assert_eq!(
            EioIntcIocsrWindow::new(0x2400, 0x800),
            Err(EioIntcIocsrWindowError::WindowTooSmall {
                actual: 0x800,
                required: 0x900,
            })
        );
        assert_eq!(
            EioIntcIocsrWindow::new(0x1400, 0x7ff),
            Err(EioIntcIocsrWindowError::WindowTooSmall {
                actual: 0x7ff,
                required: 0x900,
            })
        );
    }

    #[test]
    fn malformed_iocsr_windows_fail_closed() {
        assert_eq!(
            EioIntcIocsrWindow::new(0x1401, 0x900),
            Err(EioIntcIocsrWindowError::UnalignedBase)
        );
        assert_eq!(
            EioIntcIocsrWindow::new(usize::MAX - 3, 0x900),
            Err(EioIntcIocsrWindowError::AddressOverflow)
        );
    }
}
