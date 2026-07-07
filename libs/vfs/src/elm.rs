//! VFS 导出的 ELM provider 规格。
//!
//! VFS 的路径、读写与文件对象语义不写入 ELM Core。Core 只登记这些
//! provider 入口，后续真实实现由 VFS 在本模块内逐步补齐。

use elm_model::{ElmKernelProviderSpec, ElmPortAccessPolicy, FlowDirection, FlowMode};

const VFS_PROVIDERS: [ElmKernelProviderSpec; 3] = [
    ElmKernelProviderSpec::subsystem_todo(
        "elm.vfs",
        "lookup",
        "elm.vfs.lookup@1",
        "vfs.lookup@1",
        FlowDirection::Control,
        FlowMode::Shared,
        ElmPortAccessPolicy::Internal,
        true,
    ),
    ElmKernelProviderSpec::subsystem_todo(
        "elm.vfs",
        "read",
        "elm.vfs.read@1",
        "vfs.read@1",
        FlowDirection::Control,
        FlowMode::Shared,
        ElmPortAccessPolicy::Internal,
        true,
    ),
    ElmKernelProviderSpec::subsystem_todo(
        "elm.vfs",
        "write",
        "elm.vfs.write@1",
        "vfs.write@1",
        FlowDirection::Control,
        FlowMode::Shared,
        ElmPortAccessPolicy::Internal,
        true,
    ),
];

pub fn providers() -> &'static [ElmKernelProviderSpec] {
    // TODO(elm): 将这些入口接到路径解析、文件句柄租约和 typed I/O 请求。
    &VFS_PROVIDERS
}
