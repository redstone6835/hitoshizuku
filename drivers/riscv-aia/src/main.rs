#![no_std]
#![no_main]

extern crate alloc;

mod config;
mod driver;
mod vector;

use allocator as _;
use elm::{ElmModule, HookError, HookResult, LifecycleContext};
use general::dev::pnp::{DriverHandle, PnpError, unregister_driver};

pub(crate) use general::dev;

struct RiscvAiaElm {
    drivers: [Option<DriverHandle>; 2],
}

fn unregister_drivers<const N: usize>(drivers: &mut [Option<DriverHandle>; N]) -> HookResult {
    let mut failed = false;
    for slot in drivers.iter_mut().rev() {
        let Some(handle) = *slot else {
            continue;
        };
        match unregister_driver(handle) {
            Ok(()) | Err(PnpError::NoDriver) => *slot = None,
            Err(_) => failed = true,
        }
    }
    if failed {
        Err(HookError::new(-16))
    } else {
        Ok(())
    }
}

#[elm::module]
impl ElmModule for RiscvAiaElm {
    fn create(_context: &LifecycleContext) -> Result<Self, HookError> {
        Ok(Self { drivers: [None; 2] })
    }

    fn initialize(&mut self, _context: &LifecycleContext) -> HookResult {
        if self.drivers.iter().any(Option::is_some) {
            return Err(HookError::new(-16));
        }
        self.drivers = driver::register_builtin_drivers()
            .map_err(|_| HookError::new(-19))?
            .map(Some);
        Ok(())
    }

    fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {
        unregister_drivers(&mut self.drivers)
    }
}

#[cfg(not(feature = "elm-integrated"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    elm::runtime::abort_panic()
}
