//! 单元清单模型。

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::{ElmError, ElmResult};
use crate::nexus::{NexusIntent, NexusOffer};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ElmName(String);

impl ElmName {
    pub fn new(value: impl Into<String>) -> ElmResult<Self> {
        let value = value.into();
        if is_valid_name(&value) {
            Ok(Self(value))
        } else {
            Err(ElmError::InvalidName)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ElmVersion(String);

impl ElmVersion {
    pub fn new(value: impl Into<String>) -> ElmResult<Self> {
        let value = value.into();
        if is_valid_version(&value) {
            Ok(Self(value))
        } else {
            Err(ElmError::InvalidVersion)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElmKind {
    Manager,
    Service,
    Driver,
    Extension,
    Filesystem,
    Network,
    Debug,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmManifest {
    pub name: ElmName,
    pub version: ElmVersion,
    pub kind: ElmKind,
    pub intents: Vec<NexusIntent>,
    pub offers: Vec<NexusOffer>,
}

impl ElmManifest {
    pub fn new(name: ElmName, version: ElmVersion, kind: ElmKind) -> Self {
        Self {
            name,
            version,
            kind,
            intents: Vec::new(),
            offers: Vec::new(),
        }
    }

    pub fn with_intent(mut self, intent: NexusIntent) -> Self {
        self.intents.push(intent);
        self
    }

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
