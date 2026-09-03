#![no_std]
#![no_main]

extern crate alloc;

mod driver;

use elm::{ElmModule, HookError, HookResult, LifecycleContext};
use general::dev::pnp::{DriverHandle, PnpError, unregister_driver};

use allocator as _;

pub(crate) use general::dev;

struct DtbProvidersElm {
    driver: Option<DriverHandle>,
}

#[elm::module]
impl ElmModule for DtbProvidersElm {
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
        let Some(handle) = self.driver else {
            return Ok(());
        };
        match unregister_driver(handle) {
            Ok(()) | Err(PnpError::NoDriver) => {
                self.driver = None;
                Ok(())
            }
            Err(_) => Err(HookError::new(-16)),
        }
    }
}

#[cfg(all(not(feature = "elm-integrated"), target_os = "none"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    elm::runtime::abort_panic()
}
