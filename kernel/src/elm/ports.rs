//! 枢纽端口运行时描述。

use alloc::string::{String, ToString};

use elm_model::{ElmId, ElmPortAccessPolicy, FlowDirection, FlowMode, PortDescriptor, PortId};

#[derive(Debug, Clone)]
pub(crate) struct PortRuntime {
    pub id: PortId,
    pub owner: Option<ElmId>,
    pub contract: String,
    pub direction: FlowDirection,
    pub mode: FlowMode,
    pub access: ElmPortAccessPolicy,
    pub invokable: bool,
    pub implemented: bool,
}

impl PortRuntime {
    pub fn from_descriptor(desc: PortDescriptor) -> Self {
        Self::new(
            desc.id,
            desc.owner,
            desc.contract,
            desc.direction,
            desc.mode,
            desc.access,
            desc.invokable,
            desc.implemented,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: PortId,
        owner: Option<ElmId>,
        contract: &str,
        direction: FlowDirection,
        mode: FlowMode,
        access: ElmPortAccessPolicy,
        invokable: bool,
        implemented: bool,
    ) -> Self {
        Self {
            id,
            owner,
            contract: contract.to_string(),
            direction,
            mode,
            access,
            invokable,
            implemented,
        }
    }

    pub fn contract(&self) -> &str {
        &self.contract
    }
}
