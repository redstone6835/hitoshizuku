//! LoongArch64 启动协议抽象层。
//!
//! 本模块把"识别固件启动方式 + 执行固件快照"从 loader 的硬编码逻辑中抽出，
//! 改为可拓展的协议适配器集合。当前支持两种协议：
//!
//! - **EFI**：真 UEFI 加载（经 `efi_pe_entry`）或 QEMU `-kernel` 直启使用的
//!   EFI 兼容配置表交接。固件来源可能是 ACPI 或 DTB。
//! - **Linux 直启**：u-boot 等传统引导器直接跳入内核入口，无 EFI 系统表，
//!   固件信息唯一来自 DTB。
//!
//! ## 可拓展性
//!
//! 新增启动协议只需：
//! 1. 给 [`BootProtocolAdapter`] 加一个变体；
//! 2. 实现该适配器的 [`BootProtocolAdapter::snapshot`]；
//! 3. 在 loader 侧把新适配器接入 `__kernel_arch_loader` 的快照编排。
//!
//! loader 只负责"识别 + 分派 + 消费交接"，快照细节收敛在适配器内。
//!
//! 启动参数语义遵循 LoongArch Linux 协议：`$a0/$a1/$a2` 分别为 `efi_boot`
//! 标志、命令行物理地址、EFI 系统表物理地址（或 0）。

use general::{StartBootProtocol, StartFirmwareSource};

/// 汇编入口 `_start` 保存的原始启动参数（架构归一化）。
///
/// 该结构只保存原始整数，不做任何协议解释；解释交给适配器。
/// 注意：`cmdline_ptr` 为 0 时仍是有效地址（QEMU 直启把 cmdline 放在物理 0）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct BootRegisters {
    /// `$a0`：efi_boot 标志。
    pub(crate) efi_boot_flag: usize,
    /// `$a1`：命令行物理地址（0 也可能是有效地址）。
    ///
    /// 当前协议识别只依赖 `system_table_ptr`/`efi_boot_flag`；cmdline 由 loader
    /// 经 `CMDLINE_PTR` atomic 直接驱动，此字段保留完整原始视图，供未来
    /// Linux 直启适配器与诊断使用。
    #[allow(dead_code)]
    pub(crate) cmdline_ptr: usize,
    /// `$a2`：EFI 系统表物理地址（0 表示无）。
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

    /// 是否存在 EFI 系统表。
    pub(crate) const fn has_efi_system_table(&self) -> bool {
        self.system_table_ptr != 0
    }

    /// efi_boot 标志是否置位（诊断语义：固件声明 EFI 兼容交接）。
    pub(crate) const fn efi_boot_declared(&self) -> bool {
        self.efi_boot_flag != 0
    }
}

/// 快照引擎：由 loader 实现，提供固件快照所需的底层原语。
///
/// 快照依赖 loader 持有的静态缓冲区（`KERNEL_FIRMWARE_BUFFERS`）和 EFI 调用，
/// 因此通过依赖倒置注入给适配器，避免适配器模块直接操作 loader 私有状态。
pub(crate) trait FirmwareSnapshot {
    /// 从物理地址快照 DTB 到内核缓冲区。返回快照成功后的稳定视图。
    ///
    /// 失败返回人类可读错误描述。
    fn snapshot_dtb_from_paddr(&mut self, paddr: usize) -> Result<(), &'static str>;

    /// 尝试退出 EFI Boot Services 并快照内存映射。
    ///
    /// 返回 EFI 内存映射来源；QEMU 伪 EFI 交接无 Boot Services 时可能返回
    /// `None`（无可用内存映射）。
    fn exit_efi_boot_services(&mut self) -> Option<EfiMemoryMapSource>;

    /// 从 RSDP 物理地址快照全部 ACPI 表到内核缓冲区。
    ///
    /// 返回 RSDP 的物理地址（用于 StartAcpiTables 标识）。
    fn snapshot_acpi_from_rsdp(&mut self, rsdp_paddr: usize) -> Result<usize, &'static str>;

    /// 读取 EFI 配置表中的 RSDP 指针（无则返回 None）。
    fn efi_acpi_rsdp(&self) -> Option<usize>;

    /// 读取 EFI 配置表中的 FDT 指针（无则返回 None）。
    fn efi_fdt_paddr(&self) -> Option<usize>;

    /// 设置本次启动选定的固件来源（ACPI 或 DTB）。
    fn select_firmware_source(&self, source: StartFirmwareSource);
}

/// EFI 内存映射来源（由 efi_stub 定义，这里 re-export 供适配器使用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EfiMemoryMapSource {
    /// 成功执行 ExitBootServices 后取得的内存图（RAM 权威来源）。
    BootServicesExited,
    /// 仅观察，Boot Services 仍活跃（不能作为 RAM 权威来源）。
    #[allow(dead_code)]
    BootServicesActive,
}

/// 协议适配器快照后的统一固件交接。
#[derive(Clone, Copy, Debug)]
pub(crate) struct FirmwareHandoff {
    /// 选定的固件来源（ACPI 或 DTB）。
    pub(crate) source: StartFirmwareSource,
    /// 本次启动使用的有效协议。
    pub(crate) protocol: StartBootProtocol,
    /// 协议适配器的诊断名称。
    pub(crate) adapter: BootProtocolAdapter,
    /// ACPI 快照后的 RSDP 物理地址（仅 ACPI 来源）。
    pub(crate) acpi_rsdp: usize,
    /// EFI 内存映射来源（仅 EFI 协议且成功取得映射时）。
    pub(crate) memory_map_source: Option<EfiMemoryMapSource>,
}

impl FirmwareHandoff {
    /// 构造 DTB 来源的交接。
    pub(crate) const fn dtb_source(adapter: BootProtocolAdapter) -> Self {
        Self {
            source: StartFirmwareSource::Dtb,
            protocol: match adapter {
                BootProtocolAdapter::Efi => StartBootProtocol::Efi,
                BootProtocolAdapter::LinuxBoot => StartBootProtocol::LinuxBoot,
            },
            adapter,
            acpi_rsdp: 0,
            memory_map_source: None,
        }
    }

    /// 构造 ACPI 来源的交接。
    pub(crate) const fn acpi_source(
        acpi_rsdp: usize,
        memory_map_source: Option<EfiMemoryMapSource>,
    ) -> Self {
        Self {
            source: StartFirmwareSource::Acpi,
            protocol: StartBootProtocol::Efi,
            adapter: BootProtocolAdapter::Efi,
            acpi_rsdp,
            memory_map_source,
        }
    }
}

/// 启动协议适配器。
///
/// 每个变体对应一种固件启动方式。新增协议时在此加变体，并实现
/// [`BootProtocolAdapter::snapshot`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BootProtocolAdapter {
    /// 真 UEFI 或 QEMU EFI 兼容交接。
    Efi,
    /// u-boot 等传统引导器直接跳入（无 EFI）。
    LinuxBoot,
}

impl BootProtocolAdapter {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Efi => "efi",
            Self::LinuxBoot => "linux-boot",
        }
    }

    /// 依据适配器语义执行固件快照，产出统一交接。
    ///
    /// `engine` 提供底层快照原语（DTB/ACPI 复制、EFI Boot Services 退出）。
    pub(crate) fn snapshot(
        self,
        regs: &BootRegisters,
        engine: &mut dyn FirmwareSnapshot,
    ) -> Result<FirmwareHandoff, &'static str> {
        match self {
            Self::Efi => efi_snapshot(regs, engine),
            Self::LinuxBoot => linux_boot_snapshot(regs, engine),
        }
    }
}

/// EFI 协议快照：退出 Boot Services，优先 ACPI，否则 DTB。
fn efi_snapshot(
    _regs: &BootRegisters,
    engine: &mut dyn FirmwareSnapshot,
) -> Result<FirmwareHandoff, &'static str> {
    let memory_map_source = engine.exit_efi_boot_services();

    if let Some(rsdp) = engine.efi_acpi_rsdp() {
        // ACPI 路径需要完整内存映射来初始化分配器。
        if memory_map_source != Some(EfiMemoryMapSource::BootServicesExited) {
            return Err("[boot][efi] ACPI discovered but ExitBootServices memory map unavailable");
        }
        let rsdp_phys = engine.snapshot_acpi_from_rsdp(rsdp)?;
        engine.select_firmware_source(StartFirmwareSource::Acpi);
        return Ok(FirmwareHandoff::acpi_source(rsdp_phys, memory_map_source));
    }

    if let Some(fdt_paddr) = engine.efi_fdt_paddr() {
        engine.snapshot_dtb_from_paddr(fdt_paddr)?;
        engine.select_firmware_source(StartFirmwareSource::Dtb);
        return Ok(FirmwareHandoff::dtb_source(BootProtocolAdapter::Efi));
    }

    Err("[boot][efi] neither ACPI nor DTB found in EFI configuration tables")
}

/// Linux 直启协议快照：无 EFI 系统表，固件信息唯一来自 DTB。
///
/// DTB 来源：若 EFI 配置表可用（QEMU 直启也注册），优先取配置表 FDT；
/// 否则由 bootloader handoff 路径提供（u-boot 场景）。内核按非 EFI 分支
/// 用 DT `/memory` 初始化物理内存。
fn linux_boot_snapshot(
    _regs: &BootRegisters,
    engine: &mut dyn FirmwareSnapshot,
) -> Result<FirmwareHandoff, &'static str> {
    // 优先取 EFI 配置表暴露的 FDT（QEMU `-kernel` 直启也会注册）。
    if let Some(fdt_paddr) = engine.efi_fdt_paddr() {
        engine.snapshot_dtb_from_paddr(fdt_paddr)?;
    }
    // u-boot 直启：DTB 由 bootloader handoff 提供（当前 u-boot 适配未接入时
    // 由后续路径快照）。这里只声明固件源，避免过早失败。
    engine.select_firmware_source(StartFirmwareSource::Dtb);
    Ok(FirmwareHandoff::dtb_source(BootProtocolAdapter::LinuxBoot))
}

/// 启动协议分派器：按启动参数识别协议并驱动快照。
#[derive(Clone, Copy, Debug)]
pub(crate) struct BootProtocolDispatcher {
    regs: BootRegisters,
}

impl BootProtocolDispatcher {
    /// 从 `_start` 保存的原始参数构造分派器。
    pub(crate) const fn new(regs: BootRegisters) -> Self {
        Self { regs }
    }

    /// 选择本次启动使用的适配器。
    ///
    /// 识别规则（与 Linux LoongArch 约定一致）：
    /// - `$a2`（EFI 系统表指针）非零，或 `$a0`（efi_boot 标志）置位 → EFI 协议；
    /// - 否则 → Linux 直启协议（u-boot 等）。
    pub(crate) const fn select(&self) -> BootProtocolAdapter {
        if self.regs.has_efi_system_table() || self.regs.efi_boot_declared() {
            BootProtocolAdapter::Efi
        } else {
            BootProtocolAdapter::LinuxBoot
        }
    }

    /// 依据适配器语义执行固件快照，产出统一交接。
    pub(crate) fn dispatch(
        &self,
        engine: &mut dyn FirmwareSnapshot,
    ) -> Result<FirmwareHandoff, &'static str> {
        self.select().snapshot(&self.regs, engine)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    #[test]
    fn efi_system_table_selects_efi_adapter() {
        let regs = BootRegisters::new(1, 0, 0x200);
        assert_eq!(
            BootProtocolDispatcher::new(regs).select(),
            BootProtocolAdapter::Efi
        );
    }

    #[test]
    fn zero_system_table_selects_linux_boot() {
        let regs = BootRegisters::new(0, 0, 0);
        assert_eq!(
            BootProtocolDispatcher::new(regs).select(),
            BootProtocolAdapter::LinuxBoot
        );
    }

    #[test]
    fn efi_boot_flag_alone_selects_efi() {
        // a0=1 但 a2=0：只有 efi_boot 标志，没有系统表指针。仍按 EFI 处理。
        let regs = BootRegisters::new(1, 0, 0);
        assert_eq!(
            BootProtocolDispatcher::new(regs).select(),
            BootProtocolAdapter::Efi
        );
    }

    #[test]
    fn linux_boot_keeps_zero_cmdline_ptr_as_valid() {
        // 0 cmdline 不改变协议识别；识别只依赖 $a2。
        let regs = BootRegisters::new(0, 0, 0);
        assert_eq!(
            BootProtocolDispatcher::new(regs).select(),
            BootProtocolAdapter::LinuxBoot
        );
        assert_eq!(regs.cmdline_ptr, 0);
    }

    #[test]
    fn adapter_names_are_stable() {
        assert_eq!(BootProtocolAdapter::Efi.name(), "efi");
        assert_eq!(BootProtocolAdapter::LinuxBoot.name(), "linux-boot");
    }

    /// 记录快照调用的测试引擎。
    struct RecordingEngine {
        calls: core::cell::RefCell<Vec<&'static str>>,
        dtb_paddr: Option<usize>,
        rsdp: Option<usize>,
    }

    impl FirmwareSnapshot for RecordingEngine {
        fn snapshot_dtb_from_paddr(&mut self, paddr: usize) -> Result<(), &'static str> {
            self.calls.borrow_mut().push("snapshot_dtb_from_paddr");
            self.dtb_paddr = Some(paddr);
            Ok(())
        }

        fn exit_efi_boot_services(&mut self) -> Option<EfiMemoryMapSource> {
            self.calls.borrow_mut().push("exit_efi_boot_services");
            Some(EfiMemoryMapSource::BootServicesExited)
        }

        fn snapshot_acpi_from_rsdp(&mut self, rsdp: usize) -> Result<usize, &'static str> {
            self.calls.borrow_mut().push("snapshot_acpi_from_rsdp");
            Ok(rsdp)
        }

        fn efi_acpi_rsdp(&self) -> Option<usize> {
            self.rsdp
        }

        fn efi_fdt_paddr(&self) -> Option<usize> {
            self.dtb_paddr
        }

        fn select_firmware_source(&self, _source: StartFirmwareSource) {
            self.calls.borrow_mut().push("select_firmware_source");
        }
    }

    fn engine_with(dtb: Option<usize>, rsdp: Option<usize>) -> RecordingEngine {
        RecordingEngine {
            calls: core::cell::RefCell::new(Vec::new()),
            dtb_paddr: dtb,
            rsdp,
        }
    }

    #[test]
    fn efi_adapter_prioritizes_acpi_when_rsdp_present() {
        let regs = BootRegisters::new(1, 0, 0x200);
        let mut engine = engine_with(None, Some(0x7ff0_0000));
        let handoff = BootProtocolDispatcher::new(regs)
            .dispatch(&mut engine)
            .unwrap();
        assert_eq!(handoff.source, StartFirmwareSource::Acpi);
        assert_eq!(handoff.protocol, StartBootProtocol::Efi);
        assert_eq!(handoff.adapter, BootProtocolAdapter::Efi);
        assert_eq!(handoff.acpi_rsdp, 0x7ff0_0000);
        assert_eq!(
            handoff.memory_map_source,
            Some(EfiMemoryMapSource::BootServicesExited)
        );
        let calls = engine.calls.into_inner();
        assert!(calls.contains(&"exit_efi_boot_services"));
        assert!(calls.contains(&"snapshot_acpi_from_rsdp"));
    }

    #[test]
    fn efi_adapter_falls_back_to_dtb_when_no_rsdp() {
        let regs = BootRegisters::new(1, 0, 0x200);
        let mut engine = engine_with(Some(0x9000_0000), None);
        let handoff = BootProtocolDispatcher::new(regs)
            .dispatch(&mut engine)
            .unwrap();
        assert_eq!(handoff.source, StartFirmwareSource::Dtb);
        assert_eq!(handoff.protocol, StartBootProtocol::Efi);
        let calls = engine.calls.into_inner();
        assert!(calls.contains(&"snapshot_dtb_from_paddr"));
        assert!(!calls.contains(&"snapshot_acpi_from_rsdp"));
    }

    #[test]
    fn linux_boot_adapter_uses_dtb_source() {
        let regs = BootRegisters::new(0, 0, 0);
        let mut engine = engine_with(Some(0x9000_0000), None);
        let handoff = BootProtocolDispatcher::new(regs)
            .dispatch(&mut engine)
            .unwrap();
        assert_eq!(handoff.source, StartFirmwareSource::Dtb);
        assert_eq!(handoff.protocol, StartBootProtocol::LinuxBoot);
        assert_eq!(handoff.adapter, BootProtocolAdapter::LinuxBoot);
        let calls = engine.calls.into_inner();
        assert!(calls.contains(&"snapshot_dtb_from_paddr"));
        assert!(calls.contains(&"select_firmware_source"));
    }

    #[test]
    fn efi_adapter_rejects_acpi_without_exited_boot_services() {
        // 有 RSDP 但内存映射未完成 ExitBootServices：ACPI 路径必须拒绝。
        let regs = BootRegisters::new(1, 0, 0x200);
        struct NoExitEngine;
        impl FirmwareSnapshot for NoExitEngine {
            fn snapshot_dtb_from_paddr(&mut self, _p: usize) -> Result<(), &'static str> {
                Ok(())
            }
            fn exit_efi_boot_services(&mut self) -> Option<EfiMemoryMapSource> {
                None
            }
            fn snapshot_acpi_from_rsdp(&mut self, _r: usize) -> Result<usize, &'static str> {
                Ok(_r)
            }
            fn efi_acpi_rsdp(&self) -> Option<usize> {
                Some(0x7ff0_0000)
            }
            fn efi_fdt_paddr(&self) -> Option<usize> {
                None
            }
            fn select_firmware_source(&self, _s: StartFirmwareSource) {}
        }
        let mut engine = NoExitEngine;
        assert!(
            BootProtocolDispatcher::new(regs)
                .dispatch(&mut engine)
                .is_err()
        );
    }
}
