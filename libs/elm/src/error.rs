//! ELM 纯模型错误。

pub type ElmResult<T> = Result<T, ElmError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElmError {
    InvalidName,
    InvalidVersion,
    InvalidContract,
    DuplicateCell,
    DuplicatePort,
    DuplicateBinding,
    DuplicateExtensionPoint,
    DuplicateLease,
    CellNotFound,
    PortNotFound,
    BindingNotFound,
    ExtensionPointNotFound,
    ContractMismatch,
    ParentCycle,
    DependencyCycle,
    ExtensionCycle,
    InvalidParent,
    InvalidTransition,
    InvalidLeaseState,
    LeaseBusy,
    PermissionDenied,
}
