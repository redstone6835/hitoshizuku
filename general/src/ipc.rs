//! 通用 IPC 基础设施。
//!
//! syscall 层负责 ABI 编解码，general 层只提供可复用的对象、权限、原子操作
//! 和生命周期语义。

pub mod mqueue;
pub mod msg;
pub mod sem;
pub mod sem_undo;
pub mod shm;

pub use shm::*;
