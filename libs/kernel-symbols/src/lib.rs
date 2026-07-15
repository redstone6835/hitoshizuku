#![no_std]
#![warn(missing_docs)]

//! 内核直接符号目录的中立契约。
//!
//! 本 crate 只定义链接期描述符、能力组和导出 attribute，不依赖 ELM 运行时，也不承载
//! 任何子系统实现。常驻内核 crate 把经过审核的函数或静态对象放入
//! `.elm.kernel_symbols` 链接区；装载器在执行模块代码前按名称、契约、版本和 Rust ABI
//! 摘要解析地址。地址写入完成后，调用路径就是普通 Rust 间接调用，不经过 elm-mgr、
//! provider 或命名空间函数表。

use core::fmt;
use core::sync::atomic::{AtomicPtr, Ordering};

include!(concat!(env!("OUT_DIR"), "/interface_source.rs"));

#[cfg(feature = "macros")]
pub use kernel_symbols_macros::export;

/// 内核符号描述符的固定魔数。
pub const KERNEL_SYMBOL_DESCRIPTOR_MAGIC: u64 = u64::from_le_bytes(*b"KRSYM001");
/// 当前内核符号描述符 ABI 版本。
pub const KERNEL_SYMBOL_DESCRIPTOR_ABI_V1: u16 = 1;
/// 符号名称允许的最大字节数。
pub const KERNEL_SYMBOL_NAME_MAX_LEN: usize = 192;
/// 符号契约 identifier 允许的最大字节数。
pub const KERNEL_SYMBOL_CONTRACT_MAX_LEN: usize = 192;
/// 规范 Rust ABI 字符串允许的最大字节数。
pub const KERNEL_SYMBOL_RUST_ABI_MAX_LEN: usize = 1024;
/// 工具链链接符号允许的最大字节数。
pub const KERNEL_SYMBOL_LINK_NAME_MAX_LEN: usize = 96;

/// 描述符表示可调用的 Rust 函数。
pub const KERNEL_SYMBOL_KIND_FUNCTION: u8 = 1;
/// 描述符表示具有静态存储期的对象。
pub const KERNEL_SYMBOL_KIND_STATIC: u8 = 2;
/// 描述符表示固有实现中的方法；调用 ABI 仍是普通 Rust 函数 ABI。
pub const KERNEL_SYMBOL_KIND_METHOD: u8 = 3;

/// 符号在装载后通过直接 Rust 调用执行。
pub const KERNEL_SYMBOL_DOMAIN_DIRECT_RUST: u8 = 1;

/// 设备直接符号创建的资源统一归入 ELM `Device` 类别。
pub const KERNEL_SYMBOL_RESOURCE_KIND_DEVICE: u32 = 7;
/// 资源已登记到当前 ELM 单元。
pub const KERNEL_SYMBOL_RESOURCE_STATUS_TRACKED: i32 = 0;
/// 当前不在 ELM 执行上下文中，资源保持普通内建内核生命周期。
pub const KERNEL_SYMBOL_RESOURCE_STATUS_UNMANAGED: i32 = 1;
/// 运行时拒绝登记或解除资源。
pub const KERNEL_SYMBOL_RESOURCE_STATUS_FAILED: i32 = -1;

/// 该入口会修改内核或设备状态。
pub const KERNEL_SYMBOL_FLAG_MUTATES_STATE: u32 = 1 << 0;
/// 该入口的 Rust 签名本身是 `unsafe fn`。
pub const KERNEL_SYMBOL_FLAG_UNSAFE: u32 = 1 << 1;
/// 该入口可能返回需要调用方负责释放或撤销的长期对象。
pub const KERNEL_SYMBOL_FLAG_RETURNS_OWNED: u32 = 1 << 2;
/// 该入口只用于诊断，不应被实现用作权限真值来源。
pub const KERNEL_SYMBOL_FLAG_DIAGNOSTIC: u32 = 1 << 3;
/// 入口会把一个或多个来自 ELM 镜像的对象或函数指针保留到调用返回之后。
pub const KERNEL_SYMBOL_FLAG_RETAINS_MODULE_CODE: u32 = 1 << 4;
/// 入口返回的借用可能指向 ELM 镜像拥有的对象，调用方必须维持镜像固定。
pub const KERNEL_SYMBOL_FLAG_RETURNS_MODULE_BORROW: u32 = 1 << 5;
/// 当前版本认可的全部符号标志位。
pub const KERNEL_SYMBOL_FLAGS_MASK: u32 = KERNEL_SYMBOL_FLAG_MUTATES_STATE
    | KERNEL_SYMBOL_FLAG_UNSAFE
    | KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    | KERNEL_SYMBOL_FLAG_DIAGNOSTIC
    | KERNEL_SYMBOL_FLAG_RETAINS_MODULE_CODE
    | KERNEL_SYMBOL_FLAG_RETURNS_MODULE_BORROW;

/// 最多允许一个直接符号声明 64 个参数的长期保留关系。
pub const KERNEL_SYMBOL_MAX_TRACKED_ARGUMENTS: usize = 64;

/// 常驻子系统用于暂停、恢复和退役直接符号资源的操作。
pub type KernelSymbolOwnedResourceOp =
    fn(owner: u64, generation: u64, handle: u64) -> Result<(), i32>;

/// 直接符号资源的完整生命周期操作表。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KernelSymbolOwnedResourceOpsV1 {
    /// 暂停资源并阻止回调进入模块镜像。
    pub suspend: KernelSymbolOwnedResourceOp,
    /// 恢复已经暂停的资源。
    pub resume: KernelSymbolOwnedResourceOp,
    /// 停止接纳新工作。
    pub quiesce: KernelSymbolOwnedResourceOp,
    /// 取消尚未开始的工作。
    pub cancel: KernelSymbolOwnedResourceOp,
    /// 等待运行中工作退出。
    pub drain: KernelSymbolOwnedResourceOp,
    /// 注销资源并释放内核持有的最后一个模块对象。
    pub release: KernelSymbolOwnedResourceOp,
}

impl KernelSymbolOwnedResourceOpsV1 {
    /// 构造一个六阶段操作均完整提供的资源操作表。
    pub const fn new(
        suspend: KernelSymbolOwnedResourceOp,
        resume: KernelSymbolOwnedResourceOp,
        quiesce: KernelSymbolOwnedResourceOp,
        cancel: KernelSymbolOwnedResourceOp,
        drain: KernelSymbolOwnedResourceOp,
        release: KernelSymbolOwnedResourceOp,
    ) -> Self {
        Self {
            suspend,
            resume,
            quiesce,
            cancel,
            drain,
            release,
        }
    }
}

/// ELM 内核运行时向常驻子系统提供的资源归属钩子。
#[repr(C)]
pub struct KernelSymbolRuntimeHooksV1 {
    /// 当前必须为 1。
    pub abi_version: u16,
    /// 必须等于本结构大小。
    pub struct_size: u16,
    /// 保留字段，必须为零。
    pub reserved: u32,
    /// 把资源登记到当前 ELM 单元。
    pub register_owned_resource: fn(u32, u64, KernelSymbolOwnedResourceOpsV1) -> i32,
    /// 在模块主动注销资源后解除归属记录。
    pub release_owned_resource: fn(u32, u64) -> i32,
}

impl KernelSymbolRuntimeHooksV1 {
    /// 校验钩子表前缀和版本。
    pub const fn valid(&self) -> bool {
        self.abi_version == 1
            && self.struct_size as usize == core::mem::size_of::<Self>()
            && self.reserved == 0
    }
}

static RUNTIME_HOOKS: AtomicPtr<KernelSymbolRuntimeHooksV1> = AtomicPtr::new(core::ptr::null_mut());

/// 安装一次 ELM 资源归属钩子。
pub fn install_runtime_hooks(hooks: &'static KernelSymbolRuntimeHooksV1) -> bool {
    if !hooks.valid() {
        return false;
    }
    let pointer = core::ptr::from_ref(hooks).cast_mut();
    RUNTIME_HOOKS
        .compare_exchange(
            core::ptr::null_mut(),
            pointer,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
        || RUNTIME_HOOKS.load(Ordering::Acquire) == pointer
}

/// 返回 ELM 运行时是否已经接管直接符号资源归属。
pub fn runtime_hooks_installed() -> bool {
    !RUNTIME_HOOKS.load(Ordering::Acquire).is_null()
}

/// 把常驻子系统资源登记到当前 ELM；内建调用返回 `UNMANAGED`。
pub fn track_owned_resource(kind: u32, handle: u64, ops: KernelSymbolOwnedResourceOpsV1) -> i32 {
    let hooks = RUNTIME_HOOKS.load(Ordering::Acquire);
    if hooks.is_null() {
        return KERNEL_SYMBOL_RESOURCE_STATUS_UNMANAGED;
    }
    // Safety: 指针只由 install_runtime_hooks 写入静态、通过结构校验的只读对象。
    let hooks = unsafe { &*hooks };
    (hooks.register_owned_resource)(kind, handle, ops)
}

/// 解除当前 ELM 对已经由模块主动注销的资源的归属记录。
pub fn untrack_owned_resource(kind: u32, handle: u64) -> i32 {
    let hooks = RUNTIME_HOOKS.load(Ordering::Acquire);
    if hooks.is_null() {
        return KERNEL_SYMBOL_RESOURCE_STATUS_UNMANAGED;
    }
    // Safety: 指针只由 install_runtime_hooks 写入静态、通过结构校验的只读对象。
    let hooks = unsafe { &*hooks };
    (hooks.release_owned_resource)(kind, handle)
}

/// 内核直接符号的权限能力组。
pub mod capability {
    /// 不携带子系统权限的纯查询或纯计算入口。
    pub const CORE_SAFE: u64 = 1 << 0;

    /// 普通内核堆分配、释放和调整大小。
    pub const ALLOCATOR_MEMORY: u64 = 1 << 1;
    /// allocator 统计、能力查询和只读诊断。
    pub const ALLOCATOR_DIAGNOSTIC: u64 = 1 << 2;
    /// 显式物理页、地址空间和 DMA backing 分配。
    pub const ALLOCATOR_PHYSICAL: u64 = 1 << 3;
    /// managed heap、GC 句柄、根和回收控制。
    pub const ALLOCATOR_MANAGED: u64 = 1 << 4;
    /// allocator 初始化、后端安装和全局策略修改。
    pub const ALLOCATOR_ADMIN: u64 = 1 << 5;

    /// 设备、总线和函数对象的只读发现与快照。
    pub const DEVICE_DISCOVERY: u64 = 1 << 16;
    /// 设备驱动、工厂、函数和热插拔生命周期注册。
    pub const DEVICE_DRIVER: u64 = 1 << 17;
    /// 设备长期资源的取得、登记、释放和撤销。
    pub const DEVICE_RESOURCE: u64 = 1 << 18;
    /// DMA 映射、同步和 DMA backing 管理。
    pub const DEVICE_DMA: u64 = 1 << 19;
    /// IRQ domain、handler、MSI controller 和向量管理。
    pub const DEVICE_INTERRUPT: u64 = 1 << 20;
    /// PCI、platform、USB、virtio 和 firmware bus 操作。
    pub const DEVICE_BUS: u64 = 1 << 21;
    /// 安装全局总线后端、配置访问器或平台级设备策略。
    pub const DEVICE_ADMIN: u64 = 1 << 22;

    /// 默认不需要额外管理员批准的能力组。
    pub const SAFE_DEFAULT: u64 = CORE_SAFE | ALLOCATOR_MEMORY | ALLOCATOR_DIAGNOSTIC;
    /// allocator 当前定义的全部能力组。
    pub const ALLOCATOR_ALL: u64 = ALLOCATOR_MEMORY
        | ALLOCATOR_DIAGNOSTIC
        | ALLOCATOR_PHYSICAL
        | ALLOCATOR_MANAGED
        | ALLOCATOR_ADMIN;
    /// 设备抽象当前定义的全部能力组。
    pub const DEVICE_ALL: u64 = DEVICE_DISCOVERY
        | DEVICE_DRIVER
        | DEVICE_RESOURCE
        | DEVICE_DMA
        | DEVICE_INTERRUPT
        | DEVICE_BUS
        | DEVICE_ADMIN;
    /// 当前协议认识的全部能力组。
    pub const ALL: u64 = CORE_SAFE | ALLOCATOR_ALL | DEVICE_ALL;
}

/// 链接到内核镜像中的直接符号描述符。
///
/// 字符串和地址都必须具有静态存储期。该结构只在同一次内核构建产生的镜像内部遍历，
/// 不属于 EBI、EKI 或用户态 ABI。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KernelSymbolDescriptorV1 {
    /// 固定魔数，必须为 [`KERNEL_SYMBOL_DESCRIPTOR_MAGIC`]。
    pub magic: u64,
    /// 结构实际字节数。
    pub struct_size: u16,
    /// 描述符 ABI 版本。
    pub abi_version: u16,
    /// `KERNEL_SYMBOL_KIND_*` 中的一种。
    pub kind: u8,
    /// 当前必须为 [`KERNEL_SYMBOL_DOMAIN_DIRECT_RUST`]。
    pub execution_domain: u8,
    /// 保留字段，必须为零。
    pub reserved0: u16,
    /// `KERNEL_SYMBOL_FLAG_*` 位集合。
    pub flags: u32,
    /// 符号契约版本，零无效。
    pub version: u32,
    /// 使用该入口必须取得的能力组位集合。
    pub capabilities: u64,
    /// 位 `n` 表示第 `n` 个显式参数会被内核保留到调用返回之后。
    pub retained_argument_mask: u64,
    /// 当前内核构建采用的规范接口摘要。
    pub interface_hash: [u8; 32],
    /// 稳定、与 Rust 模块层级一致的 API 路径。
    pub api_path: &'static str,
    /// 描述符所指向实现项在内核 crate 中的真实 Rust 路径。
    pub item_path: &'static str,
    /// ELM ELF 与内核镜像共同使用的稳定链接符号。
    pub link_name: &'static str,
    /// 稳定语义契约 identifier。
    pub contract: &'static str,
    /// 由导出宏和导入宏共同生成的规范 Rust 函数签名。
    pub rust_abi: &'static str,
    /// 常驻函数或静态对象的真实地址。
    pub address: *const (),
}

// Safety: 描述符只保存静态只读元数据和常驻地址；并发访问不会修改目标对象。
unsafe impl Sync for KernelSymbolDescriptorV1 {}

impl KernelSymbolDescriptorV1 {
    /// 构造一个函数符号描述符。
    pub const fn function(
        api_path: &'static str,
        contract: &'static str,
        version: u32,
        capabilities: u64,
        flags: u32,
        retained_argument_mask: u64,
        item_path: &'static str,
        link_name: &'static str,
        rust_abi: &'static str,
        address: *const (),
    ) -> Self {
        Self {
            magic: KERNEL_SYMBOL_DESCRIPTOR_MAGIC,
            struct_size: core::mem::size_of::<Self>() as u16,
            abi_version: KERNEL_SYMBOL_DESCRIPTOR_ABI_V1,
            kind: KERNEL_SYMBOL_KIND_FUNCTION,
            execution_domain: KERNEL_SYMBOL_DOMAIN_DIRECT_RUST,
            reserved0: 0,
            flags,
            version,
            capabilities,
            retained_argument_mask,
            interface_hash: KERNEL_INTERFACE_SOURCE_SHA256,
            api_path,
            item_path,
            link_name,
            contract,
            rust_abi,
            address,
        }
    }

    /// 构造一个静态对象符号描述符。
    pub const fn static_object(
        api_path: &'static str,
        contract: &'static str,
        version: u32,
        capabilities: u64,
        flags: u32,
        item_path: &'static str,
        link_name: &'static str,
        rust_abi: &'static str,
        address: *const (),
    ) -> Self {
        let mut descriptor = Self::function(
            api_path,
            contract,
            version,
            capabilities,
            flags,
            0,
            item_path,
            link_name,
            rust_abi,
            address,
        );
        descriptor.kind = KERNEL_SYMBOL_KIND_STATIC;
        descriptor
    }

    /// 构造一个固有方法描述符。
    pub const fn method(
        api_path: &'static str,
        contract: &'static str,
        version: u32,
        capabilities: u64,
        flags: u32,
        retained_argument_mask: u64,
        item_path: &'static str,
        link_name: &'static str,
        rust_abi: &'static str,
        address: *const (),
    ) -> Self {
        let mut descriptor = Self::function(
            api_path,
            contract,
            version,
            capabilities,
            flags,
            retained_argument_mask,
            item_path,
            link_name,
            rust_abi,
            address,
        );
        descriptor.kind = KERNEL_SYMBOL_KIND_METHOD;
        descriptor
    }

    /// 校验描述符的结构不变量和文本字段。
    pub fn validate(&self) -> bool {
        self.magic == KERNEL_SYMBOL_DESCRIPTOR_MAGIC
            && self.struct_size as usize == core::mem::size_of::<Self>()
            && self.abi_version == KERNEL_SYMBOL_DESCRIPTOR_ABI_V1
            && matches!(
                self.kind,
                KERNEL_SYMBOL_KIND_FUNCTION | KERNEL_SYMBOL_KIND_STATIC | KERNEL_SYMBOL_KIND_METHOD
            )
            && self.execution_domain == KERNEL_SYMBOL_DOMAIN_DIRECT_RUST
            && self.reserved0 == 0
            && self.flags & !KERNEL_SYMBOL_FLAGS_MASK == 0
            && (self.retained_argument_mask == 0
                || self.flags & KERNEL_SYMBOL_FLAG_RETAINS_MODULE_CODE != 0)
            && (self.flags & KERNEL_SYMBOL_FLAG_RETAINS_MODULE_CODE == 0
                || self.retained_argument_mask != 0)
            && self.version != 0
            && self.capabilities != 0
            && self.capabilities & !capability::ALL == 0
            && self.interface_hash != [0; 32]
            && valid_identifier(self.api_path, KERNEL_SYMBOL_NAME_MAX_LEN)
            && valid_rust_path(self.item_path)
            && valid_link_name(self.link_name)
            && valid_identifier(self.contract, KERNEL_SYMBOL_CONTRACT_MAX_LEN)
            && !self.rust_abi.is_empty()
            && self.rust_abi.len() <= KERNEL_SYMBOL_RUST_ABI_MAX_LEN
            && !self.address.is_null()
    }
}

impl fmt::Debug for KernelSymbolDescriptorV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KernelSymbolDescriptorV1")
            .field("kind", &self.kind)
            .field("flags", &self.flags)
            .field("version", &self.version)
            .field("capabilities", &self.capabilities)
            .field("retained_argument_mask", &self.retained_argument_mask)
            .field("interface_hash", &self.interface_hash)
            .field("api_path", &self.api_path)
            .field("item_path", &self.item_path)
            .field("link_name", &self.link_name)
            .field("contract", &self.contract)
            .field("rust_abi", &self.rust_abi)
            .field("address", &self.address)
            .finish()
    }
}

fn valid_identifier(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && !value.starts_with('.')
        && !value.ends_with('.')
        && !value.contains("..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'@'))
}

fn valid_link_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= KERNEL_SYMBOL_LINK_NAME_MAX_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_rust_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= KERNEL_SYMBOL_NAME_MAX_LEN
        && !value.starts_with("::")
        && !value.ends_with("::")
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'_' | b':' | b'<' | b'>' | b' ' | b',' | b'[' | b']' | b'&' | b'\''
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example(value: usize) -> usize {
        value
    }

    #[test]
    fn descriptor_validation_rejects_unknown_capabilities() {
        let valid = KernelSymbolDescriptorV1::function(
            "allocator.example",
            "kernel.allocator.example@1",
            1,
            capability::ALLOCATOR_MEMORY,
            0,
            0,
            "kernel_symbols::tests::example",
            "__elm_kernel_api_example",
            "fn(usize)->usize",
            example as *const (),
        );
        assert!(valid.validate());

        let mut invalid = valid;
        invalid.capabilities = 1 << 63;
        assert!(!invalid.validate());
    }
}
