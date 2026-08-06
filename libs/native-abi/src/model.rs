//! 与具体可执行格式无关的 ABI 声明视图与绑定结果。

use alloc::vec::Vec;

use crate::{ObjectInterface, OperationId, Rights};

/// 用户线程当前采用的用户态 ABI。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum UserAbiKind {
    #[default]
    TomoriLinux = 0,
    MygoNative = 1,
}

impl UserAbiKind {
    pub const fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::MygoNative,
            _ => Self::TomoriLinux,
        }
    }
}

/// exec 事务对进程控制路径公开的阶段。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum ExecPhase {
    #[default]
    Running = 0,
    Transitioning = 1,
    Terminating = 2,
}

impl ExecPhase {
    pub const fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::Transitioning,
            2 => Self::Terminating,
            _ => Self::Running,
        }
    }
}

pub trait AbiImportRecord {
    fn slot(&self) -> u32;
    fn operation_id(&self) -> u32;
    fn required(&self) -> bool;
    fn signature_hash(&self) -> &[u8; 32];
}

pub trait CapabilityRequirementRecord {
    fn requirement_id(&self) -> u32;
    fn object_interface(&self) -> u16;
    fn required(&self) -> bool;
    fn required_rights(&self) -> u64;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundCallSlot {
    pub slot: u32,
    pub operation: Option<OperationId>,
    pub interface: Option<ObjectInterface>,
    pub required_rights: Rights,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeBindingPlan {
    pub call_slots: Vec<BoundCallSlot>,
}
