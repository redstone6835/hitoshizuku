//! 网络层导出的 ELM provider 规格。
//!
//! 网络层只在这里声明可被 `elm-mgr` 发现的包收发入口；真实协议栈、
//! 设备队列和包缓冲生命周期仍由网络子系统自己定义。

use elm_model::{ElmKernelProviderSpec, ElmPortAccessPolicy, FlowDirection, FlowMode};

const NET_PROVIDERS: [ElmKernelProviderSpec; 2] = [
    ElmKernelProviderSpec::subsystem_todo(
        "elm.net",
        "packet.rx",
        "elm.net.packet.rx@1",
        "io.packet.rx@1",
        FlowDirection::Source,
        FlowMode::Pipeline,
        ElmPortAccessPolicy::Internal,
        false,
    ),
    ElmKernelProviderSpec::subsystem_todo(
        "elm.net",
        "packet.tx",
        "elm.net.packet.tx@1",
        "io.packet.tx@1",
        FlowDirection::Sink,
        FlowMode::Pipeline,
        ElmPortAccessPolicy::Internal,
        true,
    ),
];

pub fn providers() -> &'static [ElmKernelProviderSpec] {
    // TODO(elm): 将包收发入口接到网络接口、协议栈轮询和缓冲区租约。
    &NET_PROVIDERS
}
