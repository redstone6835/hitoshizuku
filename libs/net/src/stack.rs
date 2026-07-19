//! 网络协议栈 ELM 与常驻 host 之间的生命周期契约。

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;

use crate::boot::NetStackBootConfig;

static NEXT_STACK_HANDLE: AtomicU64 = AtomicU64::new(1);
static STACK_BOOT_CONFIG: Mutex<Option<NetStackBootConfig>> = Mutex::new(None);
static STACK_REGISTRAR: Mutex<Option<&'static dyn NetStackRegistrar>> = Mutex::new(None);

pub const NET_STACK_CALL_ABI_VERSION: u16 = 1;
pub const NET_STACK_CALL_RUST_ABI: &str = "fn(&mutnet::stack::NetStackCallV1)->i32";
pub const NET_STACK_CALL_STATUS_OK: i32 = 0;
pub const NET_STACK_CALL_STATUS_INVALID: i32 = -22;

pub const NET_STACK_OP_PROBE: u32 = 1;
pub const NET_STACK_OP_WORKER_TURN: u32 = 2;
pub const NET_STACK_OP_QUIESCE: u32 = 3;

/// 常驻 worker shell 与 `net.stack` 间一次同步调用的固定帧。
#[repr(C)]
pub struct NetStackCallV1 {
    pub abi_version: u16,
    pub struct_size: u16,
    pub opcode: u32,
    pub generation: u64,
    pub ready: u8,
    pub quiesced: u8,
    pub reserved0: [u8; 6],
    pub reserved1: [u64; 2],
}

#[kernel_symbols::export]
impl NetStackCallV1 {
    pub fn new(opcode: u32, generation: u64) -> Self {
        Self {
            abi_version: NET_STACK_CALL_ABI_VERSION,
            struct_size: core::mem::size_of::<Self>() as u16,
            opcode,
            generation,
            ready: 0,
            quiesced: 0,
            reserved0: [0; 6],
            reserved1: [0; 2],
        }
    }

    #[kernel_symbols::export(
        name = "net.stack.NetStackCallV1.valid",
        contract = "kernel.net.stack-call-frame@1",
        version = 1,
        capabilities = kernel_symbols::capability::CORE_SAFE
    )]
    pub fn valid(&self, opcode: u32, generation: u64) -> bool {
        self.abi_version == NET_STACK_CALL_ABI_VERSION
            && self.struct_size as usize == core::mem::size_of::<Self>()
            && self.opcode == opcode
            && self.generation == generation
            && self.reserved0 == [0; 6]
            && self.reserved1 == [0; 2]
    }
}

/// 动态 `net.stack` 的代际固定 export 描述。
pub struct PinnedNetStackEndpoint {
    owner_cell: u64,
    owner_generation: u64,
    export_name: Box<str>,
    export_contract: Box<str>,
    export_version: u32,
}

#[kernel_symbols::export]
impl PinnedNetStackEndpoint {
    #[kernel_symbols::export(
        name = "net.stack.PinnedNetStackEndpoint.current",
        contract = "kernel.net.stack-endpoint@1",
        version = 1,
        capabilities = kernel_symbols::capability::NETWORK_STACK
    )]
    pub fn current(export_name: &str, export_contract: &str, export_version: u32) -> Option<Self> {
        let context = elm_model::current_context()?;
        if export_name.is_empty()
            || export_contract.is_empty()
            || export_version == 0
            || elm_model::FlowContract::new(export_contract).is_err()
        {
            return None;
        }
        Some(Self {
            owner_cell: context.cell_id.0,
            owner_generation: context.generation.0,
            export_name: export_name.into(),
            export_contract: export_contract.into(),
            export_version,
        })
    }

    pub const fn owner_cell(&self) -> u64 {
        self.owner_cell
    }

    pub const fn owner_generation(&self) -> u64 {
        self.owner_generation
    }

    pub fn export_name(&self) -> &str {
        &self.export_name
    }

    pub fn export_contract(&self) -> &str {
        &self.export_contract
    }

    pub const fn export_version(&self) -> u32 {
        self.export_version
    }
}

pub type IntegratedNetStackCall = fn(&mut NetStackCallV1) -> i32;

pub enum NetStackEndpoint {
    Integrated(IntegratedNetStackCall),
    Pinned(PinnedNetStackEndpoint),
}

/// 一个 stack generation 的原子注册单元。
pub struct NetStackRegistration {
    handle: NetStackHandle,
    endpoint: NetStackEndpoint,
}

#[kernel_symbols::export]
impl NetStackRegistration {
    pub fn integrated(call: IntegratedNetStackCall) -> Option<Self> {
        if elm_model::current_context().is_some() || call as usize == 0 {
            return None;
        }
        Some(Self {
            handle: next_stack_handle(),
            endpoint: NetStackEndpoint::Integrated(call),
        })
    }

    #[kernel_symbols::export(
        name = "net.stack.NetStackRegistration.pinned",
        contract = "kernel.net.stack-registration@1",
        version = 1,
        capabilities = kernel_symbols::capability::NETWORK_STACK
    )]
    pub fn pinned(endpoint: PinnedNetStackEndpoint) -> Self {
        Self {
            handle: next_stack_handle(),
            endpoint: NetStackEndpoint::Pinned(endpoint),
        }
    }

    pub const fn handle(&self) -> NetStackHandle {
        self.handle
    }

    pub fn owner_cell(&self) -> u64 {
        match &self.endpoint {
            NetStackEndpoint::Integrated(_) => 0,
            NetStackEndpoint::Pinned(endpoint) => endpoint.owner_cell(),
        }
    }

    pub fn generation(&self) -> u64 {
        match &self.endpoint {
            NetStackEndpoint::Integrated(_) => 1,
            NetStackEndpoint::Pinned(endpoint) => endpoint.owner_generation(),
        }
    }

    pub fn endpoint(&self) -> &NetStackEndpoint {
        &self.endpoint
    }

    fn valid_for_current_context(&self) -> bool {
        match (&self.endpoint, elm_model::current_context()) {
            (NetStackEndpoint::Integrated(_), None) => true,
            (NetStackEndpoint::Pinned(endpoint), Some(context)) => {
                endpoint.owner_cell() == context.cell_id.0
                    && endpoint.owner_generation() == context.generation.0
            }
            _ => false,
        }
    }
}

fn next_stack_handle() -> NetStackHandle {
    let raw = NEXT_STACK_HANDLE.fetch_add(1, Ordering::Relaxed);
    assert!(raw != 0, "NetStackHandle 已耗尽");
    NetStackHandle(raw)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct NetStackHandle(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NetStackState {
    Absent = 0,
    Active = 1,
    Quiescing = 2,
    Draining = 3,
    Faulted = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetStackSnapshot {
    pub state: NetStackState,
    pub handle: Option<NetStackHandle>,
    pub owner_cell: u64,
    pub generation: u64,
    pub probed: bool,
}

impl NetStackSnapshot {
    pub const fn absent() -> Self {
        Self {
            state: NetStackState::Absent,
            handle: None,
            owner_cell: 0,
            generation: 0,
            probed: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetStackRegisterErrorKind {
    RegistrarNotReady,
    AlreadyActive,
    InvalidRegistration,
    ResourceExhausted,
}

pub struct NetStackRegisterError {
    pub kind: NetStackRegisterErrorKind,
    pub registration: NetStackRegistration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetStackRemoveError {
    NoStack,
    OwnerMismatch,
    Busy,
}

pub trait NetStackRegistrar: Send + Sync {
    fn register_stack(
        &self,
        registration: NetStackRegistration,
    ) -> Result<NetStackHandle, NetStackRegisterError>;

    fn begin_remove(
        &self,
        handle: NetStackHandle,
        owner_cell: u64,
        generation: u64,
    ) -> Result<(), NetStackRemoveError>;

    fn snapshot(&self) -> NetStackSnapshot;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallNetStackRuntimeError {
    AlreadyInstalled,
}

/// 在任何 `net.stack` 初始化前一次性安装常驻 broker。
pub fn install_stack_runtime(
    config: NetStackBootConfig,
    registrar: &'static dyn NetStackRegistrar,
) -> Result<(), InstallNetStackRuntimeError> {
    let mut config_slot = STACK_BOOT_CONFIG.lock();
    let mut slot = STACK_REGISTRAR.lock();
    if config_slot.is_some() || slot.is_some() {
        return Err(InstallNetStackRuntimeError::AlreadyInstalled);
    }
    *config_slot = Some(config);
    *slot = Some(registrar);
    Ok(())
}

#[kernel_symbols::export(
    name = "net.stack.boot_config",
    contract = "kernel.net.stack-boot-config@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK
)]
pub fn boot_config() -> Option<NetStackBootConfig> {
    *STACK_BOOT_CONFIG.lock()
}

#[kernel_symbols::export(
    name = "net.stack.register_stack",
    contract = "kernel.net.stack-registration@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn register_stack(
    registration: NetStackRegistration,
) -> Result<NetStackHandle, NetStackRegisterError> {
    if !registration.valid_for_current_context() {
        return Err(NetStackRegisterError {
            kind: NetStackRegisterErrorKind::InvalidRegistration,
            registration,
        });
    }
    let registrar = *STACK_REGISTRAR.lock();
    let Some(registrar) = registrar else {
        return Err(NetStackRegisterError {
            kind: NetStackRegisterErrorKind::RegistrarNotReady,
            registration,
        });
    };
    registrar.register_stack(registration)
}

#[kernel_symbols::export(
    name = "net.stack.begin_remove",
    contract = "kernel.net.stack-registration@1",
    version = 1,
    capabilities = kernel_symbols::capability::NETWORK_STACK,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn begin_remove(handle: NetStackHandle) -> Result<(), NetStackRemoveError> {
    let registrar = *STACK_REGISTRAR.lock();
    let Some(registrar) = registrar else {
        return Err(NetStackRemoveError::NoStack);
    };
    let (owner_cell, generation) = elm_model::current_context()
        .map(|context| (context.cell_id.0, context.generation.0))
        .unwrap_or((0, 1));
    registrar.begin_remove(handle, owner_cell, generation)
}

pub fn stack_snapshot() -> NetStackSnapshot {
    let registrar = *STACK_REGISTRAR.lock();
    registrar
        .map(NetStackRegistrar::snapshot)
        .unwrap_or_else(NetStackSnapshot::absent)
}

/// 由常驻 broker 使用的严格生命周期状态机。
pub struct NetStackLifecycle {
    snapshot: NetStackSnapshot,
}

impl NetStackLifecycle {
    pub const fn new() -> Self {
        Self {
            snapshot: NetStackSnapshot::absent(),
        }
    }

    pub const fn snapshot(&self) -> NetStackSnapshot {
        self.snapshot
    }

    pub fn activate(
        &mut self,
        handle: NetStackHandle,
        owner_cell: u64,
        generation: u64,
    ) -> Result<(), NetStackRegisterErrorKind> {
        if self.snapshot.state != NetStackState::Absent {
            return Err(NetStackRegisterErrorKind::AlreadyActive);
        }
        if handle.0 == 0 || generation == 0 {
            return Err(NetStackRegisterErrorKind::InvalidRegistration);
        }
        self.snapshot = NetStackSnapshot {
            state: NetStackState::Active,
            handle: Some(handle),
            owner_cell,
            generation,
            probed: false,
        };
        Ok(())
    }

    pub fn mark_probed(&mut self, handle: NetStackHandle) -> bool {
        if self.snapshot.handle != Some(handle) || self.snapshot.state != NetStackState::Active {
            return false;
        }
        self.snapshot.probed = true;
        true
    }

    pub fn mark_faulted(&mut self, handle: NetStackHandle) -> bool {
        if self.snapshot.handle != Some(handle)
            || !matches!(
                self.snapshot.state,
                NetStackState::Active | NetStackState::Faulted
            )
        {
            return false;
        }
        self.snapshot.state = NetStackState::Faulted;
        self.snapshot.probed = false;
        true
    }

    pub fn begin_remove(
        &mut self,
        handle: NetStackHandle,
        owner_cell: u64,
        generation: u64,
    ) -> Result<(), NetStackRemoveError> {
        if self.snapshot.state == NetStackState::Absent {
            return Err(NetStackRemoveError::NoStack);
        }
        if self.snapshot.handle != Some(handle)
            || self.snapshot.owner_cell != owner_cell
            || self.snapshot.generation != generation
        {
            return Err(NetStackRemoveError::OwnerMismatch);
        }
        if !matches!(
            self.snapshot.state,
            NetStackState::Active | NetStackState::Faulted
        ) {
            return Err(NetStackRemoveError::Busy);
        }
        self.snapshot.state = NetStackState::Quiescing;
        Ok(())
    }

    pub fn begin_drain(&mut self, handle: NetStackHandle) -> bool {
        if self.snapshot.handle != Some(handle) || self.snapshot.state != NetStackState::Quiescing {
            return false;
        }
        self.snapshot.state = NetStackState::Draining;
        true
    }

    pub fn finish_remove(&mut self, handle: NetStackHandle) -> bool {
        if self.snapshot.handle != Some(handle) || self.snapshot.state != NetStackState::Draining {
            return false;
        }
        self.snapshot = NetStackSnapshot::absent();
        true
    }
}

impl Default for NetStackLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_frame_rejects_stale_generation_and_reserved_bits() {
        let mut frame = NetStackCallV1::new(NET_STACK_OP_PROBE, 7);
        assert!(frame.valid(NET_STACK_OP_PROBE, 7));
        assert!(!frame.valid(NET_STACK_OP_PROBE, 8));
        frame.reserved1[0] = 1;
        assert!(!frame.valid(NET_STACK_OP_PROBE, 7));
    }

    #[test]
    fn lifecycle_requires_owned_quiesce_and_drain() {
        let handle = NetStackHandle(9);
        let mut lifecycle = NetStackLifecycle::new();
        lifecycle.activate(handle, 3, 4).unwrap();
        assert_eq!(
            lifecycle.begin_remove(handle, 3, 5),
            Err(NetStackRemoveError::OwnerMismatch)
        );
        lifecycle.begin_remove(handle, 3, 4).unwrap();
        assert_eq!(lifecycle.snapshot().state, NetStackState::Quiescing);
        assert!(lifecycle.begin_drain(handle));
        assert!(lifecycle.finish_remove(handle));
        assert_eq!(lifecycle.snapshot(), NetStackSnapshot::absent());
    }

    #[test]
    fn lifecycle_rejects_two_active_generations() {
        let mut lifecycle = NetStackLifecycle::new();
        lifecycle.activate(NetStackHandle(1), 10, 1).unwrap();
        assert_eq!(
            lifecycle.activate(NetStackHandle(2), 11, 2),
            Err(NetStackRegisterErrorKind::AlreadyActive)
        );
    }
}
