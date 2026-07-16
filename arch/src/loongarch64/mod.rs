//! LoongArch64 架构支持总入口。
//!
//! 这个模块把 LoongArch64 相关的子模块组织成一个完整的平台实现，供上层
//! `kernel`、`general` 和各类子系统调用。阅读这个目录时，可以把它理解为
//! “从硬件启动到进入通用内核逻辑”的一条连续链路，主要包含以下几层：
//!
//! 1. `boot`：最早期入口。负责在汇编环境下建立 DMW、切换到 Rust 初始化逻辑。
//! 2. `init`：平台初始化主流程。负责解析固件数据、初始化分配器、建立正式页表、
//!    注册串口与控制台，并最终把系统带入可运行状态。
//! 3. `specific`：LoongArch64 的 CSR、异常码、寄存器位定义，以及一些地址转换
//!    辅助函数。这里集中保存“硬件位布局知识”，避免散落到其它模块。
//! 4. `paging` 与 `heap_vm`：分页相关实现。前者负责页表格式和 CSR 配置，后者
//!    负责内核堆虚拟地址空间的具体映射与解除映射。
//! 5. `trap`：异常、中断和 TLB refill 相关逻辑，连接汇编入口和 Rust 处理函数。
//! 6. `early_console`：正式控制台建立前的最小输出路径，用于启动阶段调试。
//! 7. `abi`：平台 ABI 转换层，用于把 LoongArch64/Linux 风格的整数参数翻译为
//!    内核内部的类型语义。
pub mod abi;
mod boot;
mod early_console;
mod efi_stub;
mod elm_native;
mod heap_vm;
mod loader;
mod mm;
mod paging;
mod random_source;
mod sched_ctx;
mod smp;
mod specific;
mod syscall;
mod task;
mod trap;
pub mod vdso;

pub use boot::*;
pub use early_console::*;
pub use elm_native::{
    call_elm_native, call_elm_native_current_stack, elm_native_recovery_address, resume_elm_panic,
};
// efi_stub 通过 #[unsafe(no_mangle)] 暴露入口符号，其内部项无需 re-export。
pub use heap_vm::*;
pub use loader::*;
pub use paging::*;
pub use random_source::register as register_entropy_source;
pub use sched_ctx::register as register_sched_ctx;
pub use smp::{SecondaryCpuReport, start_secondary_cpus};
pub use specific::*;
pub use task::*;
pub use trap::*;
