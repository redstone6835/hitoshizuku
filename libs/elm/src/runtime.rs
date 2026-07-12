//! ELM 普通运行时接口。

use crate::developer::runtime_api;
use crate::{ELM_API_CURRENT_VERSION, ElmApiContextV1, RuntimeApiError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeInfo {
    pub api_version: u16,
    pub capabilities: u64,
}

pub fn info() -> Result<RuntimeInfo, RuntimeApiError> {
    Ok(RuntimeInfo {
        api_version: ELM_API_CURRENT_VERSION,
        capabilities: runtime_api::features()?,
    })
}

pub fn context() -> Result<ElmApiContextV1, RuntimeApiError> {
    runtime_api::current_context()
}

pub fn log(level: u32, message: &str) -> Result<(), RuntimeApiError> {
    runtime_api::log(level, message)
}

pub fn abort(reason: u32) -> ! {
    runtime_api::abort_current(reason)
}

pub fn abort_panic() -> ! {
    runtime_api::abort_panic()
}
