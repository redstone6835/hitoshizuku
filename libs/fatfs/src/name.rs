//! FAT 8.3 短文件名(SFN)规范化与生成。
//!
//! 11 字节的 SFN 是 `[BASE(8) | EXT(3)]`,空格填充,OEM 大写。
//! 本实现将非 ASCII、非友好字符替换为 `_`,并在冲突时通过 `~N` 后缀混叠。

/// 判断一个码点是否可以直接作为 SFN 字符。
pub(crate) fn is_sfn_friendly(c: u16) -> bool {
    if c > 0x7f {
        return false;
    }
    let b = c as u8;
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'$' | b'%'
                | b'\''
                | b'-'
                | b'_'
                | b'@'
                | b'~'
                | b'`'
                | b'!'
                | b'('
                | b')'
                | b'{'
                | b'}'
                | b'^'
                | b'#'
                | b'&'
        )
}

#[inline]
fn upper_byte(b: u8) -> u8 {
    if b.is_ascii_lowercase() { b - 32 } else { b }
}

fn to_upper_ascii(input: &str, out: &mut [u8]) -> usize {
    let mut n = 0;
    for ch in input.chars() {
        if n >= out.len() {
            break;
        }
        if ch == '.' {
            continue;
        }
        if ch.is_ascii() {
            let b = ch as u8;
            out[n] = if is_sfn_friendly(b as u16) {
                upper_byte(b)
            } else {
                b'_'
            };
        } else {
            out[n] = b'_';
        }
        n += 1;
    }
    n
}

fn split_base_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}

fn digits_base10(mut n: u32, out: &mut [u8]) -> usize {
    if n == 0 {
        if !out.is_empty() {
            out[0] = b'0';
            return 1;
        }
        return 0;
    }
    let mut buf = [0u8; 10];
    let mut len = 0;
    while n > 0 && len < buf.len() {
        buf[len] = b'0' + (n % 10) as u8;
        n /= 10;
        len += 1;
    }
    let take = len.min(out.len());
    for i in 0..take {
        out[i] = buf[len - 1 - i];
    }
    take
}

/// 尝试把 `name` 直接编码为 11 字节 SFN(无需 LFN)。条件:
/// - 不为空,base ≤ 8 字符,ext ≤ 3 字符;
/// - 仅包含 SFN 友好 ASCII;
/// - 含小写时也允许(直接大写化)。
pub(crate) fn try_plain_sfn(name: &str) -> Option<[u8; 11]> {
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    let (base, ext) = split_base_ext(name);
    if base.is_empty() || base.len() > 8 || ext.len() > 3 {
        return None;
    }
    let mut out = [b' '; 11];
    for (i, ch) in base.chars().enumerate() {
        if !ch.is_ascii() {
            return None;
        }
        let b = ch as u8;
        if !is_sfn_friendly(b as u16) {
            return None;
        }
        out[i] = upper_byte(b);
    }
    for (i, ch) in ext.chars().enumerate() {
        if !ch.is_ascii() {
            return None;
        }
        let b = ch as u8;
        if !is_sfn_friendly(b as u16) {
            return None;
        }
        out[8 + i] = upper_byte(b);
    }
    // 0xE5 首字节冲突保护:0xE5 在首字节意味"已删",转义为 0x05
    if out[0] == 0xe5 {
        out[0] = 0x05;
    }
    Some(out)
}

/// 生成 `BASE~N.EXT` 形式的混叠 SFN(11 字节)。基名截断保留 `~N` 后缀位置。
pub(crate) fn build_tilde_sfn(name: &str, n: u32) -> [u8; 11] {
    let mut out = [b' '; 11];
    let (base, ext) = split_base_ext(name);

    let mut upper_base = [0u8; 32];
    let upper_base_len = to_upper_ascii(base, &mut upper_base);

    let mut tilde = [0u8; 8];
    tilde[0] = b'~';
    let digits = digits_base10(n, &mut tilde[1..]);
    let tilde_len = 1 + digits;
    let max_base = 8usize.saturating_sub(tilde_len);
    let take = upper_base_len.min(max_base);
    out[..take].copy_from_slice(&upper_base[..take]);
    out[take..take + tilde_len].copy_from_slice(&tilde[..tilde_len]);

    let mut upper_ext = [0u8; 8];
    let ue_len = to_upper_ascii(ext, &mut upper_ext);
    let ext_take = ue_len.min(3);
    out[8..8 + ext_take].copy_from_slice(&upper_ext[..ext_take]);
    if out[0] == 0xe5 {
        out[0] = 0x05;
    }
    out
}
