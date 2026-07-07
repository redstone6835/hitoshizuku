//! 枢纽连接层模型。

use alloc::string::String;

use crate::error::{ElmError, ElmResult};

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
#[repr(u32)]
pub enum FlowDirection {
    Source = 1,
    Sink = 2,
    Duplex = 3,
    Control = 4,
}

impl FlowDirection {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Source),
            2 => Some(Self::Sink),
            3 => Some(Self::Duplex),
            4 => Some(Self::Control),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum FlowMode {
    Exclusive = 1,
    Shared = 2,
    Ordered = 3,
    Pipeline = 4,
    Broadcast = 5,
}

impl FlowMode {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Exclusive),
            2 => Some(Self::Shared),
            3 => Some(Self::Ordered),
            4 => Some(Self::Pipeline),
            5 => Some(Self::Broadcast),
            _ => None,
        }
    }
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
