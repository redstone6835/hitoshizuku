//! x86_64 启动交接的安全框架。
//!
//! 真正的入口（Multiboot2 保护模式入口、UEFI PE 入口和 Linux handover
//! 入口）需要各自的汇编/页表初始化，不能用一个空函数冒充。此模块提供
//! 它们共用的“解析后交接”接口：入口适配器负责把协议数据复制到稳定的
//! 静态缓冲区，再调用 [`build_start_context`]；该函数会构造并校验统一的
//! `StartContext`，任何缺失的固件或地址转换能力都会显式返回错误。

use general::{
    StartAddressOps, StartAllocatorOps, StartArchitecture, StartBootInfo, StartContext,
    StartFirmware, StartMemory, StartPhysRange,
};

use super::boot_protocol::{X86BootProtocol, start_boot_info};

/// 由 x86 入口适配器固化后的启动元数据。
#[derive(Clone, Copy, Debug)]
pub struct X86StartMetadata {
    /// 解析后的入口协议。
    pub protocol: X86BootProtocol,
    /// boot CPU 的硬件 ID（通常为 Local APIC ID）。
    pub boot_cpu_id: usize,
    /// 已复制到内核静态存储的命令行。
    pub command_line: Option<&'static [u8]>,
}

impl X86StartMetadata {
    /// 转换为通用启动信息。
    pub const fn boot_info(self) -> StartBootInfo {
        start_boot_info(
            StartArchitecture::X86_64,
            self.protocol,
            self.boot_cpu_id,
            self.command_line,
        )
    }
}

/// 构造并校验 x86 的统一启动上下文。
///
/// 该函数不访问任何硬件，也不猜测 ACPI、E820 或 EFI 数据。调用者必须
/// 提供已稳定化的 `StartFirmware`、内核物理范围、地址转换函数和（如需）
/// allocator 回调；校验失败时返回错误，入口代码应停止启动而不是继续。
pub fn build_start_context(
    metadata: X86StartMetadata,
    firmware: StartFirmware,
    kernel_image: StartPhysRange,
    boot_map: general::StartMemoryMap,
    address: StartAddressOps,
    allocator: Option<StartAllocatorOps>,
) -> Result<StartContext, &'static str> {
    let context = StartContext {
        boot: metadata.boot_info(),
        firmware,
        memory: StartMemory {
            kernel_image,
            boot_map,
        },
        address,
        allocator,
    };
    context.validate()?;
    Ok(context)
}

/// 入口适配器的阶段状态。用于防止在 UEFI `ExitBootServices` 或 Multiboot
/// 快照完成前就把上下文交给通用内核。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoaderPhase {
    /// 仅保存了寄存器，尚未验证协议。
    Entry,
    /// 协议结构已通过边界校验。
    ProtocolValidated,
    /// 固件表和内存图已复制到稳定存储。
    FirmwareSnapshotted,
    /// `StartContext` 已构造并校验，可跳转到内核。
    ContextReady,
}

impl LoaderPhase {
    /// 检查阶段是否允许向下一个阶段推进。
    pub const fn can_advance(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Entry, Self::ProtocolValidated)
                | (Self::ProtocolValidated, Self::FirmwareSnapshotted)
                | (Self::FirmwareSnapshotted, Self::ContextReady)
        )
    }
}

/// 在没有实现实际入口时，编译期/单元测试可以使用的显式“不支持”结果。
/// 该错误避免把一个尚未建立页表或栈的函数暴露成可启动入口。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryError {
    /// 当前构建尚未提供对应协议的汇编入口。
    AssemblyEntryUnavailable(X86BootProtocol),
    /// 协议适配器尚未完成稳定快照。
    SnapshotIncomplete,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x86_64::boot_protocol::X86BootProtocol;

    #[test]
    fn metadata_uses_common_start_protocol() {
        let metadata = X86StartMetadata {
            protocol: X86BootProtocol::Multiboot2,
            boot_cpu_id: 3,
            command_line: Some(b"console=uart0"),
        };
        let boot = metadata.boot_info();
        assert_eq!(boot.architecture, StartArchitecture::X86_64);
        assert_eq!(boot.protocol, general::StartBootProtocol::Multiboot2);
        assert_eq!(boot.boot_cpu_id, 3);
        assert_eq!(boot.command_line, Some(&b"console=uart0"[..]));
    }

    #[test]
    fn loader_phases_are_monotonic() {
        assert!(LoaderPhase::Entry.can_advance(LoaderPhase::ProtocolValidated));
        assert!(!LoaderPhase::Entry.can_advance(LoaderPhase::ContextReady));
        assert!(LoaderPhase::FirmwareSnapshotted.can_advance(LoaderPhase::ContextReady));
    }
}
