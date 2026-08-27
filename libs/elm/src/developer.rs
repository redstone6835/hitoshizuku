//! Rust ELM 开发侧安全边界。
//!
//! 本模块把 EBI v1 的裸函数指针、原始地址和固定布局帧收敛到少量内部调用门。
//! ELM 业务代码只处理借用、结果类型和显式固定线编码载荷。这里的公开项会在 crate 根
//! 重导出；模块作者通常直接使用 `elm::LifecycleContext`、`elm::ManagedImport` 等路径。
//!
//! attribute 生成的 trampoline 是唯一应接触原生 ABI frame 的代码。业务函数不得保存
//! 请求借用、迁移缓冲区、当前上下文或原始回复帧内部地址，也不得让 panic 穿过 trampoline。
//! 跨 ELM 的长期关系应使用受管 import、binding、lease 和运行时登记资源表达。

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use crate::context::{
    ELM_NATIVE_HOOK_CONTEXT_ABI_VERSION, ELM_NATIVE_MIGRATION_CONTEXT_ABI_VERSION,
    ElmNativeHookContextV1, ElmNativeMigrationContextV1,
};
use crate::elmapi::{
    ELM_API_ABORT_REASON_PANIC, ELM_API_ROOT_MAGIC, ELM_API_STATUS_BUFFER_TOO_SMALL,
    ELM_API_VERSION_V1, ElmApiContextV1, ElmApiNamespaceV1, ElmApiRootV1, ElmRuntimeApiV1,
};
use crate::frame::{
    ELM_CALL_STATUS_INVALID, ELM_CALL_STATUS_OK, ELM_CALL_STATUS_PROVIDER_FAULT,
    ELM_FRAME_PAYLOAD_LEN, ELM_NATIVE_ENTRY_ABI_VERSION, ELM_NATIVE_MANAGED_CALL_ABI_VERSION,
    ELM_NATIVE_PROVIDER_CALL_ABI_VERSION, ELM_NATIVE_PROVIDER_SNAPSHOT_ABI_VERSION,
    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_MORE, ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED,
    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAGS_MASK, ElmCallFrame, ElmNativeEntryFrameV1,
    ElmNativeManagedCallV1, ElmNativeProviderCallV1, ElmNativeProviderSnapshotV1, ElmReplyFrame,
};
use crate::module_wire::{
    MGR_EXTENSION_DISPATCH_RESPONSE_SIZE, MGR_EXTENSION_PAYLOAD_LEN, MGR_RESPONSE_HEADER_SIZE,
    MGR_STATUS_OK, MIXIN_REPLY_CONTINUE, MIXIN_REPLY_DENY, MIXIN_REPLY_REPLACE, MIXIN_REPLY_STOP,
    ModuleExtensionDispatchRequest, ModuleExtensionDispatchResponse, ModuleMgrResponseHeader,
};

/// 装载器注入 ELM 根 API 表地址时使用的固定导入槽符号。
///
/// 每个由 Rust 框架构建的原生 ELM 都包含此槽。打包器把它投影为受运行时管理的特殊重定位，
/// 装载器在执行任何模块代码前写入 [`ElmApiRootV1`] 地址。模块不得自行定义同名符号。
pub const ELM_API_ROOT_SLOT_SYMBOL: &str = "__elm_api_root_slot_v1";
/// 集成组件 managed export 描述符的固定魔数。
pub const ELM_INTEGRATED_MANAGED_EXPORT_MAGIC: u64 = u64::from_le_bytes(*b"ELMEXP01");
/// 集成组件 managed export 描述符 ABI 版本。
pub const ELM_INTEGRATED_MANAGED_EXPORT_ABI_V1: u16 = 1;
/// 集成 provider 名称的固定缓冲区长度。
pub const ELM_INTEGRATED_PROVIDER_NAME_LEN: usize = 128;
/// 集成 provider 版本的固定缓冲区长度。
pub const ELM_INTEGRATED_PROVIDER_VERSION_LEN: usize = 64;
/// 集成 managed export 名称的固定缓冲区长度。
pub const ELM_INTEGRATED_EXPORT_NAME_LEN: usize = 128;
/// 集成 managed export 契约的固定缓冲区长度。
pub const ELM_INTEGRATED_EXPORT_CONTRACT_LEN: usize = 64;

const ELM_INTEGRATED_EXPORT_FLAG_MANAGED: u32 = 1 << 0;
const ELM_INTEGRATED_EXPORT_FLAG_DIRECT_PINNED: u32 = 1 << 1;
const ELM_INTEGRATED_EXPORT_FLAG_PRIVATE: u32 = 1 << 2;
const ELM_INTEGRATED_EXPORT_FLAG_DEPENDENCY: u32 = 1 << 3;
const ELM_INTEGRATED_EXPORT_FLAG_SUBTREE: u32 = 1 << 4;
const ELM_INTEGRATED_EXPORT_FLAGS_MASK: u32 = ELM_INTEGRATED_EXPORT_FLAG_MANAGED
    | ELM_INTEGRATED_EXPORT_FLAG_DIRECT_PINNED
    | ELM_INTEGRATED_EXPORT_FLAG_PRIVATE
    | ELM_INTEGRATED_EXPORT_FLAG_DEPENDENCY
    | ELM_INTEGRATED_EXPORT_FLAG_SUBTREE;

/// 常驻集成组件处理 managed export 调用的固定 ABI 入口。
pub type ElmIntegratedManagedExportInvoke =
    unsafe extern "C" fn(*mut ElmNativeManagedCallV1) -> i32;
/// 查询常驻集成组件实例是否已经初始化并可接受调用。
///
/// 返回非零表示就绪。使用固定宽度整数而不是 Rust `bool`，确保描述符可以由任意支持
/// C ABI 的工具链生成。
pub type ElmIntegratedManagedExportInitialized = extern "C" fn() -> u32;

/// 由 `y` 模式组件发布到内核链接段的语言无关 managed export 描述符。
///
/// 描述符只携带稳定标量、固定字符串和 C ABI 调用门。内核把 provider 投影为 builtin ELM
/// cell 后，仍按普通 EBI import 的名称、契约、版本、可见性和 cell policy 完成绑定；回调地址
/// 不会直接写入动态 consumer。
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy)]
pub struct ElmIntegratedManagedExportV1 {
    /// 固定魔数。
    pub magic: u64,
    /// 描述符 ABI 版本。
    pub abi_version: u16,
    /// 当前结构完整长度。
    pub struct_size: u16,
    /// provider 名称有效字节数。
    pub provider_name_len: u16,
    /// provider 版本有效字节数。
    pub provider_version_len: u16,
    /// export 名称有效字节数。
    pub name_len: u16,
    /// export 契约有效字节数。
    pub contract_len: u16,
    /// EBI export 版本。
    pub version: u32,
    /// EBI managed export 与可见性标志。
    pub flags: u32,
    /// provider 规范名称。
    pub provider_name: [u8; ELM_INTEGRATED_PROVIDER_NAME_LEN],
    /// provider 版本。
    pub provider_version: [u8; ELM_INTEGRATED_PROVIDER_VERSION_LEN],
    /// export 名称。
    pub name: [u8; ELM_INTEGRATED_EXPORT_NAME_LEN],
    /// export 契约。
    pub contract: [u8; ELM_INTEGRATED_EXPORT_CONTRACT_LEN],
    /// 常驻 C ABI trampoline。
    pub invoke: ElmIntegratedManagedExportInvoke,
    /// 组件实例初始化状态查询入口。
    pub initialized: ElmIntegratedManagedExportInitialized,
}

impl ElmIntegratedManagedExportV1 {
    /// 使用已经规范化的 provider 名称构造描述符。
    pub const fn new(
        provider_name: &str,
        provider_version: &str,
        name: &str,
        contract: &str,
        version: u32,
        flags: u32,
        invoke: ElmIntegratedManagedExportInvoke,
        initialized: ElmIntegratedManagedExportInitialized,
    ) -> Self {
        let (provider_name, provider_name_len) =
            integrated_fixed_field::<ELM_INTEGRATED_PROVIDER_NAME_LEN>(provider_name, false);
        let (provider_version, provider_version_len) =
            integrated_fixed_field::<ELM_INTEGRATED_PROVIDER_VERSION_LEN>(provider_version, false);
        let (name, name_len) =
            integrated_fixed_field::<ELM_INTEGRATED_EXPORT_NAME_LEN>(name, false);
        let (contract, contract_len) =
            integrated_fixed_field::<ELM_INTEGRATED_EXPORT_CONTRACT_LEN>(contract, false);
        Self {
            magic: ELM_INTEGRATED_MANAGED_EXPORT_MAGIC,
            abi_version: ELM_INTEGRATED_MANAGED_EXPORT_ABI_V1,
            struct_size: core::mem::size_of::<Self>() as u16,
            provider_name_len,
            provider_version_len,
            name_len,
            contract_len,
            version,
            flags,
            provider_name,
            provider_version,
            name,
            contract,
            invoke,
            initialized,
        }
    }

    /// 按 Cargo ELM 命名约定把包名中的第一个 `-` 转成命名空间分隔符。
    pub const fn from_cargo_package(
        package_name: &str,
        package_version: &str,
        name: &str,
        contract: &str,
        version: u32,
        flags: u32,
        invoke: ElmIntegratedManagedExportInvoke,
        initialized: ElmIntegratedManagedExportInitialized,
    ) -> Self {
        let (provider_name, provider_name_len) =
            integrated_fixed_field::<ELM_INTEGRATED_PROVIDER_NAME_LEN>(package_name, true);
        let mut value = Self::new(
            "placeholder",
            package_version,
            name,
            contract,
            version,
            flags,
            invoke,
            initialized,
        );
        value.provider_name = provider_name;
        value.provider_name_len = provider_name_len;
        value
    }

    /// 校验固定头部、字符串边界、模式标志和回调入口。
    pub fn valid(&self) -> bool {
        let visibility = self.flags
            & (ELM_INTEGRATED_EXPORT_FLAG_PRIVATE
                | ELM_INTEGRATED_EXPORT_FLAG_DEPENDENCY
                | ELM_INTEGRATED_EXPORT_FLAG_SUBTREE);
        self.magic == ELM_INTEGRATED_MANAGED_EXPORT_MAGIC
            && self.abi_version == ELM_INTEGRATED_MANAGED_EXPORT_ABI_V1
            && self.struct_size as usize == core::mem::size_of::<Self>()
            && self.version != 0
            && self.flags & !ELM_INTEGRATED_EXPORT_FLAGS_MASK == 0
            && self.flags
                & (ELM_INTEGRATED_EXPORT_FLAG_MANAGED | ELM_INTEGRATED_EXPORT_FLAG_DIRECT_PINNED)
                == ELM_INTEGRATED_EXPORT_FLAG_MANAGED
            && visibility.count_ones() <= 1
            && self.provider_name().is_some_and(|value| !value.is_empty())
            && self
                .provider_version()
                .is_some_and(|value| !value.is_empty())
            && self.name().is_some_and(|value| !value.is_empty())
            && self.contract().is_some_and(|value| !value.is_empty())
            && self.invoke as usize != 0
            && self.initialized as usize != 0
    }

    /// 返回 provider 规范名称。
    pub fn provider_name(&self) -> Option<&str> {
        integrated_fixed_str(&self.provider_name, self.provider_name_len)
    }

    /// 返回 provider 版本。
    pub fn provider_version(&self) -> Option<&str> {
        integrated_fixed_str(&self.provider_version, self.provider_version_len)
    }

    /// 返回 export 名称。
    pub fn name(&self) -> Option<&str> {
        integrated_fixed_str(&self.name, self.name_len)
    }

    /// 返回 export 契约。
    pub fn contract(&self) -> Option<&str> {
        integrated_fixed_str(&self.contract, self.contract_len)
    }
}

const fn integrated_fixed_field<const N: usize>(
    value: &str,
    normalize_package_name: bool,
) -> ([u8; N], u16) {
    let bytes = value.as_bytes();
    if bytes.len() > N {
        return ([0; N], u16::MAX);
    }
    let mut output = [0; N];
    let mut index = 0;
    let mut namespace_separator_written = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if normalize_package_name && !namespace_separator_written && byte == b'-' {
            output[index] = b'.';
            namespace_separator_written = true;
        } else {
            output[index] = byte;
        }
        index += 1;
    }
    (output, bytes.len() as u16)
}

fn integrated_fixed_str(bytes: &[u8], length: u16) -> Option<&str> {
    let length = usize::from(length);
    if length == 0 || length > bytes.len() || bytes[length..].iter().any(|byte| *byte != 0) {
        return None;
    }
    core::str::from_utf8(&bytes[..length]).ok()
}
/// 启用 mixin 的 ingress 阶段，即原始函数执行前的输入补缀。
pub const ELM_MIXIN_STAGE_INGRESS: u32 = 1 << 0;
/// 启用 mixin 的 substitute 阶段，该阶段可替换帧并跳过原始函数。
pub const ELM_MIXIN_STAGE_SUBSTITUTE: u32 = 1 << 1;
/// 启用 mixin 的 egress 阶段，即原始函数或替代逻辑完成后的输出补缀。
pub const ELM_MIXIN_STAGE_EGRESS: u32 = 1 << 2;
/// 启用 mixin 的 observe 阶段；该阶段最后执行，主要用于只读观察和审计。
pub const ELM_MIXIN_STAGE_OBSERVE: u32 = 1 << 3;
/// 当前 ABI 支持的全部 mixin 阶段位集合。
pub const ELM_MIXIN_STAGES_ALL: u32 = ELM_MIXIN_STAGE_INGRESS
    | ELM_MIXIN_STAGE_SUBSTITUTE
    | ELM_MIXIN_STAGE_EGRESS
    | ELM_MIXIN_STAGE_OBSERVE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 生命周期、entry、provider 和补缀点业务函数返回的稳定错误。
///
/// 错误只携带一个非零状态码，以便 trampoline 无需分配即可把失败传播给运行时。状态码的
/// 具体命名空间由调用契约决定；框架保留 `ELM_CALL_STATUS_*` 作为通用调用错误。
pub struct HookError {
    status: i32,
}

impl HookError {
    /// 从状态码构造错误。
    ///
    /// 零表示成功，不能用于错误；传入零时会归一化为
    /// [`ELM_CALL_STATUS_INVALID`](crate::ELM_CALL_STATUS_INVALID)，从而保证 `HookError`
    /// 永远表示失败。
    pub const fn new(status: i32) -> Self {
        Self {
            status: if status == 0 {
                ELM_CALL_STATUS_INVALID
            } else {
                status
            },
        }
    }

    /// 返回将由 ABI trampoline 传播给运行时的非零状态码。
    pub const fn status(self) -> i32 {
        self.status
    }
}

/// 生命周期钩子使用的结果类型；成功不携带载荷，失败携带稳定状态码。
pub type HookResult = Result<(), HookError>;
/// 设备 IRQ 业务回调使用的结果类型；成功值表示本处理器是否消费了该中断。
pub type DeviceIrqResult = Result<bool, HookError>;
/// [`ElmModule::entry`] 业务函数使用的结果类型。
pub type EntryResult = HookResult;
/// [`mixin_point`](crate::mixin_point) 原始函数使用的结果类型。
pub type PointResult = HookResult;
/// 迁移状态导出钩子的结果类型；成功值是实际写入迁移缓冲区的字节数。
pub type MigrationExportResult = Result<usize, HookError>;

/// ELM 模块描述符使用的固定魔数。
pub const ELM_MODULE_DESCRIPTOR_MAGIC: [u8; 8] = *b"ELMMOD01";
/// ELM 镜像导出统一模块描述符时使用的固定符号名。
pub const ELM_MODULE_DESCRIPTOR_SYMBOL: &str = "__elm_module_descriptor_v1";
/// ELM 模块描述符 ABI 版本。
pub const ELM_MODULE_DESCRIPTOR_ABI_VERSION: u16 = 1;
/// ELM 模块描述符中尚未定义任何公开标志。
pub const ELM_MODULE_DESCRIPTOR_FLAGS_MASK: u32 = 0;

/// 统一 ELM 模块实现接口。
///
/// 每个 ELM 镜像只能通过 [`module`](crate::module) 注册一个实现。运行时先调用
/// [`ElmModule::create`] 构造当前 generation 的唯一实例，再调用
/// [`ElmModule::initialize`]。只有初始化成功后，provider、事件、mixin 和其它回调才允许
/// 取得该实例。卸载时会在所有在途调用排空后调用 [`ElmModule::finalize`]，成功后销毁实例。
///
/// `quiesce`、`pause` 和 `resume` 默认是无状态成功操作。迁移方法默认返回不支持，因此没有
/// 实现迁移协议的模块不会被热替换流程误认为可以安全迁移。
pub trait ElmModule: Send + Sync + Sized + 'static {
    /// 构造当前 generation 的唯一模块实例。
    fn create(context: &LifecycleContext) -> Result<Self, HookError>;

    /// 完成模块初始化并发布可以对外使用的状态。
    fn initialize(&mut self, context: &LifecycleContext) -> HookResult;

    /// 撤销模块发布的状态并释放所有长期资源。
    fn finalize(&mut self, context: &LifecycleContext) -> HookResult;

    /// 停止接受新工作并排空可以排空的活动。
    fn quiesce(&mut self, _context: &LifecycleContext) -> HookResult {
        Ok(())
    }

    /// 暂停模块提供的活动能力。
    fn pause(&mut self, _context: &LifecycleContext) -> HookResult {
        Ok(())
    }

    /// 恢复此前暂停的模块能力。
    fn resume(&mut self, _context: &LifecycleContext) -> HookResult {
        Ok(())
    }

    /// 导出热替换需要的固定状态。
    fn migrate_export(
        &self,
        _context: &MigrationContext,
        _output: &mut [u8],
    ) -> MigrationExportResult {
        Err(HookError::new(crate::ELM_CALL_STATUS_UNSUPPORTED))
    }

    /// 导入旧 generation 导出的固定状态。
    fn migrate_import(&mut self, _context: &MigrationContext, _input: &[u8]) -> HookResult {
        Err(HookError::new(crate::ELM_CALL_STATUS_UNSUPPORTED))
    }

    /// 撤销尚未提交的迁移状态。
    fn migrate_abort(&mut self, _context: &MigrationContext, _input: &[u8]) -> HookResult {
        Ok(())
    }

    /// 在模块激活后执行可选的一次性入口逻辑。
    fn entry(&self, _context: &EntryContext) -> EntryResult {
        Ok(())
    }
}

/// 原生模块普通生命周期入口类型。
pub type ElmModuleLifecycleEntryV1 = unsafe extern "C" fn(*mut ElmNativeHookContextV1) -> i32;
/// 原生模块迁移入口类型。
pub type ElmModuleMigrationEntryV1 = unsafe extern "C" fn(*mut ElmNativeMigrationContextV1) -> i32;
/// 原生模块激活后入口类型。
pub type ElmModuleEntryV1 = unsafe extern "C" fn(*mut ElmNativeEntryFrameV1) -> i32;

#[repr(C)]
/// 一个 ELM 镜像唯一的模块描述符。
///
/// 描述符本身只保存固定布局和原生入口，不保存实例地址。实例由 attribute 生成的
/// [`ModuleSlot`] 管理，从而保证旧 generation 的描述符不能取得新 generation 的状态。
pub struct ElmModuleDescriptorV1 {
    /// 固定魔数 [`ELM_MODULE_DESCRIPTOR_MAGIC`]。
    pub magic: [u8; 8],
    /// 固定 ABI 版本 [`ELM_MODULE_DESCRIPTOR_ABI_VERSION`]。
    pub abi_version: u16,
    /// 当前结构的完整字节数。
    pub struct_size: u16,
    /// 当前版本必须为零。
    pub flags: u32,
    /// 模块实例的 Rust 内存布局尺寸。
    pub instance_size: u64,
    /// 模块实例的 Rust 内存布局对齐。
    pub instance_align: u64,
    /// 构造实例并执行初始化的入口。
    pub initialize: ElmModuleLifecycleEntryV1,
    /// 执行终结并销毁实例的入口。
    pub finalize: ElmModuleLifecycleEntryV1,
    /// 静默入口。
    pub quiesce: ElmModuleLifecycleEntryV1,
    /// 暂停入口。
    pub pause: ElmModuleLifecycleEntryV1,
    /// 恢复入口。
    pub resume: ElmModuleLifecycleEntryV1,
    /// 迁移导出入口。
    pub migrate_export: ElmModuleMigrationEntryV1,
    /// 迁移导入入口。
    pub migrate_import: ElmModuleMigrationEntryV1,
    /// 迁移撤销入口。
    pub migrate_abort: ElmModuleMigrationEntryV1,
    /// 激活后入口。
    pub entry: ElmModuleEntryV1,
}

impl ElmModuleDescriptorV1 {
    /// 使用完整入口表构造模块描述符。
    #[allow(clippy::too_many_arguments)]
    pub const fn new<T: ElmModule>(
        initialize: ElmModuleLifecycleEntryV1,
        finalize: ElmModuleLifecycleEntryV1,
        quiesce: ElmModuleLifecycleEntryV1,
        pause: ElmModuleLifecycleEntryV1,
        resume: ElmModuleLifecycleEntryV1,
        migrate_export: ElmModuleMigrationEntryV1,
        migrate_import: ElmModuleMigrationEntryV1,
        migrate_abort: ElmModuleMigrationEntryV1,
        entry: ElmModuleEntryV1,
    ) -> Self {
        Self {
            magic: ELM_MODULE_DESCRIPTOR_MAGIC,
            abi_version: ELM_MODULE_DESCRIPTOR_ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u16,
            flags: 0,
            instance_size: core::mem::size_of::<T>() as u64,
            instance_align: core::mem::align_of::<T>() as u64,
            initialize,
            finalize,
            quiesce,
            pause,
            resume,
            migrate_export,
            migrate_import,
            migrate_abort,
            entry,
        }
    }

    /// 检查固定头部、入口和实例布局是否满足统一模块 ABI。
    pub fn valid(&self) -> bool {
        self.magic == ELM_MODULE_DESCRIPTOR_MAGIC
            && self.abi_version == ELM_MODULE_DESCRIPTOR_ABI_VERSION
            && self.struct_size as usize == core::mem::size_of::<Self>()
            && self.flags & !ELM_MODULE_DESCRIPTOR_FLAGS_MASK == 0
            && self.instance_align != 0
            && self.instance_align.is_power_of_two()
            && self.instance_size <= usize::MAX as u64
            && self.initialize as usize != 0
            && self.finalize as usize != 0
            && self.quiesce as usize != 0
            && self.pause as usize != 0
            && self.resume as usize != 0
            && self.migrate_export as usize != 0
            && self.migrate_import as usize != 0
            && self.migrate_abort as usize != 0
            && self.entry as usize != 0
    }

    /// 检查固定头部和实例布局是否与 `T` 完全一致。
    pub fn valid_for<T: ElmModule>(&self) -> bool {
        self.valid()
            && self.instance_size == core::mem::size_of::<T>() as u64
            && self.instance_align == core::mem::align_of::<T>() as u64
    }
}

const MODULE_SLOT_EMPTY: u8 = 0;
const MODULE_SLOT_CONSTRUCTING: u8 = 1;
const MODULE_SLOT_ACTIVE: u8 = 2;
const MODULE_SLOT_TRANSITIONING: u8 = 3;

#[doc(hidden)]
/// attribute 生成代码使用的单 generation 模块实例槽。
pub struct ModuleSlot<T: ElmModule> {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<T>>,
}

impl<T: ElmModule> ModuleSlot<T> {
    /// 构造空实例槽。
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(MODULE_SLOT_EMPTY),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    fn active_ref(&self) -> Result<&T, HookError> {
        if self.state.load(Ordering::Acquire) != MODULE_SLOT_ACTIVE {
            return Err(HookError::new(ELM_CALL_STATUS_INVALID));
        }
        // Safety: ACTIVE 只会在 T 已完整写入后发布；终结前运行时已经排空所有共享调用。
        Ok(unsafe { (&*self.value.get()).assume_init_ref() })
    }

    #[doc(hidden)]
    /// 在当前 generation 的活动实例上执行一个共享回调。
    ///
    /// 该入口只供 `#[elm::module]` 生成的 trampoline 使用。运行时必须在终结前排空
    /// 所有回调；模块内部的可变状态应自行使用锁或原子类型同步。
    pub fn with_active<R>(&self, callback: impl FnOnce(&T) -> R) -> Result<R, HookError> {
        self.active_ref().map(callback)
    }

    #[doc(hidden)]
    /// 返回 attribute 生成的集成 export 是否可以进入当前模块实例。
    pub fn is_active(&self) -> bool {
        self.state.load(Ordering::Acquire) == MODULE_SLOT_ACTIVE
    }

    fn transitioning_mut(&self) -> &mut T {
        // Safety: 生命周期事务由运行时串行执行，TRANSITIONING 状态阻止新的共享借用。
        unsafe { (&mut *self.value.get()).assume_init_mut() }
    }

    fn begin_transition(&self) -> Result<(), HookError> {
        self.state
            .compare_exchange(
                MODULE_SLOT_ACTIVE,
                MODULE_SLOT_TRANSITIONING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| HookError::new(ELM_CALL_STATUS_INVALID))
    }

    fn finish_transition(&self) {
        self.state.store(MODULE_SLOT_ACTIVE, Ordering::Release);
    }

    /// 构造实例并执行初始化。
    pub fn initialize(&self, context: &LifecycleContext) -> HookResult {
        self.state
            .compare_exchange(
                MODULE_SLOT_EMPTY,
                MODULE_SLOT_CONSTRUCTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| HookError::new(ELM_CALL_STATUS_INVALID))?;
        let mut module = match T::create(context) {
            Ok(module) => module,
            Err(error) => {
                self.state.store(MODULE_SLOT_EMPTY, Ordering::Release);
                return Err(error);
            }
        };
        if let Err(error) = module.initialize(context) {
            let _ = module.finalize(context);
            self.state.store(MODULE_SLOT_EMPTY, Ordering::Release);
            return Err(error);
        }
        // Safety: 槽仍处于 CONSTRUCTING，当前事务独占尚未发布的存储。
        unsafe { (&mut *self.value.get()).write(module) };
        self.state.store(MODULE_SLOT_ACTIVE, Ordering::Release);
        Ok(())
    }

    /// 执行终结并在成功后销毁实例。
    pub fn finalize(&self, context: &LifecycleContext) -> HookResult {
        self.begin_transition()?;
        if let Err(error) = self.transitioning_mut().finalize(context) {
            self.finish_transition();
            return Err(error);
        }
        // Safety: 生命周期事务独占实例，finalize 成功后不会再有共享调用。
        unsafe { (&mut *self.value.get()).assume_init_drop() };
        self.state.store(MODULE_SLOT_EMPTY, Ordering::Release);
        Ok(())
    }

    /// 调用静默钩子。
    pub fn quiesce(&self, context: &LifecycleContext) -> HookResult {
        self.begin_transition()?;
        let result = self.transitioning_mut().quiesce(context);
        self.finish_transition();
        result
    }

    /// 调用暂停钩子。
    pub fn pause(&self, context: &LifecycleContext) -> HookResult {
        self.begin_transition()?;
        let result = self.transitioning_mut().pause(context);
        self.finish_transition();
        result
    }

    /// 调用恢复钩子。
    pub fn resume(&self, context: &LifecycleContext) -> HookResult {
        self.begin_transition()?;
        let result = self.transitioning_mut().resume(context);
        self.finish_transition();
        result
    }

    /// 调用迁移状态导出钩子。
    pub fn migrate_export(
        &self,
        context: &MigrationContext,
        output: &mut [u8],
    ) -> MigrationExportResult {
        self.active_ref()?.migrate_export(context, output)
    }

    /// 调用迁移状态导入钩子。
    pub fn migrate_import(&self, context: &MigrationContext, input: &[u8]) -> HookResult {
        self.begin_transition()?;
        let result = self.transitioning_mut().migrate_import(context, input);
        self.finish_transition();
        result
    }

    /// 调用迁移撤销钩子。
    pub fn migrate_abort(&self, context: &MigrationContext, input: &[u8]) -> HookResult {
        self.begin_transition()?;
        let result = self.transitioning_mut().migrate_abort(context, input);
        self.finish_transition();
        result
    }

    /// 调用激活后入口。
    pub fn entry(&self, context: &EntryContext) -> EntryResult {
        self.active_ref()?.entry(context)
    }

    /// 在模块处于活动状态时借用唯一实例。
    pub fn with<R>(&self, call: impl FnOnce(&T) -> R) -> Result<R, HookError> {
        self.active_ref().map(call)
    }
}

impl<T: ElmModule> Default for ModuleSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

// Safety: 状态机只在初始化完成后发布 T；T: Send + Sync 且生命周期事务由运行时串行化。
unsafe impl<T: ElmModule> Sync for ModuleSlot<T> {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 固定 ELM 载荷编码或解码失败。
pub enum PayloadError {
    /// 调用方提供的输出缓冲区小于 [`ElmPayload::WIRE_SIZE`]。
    BufferTooSmall,
    /// 输入长度不等于该契约的固定线格式尺寸，或编码器写入长度不一致。
    SizeMismatch,
    /// `bool` 字段在线格式中的字节既不是 0 也不是 1。
    InvalidBoolean,
}

/// 可跨 provider、受管 import/export 或 mixin 边界传输的固定线格式载荷。
///
/// 实现必须与 Rust 内存布局无关，并对同一值产生确定的小端字节串。推荐始终通过
/// [`payload`](crate::payload) 派生；手工实现者必须保证 `WIRE_SIZE` 固定、`encode` 恰好
/// 写入该长度、`decode` 拒绝任何其他长度，并验证所有受限字段。
///
/// # 示例
///
/// ```
/// use elm::ElmPayload;
///
/// #[elm::payload("example.counter@1")]
/// #[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// struct Counter {
///     value: u32,
///     enabled: bool,
/// }
///
/// let value = Counter { value: 0x1122_3344, enabled: true };
/// let mut bytes = [0_u8; Counter::WIRE_SIZE];
/// assert_eq!(value.encode(&mut bytes), Ok(5));
/// assert_eq!(bytes, [0x44, 0x33, 0x22, 0x11, 1]);
/// assert_eq!(Counter::decode(&bytes), Ok(value));
/// ```
pub trait ElmPayload: Sized {
    /// 载荷的完整 `identifier@version` 契约。
    ///
    /// 绑定和 mixin 分发必须按完整字节串匹配该值，不能只比较哈希或 Rust 类型名。
    const CONTRACT: &'static str;
    /// 该载荷在线格式中的精确字节数。
    const WIRE_SIZE: usize;

    /// 按稳定线格式编码到 `output`，成功时返回写入字节数。
    ///
    /// 输出容量不足时返回 [`PayloadError::BufferTooSmall`]；成功长度必须等于 `WIRE_SIZE`。
    fn encode(&self, output: &mut [u8]) -> Result<usize, PayloadError>;
    /// 从完整固定载荷解码一个值。
    ///
    /// 实现必须拒绝长度不等于 `WIRE_SIZE` 的输入和任何非规范编码。
    fn decode(input: &[u8]) -> Result<Self, PayloadError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 生命周期钩子看到的当前单元只读上下文。
///
/// 该类型从原生 ABI frame 复制稳定标量，不暴露内核指针。值只描述本次钩子调用，不能作为
/// 后续操作的授权凭据；运行时会在每个调用边界重新校验 generation、状态和策略。
pub struct LifecycleContext {
    cell_id: u64,
    parent_id: u64,
    generation: u64,
    state: u32,
    phase: u16,
    flags: u32,
}

impl LifecycleContext {
    const fn from_raw(raw: ElmNativeHookContextV1) -> Self {
        Self {
            cell_id: raw.cell_id,
            parent_id: raw.parent_id,
            generation: raw.generation,
            state: raw.state,
            phase: raw.phase,
            flags: raw.flags,
        }
    }

    #[doc(hidden)]
    /// 为直接编入内核的普通组件构造不携带 ELM 身份的生命周期上下文。
    pub const fn integrated(phase: u16) -> Self {
        Self {
            cell_id: 0,
            parent_id: 0,
            generation: 0,
            state: 0,
            phase,
            flags: 0,
        }
    }

    /// 返回正在执行生命周期钩子的 cell id。
    pub const fn cell_id(self) -> u64 {
        self.cell_id
    }

    /// 返回父 ELM 的 cell id；根单元的原始值为零并映射为 `None`。
    pub const fn parent_id(self) -> Option<u64> {
        if self.parent_id == 0 {
            None
        } else {
            Some(self.parent_id)
        }
    }

    /// 返回当前 cell generation。
    ///
    /// 热替换提交后旧 generation 立即陈旧，不得把此值缓存为永久身份。
    pub const fn generation(self) -> u64 {
        self.generation
    }

    /// 返回进入钩子时的 [`ElmState`](crate::ElmState) 原始编码。
    pub const fn state(self) -> u32 {
        self.state
    }

    /// 返回当前 [`ElmLifecyclePhase`](crate::ElmLifecyclePhase) 的原始编码。
    pub const fn phase(self) -> u16 {
        self.phase
    }

    /// 返回本次生命周期调用的附加标志；v1 未定义的位必须由 trampoline 拒绝。
    pub const fn flags(self) -> u32 {
        self.flags
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 热替换迁移钩子看到的代际上下文。
///
/// 同一个替换事务会把等价的上下文传给旧代导出、新代导入和新代回滚钩子。迁移缓冲区不在
/// 此结构中暴露，而是作为受调用期约束的切片单独传给业务函数。
pub struct MigrationContext {
    cell_id: u64,
    old_generation: u64,
    new_generation: u64,
    phase: u16,
}

impl MigrationContext {
    const fn from_raw(raw: &ElmNativeMigrationContextV1) -> Self {
        Self {
            cell_id: raw.cell_id,
            old_generation: raw.old_generation,
            new_generation: raw.new_generation,
            phase: raw.phase,
        }
    }

    /// 返回被替换逻辑单元的稳定 cell id。
    pub const fn cell_id(self) -> u64 {
        self.cell_id
    }

    /// 返回替换事务开始时对外服务的旧 generation。
    pub const fn old_generation(self) -> u64 {
        self.old_generation
    }

    /// 返回影子装载的新 generation；仅在提交成功后才成为公开 generation。
    pub const fn new_generation(self) -> u64 {
        self.new_generation
    }

    /// 返回当前迁移阶段的原始编码，用于区分 export、import 和 abort。
    pub const fn phase(self) -> u16 {
        self.phase
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 可选 entry 函数在单元激活后收到的只读上下文。
///
/// entry 在初始化和声明式拓扑激活完成后执行。该上下文不包含生命周期 phase，因为 entry
/// 不是生命周期提交钩子。
pub struct EntryContext {
    cell_id: u64,
    parent_id: u64,
    generation: u64,
    state: u32,
}

impl EntryContext {
    const fn from_raw(raw: ElmNativeEntryFrameV1) -> Self {
        Self {
            cell_id: raw.cell_id,
            parent_id: raw.parent_id,
            generation: raw.generation,
            state: raw.state,
        }
    }

    /// 返回执行 entry 的 cell id。
    pub const fn cell_id(self) -> u64 {
        self.cell_id
    }

    /// 返回父 ELM 的 cell id；根单元返回 `None`。
    pub const fn parent_id(self) -> Option<u64> {
        if self.parent_id == 0 {
            None
        } else {
            Some(self.parent_id)
        }
    }

    /// 返回 entry 所属的当前 generation。
    pub const fn generation(self) -> u64 {
        self.generation
    }

    /// 返回调用 entry 时的 [`ElmState`](crate::ElmState) 原始编码，通常为 `Active`。
    pub const fn state(self) -> u32 {
        self.state
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `#[elm::provider]` 业务函数收到的已验证请求视图。
///
/// trampoline 已经检查原生 frame 的 ABI 版本、保留字段、binding 关联和载荷边界。此结构
/// 按值复制固定调用帧，业务代码仍不应把 id 当作对象指针，也不能绕过 lease 去长期使用对应
/// 内核资源。
pub struct ProviderRequest {
    /// 实现该 provider 的 cell id。
    pub cell_id: u64,
    /// 本次调用命中的 provider port id。
    pub port_id: u64,
    /// 覆盖本次调用生命周期的 lease id。
    pub lease_id: u64,
    /// 请求的通用固定调用帧，包含 binding、call id、opcode、flags 和载荷。
    pub frame: ElmCallFrame,
}

impl ProviderRequest {
    /// 返回调用帧中前 `payload_len` 字节的有效载荷。
    pub fn payload(&self) -> &[u8] {
        &self.frame.payload[..usize::from(self.frame.payload_len)]
    }

    /// 使用 `T` 的固定载荷契约解码请求。
    ///
    /// 此方法只负责线格式校验；provider 实现仍应确认端口契约和 opcode 是否允许该类型。
    pub fn decode<T: ElmPayload>(&self) -> Result<T, PayloadError> {
        T::decode(self.payload())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `#[elm::export]` 业务函数收到的已验证受管调用。
///
/// 运行时已经完成 import handle 解析、作用域授权、版本选择和 generation 路由。调用方与被
/// 调用方信息用于审计和细粒度策略，不代表业务函数可以访问对应 cell 的内部内存。
pub struct ManagedRequest {
    /// 解析到本 export 的受管 import handle。
    pub import_handle: u64,
    /// 发起调用的 ELM 单元标识符。
    pub caller_cell_id: u64,
    /// 调用方代际，用于在分发前检测陈旧调用。
    pub caller_generation: u64,
    /// 接收调用的 ELM 单元标识符。
    pub callee_cell_id: u64,
    /// 被调用方代际，用于将调用路由到正确的热替换版本。
    pub callee_generation: u64,
    /// 请求的通用固定调用帧。
    pub frame: ElmCallFrame,
}

impl ManagedRequest {
    /// 返回调用帧中前 `payload_len` 字节的有效载荷。
    #[inline(always)]
    pub fn payload(&self) -> &[u8] {
        &self.frame.payload[..usize::from(self.frame.payload_len)]
    }

    /// 使用 `T` 的固定载荷契约解码请求。
    pub fn decode<T: ElmPayload>(&self) -> Result<T, PayloadError> {
        T::decode(self.payload())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// provider、受管 export 和 mixin trampoline 使用的安全回复构造器。
///
/// 该类型隐藏固定容量数组和长度维护，避免业务代码构造 `payload_len` 越界的
/// [`ElmReplyFrame`]。普通 provider 与 export 应返回零状态表示成功；mixin trampoline 会
/// 额外设置控制标志。
pub struct ProviderReply {
    status: i32,
    flags: u32,
    payload_len: u16,
    payload: [u8; ELM_FRAME_PAYLOAD_LEN],
}

impl ProviderReply {
    /// 构造指定状态且不携带载荷的回复。
    pub const fn empty(status: i32) -> Self {
        Self {
            status,
            flags: 0,
            payload_len: 0,
            payload: [0; ELM_FRAME_PAYLOAD_LEN],
        }
    }

    /// 构造状态为 [`ELM_CALL_STATUS_OK`](crate::ELM_CALL_STATUS_OK) 的空成功回复。
    pub const fn ok() -> Self {
        Self::empty(ELM_CALL_STATUS_OK)
    }

    /// 从原始字节构造回复。
    ///
    /// `payload` 超过 [`ELM_FRAME_PAYLOAD_LEN`](crate::ELM_FRAME_PAYLOAD_LEN) 时返回
    /// [`PayloadError::BufferTooSmall`]，不会截断数据。
    #[inline(always)]
    pub fn bytes(status: i32, payload: &[u8]) -> Result<Self, PayloadError> {
        if payload.len() > ELM_FRAME_PAYLOAD_LEN {
            return Err(PayloadError::BufferTooSmall);
        }
        let mut reply = Self::empty(status);
        reply.payload[..payload.len()].copy_from_slice(payload);
        reply.payload_len = payload.len() as u16;
        Ok(reply)
    }

    /// 编码类型化载荷并构造回复。
    ///
    /// `T::WIRE_SIZE` 必须能放入固定回复帧；编码器返回的实际长度会成为 `payload_len`。
    pub fn payload<T: ElmPayload>(status: i32, payload: &T) -> Result<Self, PayloadError> {
        if T::WIRE_SIZE > ELM_FRAME_PAYLOAD_LEN {
            return Err(PayloadError::BufferTooSmall);
        }
        let mut reply = Self::empty(status);
        let len = payload.encode(&mut reply.payload)?;
        reply.payload_len = len as u16;
        Ok(reply)
    }

    /// 设置协议回复标志并返回更新后的构造器值。
    ///
    /// 普通业务代码不应随意设置未知位。当前主要由 mixin trampoline 写入
    /// `CONTINUE`、`STOP`、`REPLACE` 或 `DENY` 控制标志。
    pub const fn with_flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }

    fn into_frame(self, binding_id: u64, call_id: u64) -> ElmReplyFrame {
        let mut frame = ElmReplyFrame::empty(binding_id, call_id, self.status);
        frame.flags = self.flags;
        frame.payload_len = self.payload_len;
        frame.payload = self.payload;
        frame
    }
}

/// provider 处理函数的规范结果类型。
pub type ProviderResult = Result<ProviderReply, HookError>;
/// 受管 export 处理函数的规范结果类型。
pub type ManagedResult = ProviderResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// [`ManagedImport`] 调用返回的已验证回复包装。
///
/// 构造阶段已经验证保留字段和载荷边界，并核对底层 reply 的 binding/call id。业务代码可先
/// 检查状态，再按契约读取字节或解码固定载荷。
pub struct ManagedReply {
    frame: ElmReplyFrame,
}

impl ManagedReply {
    fn from_frame(frame: ElmReplyFrame) -> Result<Self, RuntimeApiError> {
        if frame.reserved0 != 0
            || frame.reserved1 != 0
            || usize::from(frame.payload_len) > frame.payload.len()
        {
            return Err(RuntimeApiError::MalformedResponse);
        }
        Ok(Self { frame })
    }

    /// 返回被调用 export 写入的业务状态码。
    pub const fn status(self) -> i32 {
        self.frame.status
    }

    /// 返回回复标志；调用契约未定义的位应视为不兼容。
    pub const fn flags(self) -> u32 {
        self.frame.flags
    }

    /// 返回回复中前 `payload_len` 字节的有效载荷。
    pub fn payload(&self) -> &[u8] {
        &self.frame.payload[..usize::from(self.frame.payload_len)]
    }

    /// 使用 `T` 的固定载荷契约解码回复。
    pub fn decode<T: ElmPayload>(&self) -> Result<T, RuntimeApiError> {
        T::decode(self.payload()).map_err(RuntimeApiError::Payload)
    }

    /// 消费安全包装并取得底层固定布局回复帧。
    ///
    /// 只有需要转交给其他框架 API 时才应使用此方法；普通业务代码优先使用 `status`、
    /// `payload` 和 `decode`。
    pub const fn into_frame(self) -> ElmReplyFrame {
        self.frame
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `#[elm::provider_snapshot]` 业务函数收到的快照请求。
///
/// 快照调用由运行时用 lease 保护。分页请求的 `cursor` 只对同一 provider、binding 和快照
/// 契约有意义；实现不得把它解释为可直接解引用的地址。
pub struct SnapshotRequest {
    /// 实现该快照入口的 cell id。
    pub cell_id: u64,
    /// 被查询的 provider port id。
    pub port_id: u64,
    /// 发起快照查询的 binding id。
    pub binding_id: u64,
    /// 覆盖本次快照生成过程的 lease id。
    pub lease_id: u64,
    /// 是否请求分页快照。
    pub paged: bool,
    /// 当前分页游标；非分页请求恒为零。
    pub cursor: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// provider 快照函数对输出缓冲区的描述。
///
/// 实际字节由处理函数写入 trampoline 提供的切片，本结构只报告状态、有效前缀、记录数量和
/// 分页游标。`payload_len` 不能大于输出容量；存在下一页时 `next_cursor` 必须非零且不同于
/// 请求游标。
pub struct SnapshotReply {
    /// 操作结果状态码；零或专用成功码表示成功，其余值按所属协议解释。
    pub status: i32,
    /// 有效载荷的实际字节数；不得超过相邻载荷缓冲区容量。
    pub payload_len: usize,
    /// 回复中包含的完整记录数量。
    pub record_count: u32,
    /// 下一页游标；`None` 表示本次回复已经完整结束。
    pub next_cursor: Option<u32>,
}

impl SnapshotReply {
    /// 构造不再有后续页面的成功回复。
    pub const fn complete(payload_len: usize, record_count: u32) -> Self {
        Self {
            status: MGR_STATUS_OK,
            payload_len,
            record_count,
            next_cursor: None,
        }
    }

    /// 构造仍有后续页面的成功回复。
    ///
    /// 调用方必须确保当前请求启用了分页，且 `next_cursor` 非零并向前推进。
    pub const fn more(payload_len: usize, record_count: u32, next_cursor: u32) -> Self {
        Self {
            status: MGR_STATUS_OK,
            payload_len,
            record_count,
            next_cursor: Some(next_cursor),
        }
    }

    /// 构造不携带载荷和记录的失败回复。
    pub const fn error(status: i32) -> Self {
        Self {
            status,
            payload_len: 0,
            record_count: 0,
            next_cursor: None,
        }
    }
}

/// provider 快照处理函数的规范结果类型。
pub type SnapshotResult = Result<SnapshotReply, HookError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// mixin 处理器对当前补缀阶段的控制结果。
pub enum MixinControl {
    /// 不替换帧，继续执行当前阶段的后续处理器。
    Continue,
    /// 不替换帧，但停止当前阶段的后续处理器。
    Stop,
    /// 用处理器修改后的帧替换当前帧，然后继续当前阶段。
    Replace,
    /// 替换当前帧并停止当前阶段的后续处理器。
    ReplaceAndStop,
    /// 拒绝整个补缀点调用；包装函数返回失败且不再执行原函数或后续阶段。
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 一个分阶段 mixin 补缀点的静态描述。
///
/// 此类型主要由 [`mixin_point`](crate::mixin_point) 展开代码构造。每个非空点名已经包含阶段
/// 后缀，例如 `scheduler.select.ingress`；`None` 表示该阶段未启用。
pub struct MixinPointDescriptor {
    /// 各阶段共享的固定载荷契约。
    pub contract: &'static str,
    /// 原函数执行前的 ingress 点名。
    pub ingress: Option<&'static str>,
    /// 可替代原函数的 substitute 点名。
    pub substitute: Option<&'static str>,
    /// 原函数或替代逻辑完成后的 egress 点名。
    pub egress: Option<&'static str>,
    /// 最后执行的 observe 点名。
    pub observe: Option<&'static str>,
}

#[repr(transparent)]
#[derive(Debug)]
/// 由装载器写入受管句柄的安全 import 槽。
///
/// 该类型必须作为不可变 `static` 并由 [`import`](crate::import) 标记。装载器只写入不透明
/// handle；每次调用都回到运行时执行代际路由、授权、并发和回复关联校验，因此它是支持热替换
/// 的默认跨 ELM 调用方式。
///
/// # 示例
///
/// ```no_run
/// use elm::ManagedImport;
///
/// #[elm::import(
///     name = "example.echo",
///     contract = "example.echo@1",
///     version = 1,
///     optional = true
/// )]
/// static ECHO: ManagedImport = ManagedImport::new();
///
/// let reply = ECHO.call_bytes(1, b"hello")?;
/// if reply.status() != elm::ELM_CALL_STATUS_OK {
///     return Err(elm::RuntimeApiError::Status(reply.status()));
/// }
/// # Ok::<(), elm::RuntimeApiError>(())
/// ```
pub struct ManagedImport {
    slot: ImportSlot,
}

impl ManagedImport {
    /// 构造尚未绑定的零值 import 槽。
    ///
    /// 只有装载器可以在模块激活前写入该槽；业务代码不能自行绑定 handle。
    pub const fn new() -> Self {
        Self {
            slot: ImportSlot::new(),
        }
    }

    /// 返回装载器写入的不透明 handle；可选 import 未解析时返回 `None`。
    ///
    /// handle 不是地址，不能解引用，也不应持久化到镜像外部。
    pub fn handle(&self) -> Option<u64> {
        let value = self.slot.read();
        (value != 0).then_some(value as u64)
    }

    /// 使用已经构造的固定调用帧执行一次受管调用。
    ///
    /// 运行时会覆盖路由语义并验证返回的 binding/call id。多数业务代码应使用
    /// [`call_bytes`](Self::call_bytes)、[`call_payload`](Self::call_payload) 或
    /// [`call`](Self::call)，避免自行维护 call id。
    pub fn invoke(&self, request: &ElmCallFrame) -> Result<ElmReplyFrame, RuntimeApiError> {
        let handle = self.handle().ok_or(RuntimeApiError::ImportUnavailable)?;
        runtime_api::invoke_managed(handle, request)
    }

    /// 用原始载荷执行受管调用。
    ///
    /// 框架自动生成非零 call id，并以 binding id 0 请求运行时按 import handle 路由。载荷
    /// 超过固定帧容量时不会截断，而是返回 [`PayloadError::BufferTooSmall`]。
    pub fn call_bytes(&self, opcode: u32, payload: &[u8]) -> Result<ManagedReply, RuntimeApiError> {
        if payload.len() > ELM_FRAME_PAYLOAD_LEN {
            return Err(RuntimeApiError::Payload(PayloadError::BufferTooSmall));
        }
        let request = ElmCallFrame::new(0, next_managed_call_id(), opcode, payload);
        ManagedReply::from_frame(self.invoke(&request)?)
    }

    /// 编码一个 [`ElmPayload`] 请求并执行受管调用。
    ///
    /// 此方法不自动要求回复状态成功，也不解码回复，适合一个操作可能返回多种载荷契约的场景。
    pub fn call_payload<T: ElmPayload>(
        &self,
        opcode: u32,
        payload: &T,
    ) -> Result<ManagedReply, RuntimeApiError> {
        if T::WIRE_SIZE > ELM_FRAME_PAYLOAD_LEN {
            return Err(RuntimeApiError::Payload(PayloadError::BufferTooSmall));
        }
        let mut bytes = [0u8; ELM_FRAME_PAYLOAD_LEN];
        let len = payload.encode(&mut bytes)?;
        if len > bytes.len() {
            return Err(RuntimeApiError::MalformedResponse);
        }
        self.call_bytes(opcode, &bytes[..len])
    }

    /// 执行完整的类型化请求/回复调用。
    ///
    /// 请求使用 `T` 编码；只有回复状态为 `ELM_CALL_STATUS_OK` 时才使用 `R` 解码。状态失败、
    /// 线格式错误和运行时错误都通过 [`RuntimeApiError`] 返回。
    pub fn call<T: ElmPayload, R: ElmPayload>(
        &self,
        opcode: u32,
        payload: &T,
    ) -> Result<R, RuntimeApiError> {
        let reply = self.call_payload(opcode, payload)?;
        if reply.status() != ELM_CALL_STATUS_OK {
            return Err(RuntimeApiError::Status(reply.status()));
        }
        reply.decode()
    }
}

impl Default for ManagedImport {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(transparent)]
#[derive(Debug)]
/// 由装载器写入原生地址的类型化直接固定 import 槽。
///
/// 该路径绕过受管调用帧。attribute 会从 `F` 生成规范 Rust ABI 字符串，打包器将其 SHA-256
/// 写入 EBI/EKI；装载器只在名称、契约、版本和摘要全部匹配时写入函数地址。动态 provider 的
/// generation 会被固定，直到所有直接调用者卸载；因此该路径会限制 provider 热替换。需要
/// generation 路由、固定载荷隔离或失败重试时应使用 [`ManagedImport`]。
pub struct DirectImport<F> {
    slot: ImportSlot,
    marker: PhantomData<fn() -> F>,
}

impl<F> DirectImport<F> {
    /// 构造尚未绑定的零值直接导入槽。
    pub const fn new() -> Self {
        Self {
            slot: ImportSlot::new(),
            marker: PhantomData,
        }
    }

    /// 返回装载器写入的原生目标函数。
    ///
    /// # 安全性
    ///
    /// `F` 必须是 `#[elm::import]` 或 `#[elm::kernel_symbol]` 已写入元数据的 Rust 函数指针
    /// 类型。调用方还必须保证参数满足目标函数的语义前置条件，且 panic 不会跨 ELM 边界
    /// 展开。装载器负责在激活前核对签名摘要并固定目标 generation。
    pub unsafe fn get(&self) -> Option<F>
    where
        F: Copy,
    {
        let value = self.slot.read();
        if value == 0 {
            return None;
        }
        assert_eq!(core::mem::size_of::<F>(), core::mem::size_of::<usize>());
        // Safety: 宏只允许 Rust 函数指针作为 F，装载器已按同一规范签名摘要解析该地址。
        Some(unsafe { core::mem::transmute_copy::<usize, F>(&value) })
    }
}

impl<F> Default for DirectImport<F> {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(transparent)]
#[derive(Debug)]
struct ImportSlot(UnsafeCell<usize>);

impl ImportSlot {
    const fn new() -> Self {
        Self(UnsafeCell::new(0))
    }

    fn read(&self) -> usize {
        // 安全性：装载器只在激活前写入槽位；运行期只做易失只读访问。
        unsafe { core::ptr::read_volatile(self.0.get()) }
    }
}

unsafe impl Sync for ImportSlot {}

#[repr(transparent)]
struct RootImportSlot(UnsafeCell<usize>);

unsafe impl Sync for RootImportSlot {}

static NEXT_MANAGED_CALL_ID: AtomicU64 = AtomicU64::new(1);

fn next_managed_call_id() -> u64 {
    let id = NEXT_MANAGED_CALL_ID.fetch_add(1, Ordering::Relaxed);
    if id == 0 {
        NEXT_MANAGED_CALL_ID.fetch_add(1, Ordering::Relaxed)
    } else {
        id
    }
}

#[unsafe(export_name = "__elm_api_root_slot_v1")]
#[unsafe(link_section = ".data.elm_imports")]
#[used]
static ELM_API_ROOT_SLOT: RootImportSlot = RootImportSlot(UnsafeCell::new(0));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 安全开发包装访问 ELM 根 API、运行时表或受管 import 时的错误。
///
/// 该枚举区分装载链接问题、协议布局问题、业务状态和固定载荷问题，便于模块决定是降级、
/// 返回 `HookError` 还是主动中止。它不包含内核内部错误类型，因而可以稳定存在于外部框架。
pub enum RuntimeApiError {
    /// 根 API 导入槽仍为零，通常表示模块没有通过合规装载器启动。
    RootUnavailable,
    /// 根表魔数、版本、选择版本或最小结构尺寸与当前框架不兼容。
    IncompatibleRoot,
    /// 根表没有提供兼容的普通运行时函数表。
    RuntimeUnavailable,
    /// 受管 import 槽未绑定；可选 import 在没有匹配 export 时会产生此错误。
    ImportUnavailable,
    /// 调用方缓冲区不足；携带运行时报告的所需最小字节数。
    BufferTooSmall(usize),
    /// 运行时回复违反结构尺寸、保留字段、载荷边界或调用关联不变量。
    MalformedResponse,
    /// 运行时或被调用 export 返回了非零稳定状态码。
    Status(i32),
    /// 请求编码或回复解码失败。
    Payload(PayloadError),
}

impl From<PayloadError> for RuntimeApiError {
    fn from(value: PayloadError) -> Self {
        Self::Payload(value)
    }
}

pub(crate) mod runtime_api {
    use super::*;

    pub fn features() -> Result<u64, RuntimeApiError> {
        Ok(root()?.features)
    }

    pub fn log(level: u32, message: &str) -> Result<(), RuntimeApiError> {
        let status = (runtime()?.log)(level, message.as_ptr(), message.len());
        status_result(status)
    }

    pub fn abort_current(reason: u32) -> ! {
        match runtime() {
            Ok(runtime) => (runtime.abort_current)(reason),
            Err(_) => loop {
                core::hint::spin_loop();
            },
        }
    }

    pub fn abort_panic() -> ! {
        abort_current(ELM_API_ABORT_REASON_PANIC)
    }

    pub fn current_context() -> Result<ElmApiContextV1, RuntimeApiError> {
        let mut output = ElmApiContextV1::empty();
        let status = (runtime()?.current_context)(&mut output);
        status_result(status)?;
        Ok(output)
    }

    pub fn dispatch_mixin(input: &[u8], output: &mut [u8]) -> Result<usize, RuntimeApiError> {
        let mut output_len = 0usize;
        let status = (runtime()?.dispatch_mixin)(
            input.as_ptr(),
            input.len(),
            output.as_mut_ptr(),
            output.len(),
            &mut output_len,
        );
        if status == ELM_API_STATUS_BUFFER_TOO_SMALL {
            return Err(RuntimeApiError::BufferTooSmall(output_len));
        }
        status_result(status)?;
        if output_len > output.len() {
            return Err(RuntimeApiError::MalformedResponse);
        }
        Ok(output_len)
    }

    pub fn invoke_managed(
        import_handle: u64,
        request: &ElmCallFrame,
    ) -> Result<ElmReplyFrame, RuntimeApiError> {
        let mut reply = ElmReplyFrame::empty(
            request.binding_id,
            request.call_id,
            ELM_CALL_STATUS_PROVIDER_FAULT,
        );
        let status = (runtime()?.invoke_managed)(import_handle, request, &mut reply);
        status_result(status)?;
        if reply.binding_id != request.binding_id || reply.call_id != request.call_id {
            return Err(RuntimeApiError::MalformedResponse);
        }
        Ok(reply)
    }

    pub fn query_namespace(
        identifier: &str,
        versions: &[u16],
    ) -> Result<ElmApiNamespaceV1, RuntimeApiError> {
        let mut output = ElmApiNamespaceV1::empty();
        let status = (root()?.query_namespace)(
            identifier.as_ptr(),
            identifier.len(),
            versions.as_ptr(),
            versions.len(),
            &mut output,
        );
        status_result(status)?;
        Ok(output)
    }

    pub(crate) fn ensure_linked() {
        let _ = root_address();
    }

    fn root() -> Result<&'static ElmApiRootV1, RuntimeApiError> {
        let address = root_address();
        if address == 0 {
            return Err(RuntimeApiError::RootUnavailable);
        }
        // 安全性：槽位只由 ELM 装载器写入经过 ABI 校验的静态根表地址。
        let root = unsafe { &*(address as *const ElmApiRootV1) };
        if root.magic != ELM_API_ROOT_MAGIC
            || root.abi_version != ELM_API_VERSION_V1
            || root.selected_version != ELM_API_VERSION_V1
            || root.struct_size < core::mem::size_of::<ElmApiRootV1>() as u32
        {
            return Err(RuntimeApiError::IncompatibleRoot);
        }
        Ok(root)
    }

    fn runtime() -> Result<&'static ElmRuntimeApiV1, RuntimeApiError> {
        let root = root()?;
        if root.runtime_table.is_null()
            || root.runtime_table_size < core::mem::size_of::<ElmRuntimeApiV1>() as u32
        {
            return Err(RuntimeApiError::RuntimeUnavailable);
        }
        // 安全性：根表由内核发布，且已验证表地址和最小尺寸。
        let runtime = unsafe { &*root.runtime_table };
        if runtime.abi_version != ELM_API_VERSION_V1
            || runtime.struct_size < core::mem::size_of::<ElmRuntimeApiV1>() as u32
        {
            return Err(RuntimeApiError::RuntimeUnavailable);
        }
        Ok(runtime)
    }

    fn root_address() -> usize {
        // 安全性：装载阶段完成单次槽位重定位，运行阶段只做易失读取。
        unsafe { core::ptr::read_volatile(ELM_API_ROOT_SLOT.0.get()) }
    }

    fn status_result(status: i32) -> Result<(), RuntimeApiError> {
        if status == 0 {
            Ok(())
        } else {
            Err(RuntimeApiError::Status(status))
        }
    }
}

/// 按固定阶段顺序执行一个 mixin 补缀点。
///
/// 此函数主要供 [`mixin_point`](crate::mixin_point) 展开代码调用。它把 `frame` 编码后交给
/// elm-mgr 的 extension dispatcher，并严格按 ingress、substitute、原实现、egress、observe
/// 顺序推进。substitute 返回替换帧时跳过 `original`；observe 阶段返回的替换标志会被忽略，
/// 但拒绝和协议错误仍会使调用失败。
///
/// `descriptor.contract` 必须与 `T::CONTRACT` 以及所有 attached mixin 声明一致，且
/// `T::WIRE_SIZE` 不得超过运行时扩展载荷容量。任何阶段返回 deny、blocker、非成功状态、
/// 错误长度或不可解码替换载荷时返回 [`HookError`]。
///
/// 普通模块不应手工拼装描述符；使用 attribute 可以同时生成正确的阶段名称和 `.elm.meta`
/// 扩展点声明。
pub fn run_mixin_point<T: ElmPayload>(
    descriptor: MixinPointDescriptor,
    frame: &mut T,
    original: fn(&mut T) -> PointResult,
) -> PointResult {
    if let Some(point) = descriptor.ingress {
        dispatch_mixin_stage(point, descriptor.contract, frame)?;
    }
    let substituted = match descriptor.substitute {
        Some(point) => dispatch_mixin_stage(point, descriptor.contract, frame)?,
        None => false,
    };
    if !substituted {
        original(frame)?;
    }
    if let Some(point) = descriptor.egress {
        dispatch_mixin_stage(point, descriptor.contract, frame)?;
    }
    if let Some(point) = descriptor.observe {
        let _ = dispatch_mixin_stage(point, descriptor.contract, frame)?;
    }
    Ok(())
}

fn dispatch_mixin_stage<T: ElmPayload>(
    point: &str,
    contract: &str,
    frame: &mut T,
) -> Result<bool, HookError> {
    if T::WIRE_SIZE > MGR_EXTENSION_PAYLOAD_LEN {
        return Err(HookError::new(ELM_CALL_STATUS_INVALID));
    }
    let context = runtime_api::current_context().map_err(runtime_error_to_hook)?;
    let mut request = ModuleExtensionDispatchRequest::new(context.cell_id, point, contract)
        .ok_or_else(|| HookError::new(ELM_CALL_STATUS_INVALID))?;
    let payload_len = frame
        .encode(&mut request.payload)
        .map_err(|_| HookError::new(ELM_CALL_STATUS_INVALID))?;
    request.payload_len = payload_len as u16;
    let input = request.encode();
    let mut output = [0u8; MGR_RESPONSE_HEADER_SIZE + MGR_EXTENSION_DISPATCH_RESPONSE_SIZE];
    let output_len =
        runtime_api::dispatch_mixin(&input, &mut output).map_err(runtime_error_to_hook)?;
    let header_size = MGR_RESPONSE_HEADER_SIZE;
    let response_size = MGR_EXTENSION_DISPATCH_RESPONSE_SIZE;
    if output_len != header_size + response_size {
        return Err(HookError::new(ELM_CALL_STATUS_INVALID));
    }
    let header = ModuleMgrResponseHeader::decode(&output[..header_size])
        .ok_or_else(|| HookError::new(ELM_CALL_STATUS_INVALID))?;
    if header.status != MGR_STATUS_OK
        || header.reserved != 0
        || header.payload_len as usize != response_size
    {
        return Err(HookError::new(header.status));
    }
    let response = ModuleExtensionDispatchResponse::decode(&output[header_size..])
        .ok_or_else(|| HookError::new(ELM_CALL_STATUS_INVALID))?;
    if response.status != MGR_STATUS_OK || response.blockers != 0 {
        return Err(HookError::new(response.status));
    }
    if response.reply.flags & MIXIN_REPLY_DENY != 0 {
        return Err(HookError::new(ELM_CALL_STATUS_INVALID));
    }
    let replaced = response.reply.flags & MIXIN_REPLY_REPLACE != 0;
    if replaced {
        let len = usize::from(response.reply.payload_len);
        if len > response.reply.payload.len() {
            return Err(HookError::new(ELM_CALL_STATUS_INVALID));
        }
        *frame = T::decode(&response.reply.payload[..len])
            .map_err(|_| HookError::new(ELM_CALL_STATUS_INVALID))?;
    }
    Ok(replaced)
}

fn runtime_error_to_hook(error: RuntimeApiError) -> HookError {
    match error {
        RuntimeApiError::Status(status) => HookError::new(status),
        _ => HookError::new(ELM_CALL_STATUS_INVALID),
    }
}

#[doc(hidden)]
pub mod __private {
    use super::*;

    unsafe fn module_lifecycle_call(
        raw: *mut ElmNativeHookContextV1,
        expected_phase: u16,
        call: impl FnOnce(&LifecycleContext) -> HookResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_ref() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.abi_version != ELM_NATIVE_HOOK_CONTEXT_ABI_VERSION
            || raw.phase != expected_phase
            || raw.reserved != 0
        {
            return ELM_CALL_STATUS_INVALID;
        }
        match call(&LifecycleContext::from_raw(*raw)) {
            Ok(()) => 0,
            Err(error) => error.status(),
        }
    }

    /// 构造模块实例并执行初始化。
    pub unsafe fn module_initialize_trampoline<T: ElmModule>(
        slot: &'static ModuleSlot<T>,
        raw: *mut ElmNativeHookContextV1,
    ) -> i32 {
        unsafe { module_lifecycle_call(raw, 1, |context| slot.initialize(context)) }
    }

    /// 执行模块终结并销毁实例。
    pub unsafe fn module_finalize_trampoline<T: ElmModule>(
        slot: &'static ModuleSlot<T>,
        raw: *mut ElmNativeHookContextV1,
    ) -> i32 {
        unsafe { module_lifecycle_call(raw, 2, |context| slot.finalize(context)) }
    }

    /// 调用模块静默钩子。
    pub unsafe fn module_quiesce_trampoline<T: ElmModule>(
        slot: &'static ModuleSlot<T>,
        raw: *mut ElmNativeHookContextV1,
    ) -> i32 {
        unsafe { module_lifecycle_call(raw, 3, |context| slot.quiesce(context)) }
    }

    /// 调用模块暂停钩子。
    pub unsafe fn module_pause_trampoline<T: ElmModule>(
        slot: &'static ModuleSlot<T>,
        raw: *mut ElmNativeHookContextV1,
    ) -> i32 {
        unsafe { module_lifecycle_call(raw, 4, |context| slot.pause(context)) }
    }

    /// 调用模块恢复钩子。
    pub unsafe fn module_resume_trampoline<T: ElmModule>(
        slot: &'static ModuleSlot<T>,
        raw: *mut ElmNativeHookContextV1,
    ) -> i32 {
        unsafe { module_lifecycle_call(raw, 5, |context| slot.resume(context)) }
    }

    /// 调用模块迁移导出钩子。
    pub unsafe fn module_migration_export_trampoline<T: ElmModule>(
        slot: &'static ModuleSlot<T>,
        raw: *mut ElmNativeMigrationContextV1,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if !migration_context_valid(raw, 6) {
            return ELM_CALL_STATUS_INVALID;
        }
        let Ok(capacity) = usize::try_from(raw.buffer_capacity) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.buffer_ptr == 0 && capacity != 0 {
            return ELM_CALL_STATUS_INVALID;
        }
        let output = if capacity == 0 {
            &mut []
        } else {
            // Safety: 原生 frame 已验证非空地址和完整容量，借用只持续本次钩子调用。
            unsafe { core::slice::from_raw_parts_mut(raw.buffer_ptr as *mut u8, capacity) }
        };
        match slot.migrate_export(&MigrationContext::from_raw(raw), output) {
            Ok(len) if len <= capacity => {
                raw.buffer_len = len as u64;
                raw.status = 0;
                0
            }
            Ok(_) => ELM_CALL_STATUS_INVALID,
            Err(error) => error.status(),
        }
    }

    unsafe fn module_migration_input_call<T: ElmModule>(
        slot: &'static ModuleSlot<T>,
        raw: *mut ElmNativeMigrationContextV1,
        expected_phase: u16,
        call: impl FnOnce(&ModuleSlot<T>, &MigrationContext, &[u8]) -> HookResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if !migration_context_valid(raw, expected_phase)
            || raw.buffer_len > raw.buffer_capacity
            || raw.buffer_ptr == 0 && raw.buffer_len != 0
        {
            return ELM_CALL_STATUS_INVALID;
        }
        let Ok(len) = usize::try_from(raw.buffer_len) else {
            return ELM_CALL_STATUS_INVALID;
        };
        let input = if len == 0 {
            &[]
        } else {
            // Safety: 原生 frame 已验证非空地址和有效长度，借用只持续本次钩子调用。
            unsafe { core::slice::from_raw_parts(raw.buffer_ptr as *const u8, len) }
        };
        match call(slot, &MigrationContext::from_raw(raw), input) {
            Ok(()) => {
                raw.status = 0;
                0
            }
            Err(error) => error.status(),
        }
    }

    /// 调用模块迁移导入钩子。
    pub unsafe fn module_migration_import_trampoline<T: ElmModule>(
        slot: &'static ModuleSlot<T>,
        raw: *mut ElmNativeMigrationContextV1,
    ) -> i32 {
        unsafe {
            module_migration_input_call(slot, raw, 7, |slot, context, input| {
                slot.migrate_import(context, input)
            })
        }
    }

    /// 调用模块迁移撤销钩子。
    pub unsafe fn module_migration_abort_trampoline<T: ElmModule>(
        slot: &'static ModuleSlot<T>,
        raw: *mut ElmNativeMigrationContextV1,
    ) -> i32 {
        unsafe {
            module_migration_input_call(slot, raw, 8, |slot, context, input| {
                slot.migrate_abort(context, input)
            })
        }
    }

    /// 调用模块激活后入口。
    pub unsafe fn module_entry_trampoline<T: ElmModule>(
        slot: &'static ModuleSlot<T>,
        raw: *mut ElmNativeEntryFrameV1,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.abi_version != ELM_NATIVE_ENTRY_ABI_VERSION
            || raw.flags != 0
            || raw.reserved0 != 0
            || raw.reserved1 != 0
        {
            return ELM_CALL_STATUS_INVALID;
        }
        match slot.entry(&EntryContext::from_raw(*raw)) {
            Ok(()) => {
                raw.exit_code = 0;
                0
            }
            Err(error) => {
                raw.exit_code = error.status();
                error.status()
            }
        }
    }

    pub unsafe fn lifecycle_trampoline(
        raw: *mut ElmNativeHookContextV1,
        expected_phase: u16,
        handler: fn(&LifecycleContext) -> HookResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_ref() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.abi_version != ELM_NATIVE_HOOK_CONTEXT_ABI_VERSION
            || raw.phase != expected_phase
            || raw.reserved != 0
        {
            return ELM_CALL_STATUS_INVALID;
        }
        match handler(&LifecycleContext::from_raw(*raw)) {
            Ok(()) => 0,
            Err(error) => error.status(),
        }
    }

    pub unsafe fn migration_export_trampoline(
        raw: *mut ElmNativeMigrationContextV1,
        handler: fn(&MigrationContext, &mut [u8]) -> MigrationExportResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if !migration_context_valid(raw, 6) {
            return ELM_CALL_STATUS_INVALID;
        }
        let Ok(capacity) = usize::try_from(raw.buffer_capacity) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.buffer_ptr == 0 && capacity != 0 {
            return ELM_CALL_STATUS_INVALID;
        }
        let output = if capacity == 0 {
            &mut []
        } else {
            unsafe { core::slice::from_raw_parts_mut(raw.buffer_ptr as *mut u8, capacity) }
        };
        match handler(&MigrationContext::from_raw(raw), output) {
            Ok(len) if len <= capacity => {
                raw.buffer_len = len as u64;
                raw.status = 0;
                0
            }
            Ok(_) => ELM_CALL_STATUS_INVALID,
            Err(error) => error.status(),
        }
    }

    pub unsafe fn migration_input_trampoline(
        raw: *mut ElmNativeMigrationContextV1,
        expected_phase: u16,
        handler: fn(&MigrationContext, &[u8]) -> HookResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if !migration_context_valid(raw, expected_phase)
            || raw.buffer_len > raw.buffer_capacity
            || raw.buffer_ptr == 0 && raw.buffer_len != 0
        {
            return ELM_CALL_STATUS_INVALID;
        }
        let Ok(len) = usize::try_from(raw.buffer_len) else {
            return ELM_CALL_STATUS_INVALID;
        };
        let input = if len == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(raw.buffer_ptr as *const u8, len) }
        };
        match handler(&MigrationContext::from_raw(raw), input) {
            Ok(()) => {
                raw.status = 0;
                0
            }
            Err(error) => error.status(),
        }
    }

    pub unsafe fn entry_trampoline(
        raw: *mut ElmNativeEntryFrameV1,
        handler: fn(&EntryContext) -> EntryResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.abi_version != ELM_NATIVE_ENTRY_ABI_VERSION
            || raw.flags != 0
            || raw.reserved0 != 0
            || raw.reserved1 != 0
        {
            return ELM_CALL_STATUS_INVALID;
        }
        match handler(&EntryContext::from_raw(*raw)) {
            Ok(()) => {
                raw.exit_code = 0;
                0
            }
            Err(error) => {
                raw.exit_code = error.status();
                error.status()
            }
        }
    }

    pub unsafe fn provider_trampoline<F>(raw: *mut ElmNativeProviderCallV1, handler: F) -> i32
    where
        F: FnOnce(&ProviderRequest) -> ProviderResult,
    {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.abi_version != ELM_NATIVE_PROVIDER_CALL_ABI_VERSION
            || raw.flags != 0
            || raw.reserved0 != 0
            || raw.binding_id != raw.request.binding_id
            || usize::from(raw.request.payload_len) > raw.request.payload.len()
        {
            return ELM_CALL_STATUS_INVALID;
        }
        let request = ProviderRequest {
            cell_id: raw.cell_id,
            port_id: raw.port_id,
            lease_id: raw.lease_id,
            frame: raw.request,
        };
        match handler(&request) {
            Ok(reply) => {
                raw.reply = reply.into_frame(raw.request.binding_id, raw.request.call_id);
                0
            }
            Err(error) => error.status(),
        }
    }

    #[inline(always)]
    pub unsafe fn managed_trampoline(
        raw: *mut ElmNativeManagedCallV1,
        handler: fn(&ManagedRequest) -> ManagedResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.abi_version != ELM_NATIVE_MANAGED_CALL_ABI_VERSION
            || raw.flags != 0
            || raw.reserved0 != 0
            || usize::from(raw.request.payload_len) > raw.request.payload.len()
        {
            return ELM_CALL_STATUS_INVALID;
        }
        let request = ManagedRequest {
            import_handle: raw.import_handle,
            caller_cell_id: raw.caller_cell_id,
            caller_generation: raw.caller_generation,
            callee_cell_id: raw.callee_cell_id,
            callee_generation: raw.callee_generation,
            frame: raw.request,
        };
        match handler(&request) {
            Ok(reply) => {
                raw.reply = reply.into_frame(raw.request.binding_id, raw.request.call_id);
                0
            }
            Err(error) => error.status(),
        }
    }

    pub unsafe fn snapshot_trampoline(
        raw: *mut ElmNativeProviderSnapshotV1,
        handler: fn(&SnapshotRequest, &mut [u8]) -> SnapshotResult,
    ) -> i32 {
        runtime_api::ensure_linked();
        let Some(raw) = (unsafe { raw.as_mut() }) else {
            return ELM_CALL_STATUS_INVALID;
        };
        if raw.abi_version != ELM_NATIVE_PROVIDER_SNAPSHOT_ABI_VERSION
            || raw.flags & !ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED != 0
            || raw.reserved0 != 0
            || raw.reserved1 != 0
            || raw.payload_addr == 0 && raw.capacity != 0
        {
            return ELM_CALL_STATUS_INVALID;
        }
        let paged = raw.flags & ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED != 0;
        let request = SnapshotRequest {
            cell_id: raw.cell_id,
            port_id: raw.port_id,
            binding_id: raw.binding_id,
            lease_id: raw.lease_id,
            paged,
            cursor: if paged { raw.reserved2 } else { 0 },
        };
        let output = if raw.capacity == 0 {
            &mut []
        } else {
            unsafe {
                core::slice::from_raw_parts_mut(raw.payload_addr as *mut u8, raw.capacity as usize)
            }
        };
        match handler(&request, output) {
            Ok(reply) if reply.payload_len <= output.len() => {
                raw.status = reply.status;
                raw.payload_len = reply.payload_len as u32;
                raw.record_count = reply.record_count;
                raw.flags = if paged {
                    ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_PAGED
                } else {
                    0
                };
                if let Some(next) = reply.next_cursor {
                    if !paged || next == 0 || next == request.cursor {
                        return ELM_CALL_STATUS_INVALID;
                    }
                    raw.flags |= ELM_NATIVE_PROVIDER_SNAPSHOT_FLAG_MORE;
                    raw.reserved2 = next;
                } else {
                    raw.reserved2 = 0;
                }
                if raw.flags & !ELM_NATIVE_PROVIDER_SNAPSHOT_FLAGS_MASK != 0 {
                    return ELM_CALL_STATUS_INVALID;
                }
                0
            }
            Ok(_) => ELM_CALL_STATUS_INVALID,
            Err(error) => error.status(),
        }
    }

    pub unsafe fn mixin_trampoline<T: ElmPayload>(
        raw: *mut ElmNativeProviderCallV1,
        handler: fn(&mut T) -> MixinControl,
    ) -> i32 {
        unsafe {
            provider_trampoline(raw, |request| {
                let mut frame = request
                    .decode::<T>()
                    .map_err(|_| HookError::new(ELM_CALL_STATUS_INVALID))?;
                let control = handler(&mut frame);
                let flags = match control {
                    MixinControl::Continue => MIXIN_REPLY_CONTINUE,
                    MixinControl::Stop => MIXIN_REPLY_STOP,
                    MixinControl::Replace => MIXIN_REPLY_REPLACE,
                    MixinControl::ReplaceAndStop => MIXIN_REPLY_REPLACE | MIXIN_REPLY_STOP,
                    MixinControl::Deny => MIXIN_REPLY_DENY,
                };
                let reply = if flags & MIXIN_REPLY_REPLACE != 0 {
                    ProviderReply::payload(ELM_CALL_STATUS_OK, &frame)
                        .map_err(|_| HookError::new(ELM_CALL_STATUS_INVALID))?
                } else {
                    ProviderReply::ok()
                };
                Ok(reply.with_flags(flags))
            })
        }
    }

    pub fn write_bytes(
        output: &mut [u8],
        offset: &mut usize,
        bytes: &[u8],
    ) -> Result<(), PayloadError> {
        let end = offset
            .checked_add(bytes.len())
            .ok_or(PayloadError::BufferTooSmall)?;
        let target = output
            .get_mut(*offset..end)
            .ok_or(PayloadError::BufferTooSmall)?;
        target.copy_from_slice(bytes);
        *offset = end;
        Ok(())
    }

    pub fn read_array<const N: usize>(
        input: &[u8],
        offset: &mut usize,
    ) -> Result<[u8; N], PayloadError> {
        let end = offset.checked_add(N).ok_or(PayloadError::SizeMismatch)?;
        let source = input.get(*offset..end).ok_or(PayloadError::SizeMismatch)?;
        let mut output = [0u8; N];
        output.copy_from_slice(source);
        *offset = end;
        Ok(output)
    }

    pub fn read_bool(input: &[u8], offset: &mut usize) -> Result<bool, PayloadError> {
        match read_array::<1>(input, offset)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(PayloadError::InvalidBoolean),
        }
    }

    fn migration_context_valid(raw: &ElmNativeMigrationContextV1, phase: u16) -> bool {
        raw.abi_version == ELM_NATIVE_MIGRATION_CONTEXT_ABI_VERSION
            && raw.phase == phase
            && raw.flags == 0
            && raw.status == 0
            && raw.reserved == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ElmLifecyclePhase, ElmNativeMigrationContextV1};
    use crate::ids::{ElmId, Generation};

    fn export_empty(_context: &MigrationContext, output: &mut [u8]) -> MigrationExportResult {
        assert!(output.is_empty());
        Ok(0)
    }

    fn snapshot_empty(_request: &SnapshotRequest, output: &mut [u8]) -> SnapshotResult {
        assert!(output.is_empty());
        Ok(SnapshotReply::complete(0, 0))
    }

    #[test]
    fn import_wrappers_have_one_word_layout() {
        assert_eq!(
            core::mem::size_of::<ManagedImport>(),
            core::mem::size_of::<usize>()
        );
        assert_eq!(
            core::mem::size_of::<DirectImport<fn()>>(),
            core::mem::size_of::<usize>()
        );
    }

    #[test]
    fn zero_length_native_buffers_do_not_require_non_null_pointer() {
        let mut migration = ElmNativeMigrationContextV1::new(
            ElmLifecyclePhase::MigrateExport,
            ElmId(7),
            Generation(1),
            Generation(2),
            0,
            0,
            0,
        );
        let migration_status =
            unsafe { __private::migration_export_trampoline(&mut migration, export_empty) };
        assert_eq!(migration_status, 0);
        assert_eq!(migration.buffer_len, 0);

        let mut snapshot = ElmNativeProviderSnapshotV1::new(7, 8, 9, 10, 0, 0);
        let snapshot_status =
            unsafe { __private::snapshot_trampoline(&mut snapshot, snapshot_empty) };
        assert_eq!(snapshot_status, 0);
        assert_eq!(snapshot.payload_len, 0);
    }
}
