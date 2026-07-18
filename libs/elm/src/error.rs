//! 不依赖内核环境的 ELM 模型错误。
//!
//! 这些错误由 graph、lease、manifest 和状态机等可复用模型返回，不是系统调用 errno，也不
//! 直接跨原生 ABI。管理层会把它们映射为稳定状态码、blocker 和审计原因。

/// ELM 纯模型操作的通用结果类型。
pub type ElmResult<T> = Result<T, ElmError>;

#[derive(Debug, Clone, PartialEq, Eq)]
/// ELM 模型不变量被违反时返回的错误类别。
pub enum ElmError {
    /// 单元、端口或其他对象名称不满足 identifier 长度或字符规则。
    InvalidName,
    /// 版本为空、为零或不满足兼容范围。
    InvalidVersion,
    /// 契约 identifier 缺少版本或不满足规范格式。
    InvalidContract,
    /// graph 中已经存在同 id 的 cell。
    DuplicateCell,
    /// 已存在同 id 的端口。
    DuplicatePort,
    /// 已存在同 id 或等价端点组合的绑定。
    DuplicateBinding,
    /// 同一所有者已经声明同名补缀点。
    DuplicateExtensionPoint,
    /// 租约注册表中已经存在同 id 租约。
    DuplicateLease,
    /// 请求引用的 cell 不存在。
    CellNotFound,
    /// 请求引用的端口不存在。
    PortNotFound,
    /// 请求引用的 binding 不存在。
    BindingNotFound,
    /// 目标 ELM 没有声明指定补缀点。
    ExtensionPointNotFound,
    /// 两个待连接对象的契约或版本范围不兼容。
    ContractMismatch,
    /// parent 边会在 cell 树中形成环。
    ParentCycle,
    /// 必需依赖边会形成不允许的依赖环。
    DependencyCycle,
    /// extension 关系会形成不允许的扩展环。
    ExtensionCycle,
    /// parent 不存在、自指或不满足树约束。
    InvalidParent,
    /// 生命周期状态机不存在请求的直接迁移边。
    InvalidTransition,
    /// 租约当前状态不允许请求的 acquire、release、revoke 或 drain 操作。
    InvalidLeaseState,
    /// 活动租约仍保护调用或资源，操作不能安全提交。
    LeaseBusy,
    /// 当前主体、作用域或能力策略不允许该操作。
    PermissionDenied,
}
