#![no_std]
#![no_main]

extern crate alloc;

mod driver;
mod protocol;
mod registers;

use alloc::string::String;

use elm::{ElmModule, HookError, HookResult, LifecycleContext};
use general::dev::function::{FunctionProjectionNameAllocError, FunctionProjectionNameAllocator};
use general::dev::pnp::{DriverHandle, PnpError, unregister_driver};

use allocator as _;

pub(crate) use general::dev;

static AHCI_PROJECTION_NAMES: FunctionProjectionNameAllocator =
    FunctionProjectionNameAllocator::new("ahci");

fn alloc_ahci_dev_name(stable_key: &str) -> Result<String, FunctionProjectionNameAllocError> {
    AHCI_PROJECTION_NAMES
        .try_alloc_stable(stable_key)
        .map(|name| name.into_string())
}

struct AhciElm {
    driver: Option<DriverHandle>,
}

#[elm::module]
impl ElmModule for AhciElm {
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
