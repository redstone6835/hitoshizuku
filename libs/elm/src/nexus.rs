//! 枢纽连接层模型。

use alloc::string::String;

use crate::error::{ElmError, ElmResult};
pub use crate::wire::{FlowDirection, FlowMode};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FlowContract(String);

impl FlowContract {
    pub fn new(value: impl Into<String>) -> ElmResult<Self> {
        let value = value.into();
        if is_valid_contract(&value) {
            Ok(Self(value))
        } else {
            Err(ElmError::InvalidContract)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentKind {
    Consume,
    Offer,
    Extend,
    Observe,
    Control,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowConcurrency {
    Single,
    Parallel,
    Reentrant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowBackpressure {
    Drop,
    Queue,
    Stall,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NexusIntent {
    pub kind: IntentKind,
    pub contract: FlowContract,
}

impl NexusIntent {
    pub const fn new(kind: IntentKind, contract: FlowContract) -> Self {
        Self { kind, contract }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NexusOffer {
    pub contract: FlowContract,
    pub mode: FlowMode,
}

impl NexusOffer {
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
