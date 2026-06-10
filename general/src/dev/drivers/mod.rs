//! 内建设备驱动集合。
//!
//! 这个模块只负责把编译进内核的驱动模块接入统一的运行时注册表。每个具体驱动
//! 自己定义 factory 和 `register_builtin_driver()`；这里的 catalog 只描述启动时
//! 要启用哪些内建驱动，不是设备 core 的唯一驱动来源。

mod uart16550;
pub use uart16550::*;

mod loopback;

mod firmware_bus;

mod syscon;

mod loongson_irq;

mod ls7a_rtc;

mod fw_cfg;

mod cfi_flash;

mod virtio_blk;
pub use virtio_blk::*;

mod virtio_net;
pub use virtio_net::*;

mod virtio_pci;
pub use virtio_pci::*;

mod random;
pub use random::*;

use core::num::NonZeroU32;

use crate::dev::block::BlockLimits;
use crate::dev::function::{
    FunctionProjectionNameAllocError, FunctionProjectionNameAllocator,
};
use crate::dev::pnp::PnpError;

/// 一个编译进内核的内建设备驱动注册项。
///
/// catalog 只表达“启动期需要接入哪些驱动”和它们的依赖顺序；具体驱动仍通过
/// 各自模块里的 factory 或静态节点声明接入设备子系统。这样新增驱动时只需要
/// 增加一条表项，不需要在注册流程里继续堆叠分支。
#[derive(Clone, Copy)]
pub struct BuiltinDriverRegistration {
    name: &'static str,
    register: fn() -> Result<(), PnpError>,
}

impl BuiltinDriverRegistration {
    pub const fn new(name: &'static str, register: fn() -> Result<(), PnpError>) -> Self {
        Self { name, register }
    }

    /// 返回驱动注册项的稳定名称，供启动日志或诊断输出使用。
    pub const fn name(self) -> &'static str {
        self.name
    }

    fn register(self) -> Result<(), PnpError> {
        (self.register)()
    }
}

/// 内建驱动注册失败的上下文。
///
/// 启动期注册失败通常需要直接停止启动；携带失败驱动名称可以让上层日志定位到
/// 哪个 catalog 表项没有完成，而不是只看到一个泛化的 PnP 错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltinDriverRegisterError {
    driver: &'static str,
    error: PnpError,
}

impl BuiltinDriverRegisterError {
    pub const fn driver(self) -> &'static str {
        self.driver
    }

    pub const fn error(self) -> PnpError {
        self.error
    }
}

/// VirtIO block 的用户可见 `vd*` 投影名由所有传输层共享分配。
///
/// 这只是用户可见节点名，不参与底层设备身份；底层身份仍来自 PnP id。
static VIRTIO_BLK_PROJECTION_NAMES: FunctionProjectionNameAllocator =
    FunctionProjectionNameAllocator::new("vd");
/// VirtIO block request 的 sector 字段固定以 512 字节为单位；这是协议常量，
/// 不是底层块设备逻辑块大小。
pub(super) const VIRTIO_BLK_SECTOR_SIZE: u32 = 512;

pub(super) fn alloc_virtio_blk_dev_name(
    stable_key: &str,
) -> Result<alloc::string::String, FunctionProjectionNameAllocError> {
    VIRTIO_BLK_PROJECTION_NAMES
        .try_alloc_stable(stable_key)
        .map(|name| name.into_string())
}

pub(super) fn virtio_blk_limits(block_size: u32) -> BlockLimits {
    if block_size == 0 {
        return BlockLimits::unrestricted();
    }
    let max_blocks = NonZeroU32::new(u32::MAX / block_size);
    match BlockLimits::new(max_blocks, max_blocks, NonZeroU32::new(1)) {
        Some(limits) => limits,
        None => BlockLimits::unrestricted(),
    }
}

/// 注册当前内核镜像内建的所有 PnP 驱动。
///
/// 调用前必须已经通过 `set_dev_init_context()` 安装驱动初始化上下文。
const BUILTIN_DRIVER_CATALOG: &[BuiltinDriverRegistration] = &[
    BuiltinDriverRegistration::new("loopback", loopback::register_builtin_driver),
    BuiltinDriverRegistration::new("firmware-bus", firmware_bus::register_builtin_driver),
    BuiltinDriverRegistration::new("syscon", syscon::register_builtin_driver),
    BuiltinDriverRegistration::new("loongson-irq", loongson_irq::register_builtin_driver),
    BuiltinDriverRegistration::new("ls7a-rtc", ls7a_rtc::register_builtin_driver),
    BuiltinDriverRegistration::new("fw-cfg", fw_cfg::register_builtin_driver),
    BuiltinDriverRegistration::new("cfi-flash", cfi_flash::register_builtin_driver),
    BuiltinDriverRegistration::new("uart16550", uart16550::register_builtin_driver),
    BuiltinDriverRegistration::new("virtio-blk", virtio_blk::register_builtin_driver),
    BuiltinDriverRegistration::new("virtio-net", virtio_net::register_builtin_driver),
    BuiltinDriverRegistration::new("virtio-pci", virtio_pci::register_builtin_driver),
    BuiltinDriverRegistration::new("random", random::register_builtin_driver),
];

/// 返回当前内核镜像内建驱动 catalog。
pub fn builtin_driver_catalog() -> &'static [BuiltinDriverRegistration] {
    BUILTIN_DRIVER_CATALOG
}

pub fn register_builtin_drivers() -> Result<(), BuiltinDriverRegisterError> {
    for driver in builtin_driver_catalog() {
        driver
            .register()
            .map_err(|error| BuiltinDriverRegisterError {
                driver: driver.name(),
                error,
            })?;
    }
    Ok(())
}
