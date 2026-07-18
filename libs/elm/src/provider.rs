//! 内核 provider 规格模型。
//!
//! 本模块只定义 ELM Core 可以理解的通用 provider 描述。具体 provider 的
//! 语义属于导出它的子系统，Core 只负责登记、绑定、调用、审计和撤销回调。
//!
//! 子系统在自身 crate 内实现 invoke、snapshot、paged snapshot 和 revoke 回调，再构造
//! [`ElmKernelProviderSpec`] 注册到 elm-mgr。Core 验证 owner、port、contract、flags 和回调
//! 完整性，但不会解释契约 payload。
//!
//! revoke 回调必须在 owner 卸载、provider 注销和替换切换时安全停止后端；snapshot 回调必须
//! 只写调用方缓冲区并准确报告长度/游标。暂未接入的子系统应使用明确 TODO flag 或
//! `elm_kernel_provider_unsupported`，不能伪装成成功空实现。

use crate::frame::{ELM_CALL_STATUS_UNSUPPORTED, ElmCallFrame, ElmReplyFrame};
use crate::ids::{BindingId, ElmId, LeaseId, PortId};
use crate::mgr::ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE;
use crate::mgr::api::{
    ELM_MGR_API_FLAG_PROVIDER_OPS, ELM_MGR_API_FLAG_STABLE, ELM_MGR_API_FLAG_TODO,
    ELM_MGR_API_KIND_SUBSYSTEM, ElmMgrApiDescriptor,
};
use crate::nexus::{FlowDirection, FlowMode};
use crate::ports::{ElmPortAccessPolicy, PortDescriptor};

/// `ELM_KERNEL_PROVIDER_FLAG_NONE` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_KERNEL_PROVIDER_FLAG_NONE: u32 = 0;
/// `ELM_KERNEL_PROVIDER_FLAG_TODO` 协议标志位；可在所属字段允许时与同组标志按位或组合。
pub const ELM_KERNEL_PROVIDER_FLAG_TODO: u32 = 1 << 0;

/// `ElmKernelProviderInvoke` 为该调用路径使用的规范类型别名，统一公开签名并避免重复表达底层布局。
pub type ElmKernelProviderInvoke = fn(ElmCallFrame) -> ElmReplyFrame;
/// `ElmKernelProviderSnapshot` 为该调用路径使用的规范类型别名，统一公开签名并避免重复表达底层布局。
pub type ElmKernelProviderSnapshot = fn(&mut [u8]) -> Result<usize, i32>;
/// `ElmKernelProviderSnapshotPaged` 为该调用路径使用的规范类型别名，统一公开签名并避免重复表达底层布局。
pub type ElmKernelProviderSnapshotPaged =
    fn(cursor: u32, out: &mut [u8]) -> Result<ElmKernelProviderSnapshotPage, i32>;
/// `ElmKernelProviderRevoke` 为该调用路径使用的规范类型别名，统一公开签名并避免重复表达底层布局。
pub type ElmKernelProviderRevoke = fn(Option<BindingId>, Option<LeaseId>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 内核 provider 分页快照回调返回的载荷长度、记录数和下一游标。
pub struct ElmKernelProviderSnapshotPage {
    /// 有效载荷的实际字节数；不得超过相邻载荷缓冲区容量。
    pub payload_len: usize,
    /// 回复中包含的完整记录数量。
    pub record_count: u32,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 下一页或下一批记录的游标。
    pub next_cursor: u32,
}

impl ElmKernelProviderSnapshotPage {
    /// 执行 `final_page` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn final_page(payload_len: usize, record_count: u32) -> Self {
        Self {
            payload_len,
            record_count,
            flags: 0,
            next_cursor: 0,
        }
    }

    /// 构造包含有效下一游标的分页快照回复。
    pub const fn more(payload_len: usize, record_count: u32, next_cursor: u32) -> Self {
        Self {
            payload_len,
            record_count,
            flags: ELM_PROVIDER_SNAPSHOT_RESPONSE_FLAG_MORE,
            next_cursor,
        }
    }
}

#[derive(Debug, Clone, Copy)]
/// 子系统注册内核 provider 所需的 owner、契约、端口属性和完整回调表。
pub struct ElmKernelProviderSpec {
    /// API 或能力所在的命名空间 identifier。
    pub namespace: &'static str,
    /// 对象的固定长度名称缓冲区；实际字符串以首个零字节结束。
    pub name: &'static str,
    /// 该 API 入口要求的完整契约 identifier。
    pub api_contract: &'static str,
    /// `api_kind` 是所属枚举的稳定判别值；未知值必须拒绝。
    pub api_kind: u32,
    /// `call_kind` 是所属枚举的稳定判别值；未知值必须拒绝。
    pub call_kind: u32,
    /// 协商得到的能力位集合；调用可选入口前必须先检查对应位。
    pub capabilities: u64,
    /// 该端口发布并用于绑定匹配的完整契约。
    pub port_contract: &'static str,
    /// 端口的数据流方向编码。
    pub direction: FlowDirection,
    /// 端口、绑定或扩展点采用的并发/分发模式编码。
    pub mode: FlowMode,
    /// 端口的访问范围策略编码。
    pub access: ElmPortAccessPolicy,
    /// `invokable` 表示该条件在当前快照或计划中是否成立。
    pub invokable: bool,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 执行 `invoke` 操作的受控回调；调用方必须遵守所属表的生命周期和故障边界。
    pub invoke: ElmKernelProviderInvoke,
    /// `snapshot` 表示该条件在当前快照或计划中是否成立。
    pub snapshot: Option<ElmKernelProviderSnapshot>,
    /// `snapshot_paged` 表示该条件在当前快照或计划中是否成立。
    pub snapshot_paged: Option<ElmKernelProviderSnapshotPaged>,
    /// 执行 `on_revoke` 操作的受控回调；调用方必须遵守所属表的生命周期和故障边界。
    pub on_revoke: Option<ElmKernelProviderRevoke>,
}

impl ElmKernelProviderSpec {
    #[allow(clippy::too_many_arguments)]
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(
        namespace: &'static str,
        name: &'static str,
        api_contract: &'static str,
        api_kind: u32,
        call_kind: u32,
        capabilities: u64,
        port_contract: &'static str,
        direction: FlowDirection,
        mode: FlowMode,
        access: ElmPortAccessPolicy,
        invokable: bool,
        flags: u32,
        invoke: ElmKernelProviderInvoke,
        snapshot: Option<ElmKernelProviderSnapshot>,
        on_revoke: Option<ElmKernelProviderRevoke>,
    ) -> Self {
        Self {
            namespace,
            name,
            api_contract,
            api_kind,
            call_kind,
            capabilities,
            port_contract,
            direction,
            mode,
            access,
            invokable,
            flags,
            invoke,
            snapshot,
            snapshot_paged: None,
            on_revoke,
        }
    }

    /// 设置 `paged_snapshot` 并返回更新后的值，便于构建器式初始化。
    pub const fn with_paged_snapshot(
        mut self,
        snapshot_paged: ElmKernelProviderSnapshotPaged,
    ) -> Self {
        self.snapshot_paged = Some(snapshot_paged);
        self
    }

    /// 执行 `subsystem_todo` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn subsystem_todo(
        namespace: &'static str,
        name: &'static str,
        api_contract: &'static str,
        port_contract: &'static str,
        direction: FlowDirection,
        mode: FlowMode,
        access: ElmPortAccessPolicy,
        invokable: bool,
    ) -> Self {
        Self::new(
            namespace,
            name,
            api_contract,
            ELM_MGR_API_KIND_SUBSYSTEM,
            0,
            0,
            port_contract,
            direction,
            mode,
            access,
            invokable,
            ELM_KERNEL_PROVIDER_FLAG_TODO,
            elm_kernel_provider_unsupported,
            None,
            None,
        )
    }

    /// 执行 `is_todo` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn is_todo(&self) -> bool {
        self.flags & ELM_KERNEL_PROVIDER_FLAG_TODO != 0
    }

    /// 执行 `api_descriptor` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn api_descriptor(&self, id: u64, owner: ElmId) -> ElmMgrApiDescriptor {
        let mut flags = ELM_MGR_API_FLAG_STABLE | ELM_MGR_API_FLAG_PROVIDER_OPS;
        if self.is_todo() {
            flags |= ELM_MGR_API_FLAG_TODO;
        }
        ElmMgrApiDescriptor::new(
            id,
            owner.0,
            self.api_kind,
            flags,
            self.call_kind,
            self.namespace,
            self.name,
            self.api_contract,
        )
        .with_capabilities(self.capabilities)
    }

    /// 执行 `port_descriptor` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn port_descriptor(&self, id: PortId, owner: ElmId) -> PortDescriptor {
        PortDescriptor {
            id,
            owner: Some(owner),
            contract: self.port_contract,
            direction: self.direction,
            mode: self.mode,
            access: self.access,
            invokable: self.invokable,
            implemented: true,
        }
    }
}

/// 执行 `elm_kernel_provider_unsupported` 定义的模型或协议操作；返回值反映校验后的结果。
pub fn elm_kernel_provider_unsupported(frame: ElmCallFrame) -> ElmReplyFrame {
    ElmReplyFrame::empty(frame.binding_id, frame.call_id, ELM_CALL_STATUS_UNSUPPORTED)
}
