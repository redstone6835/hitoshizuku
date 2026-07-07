//! 内核 provider 规格模型。
//!
//! 本模块只定义 ELM Core 可以理解的通用 provider 描述。具体 provider 的
//! 语义属于导出它的子系统，Core 只负责登记、绑定、调用、审计和撤销回调。

use crate::frame::{ELM_CALL_STATUS_UNSUPPORTED, ElmCallFrame, ElmReplyFrame};
use crate::ids::{BindingId, ElmId, LeaseId, PortId};
use crate::mgr::api::{
    ELM_MGR_API_FLAG_PROVIDER_OPS, ELM_MGR_API_FLAG_STABLE, ELM_MGR_API_FLAG_TODO,
    ELM_MGR_API_KIND_SUBSYSTEM, ElmMgrApiDescriptor,
};
use crate::nexus::{FlowDirection, FlowMode};
use crate::ports::{ElmPortAccessPolicy, PortDescriptor};

pub const ELM_KERNEL_PROVIDER_FLAG_NONE: u32 = 0;
pub const ELM_KERNEL_PROVIDER_FLAG_TODO: u32 = 1 << 0;

pub type ElmKernelProviderInvoke = fn(ElmCallFrame) -> ElmReplyFrame;
pub type ElmKernelProviderSnapshot = fn(&mut [u8]) -> Result<usize, i32>;
pub type ElmKernelProviderRevoke = fn(Option<BindingId>, Option<LeaseId>);

#[derive(Debug, Clone, Copy)]
pub struct ElmKernelProviderSpec {
    pub namespace: &'static str,
    pub name: &'static str,
    pub api_contract: &'static str,
    pub api_kind: u32,
    pub call_kind: u32,
    pub capabilities: u64,
    pub port_contract: &'static str,
    pub direction: FlowDirection,
    pub mode: FlowMode,
    pub access: ElmPortAccessPolicy,
    pub invokable: bool,
    pub flags: u32,
    pub invoke: ElmKernelProviderInvoke,
    pub snapshot: Option<ElmKernelProviderSnapshot>,
    pub on_revoke: Option<ElmKernelProviderRevoke>,
}

impl ElmKernelProviderSpec {
    #[allow(clippy::too_many_arguments)]
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
            on_revoke,
        }
    }

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

    pub const fn is_todo(&self) -> bool {
        self.flags & ELM_KERNEL_PROVIDER_FLAG_TODO != 0
    }

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

pub fn elm_kernel_provider_unsupported(frame: ElmCallFrame) -> ElmReplyFrame {
    ElmReplyFrame::empty(frame.binding_id, frame.call_id, ELM_CALL_STATUS_UNSUPPORTED)
}
