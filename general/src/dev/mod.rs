//! 通用设备对象层。
//!
//! 此模块只描述"设备对象本身"及其注册关系：
//! - `char` / `block` 定义设备对象与驱动接口；
//! - `pnp` 定义 PnP 设备抽象框架（硬件身份、拓扑、热插拔）；
//! - `pci` / `usb` 提供总线级设备封装；
//! - `enumerate` 提供启动期已注册设备的全局对象表；
//! - `drivers` 放置具体驱动实现。
//!
//! 这里不再承担设备号分配、`major/minor` 编码或
//! `device number -> driver` 查找职责；这些若仍有需要，只能存在于
//! ABI/VFS 兼容边界，而不能回流到 `dev` 层。

pub mod bio;
pub mod block;
pub mod block_sync;
pub mod char;
pub mod completion;
pub mod dma;
pub mod drivers;
pub mod enumerate;
pub mod function;
pub mod net;
pub mod pci;
pub mod platform;
pub mod pnp;
pub mod random_source;
pub mod rtc;
pub mod usb;
pub mod virtio;

// ─────────────────────────── 共享设备控制 trait ──────────────────────────────

/// 类型安全的设备控制接口（字符设备与块设备共用）。
///
/// 每种驱动自行定义 `Request`、`Response`、`Error` 关联类型，
/// 不使用中心化枚举，满足开闭原则。编译器在调用端即可验证请求与响应类型匹配。
///
/// # 用法
///
/// 在持有具体驱动类型（如 `&'static Uart16550`）的调用点直接使用：
///
/// ```rust,ignore
/// uart.control(UartRequest::SetBaudRate { clock_hz: 100_000_000, baud: 9600 })?;
/// ```
///
/// 通过 `dyn CharDriver` 或 `dyn BlockIo` 的通用路径不应调用设备特定命令——
/// 类型安全由此保证。可通过 `downcast_driver` / `downcast_io` 恢复具体类型。
pub trait DriverControl {
    /// 控制请求类型（每种驱动独立定义）。
    type Request;
    /// 控制响应类型。
    type Response;
    /// 控制错误类型。
    type Error;

    /// 发送一条控制请求并返回响应。
    fn control(&self, req: Self::Request) -> Result<Self::Response, Self::Error>;
}
