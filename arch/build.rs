//! arch crate 构建脚本：板级调试开关与板级选择。
//!
//! - `MYGO_BOARD_DEBUG_UART=1` 时启用 `mygo_board_debug_uart` cfg，让
//!   LoongArch64 引导汇编在 DMW 就绪后立即向 2K1000LA UART0 输出调试字符
//!   （仅调试构建；与 kernel/build.rs 的同一环境变量保持一致）。
//! - `MYGO_LA_BOARD=ls2k1000` 时启用 `mygo_la_board_ls2k1000` cfg，让早期
//!   控制台兜底配置指向板载 UART0（0x1fe20000 @ 125MHz）而不是 QEMU 串口。
fn main() {
    println!("cargo:rerun-if-env-changed=MYGO_BOARD_DEBUG_UART");
    println!("cargo:rerun-if-env-changed=MYGO_LA_BOARD");
    // 声明 check-cfg，避免 rustc 的 unexpected_cfgs 警告（与 rustc-cfg 配套）。
    println!("cargo::rustc-check-cfg=cfg(mygo_board_debug_uart)");
    println!("cargo::rustc-check-cfg=cfg(mygo_la_board_ls2k1000)");
    if std::env::var_os("MYGO_BOARD_DEBUG_UART").is_some_and(|v| v == "1") {
        // 使用 cargo:: 新指令形式：旧形式 `cargo:rustc-cfg` 在构建脚本结果被缓存
        // 复用时不保证重放到 rustc（本次板级调试就踩到了：cfg 未被传入，导致
        // 调试版 _start 没有被编进内核镜像）。
        println!("cargo::rustc-cfg=mygo_board_debug_uart");
    }
    if std::env::var_os("MYGO_LA_BOARD").is_some_and(|v| v == "ls2k1000") {
        println!("cargo::rustc-cfg=mygo_la_board_ls2k1000");
    }
}
