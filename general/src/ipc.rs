//! 通用 IPC 基础设施。
//!
//! 目前这里只放 SysV shared memory 管理器。syscall 层负责 ABI 编解码，
//! general 层只提供可复用的对象、权限和生命周期语义。

pub mod shm;

pub use shm::*;
