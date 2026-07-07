//! ELM（可拓展内核单元）内核核心。
//!
//! 本模块只实现 ELM 自己的能力织网和管理入口，不复用 Linux 模块系统调用。

use alloc::string::String;

mod core;
mod event;
mod executor;
mod menu;
mod mgr_channel;
mod ports;
mod snapshot;
pub(crate) mod syscall;
#[cfg(feature = "kernel-tests")]
mod tests;

pub(crate) fn init_builtin_mgr() {
    match core::with_core(|core| core.init_builtin_mgr()) {
        Ok(()) => {
            general::vfs::sysfs::register_elm_renderer(render_sysfs_file);
            executor::start_provider_worker();
        }
        Err(err) => log::error!("[elm] init builtin elm-mgr failed: {:?}", err),
    }
}

pub(crate) use core::with_core;

fn render_sysfs_file(name: &str) -> String {
    core::with_core(|core| core.sysfs_text(name))
}
