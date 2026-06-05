//! 内建设备驱动集合。
//!
//! 这个模块只负责把编译进内核的驱动模块接入统一的运行时注册表。每个具体驱动
//! 自己定义 factory 和 `register_builtin_driver()`；这里的 catalog 只描述启动时
//! 要启用哪些内建驱动，不是设备 core 的唯一驱动来源。

mod uart16550;
pub use uart16550::*;

mod virtio_blk;
pub use virtio_blk::*;

mod virtio_net;
pub use virtio_net::*;

mod virtio_pci;
pub use virtio_pci::*;

mod random;
pub use random::*;

use crate::dev::pnp::PnpError;

/// 注册当前内核镜像内建的所有 PnP 驱动。
///
/// 调用前必须已经通过 `set_dev_init_context()` 安装驱动初始化上下文。
pub fn register_builtin_drivers() -> Result<(), PnpError> {
    uart16550::register_builtin_driver()?;
    virtio_blk::register_builtin_driver()?;
    virtio_net::register_builtin_driver()?;
    virtio_pci::register_builtin_driver()?;
    random::register_builtin_driver()?;
    Ok(())
}
