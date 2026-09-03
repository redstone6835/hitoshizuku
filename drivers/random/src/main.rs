#![no_std]
#![no_main]

extern crate alloc;

mod driver;

use elm::{ElmModule, HookError, HookResult, LifecycleContext};

use allocator as _;

pub(crate) use general::dev;

struct RandomElm {
    handle: Option<general::dev::random::RandomBackendHandle>,
}

#[elm::module]
impl ElmModule for RandomElm {
    fn create(_context: &LifecycleContext) -> Result<Self, HookError> {
        Ok(Self { handle: None })
    }

    fn initialize(&mut self, _context: &LifecycleContext) -> HookResult {
        self.handle = Some(driver::register_builtin_driver().map_err(|_| HookError::new(-19))?);
        Ok(())
    }

    fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {
        if let Some(handle) = self.handle.take() {
            general::dev::random::unregister_backend(handle).map_err(|_| HookError::new(-16))?;
        }
        Ok(())
    }
}

#[cfg(all(not(feature = "elm-integrated"), target_os = "none"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    elm::runtime::abort_panic()
}
