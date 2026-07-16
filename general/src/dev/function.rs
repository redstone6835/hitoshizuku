//! 开放设备 function 注册表。
//!
//! 一个物理 PnP 设备可以暴露一个或多个 function。字符设备和块设备只是当前
//! VFS 兼容层已经支持的两种 function；PnP core 只保存 `DeviceFunction`
//! trait object，不关心 function 最终会不会在 `/dev` 下形成节点。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::hash::{Hash, Hasher};
use core::sync::atomic::AtomicU64;

use spin::mutex::Mutex;

use crate::dev::block::BlockDevice;
use crate::dev::char::CharDevice;
pub use crate::dev::naming::{
    StableName as FunctionProjectionName, StableNameAllocError as FunctionProjectionNameAllocError,
    StableNameAllocator as FunctionProjectionNameAllocator,
};
use crate::dev::registry_id;

/// function 的内部类别标识。
///
/// 该标识只用于 registry 唯一性和按类查询，不代表 PnP core 理解字符/块设备语义。
#[derive(Clone, Copy, Debug)]
pub struct DeviceClassId {
    id: u64,
    builtin_name: Option<&'static str>,
}

impl PartialEq for DeviceClassId {
    fn eq(&self, other: &Self) -> bool {
        match (self.builtin_name, other.builtin_name) {
            (Some(left), Some(right)) => left == right,
            (None, None) => self.id == other.id,
            _ => false,
        }
    }
}

impl Eq for DeviceClassId {}

impl PartialOrd for DeviceClassId {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DeviceClassId {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        match (self.builtin_name, other.builtin_name) {
            (Some(left), Some(right)) => left.cmp(right),
            (None, None) => self.id.cmp(&other.id),
            (Some(_), None) => core::cmp::Ordering::Less,
            (None, Some(_)) => core::cmp::Ordering::Greater,
        }
    }
}

impl Hash for DeviceClassId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self.builtin_name {
            Some(name) => {
                0u8.hash(state);
                name.hash(state);
            }
            None => {
                1u8.hash(state);
                self.id.hash(state);
            }
        }
    }
}

impl DeviceClassId {
    pub const CHAR: Self = Self::new("char");
    pub const BLOCK: Self = Self::new("block");
    pub const RTC: Self = Self::new("rtc");

    pub const fn new(name: &'static str) -> Self {
        Self {
            id: stable_class_hash(name),
            builtin_name: Some(name),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self.builtin_name {
            Some(name) => name,
            None => "dynamic",
        }
    }

    /// 返回类别的稳定数值标识。
    pub const fn raw_id(self) -> u64 {
        self.id
    }

    /// 从动态类别注册表分配的编号构造类别标识。
    pub const fn dynamic(raw_id: u64) -> Self {
        Self {
            id: raw_id,
            builtin_name: None,
        }
    }

    /// 判断该类别是否来自运行期扩展。
    pub const fn is_dynamic(self) -> bool {
        self.builtin_name.is_none()
    }
}

const fn stable_class_hash(value: &str) -> u64 {
    let bytes = value.as_bytes();
    let mut hash = 0xcbf29ce484222325u64;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        index += 1;
    }
    hash
}

pub trait DeviceFunction: Send + Sync {
    /// function 类别，同一类别下 `dev_name` 必须唯一。
    fn class_id(&self) -> DeviceClassId;
    /// function registry key 的名称。
    ///
    /// 该名字只用于 dev core 的对象身份。用户可见文件节点由 VFS 投影层按
    /// function 类型和注册的 projector 生成，不能反向作为底层设备身份。
    fn dev_name(&self) -> &str;
    /// 返回类别的真实 identifier。
    ///
    /// 内建类别可以直接使用 [`DeviceClassId::as_str`]；动态类别必须覆盖该方法，
    /// 因为动态类别不能保存来自可卸载镜像的字符串引用。
    fn class_name(&self) -> &str {
        self.class_id().as_str()
    }
    /// 返回 function 操作契约；没有通用调用面的内建类型返回 `None`。
    fn operation_contract(&self) -> Option<&str> {
        None
    }
    /// 调用由 [`Self::operation_contract`] 定义的操作。
    fn invoke(
        &self,
        _opcode: u32,
        _input: &[u8],
        _output: &mut [u8],
    ) -> Result<usize, DeviceFunctionInvokeError> {
        Err(DeviceFunctionInvokeError::Unsupported)
    }
    /// 标记 function 已不可用，使旧句柄尽快停止访问底层设备。
    fn mark_gone(&self);
    /// 等待正在进行的 I/O 排空；没有异步 I/O 的 function 可以使用默认空实现。
    fn drain_io(&self) {}
    /// 向下转型支持。新设备类型通过此方法提供类型恢复路径。
    fn as_any(&self) -> &dyn core::any::Any;
}

/// 通用 function 契约调用错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceFunctionInvokeError {
    Invalid,
    Gone,
    Busy,
    Unsupported,
    Fault,
    NoMemory,
}

/// 由运行期注册的 function class 描述。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionClassRegistration {
    class_id: DeviceClassId,
    name: Box<str>,
}

impl FunctionClassRegistration {
    pub fn class_id(&self) -> DeviceClassId {
        self.class_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

static NEXT_DYNAMIC_CLASS_ID: AtomicU64 = AtomicU64::new(0x1000);
static DYNAMIC_CLASSES: Mutex<Vec<FunctionClassRegistration>> = Mutex::new(Vec::new());

/// 注册一个动态 function class。
///
/// class identifier 在内核生命周期内单调分配且不复用；这保证旧 function 句柄不会
/// 在类别注销后错误地指向新类别。名称由内核复制，不依赖调用方镜像存活。
#[kernel_symbols::export(
    name = "general.dev.function.register_function_class",
    contract = "kernel.general.device-function-class@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn register_function_class(
    name: &str,
) -> Result<FunctionClassRegistration, FunctionRegistryError> {
    if name.is_empty() {
        return Err(FunctionRegistryError::InvalidName);
    }
    let mut classes = DYNAMIC_CLASSES.lock();
    if classes.iter().any(|class| class.name() == name) {
        return Err(FunctionRegistryError::NameExists);
    }
    {
        // 注册表容量属于常驻内核元数据；类别名称和返回句柄仍按调用单元计量。
        let _accounting = allocator::suspend_implicit_allocation_accounting()
            .ok_or(FunctionRegistryError::OutOfMemory)?;
        classes
            .try_reserve(1)
            .map_err(|_| FunctionRegistryError::OutOfMemory)?;
    }
    let id = registry_id::alloc_atomic_id(&NEXT_DYNAMIC_CLASS_ID)
        .map_err(|_| FunctionRegistryError::IdExhausted)?;
    let mut owned = alloc::string::String::new();
    owned
        .try_reserve(name.len())
        .map_err(|_| FunctionRegistryError::OutOfMemory)?;
    owned.push_str(name);
    let registration = FunctionClassRegistration {
        class_id: DeviceClassId::dynamic(id),
        name: owned.into_boxed_str(),
    };
    classes.push(registration.clone());
    drop(classes);
    if super::elm_lifecycle::track_function_class(registration.class_id()).is_err() {
        let mut classes = DYNAMIC_CLASSES.lock();
        if let Some(index) = classes
            .iter()
            .position(|class| class.class_id == registration.class_id())
        {
            classes.swap_remove(index);
        }
        return Err(FunctionRegistryError::OutOfMemory);
    }
    Ok(registration)
}

/// 注销一个动态 function class。
#[kernel_symbols::export(
    name = "general.dev.function.unregister_function_class",
    contract = "kernel.general.device-function-class@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister_function_class(class_id: DeviceClassId) -> Result<(), FunctionRegistryError> {
    let mut classes = DYNAMIC_CLASSES.lock();
    let Some(index) = classes.iter().position(|class| class.class_id == class_id) else {
        return Err(FunctionRegistryError::NotFound);
    };
    classes.swap_remove(index);
    drop(classes);
    super::elm_lifecycle::forget_function_class(class_id);
    Ok(())
}

/// 查询动态类别名称的拥有副本。
#[kernel_symbols::export(
    name = "general.dev.function.function_class_name",
    contract = "kernel.general.device-function-class@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DISCOVERY
)]
pub fn function_class_name(class_id: DeviceClassId) -> Option<alloc::string::String> {
    let classes = DYNAMIC_CLASSES.lock();
    let class = classes.iter().find(|class| class.class_id == class_id)?;
    let mut out = alloc::string::String::new();
    out.try_reserve(class.name().len()).ok()?;
    out.push_str(class.name());
    Some(out)
}

/// 字符设备 function。
///
/// 它把 [`CharDevice`] 包装成通用 [`DeviceFunction`]，供 PnP core 注册和查询。
/// 是否以及如何投影到 `/dev` 由 VFS device_files 层决定。
pub struct CharFunction {
    dev_name: Box<str>,
    projection_name: Box<str>,
    dev: CharDevice,
}

impl CharFunction {
    /// 创建一个字符设备 function。
    pub fn new(dev_name: &str, dev: CharDevice) -> Self {
        Self::with_projection_name(dev_name, dev_name, dev)
    }

    /// 创建一个字符设备 function，并显式指定用户可见投影名。
    ///
    /// `projection_name` 只是提供给 VFS 投影层的稳定名字种子；它不参与 PnP
    /// 匹配、资源所有权或驱动身份判定。
    pub fn with_projection_name(dev_name: &str, projection_name: &str, dev: CharDevice) -> Self {
        Self {
            dev_name: dev_name.into(),
            projection_name: projection_name.into(),
            dev,
        }
    }

    /// 返回内部字符设备句柄。
    pub fn dev(&self) -> CharDevice {
        self.dev.clone()
    }

    /// 返回 VFS 投影层使用的稳定用户可见名字。
    pub fn projection_name(&self) -> &str {
        &self.projection_name
    }
}

impl DeviceFunction for CharFunction {
    fn class_id(&self) -> DeviceClassId {
        DeviceClassId::CHAR
    }

    fn dev_name(&self) -> &str {
        &self.dev_name
    }

    fn mark_gone(&self) {
        self.dev.mark_gone();
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// 块设备 function。
pub struct BlockFunction {
    dev_name: Box<str>,
    projection_name: Box<str>,
    dev: Arc<BlockDevice>,
}

#[kernel_symbols::export]
impl BlockFunction {
    /// 创建一个块设备 function。
    pub fn new(dev_name: &str, dev: Arc<BlockDevice>) -> Self {
        Self::with_projection_name(dev_name, dev_name, dev)
    }

    /// 创建一个块设备 function，并显式指定用户可见投影名。
    ///
    /// `projection_name` 由 VFS 投影层解释为用户可见设备文件名；底层 block
    /// 设备仍以 `dev_name`、PnP identity 和 typed object 表达身份。
    pub fn with_projection_name(
        dev_name: &str,
        projection_name: &str,
        dev: Arc<BlockDevice>,
    ) -> Self {
        Self {
            dev_name: dev_name.into(),
            projection_name: projection_name.into(),
            dev,
        }
    }

    /// 在常驻内核侧构造完成类型擦除的块设备 function。
    ///
    /// 动态 ELM 使用该入口时，`DeviceFunction` vtable 由定义 `BlockFunction` 的
    /// 常驻 crate 生成；模块只持有标准 `Arc<dyn DeviceFunction>`，不会隐式依赖
    /// 常驻 trait impl 的私有 Rust 链接符号。
    #[kernel_symbols::export(
        name = "general.dev.function.BlockFunction.with_projection_name_arc",
        contract = "kernel.general.device-function@1",
        version = 1,
        capabilities = kernel_symbols::capability::DEVICE_DRIVER,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
        retained_args = 4u64
    )]
    pub fn with_projection_name_arc(
        dev_name: &str,
        projection_name: &str,
        dev: Arc<BlockDevice>,
    ) -> Arc<dyn DeviceFunction> {
        Arc::new(Self::with_projection_name(dev_name, projection_name, dev))
    }

    /// 返回内部块设备对象。
    pub fn dev(&self) -> Arc<BlockDevice> {
        Arc::clone(&self.dev)
    }

    /// 返回 VFS 投影层使用的稳定用户可见名字。
    pub fn projection_name(&self) -> &str {
        &self.projection_name
    }
}

impl DeviceFunction for BlockFunction {
    fn class_id(&self) -> DeviceClassId {
        DeviceClassId::BLOCK
    }

    fn dev_name(&self) -> &str {
        &self.dev_name
    }

    fn mark_gone(&self) {
        self.dev.mark_gone();
    }

    fn drain_io(&self) {
        self.dev.drain();
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// 通用类型化查询。
///
/// 把 `&dyn DeviceFunction` 向下转型为具体的 function 类型。返回 `None`
/// 表示类型不匹配。
///
/// # 示例
///
/// ```rust,ignore
/// // 网络设备：function_as::<NetFunction>(func).map(|nf| nf.dev())
/// // 块设备：  function_as::<BlockFunction>(func).map(|bf| bf.dev())
/// ```
///
/// 这是为新设备类型提供的"开闭原则"路径——新增设备类型不需要改任何 core
/// 文件，只需实现 `DeviceFunction::as_any` 后即可被外部代码 downcast 取出。
pub fn function_as<T: 'static>(func: &dyn DeviceFunction) -> Option<&T> {
    func.as_any().downcast_ref::<T>()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionRegistryError {
    /// 同类别同名 function 已存在。
    NameExists,
    /// 要移除或查询的 registry 记录不存在。
    NotFound,
    /// 扩容或分配失败。
    OutOfMemory,
    /// 类别名称为空。
    InvalidName,
    /// 动态类别编号耗尽。
    IdExhausted,
}

/// 全局开放设备 function 注册表。
///
/// 注册表只保存 `Arc<dyn DeviceFunction>`，不持有具体字符/块类型。VFS 投影层
/// 通过 function 生命周期事件读取 typed function 并生成用户可见设备文件声明。
pub struct FunctionRegistry {
    functions: Mutex<Vec<Arc<dyn DeviceFunction>>>,
}

impl FunctionRegistry {
    /// 创建空注册表。
    pub const fn new() -> Self {
        Self {
            functions: Mutex::new(Vec::new()),
        }
    }

    /// 插入一个 function。
    ///
    /// 同一 `class_id + dev_name` 组合必须唯一。
    pub fn push(&self, func: Arc<dyn DeviceFunction>) -> Result<(), FunctionRegistryError> {
        {
            let mut list = self.functions.lock();
            if list.iter().any(|existing| {
                existing.class_id() == func.class_id() && existing.dev_name() == func.dev_name()
            }) {
                return Err(FunctionRegistryError::NameExists);
            }
            if list.len() < list.capacity() {
                list.push(func);
                return Ok(());
            }
        }

        loop {
            let initial_len = self.functions.lock().len();
            let needed = initial_len
                .checked_add(1)
                .ok_or(FunctionRegistryError::OutOfMemory)?;
            let mut replacement = Vec::new();
            {
                // 全局 function 表保留扩容后的容量；容量本身属于常驻内核元数据。
                let _accounting = allocator::suspend_implicit_allocation_accounting()
                    .ok_or(FunctionRegistryError::OutOfMemory)?;
                replacement
                    .try_reserve(needed)
                    .map_err(|_| FunctionRegistryError::OutOfMemory)?;
            }

            let mut list = self.functions.lock();
            if list.iter().any(|existing| {
                existing.class_id() == func.class_id() && existing.dev_name() == func.dev_name()
            }) {
                return Err(FunctionRegistryError::NameExists);
            }
            if list.len() < list.capacity() {
                list.push(func);
                return Ok(());
            }
            let needed = list
                .len()
                .checked_add(1)
                .ok_or(FunctionRegistryError::OutOfMemory)?;
            if needed > replacement.capacity() {
                continue;
            }
            replacement.extend(list.iter().cloned());
            replacement.push(Arc::clone(&func));
            let old = core::mem::replace(&mut *list, replacement);
            drop(list);
            drop(old);
            return Ok(());
        }
    }

    /// 删除指定类别和名称的 function。
    pub fn remove(
        &self,
        class_id: DeviceClassId,
        dev_name: &str,
    ) -> Option<Arc<dyn DeviceFunction>> {
        let func = self.remove_quiesced(class_id, dev_name)?;
        func.mark_gone();
        func.drain_io();
        Some(func)
    }

    /// 摘除已经由 PnP 生命周期完成停机和排空的 function。
    ///
    /// 该入口不再次调用 `mark_gone` 或 `drain_io`，只允许设备核心在严格完成资源
    /// 退役顺序后使用。
    pub(crate) fn remove_quiesced(
        &self,
        class_id: DeviceClassId,
        dev_name: &str,
    ) -> Option<Arc<dyn DeviceFunction>> {
        let mut list = self.functions.lock();
        let pos = list
            .iter()
            .position(|func| func.class_id() == class_id && func.dev_name() == dev_name)?;
        Some(list.swap_remove(pos))
    }

    /// 查找指定类别和名称的 function。
    pub fn lookup(
        &self,
        class_id: DeviceClassId,
        dev_name: &str,
    ) -> Option<Arc<dyn DeviceFunction>> {
        self.functions
            .lock()
            .iter()
            .find(|func| func.class_id() == class_id && func.dev_name() == dev_name)
            .cloned()
    }

    /// 判断当前注册表是否仍有指定类别的 function。
    pub fn contains_class(&self, class_id: DeviceClassId) -> bool {
        self.functions
            .lock()
            .iter()
            .any(|function| function.class_id() == class_id)
    }

    /// 返回所有 function 的快照。
    pub fn try_list(&self) -> Option<Vec<Arc<dyn DeviceFunction>>> {
        let list = self.functions.lock();
        let mut out = Vec::new();
        // devtmpfs/sysfs/procfs 会频繁读取 function 快照。先按当前长度预留，
        // 让低内存失败变成可诊断的空快照或上层错误，而不是隐式分配 panic。
        out.try_reserve(list.len()).ok()?;
        out.extend(list.iter().cloned());
        Some(out)
    }

    /// 返回所有 function 的快照。
    pub fn list(&self) -> Vec<Arc<dyn DeviceFunction>> {
        self.try_list().unwrap_or_default()
    }

    /// 返回指定类别 function 的快照。
    pub fn try_list_by_class(
        &self,
        class_id: DeviceClassId,
    ) -> Option<Vec<Arc<dyn DeviceFunction>>> {
        let list = self.functions.lock();
        let mut out = Vec::new();
        for func in list.iter().filter(|func| func.class_id() == class_id) {
            out.try_reserve(1).ok()?;
            out.push(Arc::clone(func));
        }
        Some(out)
    }

    /// 返回指定类别 function 的快照。
    pub fn list_by_class(&self, class_id: DeviceClassId) -> Vec<Arc<dyn DeviceFunction>> {
        self.try_list_by_class(class_id).unwrap_or_default()
    }
}

impl Default for FunctionRegistry {
    fn default() -> Self {
        Self::new()
    }
}
