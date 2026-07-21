//! 随机服务的设备无关注册接口。
//!
//! 这里不实现随机算法，只保存当前 ELM 提供的后端，并为系统调用和基础字符设备
//! 提供稳定代理。驱动未装载时接口明确返回不可用，不回退到弱伪随机实现。

use alloc::sync::Arc;
use core::any::Any;

use vfs::sync::Spinlock;

use super::char::{CharDriver, CharIoError};

const BOOT_SEED_CAPACITY: usize = 256;

/// 随机后端的读取语义。
///
/// 该枚举显式区分 CSPRNG 与按熵记账的读取，避免用单个 `blocking` 布尔值同时
/// 表达“读取哪一种随机源”和“熵不足时是否等待”。非阻塞模式在暂时无法返回
/// 数据时必须返回 `Ok(0)`，由系统调用或设备层转换成对应的短读或 `EAGAIN`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RandomReadMode {
    /// 仅在 CSPRNG 已获得足够熵并完成安全播种后输出。
    Secure { blocking: bool },
    /// 即使尚未完成安全播种也允许输出，只供显式请求的早期启动路径使用。
    Insecure,
    /// 等待安全播种后输出，对应 `/dev/random` 与 `GRND_RANDOM`。
    ///
    /// 熵估计只用于判断 CSPRNG 是否已经完成初始化；初始化完成后，读取
    /// 不会按输出长度耗尽一个有限的 credit 计数器。这与现代 Linux 的
    /// `/dev/random` 语义一致，也避免一个正常的长读永久等待新的“熵位”。
    Entropy { blocking: bool },
}

pub trait RandomBackend: Send + Sync {
    fn read(&self, output: &mut [u8], mode: RandomReadMode) -> Result<usize, CharIoError>;
    fn write(&self, input: &[u8]) -> Result<usize, CharIoError>;
    fn add_entropy(&self, input: &[u8], entropy_bits: u64);
    fn entropy_bits(&self) -> u64;
    fn reseed(&self);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RandomBackendHandle(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RandomServiceError {
    AlreadyInstalled,
    NotInstalled,
    InvalidHandle,
}

struct RandomRegistry {
    next_id: u64,
    active: Option<(RandomBackendHandle, Arc<dyn RandomBackend>)>,
    boot_seed: [u8; BOOT_SEED_CAPACITY],
    boot_seed_len: usize,
    boot_entropy_bits: u64,
}

impl RandomRegistry {
    const fn new() -> Self {
        Self {
            next_id: 1,
            active: None,
            boot_seed: [0; BOOT_SEED_CAPACITY],
            boot_seed_len: 0,
            boot_entropy_bits: 0,
        }
    }
}

static RANDOM: Spinlock<RandomRegistry> = Spinlock::new(RandomRegistry::new());

#[kernel_symbols::export(
    name = "general.dev.random.register_backend",
    contract = "kernel.general.random-service@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED,
    retained_args = 1 << 0
)]
pub fn register_backend(
    backend: Arc<dyn RandomBackend>,
) -> Result<RandomBackendHandle, RandomServiceError> {
    let (handle, seed, seed_len, entropy_bits) = {
        let mut registry = RANDOM.lock();
        if registry.active.is_some() {
            return Err(RandomServiceError::AlreadyInstalled);
        }
        let handle = RandomBackendHandle(registry.next_id);
        registry.next_id = registry.next_id.wrapping_add(1).max(1);
        let seed = registry.boot_seed;
        let seed_len = registry.boot_seed_len;
        let entropy_bits = registry.boot_entropy_bits;
        registry.boot_seed_len = 0;
        registry.boot_entropy_bits = 0;
        registry.active = Some((handle, Arc::clone(&backend)));
        (handle, seed, seed_len, entropy_bits)
    };
    if seed_len != 0 {
        backend.add_entropy(&seed[..seed_len], entropy_bits);
        backend.reseed();
    }
    Ok(handle)
}

#[kernel_symbols::export(
    name = "general.dev.random.unregister_backend",
    contract = "kernel.general.random-service@1",
    version = 1,
    capabilities = kernel_symbols::capability::DEVICE_DRIVER,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn unregister_backend(handle: RandomBackendHandle) -> Result<(), RandomServiceError> {
    let mut registry = RANDOM.lock();
    match registry.active.as_ref() {
        None => Err(RandomServiceError::NotInstalled),
        Some((active, _)) if *active != handle => Err(RandomServiceError::InvalidHandle),
        Some(_) => {
            registry.active = None;
            Ok(())
        }
    }
}

fn backend() -> Option<Arc<dyn RandomBackend>> {
    RANDOM
        .lock()
        .active
        .as_ref()
        .map(|(_, backend)| Arc::clone(backend))
}

#[kernel_symbols::export(
    name = "general.dev.random.fill",
    contract = "kernel.general.random-service@1",
    version = 1,
    capabilities = kernel_symbols::capability::CORE_SAFE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn fill(output: &mut [u8], mode: RandomReadMode) -> Result<usize, CharIoError> {
    backend()
        .ok_or(CharIoError::Unavailable)?
        .read(output, mode)
}

pub fn add_bootloader_randomness(input: &[u8]) {
    if input.is_empty() {
        return;
    }
    let backend = {
        let mut registry = RANDOM.lock();
        if let Some((_, backend)) = registry.active.as_ref() {
            Some(Arc::clone(backend))
        } else {
            let available = BOOT_SEED_CAPACITY.saturating_sub(registry.boot_seed_len);
            let copied = available.min(input.len());
            let start = registry.boot_seed_len;
            registry.boot_seed[start..start + copied].copy_from_slice(&input[..copied]);
            registry.boot_seed_len += copied;
            registry.boot_entropy_bits = registry
                .boot_entropy_bits
                .saturating_add((copied as u64).saturating_mul(8));
            None
        }
    };
    if let Some(backend) = backend {
        backend.add_entropy(input, (input.len() as u64).saturating_mul(8));
        backend.reseed();
    }
}

pub fn entropy_bits() -> Result<u64, RandomServiceError> {
    backend()
        .map(|backend| backend.entropy_bits())
        .ok_or(RandomServiceError::NotInstalled)
}

pub struct RandomProxyDriver;
pub struct UrandomProxyDriver;

impl CharDriver for RandomProxyDriver {
    fn read(&self, output: &mut [u8]) -> Result<usize, CharIoError> {
        fill(output, RandomReadMode::Entropy { blocking: true })
    }

    fn write(&self, input: &[u8]) -> Result<usize, CharIoError> {
        backend().ok_or(CharIoError::Unavailable)?.write(input)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl CharDriver for UrandomProxyDriver {
    fn read(&self, output: &mut [u8]) -> Result<usize, CharIoError> {
        fill(output, RandomReadMode::Insecure)
    }

    fn write(&self, input: &[u8]) -> Result<usize, CharIoError> {
        backend().ok_or(CharIoError::Unavailable)?.write(input)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub static RANDOM_PROXY_DRIVER: RandomProxyDriver = RandomProxyDriver;
pub static URANDOM_PROXY_DRIVER: UrandomProxyDriver = UrandomProxyDriver;
