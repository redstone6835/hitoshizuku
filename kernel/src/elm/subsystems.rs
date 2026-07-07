//! ELM 启动期 provider 规格汇聚。
//!
//! 本文件只把各子系统自己导出的规格表交给 ELM Core。这里不能解释
//! VFS、设备、网络等语义，也不能为某个子系统写特殊分支。

use elm_model::{ElmKernelProviderSpec, ElmResult};

use super::core::ElmCore;

pub(crate) fn register_builtin_provider_specs(core: &mut ElmCore) -> ElmResult<usize> {
    let provider_groups: [&'static [ElmKernelProviderSpec]; 3] = [
        general::dev::elm::providers(),
        net::elm::providers(),
        vfs::elm::providers(),
    ];
    let mut total = 0usize;
    for specs in provider_groups {
        total = total.saturating_add(core.register_kernel_provider_specs(specs)?);
    }
    Ok(total)
}
