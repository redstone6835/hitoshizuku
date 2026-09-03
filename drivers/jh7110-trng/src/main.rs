#![no_std]
#![no_main]

extern crate alloc;

mod driver;
mod engine;
mod status;

use elm::{ElmModule, HookError, HookResult, LifecycleContext};
use general::dev::pnp::{DriverHandle, PnpError, unregister_driver};

use allocator as _;

pub(crate) use general::dev;

struct Jh7110TrngElm {
    driver: Option<DriverHandle>,
}

#[elm::module]
impl ElmModule for Jh7110TrngElm {
    fn create(_context: &LifecycleContext) -> Result<Self, HookError> {
        Ok(Self { driver: None })
    }

    fn initialize(&mut self, _context: &LifecycleContext) -> HookResult {
        if self.driver.is_some() {
            return Err(HookError::new(-16));
        }
        self.driver = Some(driver::register_builtin_driver().map_err(|_| HookError::new(-19))?);
        Ok(())
    }

    fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {
        let Some(handle) = self.driver.take() else {
            return Ok(());
        };
        match unregister_driver(handle) {
            Ok(()) | Err(PnpError::NoDriver) => Ok(()),
            Err(_) => {
                self.driver = Some(handle);
                Err(HookError::new(-16))
            }
        }
    }
}

#[cfg(all(not(feature = "elm-integrated"), target_os = "none"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    elm::runtime::abort_panic()
}
