#![no_std]

pub use virtio::*;

#[elm::export(
    name = "virtio.framework.revision",
    contract = "driver.virtio.framework@1",
    version = 1,
    mode = "direct-pinned",
    visibility = "dependency"
)]
pub fn framework_revision() -> u32 {
    1
}
