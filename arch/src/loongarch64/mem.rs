//! LoongArch64 LSX 加速内存操作。
//!
//! 用 128-bit LSX 向量指令覆盖 compiler_builtins 的标量实现。
//! 内核态 CSR_EUEN 恒保持 SXE=1，所有路径可安全使用 vld/vst。
//!
//! 实现策略：
//!  - n < 16 字节：标量 byte/word/dword 路径
//!  - 16 ≤ n < 128 字节：展开的 st.d/ld.d 路径（最多 2 个 16B 块）
//!  - n ≥ 128 字节：LSX vst/vld 循环，128B/iteration（8× 16B vst 展开）
//!
//! memcpy/memmove 遵循 C ABI：返回 dst 指针。
//! memset 返回 dst 指针。
//! memcmp 返回负/零/正 i32。

use core::arch::asm;

/// 内核态 LSX memset。
///
/// # Safety
/// 调用者保证 [dst, dst+n) 有效可写，且 CSR_EUEN.SXE = 1。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(dst: *mut u8, c: i32, n: usize) -> *mut u8 {
    let mut p = dst as usize;
    let end = p + n;
    let byte = (c as u8) as usize;

    if n < 16 {
        // 标量小块路径
        while p < end {
            unsafe { (p as *mut u8).write(byte as u8) };
            p += 1;
        }
        return dst;
    }

    // 构建 8 字节广播值
    let word = byte | (byte << 8) | (byte << 16) | (byte << 24);
    let dword = (word | (word << 32)) as u64;

    if n < 128 {
        // 16–127 字节：st.d 展开
        let mut remaining = n;
        let mut pp = dst as *mut u64;
        while remaining >= 8 {
            unsafe { pp.write_unaligned(dword) };
            pp = unsafe { pp.add(1) };
            remaining -= 8;
        }
        let mut bp = pp as *mut u8;
        while remaining > 0 {
            unsafe { bp.write(byte as u8) };
            bp = unsafe { bp.add(1) };
            remaining -= 1;
        }
        return dst;
    }

    // ≥128 字节: LSX vst 路径
    // vrepli.b 把字节值广播到整个 128-bit 寄存器
    unsafe {
        let fill = byte as u8;
        // 先处理首部非 128B 对齐部分（最多 127B）
        let aligned_start = (p + 127) & !127;
        while p < aligned_start.min(end) {
            (p as *mut u8).write(fill);
            p += 1;
        }
        // 主循环：每次写128字节（8× vst）
        let aligned_end = end & !127;
        if p < aligned_end {
            asm!(
                // vr0 = broadcast fill byte
                "andi {tmp}, {byte}, 0xff",
                "vinsgr2vr.b $vr0, {tmp}, 0",
                "vreplvei.b $vr0, $vr0, 0",
                "2:",
                "vst $vr0, {p}, 0",
                "vst $vr0, {p}, 16",
                "vst $vr0, {p}, 32",
                "vst $vr0, {p}, 48",
                "vst $vr0, {p}, 64",
                "vst $vr0, {p}, 80",
                "vst $vr0, {p}, 96",
                "vst $vr0, {p}, 112",
                "addi.d {p}, {p}, 128",
                "blt {p}, {aligned_end}, 2b",
                p = inout(reg) p,
                aligned_end = in(reg) aligned_end,
                byte = in(reg) byte,
                tmp = out(reg) _,
                options(nostack),
            );
        }
        // 尾部不足128B
        while p < end {
            (p as *mut u8).write(fill);
            p += 1;
        }
    }
    dst
}

/// 内核态 LSX memcpy（不处理重叠）。
///
/// # Safety
/// [src, src+n) 与 [dst, dst+n) 不重叠，均有效。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if n == 0 {
        return dst;
    }

    let mut d = dst as usize;
    let mut s = src as usize;
    let end_s = s + n;

    if n < 16 {
        while s < end_s {
            unsafe { (d as *mut u8).write((s as *const u8).read()) };
            d += 1;
            s += 1;
        }
        return dst;
    }

    if n < 128 {
        let mut remaining = n;
        let mut dp = dst as *mut u64;
        let mut sp = src as *const u64;
        while remaining >= 8 {
            unsafe { dp.write_unaligned(sp.read_unaligned()) };
            dp = unsafe { dp.add(1) };
            sp = unsafe { sp.add(1) };
            remaining -= 8;
        }
        let mut bp = dp as *mut u8;
        let mut bsp = sp as *const u8;
        while remaining > 0 {
            unsafe { bp.write(bsp.read()) };
            bp = unsafe { bp.add(1) };
            bsp = unsafe { bsp.add(1) };
            remaining -= 1;
        }
        return dst;
    }

    // ≥128 字节: LSX vld/vst 路径
    unsafe {
        // 首部对齐
        let aligned_d = (d + 127) & !127;
        while d < aligned_d.min(d + n) && s < end_s {
            (d as *mut u8).write((s as *const u8).read());
            d += 1;
            s += 1;
        }
        let remaining = end_s.saturating_sub(s);
        let loop_count = remaining / 128;
        let tail = remaining % 128;
        if loop_count > 0 {
            asm!(
                "2:",
                "vld $vr0, {s}, 0",
                "vld $vr1, {s}, 16",
                "vld $vr2, {s}, 32",
                "vld $vr3, {s}, 48",
                "vld $vr4, {s}, 64",
                "vld $vr5, {s}, 80",
                "vld $vr6, {s}, 96",
                "vld $vr7, {s}, 112",
                "vst $vr0, {d}, 0",
                "vst $vr1, {d}, 16",
                "vst $vr2, {d}, 32",
                "vst $vr3, {d}, 48",
                "vst $vr4, {d}, 64",
                "vst $vr5, {d}, 80",
                "vst $vr6, {d}, 96",
                "vst $vr7, {d}, 112",
                "addi.d {s}, {s}, 128",
                "addi.d {d}, {d}, 128",
                "addi.d {cnt}, {cnt}, -1",
                "bnez {cnt}, 2b",
                s = inout(reg) s,
                d = inout(reg) d,
                cnt = inout(reg) loop_count => _,
                options(nostack),
            );
        }
        // 尾部
        let tail_s = s as *const u8;
        let tail_d = d as *mut u8;
        for i in 0..tail {
            tail_d.add(i).write(tail_s.add(i).read());
        }
    }
    dst
}

/// 内核态 memmove（处理重叠）。
///
/// # Safety
/// [dst, dst+n) 和 [src, src+n) 均有效，可以重叠。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if n == 0 || dst as usize == src as usize {
        return dst;
    }
    // 向后重叠：src < dst < src+n，倒序复制
    if (src as usize) < (dst as usize)
        && (dst as usize) < (src as usize).wrapping_add(n)
    {
        // 倒序标量复制（保守路径）
        let mut d = (dst as usize) + n;
        let mut s = (src as usize) + n;
        while d > dst as usize {
            d -= 1;
            s -= 1;
            unsafe { (d as *mut u8).write((s as *const u8).read()) };
        }
        dst
    } else {
        unsafe { memcpy(dst, src, n) }
    }
}

/// 内核态 memcmp。
///
/// # Safety
/// [s1, s1+n) 和 [s2, s2+n) 均有效可读。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(s1: *const u8, s2: *const u8, n: usize) -> i32 {
    let mut p1 = s1 as usize;
    let mut p2 = s2 as usize;
    let end = p1 + n;

    // 快速 64-bit 扫描：每次比 8 字节
    while p1 + 8 <= end {
        let a = unsafe { (p1 as *const u64).read_unaligned() };
        let b = unsafe { (p2 as *const u64).read_unaligned() };
        if a != b {
            // 找第一个不同字节
            let xor = a ^ b;
            let pos = (xor.to_le().trailing_zeros() / 8) as usize;
            let b1 = unsafe { ((p1 + pos) as *const u8).read() };
            let b2 = unsafe { ((p2 + pos) as *const u8).read() };
            return b1 as i32 - b2 as i32;
        }
        p1 += 8;
        p2 += 8;
    }
    // 剩余字节
    while p1 < end {
        let a = unsafe { (p1 as *const u8).read() };
        let b = unsafe { (p2 as *const u8).read() };
        if a != b {
            return a as i32 - b as i32;
        }
        p1 += 1;
        p2 += 1;
    }
    0
}
