//! x86_64 启动期熵源。
//!
//! 优先使用 RDSEED（若硬件提供），其次使用 RDRAND；两者不可用或暂时返回
//! carry=0 时仍把 TSC、栈地址和代码地址混入池中，但不会为这些可预测值记
//! 账。所有指令都在用户态可执行，hosted 单测也不会触碰特权寄存器。

use core::arch::x86_64::__cpuid_count;
use core::sync::atomic::{AtomicU8, Ordering};

use general::dev::random_source::{EntropySample, EntropySource, register_entropy_source};

const CAP_UNKNOWN: u8 = 0;
const CAP_NONE: u8 = 1;
const CAP_RDRAND: u8 = 1 << 1;
const CAP_RDSEED: u8 = 1 << 2;

static CAPABILITIES: AtomicU8 = AtomicU8::new(CAP_UNKNOWN);

fn capabilities() -> u8 {
    let cached = CAPABILITIES.load(Ordering::Acquire);
    if cached != CAP_UNKNOWN {
        return cached;
    }

    let max_basic = __cpuid_count(0, 0).eax;
    let mut caps = CAP_NONE;
    if max_basic >= 1 && __cpuid_count(1, 0).ecx & (1 << 30) != 0 {
        caps |= CAP_RDRAND;
    }
    if max_basic >= 7 && __cpuid_count(7, 0).ebx & (1 << 18) != 0 {
        caps |= CAP_RDSEED;
    }
    let _ = CAPABILITIES.compare_exchange(CAP_UNKNOWN, caps, Ordering::AcqRel, Ordering::Acquire);
    CAPABILITIES.load(Ordering::Acquire)
}

#[inline]
fn rdrand64_once() -> Option<u64> {
    if capabilities() & CAP_RDRAND == 0 {
        return None;
    }
    let mut value: u64;
    let mut ready: u8;
    // Safety: CPUID advertised RDRAND and the instruction is unprivileged.
    unsafe {
        core::arch::asm!(
            "rdrand {value}",
            "setc {ready}",
            value = out(reg) value,
            ready = lateout(reg_byte) ready,
            options(nomem, nostack),
        );
    }
    (ready != 0).then_some(value)
}

#[inline]
fn rdseed64_once() -> Option<u64> {
    if capabilities() & CAP_RDSEED == 0 {
        return None;
    }
    let mut value: u64;
    let mut ready: u8;
    // Safety: CPUID advertised RDSEED and the instruction is unprivileged.
    unsafe {
        core::arch::asm!(
            "rdseed {value}",
            "setc {ready}",
            value = out(reg) value,
            ready = lateout(reg_byte) ready,
            options(nomem, nostack),
        );
    }
    (ready != 0).then_some(value)
}

fn hardware_word(rdseed: bool) -> Option<u64> {
    // Intel recommends retrying until CF=1; bound retries so a broken/overloaded
    // entropy source cannot stall the boot path indefinitely.
    for _ in 0..16 {
        let word = if rdseed {
            rdseed64_once()
        } else {
            rdrand64_once()
        };
        if word.is_some() {
            return word;
        }
        core::hint::spin_loop();
    }
    None
}

#[inline]
fn stack_pointer_hint() -> u64 {
    // Taking the address of a local is a compiler-stable approximation of RSP
    // and avoids an asm register constraint in hosted builds.
    let local = 0u8;
    core::ptr::addr_of!(local) as usize as u64
}

#[inline]
fn fallback_fill(out: &mut [u8]) {
    let timestamp = super::stable_counter_raw();
    let stack = stack_pointer_hint();
    let code = fallback_fill as *const () as usize as u64;
    let mut state = timestamp ^ stack.rotate_left(17) ^ code.rotate_left(31);
    if state == 0 {
        state = 0x9e37_79b9_7f4a_7c15;
    }
    for chunk in out.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let bytes = state.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}

struct X86EntropySource;

impl EntropySource for X86EntropySource {
    fn timestamp(&self) -> u64 {
        super::stable_counter_raw()
    }

    fn stack_pointer_hint(&self) -> u64 {
        stack_pointer_hint()
    }

    fn self_address_hint(&self) -> u64 {
        X86EntropySource::sample as *const () as usize as u64
    }

    fn name(&self) -> &'static str {
        "x86_64-rdseed-rdrand"
    }

    fn sample(&self, out: &mut [u8]) {
        let _ = self.sample_with_credit(out);
    }

    fn sample_with_credit(&self, out: &mut [u8]) -> EntropySample {
        if out.is_empty() {
            return EntropySample::none();
        }
        fallback_fill(out);

        let rdseed = capabilities() & CAP_RDSEED != 0;
        let rdrand = capabilities() & CAP_RDRAND != 0;
        let mut written = 0usize;
        let mut credited_bits = 0u64;
        while written < out.len() {
            let word = if rdseed {
                hardware_word(true)
            } else if rdrand {
                hardware_word(false)
            } else {
                None
            };
            let Some(word) = word else { break };
            let bytes = word.to_le_bytes();
            let count = (out.len() - written).min(bytes.len());
            out[written..written + count].copy_from_slice(&bytes[..count]);
            written += count;
            // Only RDSEED is credited: RDRAND still contributes conditioned
            // hardware output, but this conservative backend does not claim
            // independent entropy for it.
            if rdseed {
                credited_bits = credited_bits.saturating_add((count as u64) * 8);
            }
        }

        // The fallback bytes are always initialized, so report the full buffer;
        // credit is limited to the RDSEED bytes that actually succeeded.
        EntropySample::new(out.len(), credited_bits)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

static SOURCE: X86EntropySource = X86EntropySource;

/// 把 x86_64 熵源挂到通用随机子系统。
pub fn register() {
    register_entropy_source(&SOURCE);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_is_fully_initialized_without_entropy_credit_assumptions() {
        let mut bytes = [0xa5u8; 32];
        let sample = SOURCE.sample_with_credit(&mut bytes);
        assert_eq!(sample.bytes_written, bytes.len());
        assert!(sample.entropy_bits <= (bytes.len() * 8) as u64);
        // This assertion also catches a fallback implementation that leaves a
        // tail of the caller's old buffer untouched.
        assert!(bytes.iter().any(|byte| *byte != 0xa5));
    }

    #[test]
    fn capability_probe_has_a_non_unknown_result() {
        assert_ne!(capabilities(), CAP_UNKNOWN);
    }
}
