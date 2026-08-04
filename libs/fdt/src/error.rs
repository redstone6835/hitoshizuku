//! FDT 解析与属性解码错误。
//!
//! 错误携带尽可能精确的块内或文件内偏移，便于固件移植时区分头部、
//! 布局、结构 token 与属性内容问题。

use core::fmt;

/// FDT 二进制格式错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// 输入不足以包含相应版本的完整头部。
    TruncatedHeader {
        /// 需要的字节数。
        needed: usize,
        /// 实际可用字节数。
        available: usize,
    },
    /// 魔数不是 `0xd00dfeed`。
    BadMagic(u32),
    /// 版本早于 v2，或最低兼容版本高于本解析器支持的 v17。
    UnsupportedVersion {
        /// DTB 声明的版本。
        version: u32,
        /// DTB 声明的最低兼容版本。
        last_compatible: u32,
    },
    /// `version` 与 `last_comp_version` 的关系无效。
    InvalidVersion {
        /// DTB 声明的版本。
        version: u32,
        /// DTB 声明的最低兼容版本。
        last_compatible: u32,
    },
    /// `totalsize` 小于头部或大于传入切片。
    InvalidTotalSize {
        /// 头部声明的总长度。
        declared: u32,
        /// 当前版本所需的头部长度。
        header_size: usize,
        /// 传入切片长度。
        available: usize,
    },
    /// 块偏移未满足规范对齐要求。
    MisalignedBlock {
        /// 块名称。
        block: &'static str,
        /// 文件内偏移。
        offset: u32,
        /// 所需对齐字节数。
        alignment: usize,
    },
    /// 块范围越界或发生整数溢出。
    BlockOutOfBounds {
        /// 块名称。
        block: &'static str,
        /// 文件内偏移。
        offset: u32,
        /// 块长度。
        size: u32,
        /// DTB 总长度。
        total_size: u32,
    },
    /// 两个已使用区域发生重叠。
    BlocksOverlap {
        /// 第一个区域名称。
        first: &'static str,
        /// 第二个区域名称。
        second: &'static str,
    },
    /// memory reservation block 没有合法全零终止项。
    MissingReservationTerminator {
        /// reservation block 文件内起始偏移。
        offset: u32,
    },
    /// reservation entry 不完整。
    TruncatedReservation {
        /// entry 的文件内偏移。
        offset: usize,
    },
    /// 结构块在需要数据的位置结束。
    TruncatedStructure {
        /// 结构块内偏移。
        offset: usize,
    },
    /// 遇到未知结构 token。
    InvalidToken {
        /// 结构块内偏移。
        offset: usize,
        /// token 值。
        token: u32,
    },
    /// 根节点之前或结束之后出现不允许的 token。
    InvalidTokenOrder {
        /// 结构块内偏移。
        offset: usize,
        /// token 值。
        token: u32,
    },
    /// 节点嵌套不平衡。
    UnbalancedNode {
        /// 结构块内偏移。
        offset: usize,
    },
    /// 根节点名不符合相应 FDT 版本的约定。
    InvalidRootName {
        /// 结构块内偏移。
        offset: usize,
    },
    /// 节点名不符合 Devicetree Specification 的字符或路径规则。
    InvalidNodeName {
        /// 结构块内偏移。
        offset: usize,
    },
    /// 同一父节点下存在重复的完整节点名。
    DuplicateNodeName {
        /// 后一个重复节点的结构块内偏移。
        offset: usize,
    },
    /// 节点名缺少 NUL 终止符。
    UnterminatedNodeName {
        /// 结构块内偏移。
        offset: usize,
    },
    /// 属性出现在同一节点的子节点之后。
    PropertyAfterChild {
        /// 结构块内偏移。
        offset: usize,
    },
    /// 属性头或属性值越过结构块。
    TruncatedProperty {
        /// 属性 token 的结构块内偏移。
        offset: usize,
        /// 属性声明的值长度；头部不完整时为 `None`。
        length: Option<u32>,
    },
    /// 属性名称偏移越过字符串块。
    InvalidStringOffset {
        /// 属性 token 的结构块内偏移。
        property_offset: usize,
        /// 属性声明的字符串块偏移。
        string_offset: u32,
    },
    /// 属性名称在字符串块中缺少 NUL 终止符。
    UnterminatedPropertyName {
        /// 属性 token 的结构块内偏移。
        property_offset: usize,
        /// 属性声明的字符串块偏移。
        string_offset: u32,
    },
    /// 被引用的属性名不符合 Devicetree Specification 的字符规则。
    InvalidPropertyName {
        /// 属性 token 的结构块内偏移。
        property_offset: usize,
        /// 属性声明的字符串块偏移。
        string_offset: u32,
    },
    /// 同一节点内存在重复属性名。
    DuplicatePropertyName {
        /// 后一个重复属性的结构块内偏移。
        offset: usize,
    },
    /// v17 声明的 structure block 在最终 token 后含非零尾部字节。
    NonZeroPadding {
        /// 第一个非零字节的结构块内偏移。
        offset: usize,
    },
    /// 根节点结束后缺少 `FDT_END`。
    MissingEndToken {
        /// 扫描停止处的结构块内偏移。
        offset: usize,
    },
    /// v17 声明的结构块在 `FDT_END` 后含非 NOP token。
    InvalidTrailingToken {
        /// 结构块内偏移。
        offset: usize,
        /// token 值。
        token: u32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FDT format error: {self:?}")
    }
}

/// 属性显式类型解码错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PropertyError {
    /// 属性长度与目标标量或 cell 数不匹配。
    InvalidLength {
        /// 实际字节数。
        actual: usize,
        /// 期望字节数；可变长类型使用 `None`。
        expected: Option<usize>,
    },
    /// 字符串或字符串列表缺少最后的 NUL。
    MissingNul,
    /// 单字符串属性包含额外字符串。
    MultipleStrings,
    /// 字符串不是合法 UTF-8。
    InvalidUtf8,
    /// 请求的 cell 数无法表示为 `u128`。
    TooManyCells(usize),
    /// cell 读取越过属性末尾。
    NotEnoughCells {
        /// 请求的 cell 数。
        requested: usize,
        /// 剩余 cell 数。
        remaining: usize,
    },
}

impl fmt::Display for PropertyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FDT property error: {self:?}")
    }
}

/// `/chosen/stdout-path` 的零分配解析错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChosenError {
    /// chosen 属性不是单个 NUL 结尾 UTF-8 字符串。
    InvalidValue(PropertyError),
    /// alias 属性不是单个 NUL 结尾 UTF-8 字符串。
    InvalidAlias(PropertyError),
    /// 路径、alias 或 alias 目标不存在。
    Unresolved,
}

impl fmt::Display for ChosenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FDT chosen stdout error: {self:?}")
    }
}
