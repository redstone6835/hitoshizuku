//! RISC-V64 平台熵源。
//!
//! 基于 rdtime jitter + 栈/返回地址混合，提供低质量伪熵。
//! 不具备密码学安全性，仅用于内核早期初始化（ASLR seed、初始 RNG state）。
//! 如果硬件支持 Zkr 扩展（`seed` CSR），应优先使用硬件真随机数。

use general::dev::random_source::{EntropySource, register_entropy_source};

struct Riscv64EntropySource;

impl EntropySource for Riscv64EntropySource {
    fn timestamp(&self) -> u64 {
        super::specific::kernel_timestamp_ns()
    }

    fn stack_pointer_hint(&self) -> u64 {
        let sp: u64;
        unsafe { core::arch::asm!("mv {}, sp", out(reg) sp, options(nomem, nostack)) };
        sp
    }

    fn self_address_hint(&self) -> u64 {
        // 读取当前 ra 寄存器值（受内联影响，不保证指向直接调用者），KASLR 环境下含熵
        let ra: u64;
        unsafe { core::arch::asm!("mv {}, ra", out(reg) ra, options(nomem, nostack)) };
        ra
    }

    fn name(&self) -> &'static str { "riscv64-jitter" }

    fn sample(&self, out: &mut [u8]) {
        // 收集 4 个 u64 种子值
        let ts1 = self.timestamp();
        let sp = self.stack_pointer_hint();
        let ra = self.self_address_hint();
        let ts2 = self.timestamp();

        // 简单混合：xorshift64 变体，将 4 个值折叠成伪随机流
        let mut state = ts1 ^ sp.rotate_left(17) ^ ra.rotate_left(31) ^ ts2.rotate_left(47);
        // xorshift64 的不动点是 0，用非零常量兜底
        if state == 0 { state = 0xdeadbeef_cafebabe; }
        for chunk in out.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let bytes = state.to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }

    fn as_any(&self) -> &dyn core::any::Any { self }
}

static SOURCE: Riscv64EntropySource = Riscv64EntropySource;

pub fn register() {
    register_entropy_source(&SOURCE);
}
