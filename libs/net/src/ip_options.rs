//! IPv4 选项（setsockopt(IP_OPTIONS)）的解析、校验与规范化。
//!
//! 语义对齐 Linux `ip_options_compile`：总长不超过 40 字节；EOL(0) 终止
//! 解析；NOP(1) 单字节；其余选项要求合法的长度字节（长度 ≥ 2 且不越界），
//! 带指针的选项（RR/SSRR/LSRR/TS）额外校验指针落在选项内。未知选项类型
//! 只做结构校验，不做解释（与 Linux 一致，选项原样出现在发出的 IP 头中）。
//! 规范化形式为原样拷贝并按 4 字节边界以 EOL 填充（`ip_options_get_from_user`
//! 语义）。

/// IPv4 头选项区最大长度（字节）。
pub const IP_OPTIONS_MAX_LEN: usize = 40;

/// 选项类型常量（RFC 791）。
pub const IPOPT_EOL: u8 = 0;
pub const IPOPT_NOP: u8 = 1;
pub const IPOPT_RR: u8 = 7;
pub const IPOPT_TS: u8 = 68;
pub const IPOPT_SSRR: u8 = 131;
pub const IPOPT_LSRR: u8 = 137;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpOptionsError {
    /// 超过 IP 头 40 字节选项上限。
    TooLong,
    /// 结构非法（长度/指针越界、长度字节缺失）。
    Malformed,
}

/// 规范化后的 IPv4 选项（原样拷贝 + EOL 填充到 4 字节边界）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpOptions {
    len: u8,
    bytes: [u8; IP_OPTIONS_MAX_LEN],
}

impl IpOptions {
    pub const fn empty() -> Self {
        Self {
            len: 0,
            bytes: [0; IP_OPTIONS_MAX_LEN],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    /// 4 字节对齐后的线上长度（IP 头 IHL 单位为 4 字节）。
    pub fn wire_len(&self) -> usize {
        (usize::from(self.len) + 3) & !3
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// 含 EOL 填充的线上字节（长度等于 [`IpOptions::wire_len`]）。
    pub fn wire_slice(&self) -> &[u8] {
        &self.bytes[..self.wire_len()]
    }

    /// 解析并校验用户提供的选项列表（Linux `ip_options_compile` 语义）。
    pub fn parse(input: &[u8]) -> Result<Self, IpOptionsError> {
        if input.len() > IP_OPTIONS_MAX_LEN {
            return Err(IpOptionsError::TooLong);
        }
        let mut index = 0;
        while index < input.len() {
            let kind = input[index];
            if kind == IPOPT_EOL {
                // EOL 终止解析；其后的字节不做校验（Linux 同样只拷贝）。
                break;
            }
            if kind == IPOPT_NOP {
                index += 1;
                continue;
            }
            // 变长选项：必须携带长度字节。
            if index + 1 >= input.len() {
                return Err(IpOptionsError::Malformed);
            }
            let option_len = usize::from(input[index + 1]);
            if option_len < 2 || index + option_len > input.len() {
                return Err(IpOptionsError::Malformed);
            }
            // 带指针的选项：指针必须位于选项内（指向第一个地址槽）。
            if matches!(kind, IPOPT_RR | IPOPT_SSRR | IPOPT_LSRR | IPOPT_TS) {
                if option_len < 4
                    || input[index + 2] < 4
                    || usize::from(input[index + 2]) > option_len
                {
                    return Err(IpOptionsError::Malformed);
                }
            }
            index += option_len;
        }
        let mut options = Self::empty();
        options.len = input.len() as u8;
        options.bytes[..input.len()].copy_from_slice(input);
        // 填充到 4 字节边界（不足处补 EOL）。
        let padded = options.wire_len();
        for byte in options.bytes.iter_mut().take(padded).skip(input.len()) {
            *byte = IPOPT_EOL;
        }
        Ok(options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_empty_options() {
        let options = IpOptions::parse(&[]).unwrap();
        assert!(options.is_empty());
        assert_eq!(options.wire_len(), 0);
    }

    #[test]
    fn nop_and_eol_accepted() {
        let options = IpOptions::parse(&[IPOPT_NOP, IPOPT_EOL]).unwrap();
        assert_eq!(options.as_slice(), &[IPOPT_NOP, IPOPT_EOL]);
        assert_eq!(options.wire_len(), 4);
    }

    #[test]
    fn record_route_with_valid_pointer_accepted() {
        // RR：type=7, len=7, ptr=4, 4 个地址槽。
        let input = [IPOPT_RR, 7, 4, 0, 0, 0, 0];
        let options = IpOptions::parse(&input).unwrap();
        assert_eq!(options.as_slice(), &input);
        // 7 字节 + 1 EOL 填充 = 8。
        assert_eq!(options.wire_len(), 8);
    }

    #[test]
    fn record_route_with_bad_pointer_rejected() {
        // 指针 3 < 4。
        assert_eq!(
            IpOptions::parse(&[IPOPT_RR, 7, 3, 0, 0, 0, 0]),
            Err(IpOptionsError::Malformed),
        );
        // 指针 8 > 选项长度 7。
        assert_eq!(
            IpOptions::parse(&[IPOPT_RR, 7, 8, 0, 0, 0, 0]),
            Err(IpOptionsError::Malformed),
        );
    }

    #[test]
    fn source_route_options_accepted() {
        // LSRR：type=137, len=7, ptr=4。
        let lsrr = [IPOPT_LSRR, 7, 4, 0, 0, 0, 0];
        assert_eq!(IpOptions::parse(&lsrr).unwrap().as_slice(), &lsrr);
        let ssrr = [IPOPT_SSRR, 7, 4, 0, 0, 0, 0];
        assert_eq!(IpOptions::parse(&ssrr).unwrap().as_slice(), &ssrr);
    }

    #[test]
    fn truncated_length_rejected() {
        // 长度 8 但只剩 5 字节。
        assert_eq!(
            IpOptions::parse(&[IPOPT_RR, 8, 4, 0, 0]),
            Err(IpOptionsError::Malformed),
        );
        // 长度 1 非法。
        assert_eq!(IpOptions::parse(&[42, 1]), Err(IpOptionsError::Malformed),);
        // 末字节为变长选项类型但没有长度字节。
        assert_eq!(
            IpOptions::parse(&[IPOPT_RR]),
            Err(IpOptionsError::Malformed),
        );
    }

    #[test]
    fn unknown_option_with_valid_length_accepted() {
        let input = [42, 4, 1, 2];
        assert_eq!(IpOptions::parse(&input).unwrap().as_slice(), &input);
    }

    #[test]
    fn overlong_input_rejected() {
        let mut input = alloc::vec![IPOPT_NOP; 41];
        input[40] = IPOPT_NOP;
        assert_eq!(IpOptions::parse(&input), Err(IpOptionsError::TooLong));
        let ok = alloc::vec![IPOPT_NOP; 40];
        assert!(IpOptions::parse(&ok).is_ok());
    }

    #[test]
    fn mixed_options_preserved_verbatim() {
        let input = [IPOPT_NOP, IPOPT_NOP, IPOPT_RR, 7, 4, 0, 0, 0, 0, IPOPT_EOL];
        let options = IpOptions::parse(&input).unwrap();
        assert_eq!(options.as_slice(), &input);
        // 10 字节 → 填充到 12。
        assert_eq!(options.wire_len(), 12);
        assert_eq!(options.as_slice().len(), 10);
    }
}
