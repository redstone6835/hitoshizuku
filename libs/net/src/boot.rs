//! 网络 host 向 driver 与 stack 分发的启动期只读配置。

use spin::Mutex;

static HOST_BOOT_CONFIG: Mutex<Option<NetHostBootConfig>> = Mutex::new(None);

/// 当前协议状态允许建立的最大 shard 数。
///
/// 每个 `FlowShard` 仍会预分配一套完整的 4096-flow 状态，而当前 VirtIO-net
/// 驱动只注册一个 queue pair。在状态容量能够按全局预算分摊以前，限制协议 shard
/// 数可以避免在线 CPU 增加时线性复制内存与高优先级 worker。
pub const MAX_PROTOCOL_SHARDS: u8 = 4;

/// 按在线 CPU 数选择启动期协议 shard 数。
pub const fn select_protocol_shard_count(online_cpu_count: u32) -> Option<u8> {
    if online_cpu_count == 0 {
        return None;
    }
    Some(if online_cpu_count > MAX_PROTOCOL_SHARDS as u32 {
        MAX_PROTOCOL_SHARDS
    } else {
        online_cpu_count as u8
    })
}

/// 只能由常驻 host 保存的启动配置。
#[derive(Clone, Copy)]
pub struct NetHostBootConfig {
    mac_seed: [u8; 16],
}

impl NetHostBootConfig {
    pub const fn mac_seed(&self) -> &[u8; 16] {
        &self.mac_seed
    }
}

/// 网络驱动可见的启动配置。
#[derive(Clone, Copy)]
pub struct NetDriverBootConfig {
    rss_key: [u8; 40],
    /// 驱动用于 RSS 映射的协议 shard 数，不是系统在线 CPU 总数。
    active_cpu_count: u8,
}

impl NetDriverBootConfig {
    pub const fn rss_key(&self) -> &[u8; 40] {
        &self.rss_key
    }

    pub const fn active_cpu_count(&self) -> u8 {
        self.active_cpu_count
    }
}

/// `net.stack` 可见的协议启动配置。
#[derive(Clone, Copy)]
pub struct NetStackBootConfig {
    rss_key: [u8; 40],
    tcp_isn_key: [u8; 16],
    ephemeral_port_key: [u8; 16],
    hash_seed: [u8; 16],
    generation_nonce: [u8; 8],
    /// 本代协议状态与 worker 必须共同采用的 shard 数。
    active_cpu_count: u8,
}

#[kernel_symbols::export]
impl NetStackBootConfig {
    #[kernel_symbols::export(
        name = "net.boot.NetStackBootConfig.rss_key",
        contract = "kernel.net.stack-boot-config@1",
        version = 1,
        capabilities = kernel_symbols::capability::NETWORK_STACK
    )]
    pub fn rss_key(&self) -> &[u8; 40] {
        &self.rss_key
    }

    #[kernel_symbols::export(
        name = "net.boot.NetStackBootConfig.tcp_isn_key",
        contract = "kernel.net.stack-boot-config@1",
        version = 1,
        capabilities = kernel_symbols::capability::NETWORK_STACK
    )]
    pub fn tcp_isn_key(&self) -> &[u8; 16] {
        &self.tcp_isn_key
    }

    #[kernel_symbols::export(
        name = "net.boot.NetStackBootConfig.ephemeral_port_key",
        contract = "kernel.net.stack-boot-config@1",
        version = 1,
        capabilities = kernel_symbols::capability::NETWORK_STACK
    )]
    pub fn ephemeral_port_key(&self) -> &[u8; 16] {
        &self.ephemeral_port_key
    }

    #[kernel_symbols::export(
        name = "net.boot.NetStackBootConfig.hash_seed",
        contract = "kernel.net.stack-boot-config@1",
        version = 1,
        capabilities = kernel_symbols::capability::NETWORK_STACK
    )]
    pub fn hash_seed(&self) -> &[u8; 16] {
        &self.hash_seed
    }

    #[kernel_symbols::export(
        name = "net.boot.NetStackBootConfig.generation_nonce",
        contract = "kernel.net.stack-boot-config@1",
        version = 1,
        capabilities = kernel_symbols::capability::NETWORK_STACK
    )]
    pub fn generation_nonce(&self) -> &[u8; 8] {
        &self.generation_nonce
    }

    #[kernel_symbols::export(
        name = "net.boot.NetStackBootConfig.active_cpu_count",
        contract = "kernel.net.stack-boot-config@1",
        version = 1,
        capabilities = kernel_symbols::capability::NETWORK_STACK
    )]
    pub fn active_cpu_count(&self) -> u8 {
        self.active_cpu_count
    }
}

/// 常驻 host 从原始随机材料一次性拆出的三类配置。
pub struct NetBootConfigs {
    host: NetHostBootConfig,
    driver: NetDriverBootConfig,
    stack: NetStackBootConfig,
}

impl NetBootConfigs {
    pub fn from_random_material(material: [u8; 112], active_cpu_count: u8) -> Option<Self> {
        if active_cpu_count == 0 || active_cpu_count > MAX_PROTOCOL_SHARDS {
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
            host: NetHostBootConfig { mac_seed },
            driver: NetDriverBootConfig {
                rss_key,
                active_cpu_count,
            },
            stack: NetStackBootConfig {
                rss_key,
                tcp_isn_key,
                ephemeral_port_key,
                hash_seed,
                generation_nonce,
                active_cpu_count,
            },
        })
    }

    pub const fn split(self) -> (NetHostBootConfig, NetDriverBootConfig, NetStackBootConfig) {
        (self.host, self.driver, self.stack)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallNetHostBootConfigError {
    AlreadyInstalled,
}

/// 在 ELM 装载前封存只允许常驻 host 使用的启动材料。
pub fn install_host_boot_config(
    config: NetHostBootConfig,
) -> Result<(), InstallNetHostBootConfigError> {
    let mut slot = HOST_BOOT_CONFIG.lock();
    if slot.is_some() {
        return Err(InstallNetHostBootConfigError::AlreadyInstalled);
    }
    *slot = Some(config);
    Ok(())
}

pub fn host_boot_config() -> Option<NetHostBootConfig> {
    *HOST_BOOT_CONFIG.lock()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_material_is_split_by_consumer() {
        let material = core::array::from_fn(|index| index as u8);
        let (host, driver, stack) = NetBootConfigs::from_random_material(material, 4)
            .unwrap()
            .split();
        assert_eq!(host.mac_seed()[0], 96);
        assert_eq!(driver.rss_key()[0], 0);
        assert_eq!(driver.rss_key()[39], 39);
        assert_eq!(driver.active_cpu_count(), 4);
        assert_eq!(stack.rss_key()[0], 0);
        assert_eq!(stack.tcp_isn_key()[0], 40);
        assert_eq!(stack.ephemeral_port_key()[0], 56);
        assert_eq!(stack.hash_seed()[0], 72);
        assert_eq!(stack.generation_nonce()[0], 88);
        assert_eq!(stack.active_cpu_count(), 4);
    }

    #[test]
    fn boot_material_rejects_invalid_cpu_count() {
        assert!(NetBootConfigs::from_random_material([0; 112], 0).is_none());
        assert!(NetBootConfigs::from_random_material([0; 112], MAX_PROTOCOL_SHARDS + 1).is_none());
    }

    #[test]
    fn protocol_shard_policy_caps_cpu_scaled_state() {
        assert_eq!(select_protocol_shard_count(0), None);
        assert_eq!(select_protocol_shard_count(1), Some(1));
        assert_eq!(select_protocol_shard_count(4), Some(4));
        assert_eq!(select_protocol_shard_count(8), Some(4));
    }
}
