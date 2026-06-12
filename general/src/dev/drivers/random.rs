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
//!     /dev/random  ── read：熵不足挂 WaitQueue 睡眠等待
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
use crate::dev::pnp::PnpError;
use sched::WaitQueue;

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

/// ChaCha20 输出 64 字节；每输出 1 MiB 后重新派生一次密钥流状态。
const CHACHA20_BLOCK: usize = 64;
const RESEED_BYTES: u64 = 1u64 << 20;

/// 用户态 `write(/dev/{,u}random, ...)` 注入字节的保守熵密度。
/// 设为 1 bit/byte：攻击者可以写自己，加少量熵不影响池子，但也不会被
/// 滥用来"伪造"大量熵。
const USER_WRITE_BITS_PER_BYTE: u64 = 1;

/// 调度器尚未 ready 的极早期路径无法睡眠，只能短暂 spin 等待。
/// 正常 `/dev/random` read 路径必须走 WaitQueue，不应忙等占用 CPU。
const RANDOM_WAIT_RETRIES: usize = 4096;
/// 自旋锁长时间争用时触发一次 best-effort yield 的门槛。
const RANDOM_LOCK_SPIN_YIELD_LIMIT: usize = 10_000_000;
/// 启动期采样缓冲区大小，容纳时间戳、栈指针和 self 地址提示。
const STARTUP_SEED_BYTES: usize = 64;
/// 启动期连续采几次 EntropySource 样本，靠时间差分增加不可复现性。
const STARTUP_SOURCE_SAMPLES: usize = 2;

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
            if spins > RANDOM_LOCK_SPIN_YIELD_LIMIT {
                // 与 uart 行为一致：长时间争用时让出调度。
                sched_yield_best_effort();
                spins = 0;
            } else {
                core::hint::spin_loop();
            }
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

/// 16 × u64 状态的熵池。
///
/// 状态本身是公开的 `s`，但只有 `mix_*` 知道怎么把外部字节喂进去；
/// 直接访问 `state` 是私有 API。
struct EntropyPool {
    /// 池内 16 个 64-bit 字，按小端 `u64` 混入外部字节。
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
    /// 混合步骤：
    ///   1. 每 8 字节折叠成 `u64`；
    ///   2. 在状态字之间做 (rotate-left, add, xor) 三角链；
    ///   3. 剩余 < 8 字节折叠到 state[0]；
    ///   4. 末尾再 "tap" 一遍以增加扩散。
    fn mix(&mut self, mut input: &[u8]) {
        while input.len() >= 8 {
            let (word, rest) = input.split_at(core::mem::size_of::<u64>());
            let mut bytes = [0u8; core::mem::size_of::<u64>()];
            bytes.copy_from_slice(word);
            let w = u64::from_le_bytes(bytes);
            input = rest;
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
        for (slot, chunk) in state[4..12].iter_mut().zip(key.chunks_exact(4)) {
            let mut bytes = [0u8; core::mem::size_of::<u32>()];
            bytes.copy_from_slice(chunk);
            *slot = u32::from_le_bytes(bytes);
        }
        // state[12] = counter = 0
        for (slot, chunk) in state[13..16].iter_mut().zip(nonce.chunks_exact(4)) {
            let mut bytes = [0u8; core::mem::size_of::<u32>()];
            bytes.copy_from_slice(chunk);
            *slot = u32::from_le_bytes(bytes);
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
    /// `/dev/random` 等待真实 entropy credit 的睡眠队列。
    ///
    /// 注意：队列只表示“可能有新 credit”，唤醒后仍必须重新检查并在池锁
    /// 下扣减。这样多个 reader 同时被唤醒时不会重复消费同一份熵估计。
    entropy_wait: WaitQueue,
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
            entropy_wait: WaitQueue::new(),
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
    /// - `entropy_bits`：调用方显式声明本次最多可记入的熵 bit 数。
    ///
    /// 安全语义：mix 和 credit 分离。timestamp、地址、用户可控字节都可以
    /// 混入池子扰动状态，但只有调用方明确给出的 `entropy_bits` 会影响
    /// `/dev/random` 的阻塞条件。
    fn add_input(&self, data: &[u8], entropy_bits: u64) {
        if data.is_empty() {
            return;
        }
        self.total_add_calls.fetch_add(1, Ordering::Relaxed);
        let credited_bits = entropy_bits.min((data.len() as u64).saturating_mul(8));
        {
            let mut pool = self.pool.lock();
            pool.mix(data);
            pool.bytes_added = pool.bytes_added.saturating_add(data.len() as u64);
            pool.credit(credited_bits);
        }
        if credited_bits != 0 {
            self.wake_entropy_waiters();
        }
    }

    fn wake_entropy_waiters(&self) {
        self.entropy_wait.wake_all();
    }

    fn try_debit_entropy(&self, bits: u64) -> bool {
        let mut pool = self.pool.lock();
        if pool.estimated_entropy_bits < bits {
            return false;
        }
        pool.debit(bits);
        true
    }

    fn wait_and_debit_entropy(&self, bits: u64, blocking: bool) -> bool {
        loop {
            if self.try_debit_entropy(bits) {
                return true;
            }
            if !blocking {
                return false;
            }
            self.wait_for_entropy(bits);
        }
    }

    /// 估算可用熵。
    fn estimated_entropy_bits(&self) -> u64 {
        self.pool.lock().estimated_entropy_bits
    }

    /// 阻塞到至少有 `bits` 可用熵。
    fn wait_for_entropy(&self, bits: u64) {
        if bits == 0 || self.estimated_entropy_bits() >= bits {
            return;
        }

        if sched::is_ready() {
            loop {
                if self.estimated_entropy_bits() >= bits {
                    return;
                }
                let task = sched::current_task();
                self.entropy_wait
                    .prepare_to_wait(&task, sched::TaskState::Sleeping);
                if self.estimated_entropy_bits() >= bits {
                    self.entropy_wait.finish_wait(&task);
                    return;
                }
                drop(task);
                sched::schedule_once(sched::now_ns_public());
                let task = sched::current_task();
                self.entropy_wait.finish_wait(&task);
            }
        }

        // 调度器启动前没有 current task 可挂队列，只能保留极早期兼容兜底；
        // 调度器 ready 后的 `/dev/random` read 不会走到这里。
        while self.estimated_entropy_bits() < bits {
            for _ in 0..RANDOM_WAIT_RETRIES {
                core::hint::spin_loop();
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
    /// `blocking == true`：熵不足时挂 WaitQueue 阻塞等待。
    /// `blocking == false`：立刻返回 0。
    fn read_blocking(&self, buf: &mut [u8], blocking: bool) -> usize {
        if buf.is_empty() {
            return 0;
        }

        let mut done = 0usize;
        while done < buf.len() {
            // 熵池最多只记 POOL_BITS；大读按池容量分段等待，避免要求一个
            // 永远不可能同时满足的 credit 数。
            let chunk = (buf.len() - done).min(POOL_BYTES);
            let need_bits = (chunk as u64).saturating_mul(8);
            if !self.wait_and_debit_entropy(need_bits, blocking) {
                break;
            }
            self.crng_fill(&mut buf[done..done + chunk]);
            done += chunk;
        }
        done
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
        let entropy_bits = (buf.len() as u64).saturating_mul(USER_WRITE_BITS_PER_BYTE);
        self.add_input(buf, entropy_bits);
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
/// `n_bits` 是调用方对本次采样的最佳熵估计，按本次 buffer 长度 clamp；
/// 不再按 byte 向上折算，避免 1 bit jitter 被记成 8 bit。
pub fn add_hw_randomness(data: &[u8], n_bits: u64) {
    if data.is_empty() {
        return;
    }
    random_core().add_input(data, n_bits);
}

/// 添加启动期"已知熵"——这种来源调用方声称自己已验证熵密度为 100%。
/// 适用于 bootloader 提供的随机种子、rdrand 输出、tpm2 pcr digest 等。
pub fn add_bootloader_randomness(data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let entropy_bits = (data.len() as u64).saturating_mul(8);
    random_core().add_input(data, entropy_bits);
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

// 字符设备驱动不需要 PnP factory；这里提供 `register_builtin_driver` 以满足
// `drivers::register_builtin_drivers()` 调用约定。`/dev/random` 和
// `/dev/urandom` 的路径发布由 VFS device_files 层负责，不能回流到驱动层。
/// 注册 random 子系统。
///
/// 内部会：
///   1. 喂一次启动期样本（时间戳 + 栈地址 + self 地址），并按熵源显式
///      credit reseed 一次。
///   2. 标记 `first_seed_done`，供诊断。
pub fn register_builtin_driver() -> Result<(), PnpError> {
    seed_from_startup();
    Ok(())
}

/// 启动期喂熵 + 首次 reseed。
///
/// 设计原则：使用任何"在启动时容易拿到、攻击者难以复现"的来源。
/// - 时间戳（rdtime）
/// - 栈指针（地址空间布局随机化的代理）
/// - 熵源实现愿意提供的其它运行时 hint
///
/// 所有这些都通过 [`crate::dev::random_source`] 的 `EntropySource` 取得，
/// 避免 `general` 层出现 `cfg(target_arch = ...)` 的内联汇编。
fn seed_from_startup() {
    let mut buf = [0u8; STARTUP_SEED_BYTES];
    let mut pos = 0usize;
    let mut entropy_bits = 0u64;

    if let Some(src) = crate::dev::random_source::installed_source() {
        for _ in 0..STARTUP_SOURCE_SAMPLES {
            if pos >= buf.len() {
                break;
            }
            let sample = src.sample_with_credit(&mut buf[pos..]);
            let n = sample.bytes_written.min(buf.len() - pos);
            if n == 0 {
                break;
            }
            entropy_bits =
                entropy_bits.saturating_add(sample.entropy_bits.min((n as u64).saturating_mul(8)));
            pos += n;
        }
    } else {
        // 没有 arch 熵源：跳过硬件项，只靠用户态 write + reseed。
        // 这保持“无可信熵时不解除阻塞随机读”的内部不变量。
    }

    // 启动 hint 可以 mix，但默认不 credit。只有 EntropySource 通过
    // sample_with_credit() 明确声明的 bit 数才会解除 `/dev/random` 等待。
    random_core().add_input(&buf[..pos], entropy_bits);
    random_core().reseed_locked();

    random_core().first_seed_done.store(true, Ordering::Release);
}

/// 通过注入的 `EntropySource` 取得时间戳（仅在 `with_source` 上下文里
/// 使用；arch 熵源未注册时返回 0，random 仍能工作但熵较弱）。
#[inline]
fn read_timestamp() -> u64 {
    crate::dev::random_source::with_source(|src| src.timestamp()).unwrap_or(0)
}
