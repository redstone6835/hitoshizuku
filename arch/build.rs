//! 架构 crate 的板级构建选择。

fn main() {
    println!("cargo:rerun-if-env-changed=MYGO_BOARD_DEBUG_UART");
    println!("cargo:rerun-if-env-changed=MYGO_LA_BOARD");
    println!("cargo::rustc-check-cfg=cfg(mygo_board_debug_uart)");
    println!("cargo::rustc-check-cfg=cfg(mygo_la_board_ls2k1000)");

    if std::env::var_os("MYGO_BOARD_DEBUG_UART").is_some_and(|value| value == "1") {
        println!("cargo::rustc-cfg=mygo_board_debug_uart");
    }
    if std::env::var_os("MYGO_LA_BOARD").is_some_and(|value| value == "ls2k1000") {
        println!("cargo::rustc-cfg=mygo_la_board_ls2k1000");
    }
}
