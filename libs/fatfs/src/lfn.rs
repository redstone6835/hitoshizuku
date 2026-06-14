//! Long File Name(LFN)目录项编码/解码 + SFN checksum。
//!
//! LFN 以 32 字节条目连续排列,紧邻对应 SFN **之前**按 order 递增倒序放置:
//! 条目 N-1, N-2, ..., 1, SFN;末位 LFN(`order & 0x40 != 0`)先落盘。
//! 每条 LFN 承载 13 个 UCS-2 字符,并记录由 SFN 11 字节算得的 8 位 checksum。

use alloc::string::String;
use alloc::vec::Vec;

/// 由 11 字节 SFN 原始名(空格填充,不含点)计算 LFN 校验和。
pub(crate) fn lfn_checksum(sfn: &[u8; 11]) -> u8 {
    let mut sum: u8 = 0;
    for &b in sfn.iter() {
        sum = ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(b);
    }
    sum
}

/// 编码一条 LFN 目录项。
///
/// `order`: 1-based,末尾项须 OR `0x40`。`chars` 为 13 个 UCS-2 码点。
pub(crate) fn encode_lfn_entry(order: u8, chars: &[u16; 13], checksum: u8, out: &mut [u8; 32]) {
    out[0] = order;
    for i in 0..5 {
        let be = chars[i].to_le_bytes();
        out[1 + i * 2] = be[0];
        out[2 + i * 2] = be[1];
    }
    out[11] = 0x0f;
    out[12] = 0;
    out[13] = checksum;
    for i in 0..6 {
        let be = chars[5 + i].to_le_bytes();
        out[14 + i * 2] = be[0];
        out[15 + i * 2] = be[1];
    }
    out[26] = 0;
    out[27] = 0;
    for i in 0..2 {
        let be = chars[11 + i].to_le_bytes();
        out[28 + i * 2] = be[0];
        out[29 + i * 2] = be[1];
    }
}

/// 从一条 LFN 条目中抽取最多 13 个 UCS-2 码点到固定缓冲。
///
/// 返回 `(写入数量,是否遇到结束标记)`。目录扫描热路径用固定数组承接,
/// 避免每个 LFN 槽都分配一个临时 `Vec`。
pub(crate) fn decode_lfn_entry_fixed(entry: &[u8], out: &mut [u16; 13]) -> (usize, bool) {
    let mut terminated = false;
    let mut len = 0;
    let ranges: [(usize, usize); 3] = [(1, 11), (14, 26), (28, 32)];
    for (start, end) in ranges {
        let mut i = start;
        while i < end {
            let u = u16::from_le_bytes([entry[i], entry[i + 1]]);
            i += 2;
            if u == 0 || u == 0xffff {
                terminated = true;
                continue;
            }
            if len < out.len() {
                out[len] = u;
                len += 1;
            }
        }
    }
    (len, terminated)
}

/// 将一组 UCS-2 码点拼为 `String`(非法代理对用 U+FFFD 替换)。
pub(crate) fn ucs2_to_string(units: &[u16]) -> String {
    let mut s = String::with_capacity(units.len());
    for &u in units {
        if (0xd800..=0xdfff).contains(&u) {
            s.push('\u{fffd}');
        } else {
            s.push(core::char::from_u32(u as u32).unwrap_or('\u{fffd}'));
        }
    }
    s
}

/// 将 `&str` 按 UCS-2 码点展开(超出 BMP 的码点写 0xFFFD)。
pub(crate) fn str_to_ucs2(s: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    for ch in s.chars() {
        let cp = ch as u32;
        if cp > 0xffff {
            out.push(0xfffd);
        } else {
            out.push(cp as u16);
        }
    }
    out
}
