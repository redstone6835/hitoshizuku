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
use crate::dev::function::{DevNodeNameAllocError, DevNodeNameAllocator};
use crate::dev::pnp::PnpError;

/// VirtIO block 的 POSIX `/dev/vd*` 投影名由所有传输层共享分配。
///
/// 这只是用户可见节点名，不参与底层设备身份；底层身份仍来自 PnP id。
static VIRTIO_BLK_DEV_NAMES: DevNodeNameAllocator = DevNodeNameAllocator::new("vd");
/// VirtIO block request 的 sector 字段固定以 512 字节为单位；这是协议常量，
/// 不是底层块设备逻辑块大小。
pub(super) const VIRTIO_BLK_SECTOR_SIZE: u32 = 512;

pub(super) fn alloc_virtio_blk_dev_name(
    stable_key: &str,
) -> Result<alloc::string::String, DevNodeNameAllocError> {
    VIRTIO_BLK_DEV_NAMES
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
pub fn register_builtin_drivers() -> Result<(), PnpError> {
    loopback::register_builtin_driver()?;
    firmware_bus::register_builtin_driver()?;
    syscon::register_builtin_driver()?;
    loongson_irq::register_builtin_driver()?;
    ls7a_rtc::register_builtin_driver()?;
    fw_cfg::register_builtin_driver()?;
    cfi_flash::register_builtin_driver()?;
    uart16550::register_builtin_driver()?;
    virtio_blk::register_builtin_driver()?;
    virtio_net::register_builtin_driver()?;
    virtio_pci::register_builtin_driver()?;
    random::register_builtin_driver()?;
    Ok(())
}
