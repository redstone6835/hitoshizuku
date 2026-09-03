#![no_std]
#![no_main]

extern crate alloc;

mod driver;
mod eio_layout;
mod ls2k_icu_layout;

use elm::{ElmModule, HookError, HookResult, LifecycleContext};
use general::dev::pnp::{DriverHandle, PnpError, unregister_driver};

use allocator as _;

pub(crate) use general::dev;

struct LoongsonIrqElm {
    drivers: [Option<DriverHandle>; 5],
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
impl ElmModule for LoongsonIrqElm {
    fn create(_context: &LifecycleContext) -> Result<Self, HookError> {
        Ok(Self { drivers: [None; 5] })
    }

    fn initialize(&mut self, _context: &LifecycleContext) -> HookResult {
        if self.drivers.iter().any(Option::is_some) {
            return Err(HookError::new(-16));
        }
        self.drivers = driver::register_builtin_driver()
            .map_err(|_| HookError::new(-19))?
            .map(Some);
        Ok(())
    }

    fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {
        unregister_drivers(&mut self.drivers)
    }
}

#[cfg(all(not(feature = "elm-integrated"), target_os = "none"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    elm::runtime::abort_panic()
}
