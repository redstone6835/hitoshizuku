//! ELM 原生 API 根协议。
//!
//! `elmapi` 是 ELM 原生代码与 `elm-mgr` 之间的稳定入口。模块只导入一个根槽位，
//! 再通过根表取得运行时表或按 identifier 查询其他命名空间，避免把内核实现符号直接
//! 暴露给模块。这里使用固定布局和显式函数指针；Rust 开发包在其上提供安全包装。

use alloc::string::String;
use core::fmt::Write as _;

use crate::context::ElmLifecyclePhase;
use crate::state::ElmState;

pub const ELM_API_MAX_COMPATIBLE_VERSIONS: usize = 16;
pub const ELM_API_VERSION_V1: u16 = 1;
pub const ELM_API_CURRENT_VERSION: u16 = ELM_API_VERSION_V1;
pub const ELM_API_ROOT_MAGIC: u64 = u64::from_le_bytes(*b"ELMAPI1\0");
pub const ELM_API_ROOT_IMPORT_NAME: &str = "elm.api.root";
pub const ELM_API_ROOT_IMPORT_CONTRACT: &str = "elm.api.root@1";
pub const ELM_API_RUNTIME_IDENTIFIER: &str = "elmmgr.runtime";
pub const ELM_API_FEATURE_DISPATCH: u64 = 1 << 0;
pub const ELM_API_FEATURE_CONTEXT: u64 = 1 << 1;
pub const ELM_API_FEATURE_NAMESPACE_QUERY: u64 = 1 << 2;
pub const ELM_API_FEATURE_LOG: u64 = 1 << 3;
pub const ELM_API_FEATURE_ABORT: u64 = 1 << 4;
pub const ELM_API_FEATURE_MANAGED_CALL: u64 = 1 << 5;
pub const ELM_API_FEATURES_V1: u64 = ELM_API_FEATURE_DISPATCH
    | ELM_API_FEATURE_CONTEXT
    | ELM_API_FEATURE_NAMESPACE_QUERY
    | ELM_API_FEATURE_LOG
    | ELM_API_FEATURE_ABORT
    | ELM_API_FEATURE_MANAGED_CALL;

pub const ELM_API_ABORT_REASON_CANCEL: u32 = 1;
pub const ELM_API_ABORT_REASON_TIMEOUT: u32 = 2;
pub const ELM_API_ABORT_REASON_PANIC: u32 = 4;

pub const ELM_API_STATUS_OK: i32 = 0;
pub const ELM_API_STATUS_INVALID: i32 = -1;
pub const ELM_API_STATUS_NOT_FOUND: i32 = -2;
pub const ELM_API_STATUS_UNSUPPORTED: i32 = -3;
pub const ELM_API_STATUS_BUFFER_TOO_SMALL: i32 = -4;
pub const ELM_API_STATUS_PERMISSION: i32 = -5;

pub type ElmApiDispatchV1 = extern "C" fn(
    kind: u32,
    input: *const u8,
    input_len: usize,
    output: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32;
pub type ElmApiCurrentContextV1 = extern "C" fn(output: *mut ElmApiContextV1) -> i32;
pub type ElmApiLogV1 = extern "C" fn(level: u32, message: *const u8, message_len: usize) -> i32;
pub type ElmApiAbortCurrentV1 = extern "C" fn(reason: u32) -> !;
pub type ElmApiInvokeManagedV1 = extern "C" fn(
    import_handle: u64,
    request: *const crate::frame::ElmCallFrame,
    reply: *mut crate::frame::ElmReplyFrame,
) -> i32;
pub type ElmApiQueryNamespaceV1 = extern "C" fn(
    identifier: *const u8,
    identifier_len: usize,
    compatible_versions: *const u16,
    compatible_version_count: usize,
    output: *mut ElmApiNamespaceV1,
) -> i32;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmApiContextV1 {
    pub struct_size: u32,
    pub flags: u32,
    pub cell_id: u64,
    pub parent_id: u64,
    pub generation: u64,
    pub state: u32,
    pub phase: u32,
    pub allowed_actions: u32,
    pub reserved: u32,
}

impl ElmApiContextV1 {
    pub const fn empty() -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: 0,
            cell_id: 0,
            parent_id: 0,
            generation: 0,
            state: 0,
            phase: 0,
            allowed_actions: 0,
            reserved: 0,
        }
    }

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

    pub const fn state_code(state: ElmState) -> u32 {
        state as u32
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmApiNamespaceV1 {
    pub struct_size: u32,
    pub flags: u32,
    pub selected_version: u16,
    pub reserved0: u16,
    pub table_size: u32,
    pub table_address: usize,
    pub generation: u64,
    pub capabilities: u64,
}

impl ElmApiNamespaceV1 {
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

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ElmRuntimeApiV1 {
    pub struct_size: u32,
    pub abi_version: u16,
    pub reserved0: u16,
    pub features: u64,
    pub dispatch: ElmApiDispatchV1,
    pub current_context: ElmApiCurrentContextV1,
    pub log: ElmApiLogV1,
    pub abort_current: ElmApiAbortCurrentV1,
    pub invoke_managed: ElmApiInvokeManagedV1,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ElmApiRootV1 {
    pub magic: u64,
    pub struct_size: u32,
    pub abi_version: u16,
    pub selected_version: u16,
    pub features: u64,
    pub runtime_table: *const ElmRuntimeApiV1,
    pub runtime_table_size: u32,
    pub reserved0: u32,
    pub query_namespace: ElmApiQueryNamespaceV1,
}

// 安全性：根表只含只读元数据、不可变函数指针和指向静态只读表的指针。
unsafe impl Sync for ElmApiRootV1 {}

/// 生成 ELM Rust ABI v1 的规范清单。
///
/// 清单直接读取真实 Rust 类型布局，不依赖人工维护的版本字符串。任何字段顺序、
/// 大小、对齐、函数签名或表能力变化都会改变清单摘要，从而在装载前拒绝不兼容镜像。
pub fn kernel_api_manifest_v1(target_arch: u32) -> String {
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
    writeln!(out, "domain=elm.kernel-api.manifest.v1").unwrap();
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
    writeln!(out, "runtime.version={ELM_API_VERSION_V1}").unwrap();
    writeln!(out, "runtime.features={ELM_API_FEATURES_V1}").unwrap();

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
            dispatch,
            current_context,
            log,
            abort_current,
            invoke_managed,
        ]
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
        "fn.dispatch=extern-C(u32,*const-u8,usize,*mut-u8,usize,*mut-usize)->i32"
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
