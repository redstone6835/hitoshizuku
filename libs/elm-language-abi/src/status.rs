//! language-runtime V1 的状态和错误码。

/// 调用成功。
pub const LANGUAGE_STATUS_OK: i32 = 0;
/// 输入字段、ID 或载荷不满足协议约束。
pub const LANGUAGE_STATUS_INVALID_ARGUMENT: i32 = -22;
/// ABI 版本不受支持。
pub const LANGUAGE_STATUS_ABI_MISMATCH: i32 = -1000;
/// 结构尺寸不是当前 V1 结构的精确尺寸。
pub const LANGUAGE_STATUS_SIZE_MISMATCH: i32 = -1001;
/// flags 设置了 V1 未定义的位。
pub const LANGUAGE_STATUS_FLAGS_INVALID: i32 = -1002;
/// 当前版本要求为零的保留字段非零。
pub const LANGUAGE_STATUS_RESERVED_NONZERO: i32 = -1003;
/// 运行时 ID 为零或使用了错误的 ID。
pub const LANGUAGE_STATUS_INVALID_ID: i32 = -1004;
/// 句柄槽位或 generation 无效。
pub const LANGUAGE_STATUS_HANDLE_INVALID: i32 = -1005;
/// 句柄 generation 已经过期。
pub const LANGUAGE_STATUS_HANDLE_STALE: i32 = -1006;
/// 请求 owner 与句柄记录的 owner 不一致。
pub const LANGUAGE_STATUS_OWNER_MISMATCH: i32 = -1007;
/// inline 载荷超过协议上限。
pub const LANGUAGE_STATUS_PAYLOAD_TOO_LARGE: i32 = -1008;
/// 目标后端、实例或请求不存在。
pub const LANGUAGE_STATUS_NOT_FOUND: i32 = -1009;
/// 当前资源处于繁忙或排空状态。
pub const LANGUAGE_STATUS_BUSY: i32 = -1010;
/// 达到 cell 或运行时的容量预算。
pub const LANGUAGE_STATUS_NO_CAPACITY: i32 = -1011;
/// 当前请求状态不允许该操作。
pub const LANGUAGE_STATUS_BAD_STATE: i32 = -1012;
/// 操作被显式取消。
pub const LANGUAGE_STATUS_CANCELED: i32 = -1013;
/// 后端没有实现对应操作。
pub const LANGUAGE_STATUS_UNSUPPORTED: i32 = -95;
/// 后端或运行时执行失败。
pub const LANGUAGE_STATUS_FAULT: i32 = -5;

/// V1 语言运行时状态码的固定布局包装。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct LanguageRuntimeStatus(pub i32);

impl LanguageRuntimeStatus {
    /// 成功状态。
    pub const OK: Self = Self(LANGUAGE_STATUS_OK);
    /// 输入参数无效。
    pub const INVALID_ARGUMENT: Self = Self(LANGUAGE_STATUS_INVALID_ARGUMENT);
    /// ABI 版本不匹配。
    pub const ABI_MISMATCH: Self = Self(LANGUAGE_STATUS_ABI_MISMATCH);
    /// 结构尺寸不匹配。
    pub const SIZE_MISMATCH: Self = Self(LANGUAGE_STATUS_SIZE_MISMATCH);
    /// flags 不合法。
    pub const FLAGS_INVALID: Self = Self(LANGUAGE_STATUS_FLAGS_INVALID);
    /// 保留字段非零。
    pub const RESERVED_NONZERO: Self = Self(LANGUAGE_STATUS_RESERVED_NONZERO);
    /// ID 无效。
    pub const INVALID_ID: Self = Self(LANGUAGE_STATUS_INVALID_ID);
    /// 句柄无效。
    pub const HANDLE_INVALID: Self = Self(LANGUAGE_STATUS_HANDLE_INVALID);
    /// 句柄已过期。
    pub const HANDLE_STALE: Self = Self(LANGUAGE_STATUS_HANDLE_STALE);
    /// owner 不匹配。
    pub const OWNER_MISMATCH: Self = Self(LANGUAGE_STATUS_OWNER_MISMATCH);
    /// 载荷超出上限。
    pub const PAYLOAD_TOO_LARGE: Self = Self(LANGUAGE_STATUS_PAYLOAD_TOO_LARGE);
    /// 目标不存在。
    pub const NOT_FOUND: Self = Self(LANGUAGE_STATUS_NOT_FOUND);
    /// 目标繁忙。
    pub const BUSY: Self = Self(LANGUAGE_STATUS_BUSY);
    /// 容量耗尽。
    pub const NO_CAPACITY: Self = Self(LANGUAGE_STATUS_NO_CAPACITY);
    /// 状态不允许当前操作。
    pub const BAD_STATE: Self = Self(LANGUAGE_STATUS_BAD_STATE);
    /// 请求被取消。
    pub const CANCELED: Self = Self(LANGUAGE_STATUS_CANCELED);
    /// 操作不受支持。
    pub const UNSUPPORTED: Self = Self(LANGUAGE_STATUS_UNSUPPORTED);
    /// 后端执行故障。
    pub const FAULT: Self = Self(LANGUAGE_STATUS_FAULT);

    /// 从原始整数构造状态，不对未知值做截断。
    pub const fn from_raw(raw: i32) -> Self {
        Self(raw)
    }

    /// 返回原始协议整数。
    pub const fn raw(self) -> i32 {
        self.0
    }

    /// 判断状态是否成功。
    pub const fn is_ok(self) -> bool {
        self.0 == LANGUAGE_STATUS_OK
    }
}
