//! Nexus 能力连接层的契约、offer 和 intent 模型。
//!
//! 子系统和 ELM 通过完整契约 identifier 发布 offer 或声明 intent，elm-mgr 在方向、模式、
//! 并发、背压、访问策略和 cell policy 都兼容时建立 binding。Nexus 不把“设备”“网络”“VFS”
//! 等类型写死在 Core 中；具体语义由契约和提供该契约的子系统定义。
//!
//! 字符串构造器会验证 identifier 与版本，绑定时必须比较完整契约，不得只比较哈希或名称前缀。

use alloc::string::String;

use crate::error::{ElmError, ElmResult};
pub use crate::wire::{FlowDirection, FlowMode};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// 已验证的 Nexus `identifier@version` 能力契约。
pub struct FlowContract(String);

impl FlowContract {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub fn new(value: impl Into<String>) -> ElmResult<Self> {
        let value = value.into();
        if is_valid_contract(&value) {
            Ok(Self(value))
        } else {
            Err(ElmError::InvalidContract)
        }
    }

    /// 执行 `as_str` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `IntentKind` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum IntentKind {
    /// `Consume` 表示 `IntentKind` 的对象类别：`consume`。
    Consume,
    /// `Offer` 表示 `IntentKind` 的对象类别：`offer`。
    Offer,
    /// `Extend` 表示 `IntentKind` 的对象类别：`extend`。
    Extend,
    /// `Observe` 表示 `IntentKind` 的对象类别：`observe`。
    Observe,
    /// `Control` 表示 `IntentKind` 的对象类别：`control`。
    Control,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `FlowConcurrency` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum FlowConcurrency {
    /// `Single` 是 `FlowConcurrency` 中的稳定判别值，表示 `single`。
    Single,
    /// `Parallel` 是 `FlowConcurrency` 中的稳定判别值，表示 `parallel`。
    Parallel,
    /// `Reentrant` 是 `FlowConcurrency` 中的稳定判别值，表示 `reentrant`。
    Reentrant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `FlowBackpressure` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum FlowBackpressure {
    /// `Drop` 是 `FlowBackpressure` 中的稳定判别值，表示 `drop`。
    Drop,
    /// `Queue` 是 `FlowBackpressure` 中的稳定判别值，表示 `queue`。
    Queue,
    /// `Stall` 是 `FlowBackpressure` 中的稳定判别值，表示 `stall`。
    Stall,
    /// `Reject` 是 `FlowBackpressure` 中的稳定判别值，表示 `reject`。
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// consumer 对契约、方向、模式、并发和背压的连接需求。
pub struct NexusIntent {
    /// 该记录、资源或关系的类别编码。
    pub kind: IntentKind,
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: FlowContract,
}

impl NexusIntent {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(kind: IntentKind, contract: FlowContract) -> Self {
        Self { kind, contract }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// provider 对外发布的端口契约与连接属性。
pub struct NexusOffer {
    /// 端口、调用或载荷采用的完整契约 identifier。
    pub contract: FlowContract,
    /// 端口、绑定或扩展点采用的并发/分发模式编码。
    pub mode: FlowMode,
}

impl NexusOffer {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(contract: FlowContract, mode: FlowMode) -> Self {
        Self { contract, mode }
    }
}

fn is_valid_contract(value: &str) -> bool {
    let Some((name, version)) = value.rsplit_once('@') else {
        return false;
    };
    !name.is_empty()
        && !version.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
        && version
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
}
