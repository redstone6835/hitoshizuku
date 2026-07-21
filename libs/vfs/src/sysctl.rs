//! 可写 `/proc/sys` 兼容文件共用的文本解析辅助函数。

use crate::error::{VfsError, VfsResult};

/// 解析 Linux 有符号 `long` 型 sysctl 值的非负区间。
pub fn parse_nonnegative_long(buf: &[u8]) -> VfsResult<u64> {
    let text = core::str::from_utf8(buf).map_err(|_| VfsError::InvalidArgument)?;
    let value = text
        .trim_matches(|ch: char| ch.is_ascii_whitespace())
        .parse::<u64>()
        .map_err(|_| VfsError::InvalidArgument)?;
    if value > i64::MAX as u64 {
        return Err(VfsError::InvalidArgument);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::parse_nonnegative_long;

    #[test]
    fn accepts_nonnegative_signed_long_range() {
        assert_eq!(parse_nonnegative_long(b"0\n").unwrap(), 0);
        assert_eq!(
            parse_nonnegative_long(b" 9223372036854775807\n").unwrap(),
            i64::MAX as u64
        );
    }

    #[test]
    fn rejects_overflow_and_invalid_input() {
        assert!(parse_nonnegative_long(b"9223372036854775808\n").is_err());
        assert!(parse_nonnegative_long(b"18446744073709551615\n").is_err());
        assert!(parse_nonnegative_long(b"18446744073709551616\n").is_err());
        assert!(parse_nonnegative_long(b"-1\n").is_err());
        assert!(parse_nonnegative_long(b"\n").is_err());
    }
}
