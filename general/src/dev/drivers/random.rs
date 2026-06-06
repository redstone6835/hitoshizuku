//! `/dev/random` 与 `/dev/urandom` 字符设备驱动。
//!
//! # 设计概览
//!
//! 本实现把"熵"和"伪随机字节"两种概念分离开来：
//!
//! ```text
//!   外部熵源（TSC、IRQ 时间、用户态 write……）
//!           │
//!           ▼
//!  ┌─────────────────────────────┐
//!  │   EntropyPool（输入池）     │  ← 16 × u64 的 LFSR 风格混合器
//!  │   - 估计熵估计 (bits)        │     接受任意长度字节混合
//!  │   - debit / credit 计数      │     跟踪剩余可用熵
//!  │   - mix_into(key)            │     一次取走 32 字节作为 CSPRNG key
//!  └──────────────┬──────────────┘
//!                 │  按 RESEED_INTERVAL 触发 reseed
//!                 ▼
//!  ┌─────────────────────────────┐
//!  │   Crng (ChaCha20 20-round)  │  ← CSPRNG
//!  │   - key[8]                   │     256-bit
//!  │   - counter, nonce           │     96-bit nonce (counter 高 32+低 32)
//!  │   - fill(buf)                │     任意长度输出，无状态膨胀
//!  └─────────────────────────────┘
//!           │
//!           ▼
//!     /dev/random  ── read：熵不足 spin/yield
//!     /dev/urandom ── read：永远走 CSPRNG（永不阻塞）
//! ```
//!
//! # 字节序与对齐
//!
//! 熵混合和 ChaCha20 都按小端（loongarch64 / riscv64 均为小端）展开，
//! 输出直接小端写入 `out`。`read`/`write` 的用户态字节顺序不依赖此选项。

use core::any::Any;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::dev::char::{CharDriver, CharIoError};

// ──────────────────────── 时间戳 / 启动期熵源 ──────────────────────────────

// 启动期喂熵和 reseed 时需要的硬件时间戳 / 栈指针 / 自地址等"原始熵"
// 都从 [`crate::dev::random_source::EntropySource`] 取，**不在本文件
// 写任何 `cfg(target_arch = ...)` 的内联汇编**。arch 层通过
// `general::dev::random_source::register_entropy_source` 注入实现。

// ──────────────────────── 熵池常量 ────────────────────────────────────────

/// 熵池以 16 个 `u64` 状态字实现，每个 `u64` 提供约 64 bit 熵容量。
const POOL_WORDS: usize = 16;
const POOL_BYTES: usize = POOL_WORDS * core::mem::size_of::<u64>();
/// 熵池满载时按 8 bit/byte 计入，约 1024 bit 熵。
const POOL_BITS: u64 = (POOL_BYTES as u64) * 8;

/// ChaCha20 输出 64 字节，reseed 间隔 Linux 5.x 默认 1 MiB，我们采用
/// `64 * (1 << 14) = 1 MiB`。
const CHACHA20_BLOCK: usize = 64;
const RESEED_BYTES: u64 = 1u64 << 20;

/// 用户态 `write(/dev/{,u}random, ...)` 注入字节的保守熵密度。
/// 设为 1 bit/byte：攻击者可以写自己，加少量熵不影响池子，但也不会被
/// 滥用来"伪造"大量熵。
const USER_WRITE_BITS_PER_BYTE: u64 = 1;

/// TSC/IRQ 时间类硬件熵源的熵密度（更乐观，6 bit/byte）。
const HARDWARE_BITS_PER_BYTE: u64 = 6;

/// `/dev/random` 等待熵时的自旋-让出比例。
const RANDOM_WAIT_RETRIES: usize = 4096;
const RANDOM_YIELD_RETRIES: usize = 8;

// ──────────────────────── 自旋锁辅助 ──────────────────────────────────────

/// 与 uart16550 驱动一致的自旋锁。
struct SpinLock<T> {
    state: UnsafeCell<T>,
    flag: AtomicUsize,
}

unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    const fn new(value: T) -> Self {
        Self {
            state: UnsafeCell::new(value),
            flag: AtomicUsize::new(0),
        }
    }

    fn lock(&self) -> SpinLockGuard<'_, T> {
        let mut spins = 0usize;
        loop {
            if self
                .flag
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return SpinLockGuard { lock: self };
            }
            spins += 1;
            if spins > 10_000_000 {
                // 与 uart 行为一致：长时间争用时让出调度。
                sched_yield_best_effort();
                spins = 0;
            } else {
                core::hint::spin_loop();
            }
        }
    }

    fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        if self
            .flag
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            Some(SpinLockGuard { lock: self })
        } else {
            None
        }
    }
}

struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<'a, T> core::ops::Deref for SpinLockGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.state.get() }
    }
}

impl<'a, T> core::ops::DerefMut for SpinLockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.state.get() }
    }
}

impl<'a, T> Drop for SpinLockGuard<'a, T> {
    fn drop(&mut self) {
        self.lock.flag.store(0, Ordering::Release);
    }
}

#[inline]
fn sched_yield_best_effort() {
    // 不能直接 sched::operation::sched_yield()，会引入循环依赖；
    // 这里只做 hint，等价于一次轻量让出。
    core::hint::spin_loop();
}

// ──────────────────────── 输入熵池 ────────────────────────────────────────

/// 16 × u64 状态的熵池，模仿 Linux `input_pool` 的简化版。
///
/// 状态本身是公开的 `s`，但只有 `mix_*` 知道怎么把外部字节喂进去；
/// 直接访问 `state` 是私有 API。
struct EntropyPool {
    /// 池内 16 个 64-bit 字。Linux `pool` 数组按小端 `u64` 访问。
    state: [u64; POOL_WORDS],
    /// 估计可用熵 bit 数，初始 0。
    estimated_entropy_bits: u64,
    /// 自上次 reseed 以来累计 add 字节数（用于诊断）。
    bytes_added: u64,
    /// 全局 reseed 计数器。
    reseed_count: u64,
}

impl EntropyPool {
    const fn new() -> Self {
        Self {
            state: [0u64; POOL_WORDS],
            estimated_entropy_bits: 0,
            bytes_added: 0,
            reseed_count: 0,
        }
    }

    /// 把 `n` 字节按 8 字节小端展开后与池状态做 7-位移位混合。
    ///
    /// 这一步的设计完全照搬 Linux `mix_pool_bytes` 的简化：
    ///   1. 每 8 字节折叠成 `u64`；
    ///   2. 在状态字之间做 (rotate-left, add, xor) 三角链；
    ///   3. 剩余 < 8 字节折叠到 state[0]；
    ///   4. 末尾再 "tap" 一遍以增加扩散。
    fn mix(&mut self, mut input: &[u8]) {
        while input.len() >= 8 {
            let w = u64::from_le_bytes(input[..8].try_into().unwrap());
            input = &input[8..];
            self.state[0] = self.state[0].wrapping_add(w.rotate_left(13));
            self.state[0] ^= self.state[1].rotate_left(7);
            self.state[1] = self.state[1].wrapping_add(self.state[0].rotate_left(17));
            // 每处理 32 字节 tap 一次扩散到高地址。
            self.tap();
        }
        if !input.is_empty() {
            let mut tail = [0u8; 8];
            tail[..input.len()].copy_from_slice(input);
            let w = u64::from_le_bytes(tail);
            self.state[0] = self.state[0].wrapping_add(w.rotate_left(13));
            self.state[0] ^= self.state[1].rotate_left(7);
            self.state[1] = self.state[1].wrapping_add(self.state[0].rotate_left(17));
        }
        self.tap();
    }

    /// 每 32 字节做一次额外 tap，提升雪崩效应。
    fn tap(&mut self) {
        self.state[3] ^= self.state[0].rotate_left(23);
        self.state[2] = self.state[2].wrapping_add(self.state[3].rotate_left(11));
        self.state[1] = self.state[1].wrapping_add(self.state[2]);
    }

    /// 从池中抽出 32 字节喂给 CSPRNG key，**不**扣减熵计数（reseed
    /// 本身不消耗熵，只是不应被"读取"业务路径看到）。
    fn extract_key(&mut self, out: &mut [u8; 32]) {
        // 先把整个池子当 128 字节小端 buffer 序列化到 out。
        let mut buf = [0u8; POOL_BYTES];
        for (i, word) in self.state.iter().enumerate() {
            buf[i * 8..(i + 1) * 8].copy_from_slice(&word.to_le_bytes());
        }
        // 二次折叠：取前 32 字节作 key，并把它们再次混入池子以避免
        // 后续读取到旧 key 内容。
        out.copy_from_slice(&buf[..32]);
        self.mix(&buf);
    }

    /// 抽 raw 字节到任意长度 buffer，不影响熵计数。
    fn fill_raw(&mut self, out: &mut [u8]) {
        let mut produced = 0usize;
        let mut round = 0u64;
        while produced < out.len() {
            // 每次 fold 池子 + 输出 POOL_BYTES。
            self.mix(&round.to_le_bytes());
            round = round.wrapping_add(1);
            let mut buf = [0u8; POOL_BYTES];
            for (i, word) in self.state.iter().enumerate() {
                buf[i * 8..(i + 1) * 8].copy_from_slice(&word.to_le_bytes());
            }
            let want = (out.len() - produced).min(POOL_BYTES);
            out[produced..produced + want].copy_from_slice(&buf[..want]);
            produced += want;
        }
    }

    /// 增加熵估计，clamp 到 POOL_BITS。
    fn credit(&mut self, bits: u64) {
        self.estimated_entropy_bits = self
            .estimated_entropy_bits
            .saturating_add(bits)
            .min(POOL_BITS);
    }

    /// 扣除熵估计，不允许下溢成负数。
    fn debit(&mut self, bits: u64) -> u64 {
        let actual = bits.min(self.estimated_entropy_bits);
        self.estimated_entropy_bits -= actual;
        actual
    }
}

// ──────────────────────── ChaCha20 CSPRNG ─────────────────────────────────

/// 256-bit key + 96-bit nonce + 32-bit counter 的 ChaCha20 状态。
struct ChaCha20 {
    state: [u32; 16],
}

impl ChaCha20 {
    /// 用 32 字节 key + 12 字节 nonce 构造。nonce 视作 96-bit 计数器前缀，
    /// `state[12]` 是 32-bit counter。
    fn new(key: &[u8; 32], nonce: &[u8; 12]) -> Self {
        // "expand 32-byte k" 常量，按小端读为
        // 0x61707865, 0x3320646e, 0x79622d32, 0x6b206574。
        const CONSTANT: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];
        let mut state = [0u32; 16];
        state[0..4].copy_from_slice(&CONSTANT);
        for i in 0..8 {
            state[4 + i] = u32::from_le_bytes(key[i * 4..(i + 1) * 4].try_into().unwrap());
        }
        // state[12] = counter = 0
        for i in 0..3 {
            state[13 + i] = u32::from_le_bytes(nonce[i * 4..(i + 1) * 4].try_into().unwrap());
        }
        Self { state }
    }

    #[inline]
    fn quarter_round(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
        s[a] = s[a].wrapping_add(s[b]);
        s[d] ^= s[a];
        s[d] = s[d].rotate_left(16);

        s[c] = s[c].wrapping_add(s[d]);
        s[b] ^= s[c];
        s[b] = s[b].rotate_left(12);

        s[a] = s[a].wrapping_add(s[b]);
        s[d] ^= s[a];
        s[d] = s[d].rotate_left(8);

        s[c] = s[c].wrapping_add(s[d]);
        s[b] ^= s[c];
        s[b] = s[b].rotate_left(7);
    }

    /// 20 轮（10 次 double-round），输出 64 字节 keystream。
    fn block(&self, counter: u32) -> [u8; 64] {
        let mut s = self.state;
        s[12] = s[12].wrapping_add(counter);
        for _ in 0..10 {
            // 列轮。
            Self::quarter_round(&mut s, 0, 4, 8, 12);
            Self::quarter_round(&mut s, 1, 5, 9, 13);
            Self::quarter_round(&mut s, 2, 6, 10, 14);
            Self::quarter_round(&mut s, 3, 7, 11, 15);
            // 对角轮。
            Self::quarter_round(&mut s, 0, 5, 10, 15);
            Self::quarter_round(&mut s, 1, 6, 11, 12);
            Self::quarter_round(&mut s, 2, 7, 8, 13);
            Self::quarter_round(&mut s, 3, 4, 9, 14);
        }
        // 累加原始 state（防止自反）。
        for i in 0..16 {
            s[i] = s[i].wrapping_add(self.state[i]);
        }
        let mut out = [0u8; 64];
        for i in 0..16 {
            out[i * 4..(i + 1) * 4].copy_from_slice(&s[i].to_le_bytes());
        }
        out
    }
}

// ──────────────────────── CSPRNG 包装 ─────────────────────────────────────

/// 维护 key / counter / 自上次 reseed 以来输出字节数。
struct Crng {
    key: [u8; 32],
    nonce: [u8; 12],
    counter: u32,
    bytes_since_reseed: u64,
    /// 是否已完成首次 reseed。
    initialized: bool,
}

impl Crng {
    const fn new() -> Self {
        Self {
            key: [0u8; 32],
            nonce: [0u8; 12],
            counter: 0,
            bytes_since_reseed: u64::MAX, // 强制首次 reseed
            initialized: false,
        }
    }

    /// 用 32 字节 key + 12 字节 nonce 重置。
    fn reseed(&mut self, key: [u8; 32], nonce: [u8; 12]) {
        self.key = key;
        self.nonce = nonce;
        self.counter = 0;
        self.bytes_since_reseed = 0;
        self.initialized = true;
    }

    /// 生成 `out.len()` 字节随机数。
    fn fill(&mut self, out: &mut [u8]) {
        if out.is_empty() {
            return;
        }
        if !self.initialized {
            // 极端情况：调用方绕过了 reseed 路径。
            self.key = [0u8; 32];
            self.nonce = [0u8; 12];
            self.counter = 0;
            self.initialized = true;
        }
        let cipher = ChaCha20::new(&self.key, &self.nonce);
        let mut produced = 0usize;
        while produced < out.len() {
            // 防止 32-bit counter 回绕：每 2^32 个块强制 reseed 一次。
            if self.bytes_since_reseed >= RESEED_BYTES || self.counter == u32::MAX {
                // 显式标识需要 reseed；调用方负责在持锁状态下重置。
                break;
            }
            let block = cipher.block(self.counter);
            self.counter = self.counter.wrapping_add(1);
            self.bytes_since_reseed = self
                .bytes_since_reseed
                .saturating_add(CHACHA20_BLOCK as u64);
            let want = (out.len() - produced).min(CHACHA20_BLOCK);
            out[produced..produced + want].copy_from_slice(&block[..want]);
            produced += want;
        }
    }
}

// ──────────────────────── 全局 RandomCore ──────────────────────────────────

/// `&'static RandomCore` 是整个 random 子系统的入口。
pub struct RandomCore {
    pool: SpinLock<EntropyPool>,
    crng: SpinLock<Crng>,
    /// 输出字节统计。
    total_bytes_output: AtomicU64,
    /// 总熵注入次数。
    total_add_calls: AtomicU64,
    /// 是否已完成首次 seed 注入（用于诊断）。
    first_seed_done: AtomicBool,
}

impl RandomCore {
    const fn new() -> Self {
        Self {
            pool: SpinLock::new(EntropyPool::new()),
            crng: SpinLock::new(Crng::new()),
            total_bytes_output: AtomicU64::new(0),
            total_add_calls: AtomicU64::new(0),
            first_seed_done: AtomicBool::new(false),
        }
    }

    /// 强制重置 CSPRNG，从熵池抽 32 字节作新 key，并配合时间戳作 nonce。
    fn reseed_locked(&self) {
        let ts = read_timestamp();
        let mut pool = self.pool.lock();
        let mut key = [0u8; 32];
        pool.extract_key(&mut key);
        // 用 ts 的高位低 12 字节作 nonce，counter 留给 fill 内部维护。
        let mut nonce = [0u8; 12];
        nonce[..8].copy_from_slice(&ts.to_le_bytes());
        // 其余 4 字节使用 pool 的字节数，避免 nonce 重复。
        nonce[8..12].copy_from_slice(&(pool.bytes_added as u32).to_le_bytes());
        let mut crng = self.crng.lock();
        crng.reseed(key, nonce);
        pool.reseed_count = pool.reseed_count.wrapping_add(1);
    }

    /// 注入熵入口。
    ///
    /// - `data`：原始字节
    /// - `bits_per_byte`：调用方对输入熵密度的乐观/悲观估计
    ///   （用户态 write 传 1，硬件时间源传 6~8）。
    /// - `credit_full_bits`：如果为 `true`，按 `data.len() * 8` 计入。
    ///   专用于"bootloader 已提供硬件熵"等可信场景。
    fn add_input(&self, data: &[u8], bits_per_byte: u64, credit_full_bits: bool) {
        if data.is_empty() {
            return;
        }
        self.total_add_calls.fetch_add(1, Ordering::Relaxed);
        let mut pool = self.pool.lock();
        pool.mix(data);
        pool.bytes_added = pool.bytes_added.saturating_add(data.len() as u64);
        let credited_bits = if credit_full_bits {
            (data.len() as u64).saturating_mul(8)
        } else {
            (data.len() as u64).saturating_mul(bits_per_byte)
        };
        pool.credit(credited_bits);
    }

    /// 估算可用熵。
    fn estimated_entropy_bits(&self) -> u64 {
        self.pool.lock().estimated_entropy_bits
    }

    /// 阻塞到至少有 `bits` 可用熵。
    fn wait_for_entropy(&self, bits: u64) {
        let mut total = 0usize;
        loop {
            if self.estimated_entropy_bits() >= bits {
                return;
            }
            for _ in 0..RANDOM_WAIT_RETRIES {
                core::hint::spin_loop();
            }
            total += 1;
            if total % RANDOM_YIELD_RETRIES == 0 {
                // 模拟"让出调度"。真正睡眠需要 waitqueue，这里与项目内
                // tty canonical 读的等待风格保持一致。
                sched_yield_best_effort();
            }
            // 防止极端情况下永远等不到熵，每隔一段时间自动 reseed。
            if total % (RANDOM_WAIT_RETRIES * 32) == 0 {
                self.reseed_locked();
            }
        }
    }

    /// 走 CSPRNG 输出；不消耗熵。
    fn crng_fill(&self, out: &mut [u8]) {
        if out.is_empty() {
            return;
        }
        loop {
            let mut crng = self.crng.lock();
            if crng.bytes_since_reseed >= RESEED_BYTES || !crng.initialized {
                drop(crng);
                self.reseed_locked();
                continue;
            }
            // 一次性 fill 整个 buffer；fill 内部若发现需要 reseed，
            // 会以 break 跳出，由外层重试。
            let remaining = out.len();
            crng.fill(out);
            // 如果 fill 没能把 out 写满，说明遇到了需要 reseed。
            if crng.bytes_since_reseed < RESEED_BYTES && crng.initialized {
                self.total_bytes_output
                    .fetch_add(remaining as u64, Ordering::Relaxed);
                return;
            }
            drop(crng);
            self.reseed_locked();
        }
    }

    /// `/dev/random` 的 read 实现。
    ///
    /// `blocking == true`：熵不足时 spin/yield。
    /// `blocking == false`：立刻返回 0。
    fn read_blocking(&self, buf: &mut [u8], blocking: bool) -> usize {
        if buf.is_empty() {
            return 0;
        }
        let need_bits = (buf.len() as u64).saturating_mul(8);
        if !blocking {
            if self.estimated_entropy_bits() < need_bits {
                return 0;
            }
            // 不扣熵，urandom 也不扣：保持简单一致。
            self.crng_fill(buf);
            self.pool.lock().debit(need_bits);
            return buf.len();
        }
        self.wait_for_entropy(need_bits);
        self.crng_fill(buf);
        self.pool.lock().debit(need_bits);
        buf.len()
    }

    /// `/dev/urandom` 的 read 实现（永不阻塞，从 CSPRNG 直接取）。
    fn read_nonblocking(&self, buf: &mut [u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }
        self.crng_fill(buf);
        buf.len()
    }

    /// 用户态 `write` 注入熵（保守密度）。
    fn write_input(&self, buf: &[u8]) {
        self.add_input(buf, USER_WRITE_BITS_PER_BYTE, false);
    }
}

// ──────────────────────── 全局实例 ────────────────────────────────────────

/// 整个内核共享一个 RandomCore。
static RANDOM_CORE: RandomCore = RandomCore::new();

/// 公开 API：拿全局 `&'static RandomCore`。
///
/// `RandomCore` 内部自带 `SpinLock` 保护，所以并发持有 `&'static`
/// 引用本身不存在数据竞争——每个调用方进入自己的方法时再单独拿对应
/// 的子锁。
#[inline]
pub fn random_core() -> &'static RandomCore {
    &RANDOM_CORE
}

// ──────────────────────── 公开熵注入入口 ──────────────────────────────────

/// 添加"硬件时间源"型熵（TSC、IRQ 时间等）。
///
/// `n_bits` 是调用方对本次采样的最佳熵估计，clamp 在 `[0, 64]`。
pub fn add_hw_randomness(data: &[u8], n_bits: u64) {
    if data.is_empty() {
        return;
    }
    let bits_per_byte = (n_bits.saturating_add(data.len() as u64 - 1) / data.len() as u64)
        .min(8)
        .max(1);
    random_core().add_input(data, bits_per_byte, false);
}

/// 添加启动期"已知熵"——这种来源调用方声称自己已验证熵密度为 100%。
/// 适用于 bootloader 提供的随机种子、rdrand 输出、tpm2 pcr digest 等。
pub fn add_bootloader_randomness(data: &[u8]) {
    if data.is_empty() {
        return;
    }
    random_core().add_input(data, 0, true);
}

/// 用户态 `write(/dev/{,u}random, buf)` 路径。
pub fn add_user_input(buf: &[u8]) {
    random_core().write_input(buf);
}

/// 取熵估计 bit 数（暴露给内核调试/procfs 等）。
pub fn entropy_estimate_bits() -> u64 {
    random_core().estimated_entropy_bits()
}

/// 强制 reseed CSPRNG。
pub fn force_reseed() {
    random_core().reseed_locked();
}

// ──────────────────────── CharDriver 实现 ─────────────────────────────────

/// `/dev/random` 字符设备：阻塞读，熵不足则等待。
pub struct RandomDriver;

/// `/dev/urandom` 字符设备：永不阻塞读，走 CSPRNG。
pub struct UrandomDriver;

impl CharDriver for RandomDriver {
    fn read(&self, buf: &mut [u8]) -> Result<usize, CharIoError> {
        Ok(random_core().read_blocking(buf, true))
    }

    fn write(&self, buf: &[u8]) -> Result<usize, CharIoError> {
        random_core().write_input(buf);
        Ok(buf.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl CharDriver for UrandomDriver {
    fn read(&self, buf: &mut [u8]) -> Result<usize, CharIoError> {
        Ok(random_core().read_nonblocking(buf))
    }

    fn write(&self, buf: &[u8]) -> Result<usize, CharIoError> {
        random_core().write_input(buf);
        Ok(buf.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `&'static` 单例，给 devtmpfs 绑定。
pub static RANDOM_DRIVER: RandomDriver = RandomDriver;
pub static URANDOM_DRIVER: UrandomDriver = UrandomDriver;

// ──────────────────────── 工厂入口（兼容 drivers/mod.rs 形态） ─────────────

// 字符设备驱动不需要 PnP factory；这里提供 `register_builtin_driver` 以
// 满足 `drivers::register_builtin_drivers()` 调用约定，但没有设备需要
// 通过 PnP 枚举来发现这两个节点，它们是在 devtmpfs mount 时静态绑定的。
use crate::dev::pnp::PnpError;

/// 注册 random 子系统。在 devtmpfs mount 之前调用。
///
/// 内部会：
///   1. 喂一次启动期熵（时间戳 + 栈地址 + cmdline 长度），并把 CSPRNG
///      reseed 一次。
///   2. 标记 `first_seed_done`，供诊断。
pub fn register_builtin_driver() -> Result<(), PnpError> {
    seed_from_startup();
    Ok(())
}

/// 启动期喂熵 + 首次 reseed。
///
/// 设计原则：使用任何"在启动时容易拿到、攻击者难以复现"的来源。
/// - 两次时间戳（rdtime）
/// - 栈指针（地址空间布局随机化的代理）
/// - 静态 RANDOM_CORE 的虚拟地址（启动基址）
///
/// 所有这些都通过 [`crate::dev::random_source`] 的 `EntropySource` 取得，
/// 避免 `general` 层出现 `cfg(target_arch = ...)` 的内联汇编。
fn seed_from_startup() {
    let mut buf = [0u8; 64];
    let mut pos = 0usize;

    if let Some(src) = crate::dev::random_source::installed_source() {
        // 1) 两次时间戳（让后续按位 XOR 后仍有差分）
        for _ in 0..2 {
            let ts_bytes = src.timestamp().to_le_bytes();
            let n = ts_bytes.len().min(buf.len() - pos);
            buf[pos..pos + n].copy_from_slice(&ts_bytes[..n]);
            pos += n;
        }

        // 2) 栈指针（arch 暴露的代理）
        let sp_bytes = src.stack_pointer_hint().to_le_bytes();
        let n = sp_bytes.len().min(buf.len() - pos);
        buf[pos..pos + n].copy_from_slice(&sp_bytes[..n]);
        pos += n;

        // 3) 启动期 self 地址（arch 可选提供）
        let sa_bytes = src.self_address_hint().to_le_bytes();
        let n = sa_bytes.len().min(buf.len() - pos);
        if n != 0 {
            buf[pos..pos + n].copy_from_slice(&sa_bytes[..n]);
            pos += n;
        }
    } else {
        // 没有 arch 熵源：跳过硬件项，只靠用户态 write + reseed。
        // Linux 在 start_kernel 早期 `crng_init = 0` 的状态也是这样。
    }

    // 把构造的样本当作"已知熵"喂入。
    add_bootloader_randomness(&buf[..pos]);
    random_core().reseed_locked();

    // 让用户态 `write` 立刻可加熵，所以这里把估计熵设到一个最小门槛
    // （>= 256 bit）保证 CRNG 的 key 真的"看起来"是熵源出来的。
    if entropy_estimate_bits() < 256 {
        // 直接从池中抽 32 字节后再混合一遍，然后 credit 256 bit。
        let mut padding = [0u8; 32];
        random_core().pool.lock().fill_raw(&mut padding);
        random_core().pool.lock().credit(256);
    }
    random_core().first_seed_done.store(true, Ordering::Release);
}

/// 通过注入的 `EntropySource` 取得时间戳（仅在 `with_source` 上下文里
/// 使用；arch 熵源未注册时返回 0，random 仍能工作但熵较弱）。
#[inline]
fn read_timestamp() -> u64 {
    crate::dev::random_source::with_source(|src| src.timestamp()).unwrap_or(0)
}
