#![no_std]

pub use virtio::*;

#[cfg(not(feature = "elm-integrated"))]
#[elm::import(
    name = "virtio.framework.revision",
    contract = "driver.virtio.framework@1",
    version = 1,
    mode = "direct-pinned"
)]
static FRAMEWORK_REVISION: elm::DirectImport<fn() -> u32> = elm::DirectImport::new();

/// 验证当前 VirtIO consumer ELM 已绑定到兼容的 framework provider。
pub fn framework_ready() -> bool {
    #[cfg(feature = "elm-integrated")]
    {
        true
    }
    #[cfg(not(feature = "elm-integrated"))]
    {
        // Safety: ELM 装载器按精确 Rust ABI 摘要绑定不可变函数指针槽。
        unsafe { FRAMEWORK_REVISION.get() }.is_some_and(|revision| revision() == 1)
    }
}
