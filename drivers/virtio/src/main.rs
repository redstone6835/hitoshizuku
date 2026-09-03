#![no_std]
#![no_main]

use elm::{ElmModule, HookError, HookResult, LifecycleContext};

use allocator as _;
use general as _;

struct VirtioFramework;

#[elm::module]
impl ElmModule for VirtioFramework {
    fn create(_context: &LifecycleContext) -> Result<Self, HookError> {
        Ok(Self)
    }

    fn initialize(&mut self, _context: &LifecycleContext) -> HookResult {
        if virtio::framework_revision() != 1 {
            return Err(HookError::new(-1));
        }
        Ok(())
    }

    fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {
        Ok(())
    }
}

#[cfg(all(not(feature = "elm-integrated"), target_os = "none"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    elm::runtime::abort_panic()
}
