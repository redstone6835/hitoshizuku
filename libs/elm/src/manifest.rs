//! ELM manifest 的强类型模型。
//!
//! manifest 是单元身份和声明式表面的入口，包含规范名称、版本、kind、来源、父关系、依赖、
//! Nexus offer/intent 与菜单等信息。名称和版本在构造时验证，Core 不从文件名、Rust crate
//! 名或导出符号猜测单元身份。
//!
//! manifest 只描述声明，不授予权限。装载器仍需把它与 EBI 其余表、proof、策略、资源预算
//! 和父 cell 约束一起验证。

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::{ElmError, ElmResult};
pub use crate::kind::ElmKind;
use crate::nexus::{NexusIntent, NexusOffer};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// 已通过长度和 identifier 字符规则验证的 ELM 规范名称。
pub struct ElmName(String);

impl ElmName {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub fn new(value: impl Into<String>) -> ElmResult<Self> {
        let value = value.into();
        if is_valid_name(&value) {
            Ok(Self(value))
        } else {
            Err(ElmError::InvalidName)
        }
    }

    /// 执行 `as_str` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// 已验证的 ELM 版本字符串，不与 Rust crate 版本或容器版本混用。
pub struct ElmVersion(String);

impl ElmVersion {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub fn new(value: impl Into<String>) -> ElmResult<Self> {
        let value = value.into();
        if is_valid_version(&value) {
            Ok(Self(value))
        } else {
            Err(ElmError::InvalidVersion)
        }
    }

    /// 执行 `as_str` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// 汇总单元身份、kind、来源、依赖、offer、intent 和菜单的强类型清单。
pub struct ElmManifest {
    /// 对象的固定长度名称缓冲区；实际字符串以首个零字节结束。
    pub name: ElmName,
    /// 该对象或契约的版本号。
    pub version: ElmVersion,
    /// 该记录、资源或关系的类别编码。
    pub kind: ElmKind,
    /// 该单元希望由 Nexus 满足的能力 intent 集合。
    pub intents: Vec<NexusIntent>,
    /// 该单元向 Nexus 发布的能力 offer 集合。
    pub offers: Vec<NexusOffer>,
}

impl ElmManifest {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub fn new(name: ElmName, version: ElmVersion, kind: ElmKind) -> Self {
        Self {
            name,
            version,
            kind,
            intents: Vec::new(),
            offers: Vec::new(),
        }
    }

    /// 设置 `intent` 并返回更新后的值，便于构建器式初始化。
    pub fn with_intent(mut self, intent: NexusIntent) -> Self {
        self.intents.push(intent);
        self
    }

    /// 设置 `offer` 并返回更新后的值，便于构建器式初始化。
    pub fn with_offer(mut self, offer: NexusOffer) -> Self {
        self.offers.push(offer);
        self
    }
}

fn is_valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
}

fn is_valid_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
}
