//! 网络设备注册、boot key 与 IRQ 唤醒边界。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use spin::Mutex;

use crate::buf::NetBufPoolOwner;
use crate::queue::NetQueuePair;
use crate::{NetDeviceId, QueuePairId};

static NEXT_DEVICE_ID: AtomicU32 = AtomicU32::new(1);
static BOOT_CONFIG: Mutex<Option<NetBootConfig>> = Mutex::new(None);
static REGISTRAR: Mutex<Option<&'static dyn NetDeviceRegistrar>> = Mutex::new(None);

/// 网络用途的启动期独立 key 材料。
#[derive(Clone, Copy)]
pub struct NetBootConfig {
    rss_key: [u8; 40],
    tcp_isn_key: [u8; 16],
    ephemeral_port_key: [u8; 16],
    hash_seed: [u8; 16],
    generation_nonce: [u8; 8],
    mac_seed: [u8; 16],
    active_cpu_count: u8,
}

impl NetBootConfig {
    pub fn from_random_material(material: [u8; 112], active_cpu_count: u8) -> Option<Self> {
        if active_cpu_count == 0 || active_cpu_count > 8 {
            return None;
        }
        let mut rss_key = [0; 40];
        let mut tcp_isn_key = [0; 16];
        let mut ephemeral_port_key = [0; 16];
        let mut hash_seed = [0; 16];
        let mut generation_nonce = [0; 8];
        let mut mac_seed = [0; 16];
        rss_key.copy_from_slice(&material[0..40]);
        tcp_isn_key.copy_from_slice(&material[40..56]);
        ephemeral_port_key.copy_from_slice(&material[56..72]);
        hash_seed.copy_from_slice(&material[72..88]);
        generation_nonce.copy_from_slice(&material[88..96]);
        mac_seed.copy_from_slice(&material[96..112]);
        Some(Self {
            rss_key,
            tcp_isn_key,
            ephemeral_port_key,
            hash_seed,
            generation_nonce,
            mac_seed,
            active_cpu_count,
        })
    }

    pub const fn rss_key(&self) -> &[u8; 40] {
        &self.rss_key
    }

    pub const fn tcp_isn_key(&self) -> &[u8; 16] {
        &self.tcp_isn_key
    }

    pub const fn ephemeral_port_key(&self) -> &[u8; 16] {
        &self.ephemeral_port_key
    }

    pub const fn hash_seed(&self) -> &[u8; 16] {
        &self.hash_seed
    }

    pub const fn generation_nonce(&self) -> &[u8; 8] {
        &self.generation_nonce
    }

    pub const fn mac_seed(&self) -> &[u8; 16] {
        &self.mac_seed
    }

    pub const fn active_cpu_count(&self) -> u8 {
        self.active_cpu_count
    }
}

/// IRQ handler 安装到 queue 的唤醒目标。
pub trait QueueWakeHandle: Send + Sync {
    fn wake(&self);
}

/// driver 与 IRQ handler 共享的唯一 queue 控制对象。
pub trait QueueIrqControl: Send + Sync {
    fn ack_and_mask(&self) -> bool;
    fn unmask(&self);
    fn set_waker(&self, waker: Arc<dyn QueueWakeHandle>) -> Result<(), QueueIrqError>;
    fn stats(&self) -> QueueIrqStats;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueIrqError {
    WakerAlreadyInstalled,
    DeviceGone,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueIrqStats {
    pub irq_total: u64,
    pub irq_mask: u64,
    pub irq_unmask: u64,
}

/// 一个 queue pair 连同其三个独立 pool owner 的原子注册单元。
pub struct NetQueueRegistration {
    pub id: QueuePairId,
    pub queue: Box<dyn NetQueuePair>,
    pub rx_pool: NetBufPoolOwner,
    pub tx_header_pool: NetBufPoolOwner,
    pub tx_payload_pool: NetBufPoolOwner,
    pub irq: Arc<dyn QueueIrqControl>,
}

/// driver 完成协商后一次性交给 kernel 的设备资源。
pub struct NetDeviceRegistration {
    pub id: NetDeviceId,
    pub name: Box<str>,
    pub mac_address: [u8; 6],
    pub mtu: u32,
    pub running: bool,
    pub queues: Box<[NetQueueRegistration]>,
}

impl NetDeviceRegistration {
    pub fn new(
        name: Box<str>,
        mac_address: [u8; 6],
        mtu: u32,
        running: bool,
        queues: Box<[NetQueueRegistration]>,
    ) -> Self {
        let raw = NEXT_DEVICE_ID.fetch_add(1, Ordering::Relaxed);
        assert!(raw != 0, "NetDeviceId 已耗尽");
        Self {
            id: NetDeviceId(raw),
            name,
            mac_address,
            mtu,
            running,
            queues,
        }
    }
}

/// kernel 成功接管设备后的不透明句柄。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct NetDeviceHandle(pub u64);

/// 同步 remove 完成后交还给 driver 的令牌。
pub struct NetDeviceTeardown {
    pub handle: NetDeviceHandle,
}

/// 控制面按需生成的只读设备快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetDeviceSnapshot {
    pub id: NetDeviceId,
    pub name: Box<str>,
    pub mac_address: [u8; 6],
    pub mtu: u32,
    pub queue_pairs: u16,
    pub running: bool,
    pub stats: NetDeviceStats,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetDeviceStats {
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_errors: u64,
    pub rx_dropped: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_errors: u64,
    pub tx_dropped: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetStat {
    pub device: NetDeviceId,
    pub queue: QueuePairId,
    pub key: &'static str,
    pub value: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetDeviceRegisterErrorKind {
    RegistrarNotReady,
    InvalidRegistration,
    ResourceExhausted,
}

/// 注册失败必须原样返还全部 ownership。
pub struct NetDeviceRegisterError {
    pub kind: NetDeviceRegisterErrorKind,
    pub registration: NetDeviceRegistration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetDeviceRemoveError {
    NoDevice,
    Busy,
    AlreadyRemoving,
}

/// 由 kernel 实现的设备接管入口。
pub trait NetDeviceRegistrar: Send + Sync {
    fn register_device(
        &self,
        registration: NetDeviceRegistration,
    ) -> Result<NetDeviceHandle, NetDeviceRegisterError>;

    fn begin_remove(
        &self,
        handle: NetDeviceHandle,
    ) -> Result<NetDeviceTeardown, NetDeviceRemoveError>;

    fn snapshot_devices(&self) -> Vec<NetDeviceSnapshot>;

    fn snapshot_stats(&self) -> Vec<NetStat>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallNetRuntimeError {
    AlreadyInstalled,
}

/// 在任何网络 PnP probe 前一次性安装 boot key 与 kernel registrar。
pub fn install_net_runtime(
    config: NetBootConfig,
    registrar: &'static dyn NetDeviceRegistrar,
) -> Result<(), InstallNetRuntimeError> {
    let mut config_slot = BOOT_CONFIG.lock();
    let mut registrar_slot = REGISTRAR.lock();
    if config_slot.is_some() || registrar_slot.is_some() {
        return Err(InstallNetRuntimeError::AlreadyInstalled);
    }
    *config_slot = Some(config);
    *registrar_slot = Some(registrar);
    Ok(())
}

pub fn boot_config() -> Option<NetBootConfig> {
    *BOOT_CONFIG.lock()
}

pub fn register_device(
    registration: NetDeviceRegistration,
) -> Result<NetDeviceHandle, NetDeviceRegisterError> {
    let registrar = *REGISTRAR.lock();
    let Some(registrar) = registrar else {
        return Err(NetDeviceRegisterError {
            kind: NetDeviceRegisterErrorKind::RegistrarNotReady,
            registration,
        });
    };
    registrar.register_device(registration)
}

pub fn begin_remove(handle: NetDeviceHandle) -> Result<NetDeviceTeardown, NetDeviceRemoveError> {
    let registrar = *REGISTRAR.lock();
    let Some(registrar) = registrar else {
        return Err(NetDeviceRemoveError::NoDevice);
    };
    registrar.begin_remove(handle)
}

/// registrar 尚未安装时返回空列表，供早期 procfs/sysfs/netlink 安全读取。
pub fn snapshot_devices() -> Vec<NetDeviceSnapshot> {
    let registrar = *REGISTRAR.lock();
    registrar
        .map(NetDeviceRegistrar::snapshot_devices)
        .unwrap_or_default()
}

pub fn snapshot_stats() -> Vec<NetStat> {
    let registrar = *REGISTRAR.lock();
    registrar
        .map(NetDeviceRegistrar::snapshot_stats)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_material_is_split_without_overlap() {
        let material = core::array::from_fn(|index| index as u8);
        let config = NetBootConfig::from_random_material(material, 4).unwrap();
        assert_eq!(config.rss_key()[0], 0);
        assert_eq!(config.rss_key()[39], 39);
        assert_eq!(config.tcp_isn_key()[0], 40);
        assert_eq!(config.ephemeral_port_key()[0], 56);
        assert_eq!(config.hash_seed()[0], 72);
        assert_eq!(config.generation_nonce()[0], 88);
        assert_eq!(config.mac_seed()[0], 96);
        assert_eq!(config.active_cpu_count(), 4);
    }
}
