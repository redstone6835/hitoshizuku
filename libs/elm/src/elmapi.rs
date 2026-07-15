//! ELM 原生 API 根协议。
//!
//! `elmapi` 是 ELM 原生代码与 ELM 运行时之间的稳定入口。模块只导入一个根槽位，
//! 再通过根表取得运行时表或查询 Manager 命名空间。allocator、设备等内核子系统不经
//! 该根表转发，而是在装载期由直接内核符号目录绑定。这里使用固定布局和显式函数指针；
//! Rust 开发包在其上提供安全包装。
//!
//! 普通模块不应直接解引用这些表，而应使用 [`crate::runtime`]、[`crate::ManagedImport`] 和
//! [`crate::management::Client`]。本模块主要服务于装载器、ABI 指纹生成、框架 trampoline
//! 和实现其他语言绑定的低层代码。
//!
//! 所有表遵循“前缀稳定、尾部扩展”规则：先验证 `struct_size` 和版本，只读取已声明存在的
//! 字段；保留字段必须为零。函数表及命名空间地址由内核静态发布，但权限与 generation 仍需
//! 在每次调用时检查，模块不能把取得表地址等同于永久授权。

#[cfg(feature = "runtime-model")]
use alloc::string::String;
#[cfg(feature = "runtime-model")]
use core::fmt::Write as _;

use crate::context::ElmLifecyclePhase;
use crate::state::ElmState;

/// 单次命名空间协商允许提交的兼容版本数量上限。
pub const ELM_API_MAX_COMPATIBLE_VERSIONS: usize = 16;
/// 尚未发布的 ELM API 当前版本，也是首个稳定布局编号。
pub const ELM_API_VERSION_V1: u16 = 1;
/// 本框架编译时选择的 ELM API 版本。
pub const ELM_API_CURRENT_VERSION: u16 = ELM_API_VERSION_V1;
/// `ELM_API_ROOT_MAGIC` 的固定魔数；解析器必须先校验该值，再解释后续布局。
pub const ELM_API_ROOT_MAGIC: u64 = u64::from_le_bytes(*b"ELMAPI1\0");
/// EBI 中根 API 特殊 import 的规范名称。
pub const ELM_API_ROOT_IMPORT_NAME: &str = "elm.api.root";
/// 根 API 特殊 import 的 v1 契约。
pub const ELM_API_ROOT_IMPORT_CONTRACT: &str = "elm.api.root@1";
/// 普通运行时命名空间 identifier。
pub const ELM_API_RUNTIME_IDENTIFIER: &str = "elm.runtime";
/// 受授权 Manager ELM 使用的管理命名空间 identifier。
pub const ELM_API_MANAGEMENT_IDENTIFIER: &str = "elm.management";
/// ELM 运行时命名空间 identifier 的最大 UTF-8 字节数。
pub const ELM_API_NAMESPACE_IDENTIFIER_MAX_LEN: usize = 64;
/// 命名空间可以被所有 ELM 查询。
pub const ELM_API_NAMESPACE_FLAG_PUBLIC: u32 = 1 << 0;
/// 命名空间只允许通过 elm-mgr 管理鉴权的 Manager ELM 查询。
pub const ELM_API_NAMESPACE_FLAG_MANAGEMENT: u32 = 1 << 1;
/// v1 允许的全部命名空间发布标志。
pub const ELM_API_NAMESPACE_FLAGS_V1: u32 =
    ELM_API_NAMESPACE_FLAG_PUBLIC | ELM_API_NAMESPACE_FLAG_MANAGEMENT;

/// 检查一个 identifier 是否属于规范的 ELM 运行时命名空间。
///
/// v1 名称必须以 `elm.` 开头，只包含小写 ASCII 字母、数字、点、连字符和下划线，且不能
/// 包含空段。该规则只适用于根表命名空间，不约束内核直接符号的名称或契约。
pub fn is_valid_runtime_api_identifier(identifier: &str) -> bool {
    !identifier.is_empty()
        && identifier.len() <= ELM_API_NAMESPACE_IDENTIFIER_MAX_LEN
        && identifier.starts_with("elm.")
        && !identifier.ends_with('.')
        && !identifier.contains("..")
        && identifier.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
}
/// 普通运行时表支持 mixin 阶段分发。
pub const ELM_API_FEATURE_MIXIN_DISPATCH: u64 = 1 << 0;
/// 普通运行时表支持查询当前 ELM 上下文。
pub const ELM_API_FEATURE_CONTEXT: u64 = 1 << 1;
/// 根表支持按 identifier 和版本列表协商附加命名空间。
pub const ELM_API_FEATURE_NAMESPACE_QUERY: u64 = 1 << 2;
/// 普通运行时表支持归属当前 cell 的结构化日志入口。
pub const ELM_API_FEATURE_LOG: u64 = 1 << 3;
/// 普通运行时表支持从 panic、取消或超时路径主动中止当前 native execution。
pub const ELM_API_FEATURE_ABORT: u64 = 1 << 4;
/// 普通运行时表支持通过不透明 import handle 发起受管调用。
pub const ELM_API_FEATURE_MANAGED_CALL: u64 = 1 << 5;
/// v1 普通运行时和根表定义的全部 feature 位。
pub const ELM_API_FEATURES_V1: u64 = ELM_API_FEATURE_MIXIN_DISPATCH
    | ELM_API_FEATURE_CONTEXT
    | ELM_API_FEATURE_NAMESPACE_QUERY
    | ELM_API_FEATURE_LOG
    | ELM_API_FEATURE_ABORT
    | ELM_API_FEATURE_MANAGED_CALL;

/// 调用被显式取消。
pub const ELM_API_ABORT_REASON_CANCEL: u32 = 1;
/// 调用超过运行时预算或截止时间。
pub const ELM_API_ABORT_REASON_TIMEOUT: u32 = 2;
/// 模块触发 Rust panic；panic handler 应使用该原因退出。
pub const ELM_API_ABORT_REASON_PANIC: u32 = 4;

/// ELM API 调用成功。
pub const ELM_API_STATUS_OK: i32 = 0;
/// 指针、长度、结构字段或调用状态无效。
pub const ELM_API_STATUS_INVALID: i32 = -1;
/// 请求的命名空间、import 或运行时对象不存在。
pub const ELM_API_STATUS_NOT_FOUND: i32 = -2;
/// 当前版本或运行时不支持请求能力。
pub const ELM_API_STATUS_UNSUPPORTED: i32 = -3;
/// 输出容量不足；实现应通过 `output_len` 返回所需最小尺寸。
pub const ELM_API_STATUS_BUFFER_TOO_SMALL: i32 = -4;
/// 当前 cell、generation、kind 或策略没有调用权限。
pub const ELM_API_STATUS_PERMISSION: i32 = -5;

/// mixin 调度入口的原生函数指针类型。
///
/// `input` 指向框架编码的 extension dispatch 请求，`output` 是调用方缓冲区。实现必须在任何
/// 读取/写入前验证空指针与长度；容量不足时返回 `BUFFER_TOO_SMALL` 并填写所需 `output_len`。
pub type ElmApiMixinDispatchV1 = extern "C" fn(
    input: *const u8,
    input_len: usize,
    output: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32;
/// elm-mgr 类型化管理命名空间的统一分发入口。
///
/// `kind` 使用 [`ElmMgrCallKind`](crate::ElmMgrCallKind) 稳定编号。实现必须在每次调用时重新
/// 鉴权，并按固定回复头协议填写输出。
pub type ElmManagementDispatchV1 = extern "C" fn(
    kind: u32,
    input: *const u8,
    input_len: usize,
    output: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32;
/// 把当前 ELM 身份快照写入调用方 `ElmApiContextV1` 的入口。
pub type ElmApiCurrentContextV1 = extern "C" fn(output: *mut ElmApiContextV1) -> i32;
/// 提交当前 ELM 日志的入口；message 只在调用期间有效，不要求 NUL 结尾。
pub type ElmApiLogV1 = extern "C" fn(level: u32, message: *const u8, message_len: usize) -> i32;
/// 通过运行时故障恢复出口中止当前原生调用的不可返回入口。
pub type ElmApiAbortCurrentV1 = extern "C" fn(reason: u32) -> !;
/// 使用不透明 import handle 执行受管调用的入口。
///
/// 实现必须验证请求和回复指针、handle 所有者、当前 generation、调用权限以及回复中的
/// binding/call id。`request` 和 `reply` 不能指向重叠内存。
pub type ElmApiInvokeManagedV1 = extern "C" fn(
    import_handle: u64,
    request: *const crate::frame::ElmCallFrame,
    reply: *mut crate::frame::ElmReplyFrame,
) -> i32;
/// 按 identifier 和调用方可接受版本列表协商附加 API 命名空间。
///
/// 版本列表按调用方偏好顺序传入，数量不得超过 `ELM_API_MAX_COMPATIBLE_VERSIONS`。内核只在
/// 当前 cell 获授权时返回表地址，并在 `ElmApiNamespaceV1` 中给出选中版本和 generation。
pub type ElmApiQueryNamespaceV1 = extern "C" fn(
    identifier: *const u8,
    identifier_len: usize,
    compatible_versions: *const u16,
    compatible_version_count: usize,
    output: *mut ElmApiNamespaceV1,
) -> i32;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 通过普通运行时 API 返回的当前 ELM 身份快照。
///
/// 所有字段都是稳定标量；调用方必须验证 `struct_size`、零 flags/保留字段，并把 state、phase
/// 和 kind 作为受校验的稳定判别值解析。该值不是长期授权 token。
pub struct ElmApiContextV1 {
    /// 生产者写入的完整结构字节数，用于向前兼容地判断可读取字段范围。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// ELM 单元的稳定运行时标识符。
    pub cell_id: u64,
    /// 父 cell id；零表示没有父单元。
    pub parent_id: u64,
    /// 对象当前代际；用于拒绝热替换前遗留的陈旧引用。
    pub generation: u64,
    /// 对象或单元的当前状态编码。
    pub state: u32,
    /// 当前生命周期或迁移阶段编码。
    pub phase: u32,
    /// [`ElmKind`](crate::ElmKind) 稳定编码。
    pub kind: u32,
    /// 当前上下文允许执行的管理动作位集合。
    pub allowed_actions: u32,
    /// 保留字段；生产者必须写零，消费者在当前版本必须拒绝非零值。
    pub reserved: u32,
}

impl ElmApiContextV1 {
    /// 构造供 `current_context` 入口填写的规范零值输出结构。
    pub const fn empty() -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: 0,
            cell_id: 0,
            parent_id: 0,
            generation: 0,
            state: 0,
            phase: 0,
            kind: 0,
            allowed_actions: 0,
            reserved: 0,
        }
    }

    /// 把强类型生命周期阶段映射为 v1 稳定编码。
    pub const fn phase_code(phase: ElmLifecyclePhase) -> u32 {
        match phase {
            ElmLifecyclePhase::Initialize => 1,
            ElmLifecyclePhase::Finalize => 2,
            ElmLifecyclePhase::Quiesce => 3,
            ElmLifecyclePhase::Pause => 4,
            ElmLifecyclePhase::Resume => 5,
            ElmLifecyclePhase::MigrateExport => 6,
            ElmLifecyclePhase::MigrateImport => 7,
            ElmLifecyclePhase::MigrateAbort => 8,
        }
    }

    /// 把强类型状态映射为当前 v1 稳定编码。
    pub const fn state_code(state: ElmState) -> u32 {
        state as u32
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 根表命名空间协商的固定回复。
///
/// `table_address` 只在 `selected_version`、`table_size`、generation 和 capabilities 全部通过
/// 对应命名空间包装校验后使用。表地址本身与内核同寿命，但访问权限可能随当前 cell 状态改变。
pub struct ElmApiNamespaceV1 {
    /// 生产者写入的完整结构字节数，用于向前兼容地判断可读取字段范围。
    pub struct_size: u32,
    /// v1 必须为零。
    pub flags: u32,
    /// 内核从调用方兼容列表中选择的版本。
    pub selected_version: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u16,
    /// 函数表可读取的字节数，用于兼容尾部扩展。
    pub table_size: u32,
    /// 协商所得只读函数表地址，其有效期由返回命名空间的代际约束。
    pub table_address: usize,
    /// 发布该命名空间时当前调用 cell 的 generation。
    pub generation: u64,
    /// 协商得到的能力位集合；调用可选入口前必须先检查对应位。
    pub capabilities: u64,
}

impl ElmApiNamespaceV1 {
    /// 构造供 `query_namespace` 填写的规范零值输出结构。
    pub const fn empty() -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: 0,
            selected_version: 0,
            reserved0: 0,
            table_size: 0,
            table_address: 0,
            generation: 0,
            capabilities: 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
/// 内核向 ELM 根表注册一个静态版本化函数表所需的完整描述。
///
/// 描述符本身只存在于内核地址空间，不进入 EKI。它只允许发布 `elm.*` 运行时命名空间；
/// 同名同版本描述符只能注册一次。
pub struct ElmApiNamespaceDescriptorV1 {
    /// 命名空间 identifier。
    pub identifier: &'static str,
    /// 精确 ABI 版本。
    pub version: u16,
    /// 发布与授权策略标志。
    pub flags: u32,
    /// 函数表实现支持的全部能力位。
    pub capabilities: u64,
    /// 只读静态函数表地址。
    pub table_address: *const (),
    /// 函数表完整字节数。
    pub table_size: u32,
}

impl ElmApiNamespaceDescriptorV1 {
    /// 构造一个静态命名空间描述符。
    pub const fn new<T>(
        identifier: &'static str,
        version: u16,
        flags: u32,
        capabilities: u64,
        table: &'static T,
    ) -> Self {
        Self {
            identifier,
            version,
            flags,
            capabilities,
            table_address: table as *const T as *const (),
            table_size: core::mem::size_of::<T>() as u32,
        }
    }

    /// 验证不依赖运行时注册表的描述符不变量。
    pub fn validate(&self) -> bool {
        is_valid_runtime_api_identifier(self.identifier)
            && self.version != 0
            && self.flags != 0
            && self.flags & !ELM_API_NAMESPACE_FLAGS_V1 == 0
            && self.flags.count_ones() == 1
            && !self.table_address.is_null()
            && self.table_size != 0
    }
}

// Safety: 描述符只包含静态字符串、只读静态表地址和不可变标量。
unsafe impl Sync for ElmApiNamespaceDescriptorV1 {}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
/// 所有普通 ELM 可访问的 v1 运行时函数表。
///
/// 表对象由内核静态发布。调用方必须先验证版本、最小 `struct_size` 和 feature 位，再调用
/// 对应入口。安全开发框架在 [`crate::runtime`] 和 [`crate::ManagedImport`] 中完成这些检查。
pub struct ElmRuntimeApiV1 {
    /// 生产者写入的完整结构字节数，用于向前兼容地判断可读取字段范围。
    pub struct_size: u32,
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u16,
    /// 该 API 根表声明的可选功能位集合。
    pub features: u64,
    /// 执行一个经过授权的 mixin 阶段分发。
    pub dispatch_mixin: ElmApiMixinDispatchV1,
    /// 查询当前调用上下文。
    pub current_context: ElmApiCurrentContextV1,
    /// 提交归属当前 cell 的日志。
    pub log: ElmApiLogV1,
    /// 从 panic、取消或超时路径中止当前 native execution。
    pub abort_current: ElmApiAbortCurrentV1,
    /// 通过受管 import handle 发起调用。
    pub invoke_managed: ElmApiInvokeManagedV1,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
/// 受授权 Manager ELM 获得的 v1 管理函数表。
///
/// 该表只有一个统一 dispatch 入口，具体请求/回复由 [`crate::management::Client`] 类型化。
/// 普通模块不应直接请求或调用此表。
pub struct ElmManagementApiV1 {
    /// 生产者写入的完整结构字节数，用于向前兼容地判断可读取字段范围。
    pub struct_size: u32,
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u16,
    /// 执行 elm-mgr 管理动作的统一鉴权分发入口。
    pub dispatch: ElmManagementDispatchV1,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
/// 装载器注入每个原生 ELM 的 v1 根 API 表。
///
/// 根表是模块唯一必须导入的运行时对象。它直接提供普通运行时表，并允许按 identifier 协商
/// 受授权扩展命名空间，从而避免向模块暴露内核全局符号表或内部 crate ABI。
pub struct ElmApiRootV1 {
    /// 识别该线格式的固定魔数。
    pub magic: u64,
    /// 生产者写入的完整结构字节数，用于向前兼容地判断可读取字段范围。
    pub struct_size: u32,
    /// 该结构遵循的 ABI 版本；解析其余字段前必须验证兼容性。
    pub abi_version: u16,
    /// 装载器与模块共同选择的根 API 版本。
    pub selected_version: u16,
    /// 该 API 根表声明的可选功能位集合。
    pub features: u64,
    /// 指向内核静态只读普通运行时表；不能为空。
    pub runtime_table: *const ElmRuntimeApiV1,
    /// `runtime_table` 可读取的字节数，用于前缀兼容。
    pub runtime_table_size: u32,
    /// 第一保留字段；生产者必须写零，消费者在当前版本必须验证为零。
    pub reserved0: u32,
    /// 按 identifier、版本和当前权限协商其他 API 表。
    pub query_namespace: ElmApiQueryNamespaceV1,
}

// 安全性：根表只含只读元数据、不可变函数指针和指向静态只读表的指针。
unsafe impl Sync for ElmApiRootV1 {}

/// 生成 ELM Rust ABI v1 的规范清单。
///
/// 清单直接读取真实 Rust 类型布局，不依赖人工维护的版本字符串。任何字段顺序、
/// 大小、对齐、函数签名或表能力变化都会改变清单摘要，从而在装载前拒绝不兼容镜像。
#[cfg(feature = "runtime-model")]
pub fn kernel_interface_manifest_v1(target_arch: u32) -> String {
    macro_rules! write_layout {
        ($out:expr, $ty:ty, $name:literal, [$($field:ident),+ $(,)?]) => {{
            writeln!(
                $out,
                "type={} size={} align={}",
                $name,
                core::mem::size_of::<$ty>(),
                core::mem::align_of::<$ty>()
            )
            .unwrap();
            $(
                writeln!(
                    $out,
                    "field={}.{} offset={}",
                    $name,
                    stringify!($field),
                    core::mem::offset_of!($ty, $field)
                )
                .unwrap();
            )+
        }};
    }

    let mut out = String::new();
    writeln!(out, "domain=elm.kernel-interface.manifest.v1").unwrap();
    writeln!(out, "target.arch={target_arch}").unwrap();
    writeln!(out, "target.pointer-width=64").unwrap();
    writeln!(out, "target.endian=little").unwrap();
    writeln!(out, "target.usize-size=8").unwrap();
    writeln!(out, "target.function-pointer-size=8").unwrap();
    writeln!(out, "target.calling-convention=C").unwrap();
    writeln!(out, "panic-strategy=abort-through-runtime").unwrap();
    writeln!(out, "root.magic={ELM_API_ROOT_MAGIC}").unwrap();
    writeln!(out, "root.import-name={ELM_API_ROOT_IMPORT_NAME}").unwrap();
    writeln!(out, "root.import-contract={ELM_API_ROOT_IMPORT_CONTRACT}").unwrap();
    writeln!(out, "runtime.identifier={ELM_API_RUNTIME_IDENTIFIER}").unwrap();
    writeln!(out, "management.identifier={ELM_API_MANAGEMENT_IDENTIFIER}").unwrap();
    writeln!(out, "runtime.version={ELM_API_VERSION_V1}").unwrap();
    writeln!(out, "runtime.features={ELM_API_FEATURES_V1}").unwrap();
    writeln!(
        out,
        "kernel-symbol.descriptor-abi={}",
        kernel_symbols::KERNEL_SYMBOL_DESCRIPTOR_ABI_V1
    )
    .unwrap();
    writeln!(
        out,
        "kernel-symbol.capabilities={}",
        kernel_symbols::capability::ALL
    )
    .unwrap();
    writeln!(
        out,
        "kernel-symbol.interface-source-files={}",
        kernel_symbols::KERNEL_INTERFACE_SOURCE_FILE_COUNT
    )
    .unwrap();
    write!(out, "kernel-symbol.interface-source-sha256=").unwrap();
    for byte in kernel_symbols::KERNEL_INTERFACE_SOURCE_SHA256 {
        write!(out, "{byte:02x}").unwrap();
    }
    writeln!(out).unwrap();

    write_layout!(
        out,
        kernel_symbols::KernelSymbolDescriptorV1,
        "KernelSymbolDescriptorV1",
        [
            magic,
            struct_size,
            abi_version,
            kind,
            execution_domain,
            reserved0,
            flags,
            version,
            capabilities,
            retained_argument_mask,
            interface_hash,
            api_path,
            item_path,
            link_name,
            contract,
            rust_abi,
            address,
        ]
    );
    writeln!(
        out,
        "type=DirectImport<fn()> size={} align={}",
        core::mem::size_of::<crate::DirectImport<fn()>>(),
        core::mem::align_of::<crate::DirectImport<fn()>>()
    )
    .unwrap();

    write_layout!(
        out,
        ElmApiContextV1,
        "ElmApiContextV1",
        [
            struct_size,
            flags,
            cell_id,
            parent_id,
            generation,
            state,
            phase,
            kind,
            allowed_actions,
            reserved,
        ]
    );
    write_layout!(
        out,
        ElmApiNamespaceV1,
        "ElmApiNamespaceV1",
        [
            struct_size,
            flags,
            selected_version,
            reserved0,
            table_size,
            table_address,
            generation,
            capabilities,
        ]
    );
    write_layout!(
        out,
        ElmRuntimeApiV1,
        "ElmRuntimeApiV1",
        [
            struct_size,
            abi_version,
            reserved0,
            features,
            dispatch_mixin,
            current_context,
            log,
            abort_current,
            invoke_managed,
        ]
    );
    write_layout!(
        out,
        ElmManagementApiV1,
        "ElmManagementApiV1",
        [struct_size, abi_version, reserved0, dispatch,]
    );
    write_layout!(
        out,
        ElmApiRootV1,
        "ElmApiRootV1",
        [
            magic,
            struct_size,
            abi_version,
            selected_version,
            features,
            runtime_table,
            runtime_table_size,
            reserved0,
            query_namespace,
        ]
    );
    write_layout!(
        out,
        crate::frame::ElmCallFrame,
        "ElmCallFrame",
        [
            binding_id,
            call_id,
            opcode,
            flags,
            payload_len,
            reserved0,
            reserved1,
            payload,
        ]
    );
    write_layout!(
        out,
        crate::frame::ElmReplyFrame,
        "ElmReplyFrame",
        [
            binding_id,
            call_id,
            status,
            flags,
            payload_len,
            reserved0,
            reserved1,
            payload,
        ]
    );
    write_layout!(
        out,
        crate::frame::ElmNativeEntryFrameV1,
        "ElmNativeEntryFrameV1",
        [
            abi_version,
            flags,
            reserved0,
            cell_id,
            parent_id,
            generation,
            state,
            exit_code,
            reserved1,
        ]
    );
    write_layout!(
        out,
        crate::frame::ElmNativeManagedCallV1,
        "ElmNativeManagedCallV1",
        [
            abi_version,
            flags,
            reserved0,
            import_handle,
            caller_cell_id,
            caller_generation,
            callee_cell_id,
            callee_generation,
            request,
            reply,
        ]
    );
    write_layout!(
        out,
        crate::frame::ElmNativeProviderCallV1,
        "ElmNativeProviderCallV1",
        [
            abi_version,
            flags,
            reserved0,
            cell_id,
            port_id,
            lease_id,
            binding_id,
            request,
            reply,
        ]
    );
    write_layout!(
        out,
        crate::frame::ElmNativeProviderSnapshotV1,
        "ElmNativeProviderSnapshotV1",
        [
            abi_version,
            flags,
            reserved0,
            cell_id,
            port_id,
            binding_id,
            lease_id,
            status,
            reserved1,
            capacity,
            payload_len,
            record_count,
            reserved2,
            payload_addr,
        ]
    );
    write_layout!(
        out,
        crate::context::ElmNativeHookContextV1,
        "ElmNativeHookContextV1",
        [
            abi_version,
            phase,
            flags,
            cell_id,
            parent_id,
            generation,
            state,
            reserved,
        ]
    );
    write_layout!(
        out,
        crate::context::ElmNativeMigrationContextV1,
        "ElmNativeMigrationContextV1",
        [
            abi_version,
            phase,
            flags,
            cell_id,
            old_generation,
            new_generation,
            buffer_ptr,
            buffer_capacity,
            buffer_len,
            status,
            reserved,
        ]
    );

    writeln!(
        out,
        "fn.dispatch-mixin=extern-C(*const-u8,usize,*mut-u8,usize,*mut-usize)->i32"
    )
    .unwrap();
    writeln!(
        out,
        "fn.management-dispatch=extern-C(u32,*const-u8,usize,*mut-u8,usize,*mut-usize)->i32"
    )
    .unwrap();
    writeln!(
        out,
        "fn.current-context=extern-C(*mut-ElmApiContextV1)->i32"
    )
    .unwrap();
    writeln!(out, "fn.log=extern-C(u32,*const-u8,usize)->i32").unwrap();
    writeln!(out, "fn.abort-current=extern-C(u32)->never").unwrap();
    writeln!(
        out,
        "fn.invoke-managed=extern-C(u64,*const-ElmCallFrame,*mut-ElmReplyFrame)->i32"
    )
    .unwrap();
    writeln!(
        out,
        "fn.query-namespace=extern-C(*const-u8,usize,*const-u16,usize,*mut-ElmApiNamespaceV1)->i32"
    )
    .unwrap();
    writeln!(
        out,
        "hook.entry=extern-C(*const-ElmNativeEntryFrameV1)->i32"
    )
    .unwrap();
    writeln!(
        out,
        "hook.lifecycle=extern-C(*const-ElmNativeHookContextV1)->i32"
    )
    .unwrap();
    writeln!(
        out,
        "hook.migration=extern-C(*mut-ElmNativeMigrationContextV1)->i32"
    )
    .unwrap();
    writeln!(
        out,
        "hook.provider=extern-C(*mut-ElmNativeProviderCallV1)->i32"
    )
    .unwrap();
    writeln!(
        out,
        "hook.provider-snapshot=extern-C(*mut-ElmNativeProviderSnapshotV1)->i32"
    )
    .unwrap();
    out
}
