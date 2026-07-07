//! ELM（可拓展内核单元）内核核心。
//!
//! 本模块只实现 ELM 自己的能力织网和管理入口，不复用 Linux 模块系统调用。

mod core;
mod event;
mod menu;
mod mgr_channel;
mod ports;
mod snapshot;
pub(crate) mod syscall;
#[cfg(feature = "kernel-tests")]
mod tests;

pub(crate) fn init_builtin_mgr() {
    match core::with_core(|core| core.init_builtin_mgr()) {
        Ok(()) => {}
        Err(err) => log::error!("[elm] init builtin elm-mgr failed: {:?}", err),
    }
}

pub(crate) use core::with_core;
