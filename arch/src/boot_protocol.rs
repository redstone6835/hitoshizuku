//! LoongArch64 启动协议抽象层。
//!
//! 本模块把"识别固件启动方式 + 执行固件快照"从 loader 的硬编码逻辑中抽出。
//! MyGO 的 LoongArch64 目标只支持 **U-Boot / 传统引导器直启**（无 EFI）：
//!
//! - 启动参数遵循 LoongArch Linux 协议：`$a0/$a1/$a2` 分别为 `efi_boot`
//!   标志（U-Boot 直启为 0）、命令行或 DTB 物理地址、EFI system table
//!   或 DTB 物理地址（U-Boot 直启通常为 0）。
//! - 固件来源只有 DTB：QEMU `-kernel` 直启会经伪 EFI 配置表暴露 FDT；
//!   板载 fork U-Boot 的 `bootm` 显式传 fdt 时，DTB 经 `$a1`/`$a2` 直传，
//!   由 loader 用 FDT magic 探测识别（不硬编码任何板级数据）。
//!
//! 原 EFI 适配器（真 UEFI / QEMU EFI 兼容交接）与 efi_stub 已随 U-Boot 直启
//! 改造删除：内核镜像不再携带 PE 头，EFI Boot Services / ACPI 路径不再支持。

use general::{StartBootProtocol, StartFirmwareSource};

/// 汇编入口 `_start` 保存的原始启动参数（架构归一化）。
///
/// 该结构只保存原始整数，不做任何协议解释；解释交给适配器。
/// 注意：`cmdline_ptr` 为 0 时仍是有效地址（QEMU 直启把 cmdline 放在物理 0）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct BootRegisters {
    /// `$a0`：efi_boot 标志（U-Boot 直启为 0）。
    #[allow(dead_code)]
    pub(crate) efi_boot_flag: usize,
    /// `$a1`：命令行或 DTB 物理地址（0 也可能是有效地址）。
    #[allow(dead_code)]
    pub(crate) cmdline_ptr: usize,
    /// `$a2`：EFI system table 或 DTB 物理地址（0 表示无；QEMU 伪 EFI 交接会提供）。
    #[allow(dead_code)]
    pub(crate) system_table_ptr: usize,
}

impl BootRegisters {
    /// 从 `_start` 传入的三个寄存器参数构造。
    pub(crate) const fn new(a0: usize, a1: usize, a2: usize) -> Self {
        Self {
            efi_boot_flag: a0,
            cmdline_ptr: a1,
            system_table_ptr: a2,
        }
    }
}

/// 快照引擎：由 loader 实现，提供固件快照所需的底层原语。
///
/// 快照依赖 loader 持有的静态缓冲区（`KERNEL_FIRMWARE_BUFFERS`），因此通过
/// 依赖倒置注入给适配器，避免适配器模块直接操作 loader 私有状态。
pub(crate) trait FirmwareSnapshot {
    /// 从物理地址快照 DTB 到内核缓冲区。返回快照成功后的稳定视图。
    ///
    /// 失败返回人类可读错误描述。
    fn snapshot_dtb_from_paddr(&mut self, paddr: usize) -> Result<(), &'static str>;

    /// 读取固件提供的 FDT 指针（无则返回 None）。
    ///
    /// 优先返回 EFI 配置表中的 FDT（QEMU `-kernel` 直启会注册），否则返回
    /// 交接寄存器直传的 DTB（板载 fork U-Boot `bootm` 显式传 fdt）。
    fn efi_fdt_paddr(&self) -> Option<usize>;

    /// 设置本次启动选定的固件来源（本实现固定为 DTB）。
    fn select_firmware_source(&self, source: StartFirmwareSource);
}

/// 协议适配器快照后的统一固件交接。
#[derive(Clone, Copy, Debug)]
pub(crate) struct FirmwareHandoff {
    /// 本次启动使用的有效协议。
    pub(crate) protocol: StartBootProtocol,
    /// 协议适配器的诊断名称。
    pub(crate) adapter: &'static str,
}

impl FirmwareHandoff {
    /// 构造 DTB 来源的交接。
    pub(crate) const fn dtb_source() -> Self {
        Self {
            protocol: StartBootProtocol::LinuxBoot,
            adapter: "u-boot-direct",
        }
    }
}

/// 启动协议分派器：按启动参数识别协议并驱动快照。
///
/// MyGO 的 LoongArch64 只支持 U-Boot / 传统引导器直启，分派恒为
/// LinuxBoot 适配器。`$a0/$a2` 仅保留诊断语义（QEMU 伪 EFI 交接仍能从
/// `$a2` 配置表取得 FDT）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct BootProtocolDispatcher {
    /// `_start` 原始参数快照，仅保留诊断语义（当前分派恒为 LinuxBoot，
    /// 不再读取字段值）。
    #[allow(dead_code)]
    regs: BootRegisters,
}

impl BootProtocolDispatcher {
    /// 从 `_start` 保存的原始参数构造分派器。
    pub(crate) const fn new(regs: BootRegisters) -> Self {
        Self { regs }
    }

    /// 执行 U-Boot 直启快照，产出统一交接。
    ///
    /// DTB 来源：优先取 EFI 配置表暴露的 FDT（QEMU `-kernel` 直启会注册），
    /// 否则使用交接寄存器直传的 DTB（板载 fork U-Boot `bootm` 显式传 fdt）。
    /// 内核按非 EFI 分支用 DT `/memory` 初始化物理内存。
    pub(crate) fn dispatch(
        &self,
        engine: &mut dyn FirmwareSnapshot,
    ) -> Result<FirmwareHandoff, &'static str> {
        if let Some(fdt_paddr) = engine.efi_fdt_paddr() {
            engine.snapshot_dtb_from_paddr(fdt_paddr)?;
        }
        engine.select_firmware_source(StartFirmwareSource::Dtb);
        Ok(FirmwareHandoff::dtb_source())
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    /// 记录快照调用的测试引擎。
    struct RecordingEngine {
        calls: core::cell::RefCell<Vec<&'static str>>,
        dtb_paddr: Option<usize>,
    }

    impl FirmwareSnapshot for RecordingEngine {
        fn snapshot_dtb_from_paddr(&mut self, paddr: usize) -> Result<(), &'static str> {
            self.calls.borrow_mut().push("snapshot_dtb_from_paddr");
            self.dtb_paddr = Some(paddr);
            Ok(())
        }

        fn efi_fdt_paddr(&self) -> Option<usize> {
            self.dtb_paddr
        }

        fn select_firmware_source(&self, _source: StartFirmwareSource) {
            self.calls.borrow_mut().push("select_firmware_source");
        }
    }

    fn engine_with(dtb: Option<usize>) -> RecordingEngine {
        RecordingEngine {
            calls: core::cell::RefCell::new(Vec::new()),
            dtb_paddr: dtb,
        }
    }

    #[test]
    fn linux_boot_uses_config_table_fdt_when_present() {
        // QEMU `-kernel` 直启：a0=1 + 伪 EFI 配置表暴露 FDT。
        let regs = BootRegisters::new(1, 0, 0x200);
        let mut engine = engine_with(Some(0x9000_0000));
        let handoff = BootProtocolDispatcher::new(regs)
            .dispatch(&mut engine)
            .unwrap();
        assert_eq!(handoff.protocol, StartBootProtocol::LinuxBoot);
        let calls = engine.calls.into_inner();
        assert!(calls.contains(&"snapshot_dtb_from_paddr"));
        assert!(calls.contains(&"select_firmware_source"));
    }

    #[test]
    fn linux_boot_without_fdt_is_ok() {
        // 板载 U-Boot 直启：无 EFI 配置表，DTB 由 loader 从交接探测。
        let regs = BootRegisters::new(0, 0, 0);
        let mut engine = engine_with(None);
        let handoff = BootProtocolDispatcher::new(regs)
            .dispatch(&mut engine)
            .unwrap();
        assert_eq!(handoff.protocol, StartBootProtocol::LinuxBoot);
        let calls = engine.calls.into_inner();
        assert!(!calls.contains(&"snapshot_dtb_from_paddr"));
        assert!(calls.contains(&"select_firmware_source"));
    }

    #[test]
    fn regs_keep_raw_view() {
        let regs = BootRegisters::new(0, 0, 0);
        assert_eq!(regs.cmdline_ptr, 0);
        assert_eq!(regs.system_table_ptr, 0);
        assert_eq!(regs.efi_boot_flag, 0);
    }
}
