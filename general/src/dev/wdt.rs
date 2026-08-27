//! 通用看门狗设备抽象。
//!
//! 本模块分两层：
//!
//! 1. [`WdtDriver`] 描述硬件驱动能力，只表达平台无关的看门狗语义
//!    （启动/停止/喂狗/超时）；
//! 2. [`WdtDevice`] 为驱动实例提供生命周期、稳定投影名与运行期状态；
//! 3. [`WdtFunction`] 把看门狗设备暴露为通用 function，供设备模型消费。
//!
//! 这样 LS2K、LS7A 或其它未来看门狗驱动只需要实现 [`WdtDriver`]，不需要在
//! devtmpfs 或 syscall 层增加硬件类型特判。`/dev/wdt` 风格的用户态 ioctl 面
//! 由 VFS device_files 层按需扩展。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::dev::function::{
    DeviceClassId, DeviceFunction, DeviceFunctionInvokeError, FunctionProjectionName,
    FunctionProjectionNameAllocError, FunctionProjectionNameAllocator,
};

/// 看门狗投影名前缀（`wdt0`、`wdt1` …）。
static WDT_PROJECTION_NAMES: FunctionProjectionNameAllocator =
    FunctionProjectionNameAllocator::new("wdt");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WdtError {
    /// 硬件或驱动不支持该操作。
    Unsupported,
    /// 参数非法。
    Invalid,
    /// 设备已移除。
    NoDevice,
    /// 忙或正在运行，无法完成请求。
    Busy,
    /// IO 错误。
    Io,
}

/// 平台无关的看门狗硬件驱动能力。
pub trait WdtDriver: Send + Sync {
    /// 返回当前硬件超时（秒）。
    fn timeout_secs(&self) -> u32;

    /// 返回硬件支持的最大超时（秒）。
    ///
    /// 硬件计数器宽度有限（例如 2K1000 的 32 位 TMR 在 125 MHz APB 时钟下
    /// 约 34 秒），超出部分需要上层周期性喂狗才能维持。
    fn max_timeout_secs(&self) -> u32;

    /// 设置硬件超时（秒），返回实际生效值。
    fn set_timeout(&self, secs: u32) -> Result<u32, WdtError>;

    /// 启动看门狗。
    fn start(&self) -> Result<(), WdtError>;

    /// 停止看门狗。
    fn stop(&self) -> Result<(), WdtError>;

    /// 喂狗：重载倒计时，避免溢出复位。
    fn ping(&self) -> Result<(), WdtError>;

    /// 看门狗当前是否在运行。
    fn running(&self) -> bool;

    fn as_any(&self) -> &dyn Any;
}

/// 看门狗设备实例。
///
/// 持有硬件驱动与用户可见投影名，维护本次启动内的生命周期状态。
/// 所有方法都转发到底层 [`WdtDriver`]。
pub struct WdtDevice {
    index: usize,
    name: Box<str>,
    driver: Arc<dyn WdtDriver>,
    active: AtomicBool,
    timeout_secs: AtomicU32,
}

#[kernel_symbols::export]
impl WdtDevice {
    #[kernel_symbols::export(
        name = "general.dev.wdt.WdtDevice.new",
        contract = "kernel.general.wdt@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
        retained_args = 2u64
    )]
    pub fn new(projection_name: FunctionProjectionName, driver: Arc<dyn WdtDriver>) -> Self {
        let index = projection_name.index();
        let timeout_secs = driver.timeout_secs();
        Self {
            index,
            name: projection_name.into_string().into_boxed_str(),
            driver,
            active: AtomicBool::new(true),
            timeout_secs: AtomicU32::new(timeout_secs),
        }
    }

    /// 为一个稳定硬件实例分配或复用看门狗用户可见投影名。
    ///
    /// `stable_key` 由 PnP 设备身份或固件路径提供。WDT core 统一管理投影命名，
    /// 具体硬件驱动只需要传入自身实例身份，避免在驱动里散落 `wdt{n}` 拼接逻辑。
    #[kernel_symbols::export(
        name = "general.dev.wdt.WdtDevice.alloc_stable_projection_name",
        contract = "kernel.general.wdt@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
            | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    )]
    pub fn alloc_stable_projection_name(
        stable_key: &str,
    ) -> Result<FunctionProjectionName, FunctionProjectionNameAllocError> {
        WDT_PROJECTION_NAMES.try_alloc_stable(stable_key)
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    #[kernel_symbols::export(
        name = "general.dev.wdt.WdtDevice.mark_gone",
        contract = "kernel.general.wdt@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn mark_gone(&self) {
        self.active.store(false, Ordering::Release);
    }

    /// 设置超时并记录实际生效值（秒）。
    #[kernel_symbols::export(
        name = "general.dev.wdt.WdtDevice.set_timeout",
        contract = "kernel.general.wdt@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn set_timeout(&self, secs: u32) -> Result<u32, WdtError> {
        if !self.is_active() {
            return Err(WdtError::NoDevice);
        }
        let actual = self.driver.set_timeout(secs)?;
        self.timeout_secs.store(actual, Ordering::Release);
        Ok(actual)
    }

    /// 当前生效超时（秒）。
    pub fn timeout_secs(&self) -> u32 {
        self.timeout_secs.load(Ordering::Acquire)
    }

    /// 硬件最大超时（秒）。
    #[kernel_symbols::export(
        name = "general.dev.wdt.WdtDevice.max_timeout_secs",
        contract = "kernel.general.wdt@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER
    )]
    pub fn max_timeout_secs(&self) -> u32 {
        self.driver.max_timeout_secs()
    }

    pub fn start(&self) -> Result<(), WdtError> {
        if !self.is_active() {
            return Err(WdtError::NoDevice);
        }
        self.driver.start()
    }

    pub fn stop(&self) -> Result<(), WdtError> {
        if !self.is_active() {
            return Err(WdtError::NoDevice);
        }
        self.driver.stop()
    }

    pub fn ping(&self) -> Result<(), WdtError> {
        if !self.is_active() {
            return Err(WdtError::NoDevice);
        }
        self.driver.ping()
    }

    pub fn running(&self) -> bool {
        self.driver.running()
    }

    pub fn driver(&self) -> &Arc<dyn WdtDriver> {
        &self.driver
    }
}

/// 把看门狗设备暴露为通用 function。
pub struct WdtFunction {
    dev: Arc<WdtDevice>,
}

#[kernel_symbols::export]
impl WdtFunction {
    #[kernel_symbols::export(
        name = "general.dev.wdt.WdtFunction.new",
        contract = "kernel.general.wdt@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
        retained_args = 1u64
    )]
    pub fn new(dev: Arc<WdtDevice>) -> Self {
        Self { dev }
    }

    #[kernel_symbols::export(
        name = "general.dev.wdt.WdtFunction.new_arc",
        contract = "kernel.general.wdt@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
        retained_args = 1u64
    )]
    pub fn new_arc(dev: Arc<WdtDevice>) -> Arc<dyn DeviceFunction> {
        Arc::new(Self::new(dev))
    }

    pub fn dev(&self) -> Arc<WdtDevice> {
        Arc::clone(&self.dev)
    }
}

impl DeviceFunction for WdtFunction {
    fn class_id(&self) -> DeviceClassId {
        DeviceClassId::WDT
    }

    fn dev_name(&self) -> &str {
        self.dev.name()
    }

    fn operation_contract(&self) -> Option<&str> {
        Some(
            "mygo.device.wdt@1;1=ping;2=set_timeout:u32;3=timeout_secs:u32;4=start;5=stop;6=running:u32",
        )
    }

    fn invoke(
        &self,
        opcode: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, DeviceFunctionInvokeError> {
        use crate::dev::function::DeviceFunctionInvokeError as InvokeError;

        if self.is_gone() {
            return Err(InvokeError::Gone);
        }
        match opcode {
            1 => {
                if !input.is_empty() {
                    return Err(InvokeError::Invalid);
                }
                self.dev.ping().map_err(map_wdt_invoke_error)?;
                Ok(0)
            }
            2 => {
                if input.len() != core::mem::size_of::<u32>() || !output.is_empty() {
                    return Err(InvokeError::Invalid);
                }
                let secs = u32::from_le_bytes(input[..4].try_into().unwrap_or([0u8; 4]));
                let actual = self.dev.set_timeout(secs).map_err(map_wdt_invoke_error)?;
                if output.len() < core::mem::size_of::<u32>() {
                    return Err(InvokeError::Invalid);
                }
                output[..4].copy_from_slice(&actual.to_le_bytes());
                Ok(4)
            }
            3 => {
                if !input.is_empty() || output.len() < core::mem::size_of::<u32>() {
                    return Err(InvokeError::Invalid);
                }
                output[..4].copy_from_slice(&self.dev.timeout_secs().to_le_bytes());
                Ok(4)
            }
            4 => {
                if !input.is_empty() {
                    return Err(InvokeError::Invalid);
                }
                self.dev.start().map_err(map_wdt_invoke_error)?;
                Ok(0)
            }
            5 => {
                if !input.is_empty() {
                    return Err(InvokeError::Invalid);
                }
                self.dev.stop().map_err(map_wdt_invoke_error)?;
                Ok(0)
            }
            6 => {
                if !input.is_empty() || output.len() < core::mem::size_of::<u32>() {
                    return Err(InvokeError::Invalid);
                }
                output[..4].copy_from_slice(&u32::from(self.dev.running()).to_le_bytes());
                Ok(4)
            }
            _ => Err(InvokeError::Unsupported),
        }
    }

    fn mark_gone(&self) {
        self.dev.mark_gone();
    }

    fn is_gone(&self) -> bool {
        !self.dev.is_active()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn map_wdt_invoke_error(error: WdtError) -> DeviceFunctionInvokeError {
    use crate::dev::function::DeviceFunctionInvokeError as InvokeError;

    match error {
        WdtError::Unsupported => InvokeError::Unsupported,
        WdtError::Invalid => InvokeError::Invalid,
        WdtError::NoDevice => InvokeError::Gone,
        WdtError::Busy => InvokeError::Busy,
        WdtError::Io => InvokeError::Fault,
    }
}
