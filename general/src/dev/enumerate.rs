//! 全局设备枚举。
//!
//! # 概览
//!
//! ```text
//! devices()               → &'static DeviceList
//!   └── .functions        → FunctionRegistry（开放设备 function 注册表）
//! ```
//!
//! # PnP 设备与驱动
//!
//! ```text
//! PNP_DEVICES             → PnpDeviceList （PnP 设备全局注册表）
//! PNP_DRIVERS             → PnpDriverRegistry（PnP 驱动注册表）
//! ```
//!
//! 固件或总线发现路径只注册 `PnpDevice`。具体驱动通过 PnP probe 认领设备，
//! 创建对应 function 并调用 `PnpDevice::register_function()`。

// ─────────────────────────── 顶层设备列表 ─────────────────────────────────

use alloc::sync::Arc;
use alloc::vec::Vec;

use vfs::sync::Spinlock;

use crate::dev::function::{DeviceFunction, FunctionRegistry, FunctionRegistryError};

/// function 生命周期事件类型。
///
/// 这组事件只描述全局 function registry 中对象的可见性变化，不表示 VFS、sysfs
/// 或其它用户态投影已经完成。投影层通过订阅事件维护自己的名字空间，不能反向
/// 参与底层设备对象的所有权和 probe/remove 事务。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceFunctionEventKind {
    /// function 已经进入全局 registry，可被其它子系统查询。
    Registered,
    /// function 已经从全局 registry 移除，旧句柄应只做清理。
    Unregistered,
}

/// function 生命周期事件。
#[derive(Clone)]
pub struct DeviceFunctionEvent {
    kind: DeviceFunctionEventKind,
    function: Arc<dyn DeviceFunction>,
}

impl DeviceFunctionEvent {
    pub fn kind(&self) -> DeviceFunctionEventKind {
        self.kind
    }

    pub fn function(&self) -> &Arc<dyn DeviceFunction> {
        &self.function
    }
}

/// function 事件订阅回调。
///
/// 回调返回值不会影响 registry 的注册或移除结果；如果投影层创建节点失败，应在
/// 自己的诊断通道记录错误。这样底层设备生命周期不会依赖 `/dev`、`/sys` 等用户态
/// 视图是否已经挂载或是否分配成功。
pub type DeviceFunctionEventCallback = fn(&DeviceFunctionEvent);

#[derive(Clone, Copy)]
struct DeviceFunctionEventSubscriber {
    owner: &'static str,
    name: &'static str,
    callback: DeviceFunctionEventCallback,
}

static FUNCTION_EVENT_SUBSCRIBERS: Spinlock<Vec<DeviceFunctionEventSubscriber>> =
    Spinlock::new(Vec::new());

/// function 事件订阅注册结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceFunctionEventSubscription {
    inserted: bool,
}

impl DeviceFunctionEventSubscription {
    pub const fn inserted(self) -> bool {
        self.inserted
    }
}

/// 注册 function 生命周期事件订阅者。
///
/// `owner/name` 组成幂等键，重复安装同一订阅者不会插入多份回调。该接口面向
/// devtmpfs/sysfs/procfs 这类观察者，不能用于表达设备资源所有权。
pub fn subscribe_function_events(
    owner: &'static str,
    name: &'static str,
    callback: DeviceFunctionEventCallback,
) -> Result<DeviceFunctionEventSubscription, FunctionRegistryError> {
    let mut subscribers = FUNCTION_EVENT_SUBSCRIBERS.lock();
    if subscribers
        .iter()
        .any(|subscriber| subscriber.owner == owner && subscriber.name == name)
    {
        return Ok(DeviceFunctionEventSubscription { inserted: false });
    }
    subscribers
        .try_reserve(1)
        .map_err(|_| FunctionRegistryError::OutOfMemory)?;
    subscribers.push(DeviceFunctionEventSubscriber {
        owner,
        name,
        callback,
    });
    Ok(DeviceFunctionEventSubscription { inserted: true })
}

/// 注销 function 生命周期事件订阅者。
///
/// 事件订阅是观察者关系，不表达资源所有权；可选 VFS/诊断适配层卸载时应先注销
/// 订阅，再释放自己的投影状态，避免后续 function 事件回调到已经失效的适配层。
pub fn unsubscribe_function_events(
    owner: &'static str,
    name: &str,
) -> Result<(), FunctionRegistryError> {
    let mut subscribers = FUNCTION_EVENT_SUBSCRIBERS.lock();
    let Some(index) = subscribers
        .iter()
        .position(|subscriber| subscriber.owner == owner && subscriber.name == name)
    else {
        return Err(FunctionRegistryError::NotFound);
    };
    subscribers.remove(index);
    Ok(())
}

fn publish_function_event(kind: DeviceFunctionEventKind, function: Arc<dyn DeviceFunction>) {
    let event = DeviceFunctionEvent { kind, function };
    let mut last_key = None;
    while let Some(subscriber) = next_function_event_subscriber(last_key) {
        // 回调可能进入 VFS、分配 inode 或获取其它锁；必须在订阅者锁外执行。
        // 按 owner/name 稳定推进，避免回调期间注销前序订阅者导致下标遍历跳项，
        // 同时不为生命周期事件分发分配临时 Vec。
        (subscriber.callback)(&event);
        last_key = Some((subscriber.owner, subscriber.name));
    }
}

fn next_function_event_subscriber(
    last_key: Option<(&'static str, &'static str)>,
) -> Option<DeviceFunctionEventSubscriber> {
    let subscribers = FUNCTION_EVENT_SUBSCRIBERS.lock();
    let mut next = None;
    for subscriber in subscribers.iter().copied() {
        let key = (subscriber.owner, subscriber.name);
        if let Some(last_key) = last_key
            && key <= last_key
        {
            continue;
        }
        if next
            .map(|existing: DeviceFunctionEventSubscriber| key < (existing.owner, existing.name))
            .unwrap_or(true)
        {
            next = Some(subscriber);
        }
    }
    next
}

/// 全局设备列表，包含各类设备的对象注册表。
///
/// 此层只管理"已注册的设备对象"，不再维护设备号分配或
/// `major/minor -> driver` 映射。
pub struct DeviceList {
    /// 开放设备 function 注册表。
    pub functions: FunctionRegistry,
}

impl DeviceList {
    const fn new() -> Self {
        Self {
            functions: FunctionRegistry::new(),
        }
    }

    pub fn register_function(
        &self,
        func: Arc<dyn DeviceFunction>,
    ) -> Result<(), FunctionRegistryError> {
        self.functions.push(Arc::clone(&func))?;
        publish_function_event(DeviceFunctionEventKind::Registered, func);
        Ok(())
    }

    pub fn unregister_function(&self, func: &Arc<dyn DeviceFunction>) {
        if let Some(removed) = self.functions.remove(func.class_id(), func.dev_name()) {
            publish_function_event(DeviceFunctionEventKind::Unregistered, removed);
        }
    }
}

pub static DEVICES: DeviceList = DeviceList::new();

pub use crate::dev::pnp::{PNP_DEVICES, PNP_DRIVERS};
