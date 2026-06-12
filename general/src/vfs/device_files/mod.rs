//! `/dev` 文件适配层。
//!
//! 本目录只放具体设备文件的用户态 ABI 适配，例如 loop 和 RTC。devtmpfs 核心
//! 负责 inode 投影与生命周期，这些适配器负责把 ioctl、用户结构体和文件操作
//! 翻译为底层 typed device/control 接口。

pub mod base;
pub mod cpu_dma_latency;
pub mod loop_device;
pub mod projection;
pub mod rtc;
pub mod spec;
