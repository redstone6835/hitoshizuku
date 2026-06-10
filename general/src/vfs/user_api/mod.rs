//! 用户态 ABI 投影层。
//!
//! 这里集中放置面向用户接口的兼容视图：设备号、socket ioctl、共享内存
//! 标准挂载点等。它们可以读取 typed VFS/dev 信息并生成用户可见结构，但不能
//! 反向污染底层设备身份、PnP 匹配或驱动资源所有权。

pub mod block_device;
pub mod device_numbers;
pub mod ioctl;
pub mod net_socket;
pub mod shm;
pub mod standard_devices;
pub mod tty;
