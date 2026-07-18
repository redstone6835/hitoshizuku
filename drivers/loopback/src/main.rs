#![no_std]
#![no_main]

extern crate alloc;

mod driver;

use elm::{ElmModule, HookError, HookResult, LifecycleContext};

use allocator as _;

struct LoopbackElm {
    handle: Option<driver::LoopbackHandle>,
}

fn map_net_error(error: driver::LoopbackError) -> HookError {
    let code = match error {
        driver::LoopbackError::Pool => -12,
        driver::LoopbackError::Register(net::device::NetDeviceRegisterErrorKind::RegistrarNotReady) => -19,
        driver::LoopbackError::Register(net::device::NetDeviceRegisterErrorKind::InvalidRegistration) => -22,
        driver::LoopbackError::Register(net::device::NetDeviceRegisterErrorKind::ResourceExhausted) => -12,
        driver::LoopbackError::Remove(net::device::NetDeviceRemoveError::NoDevice) => -19,
        driver::LoopbackError::Remove(net::device::NetDeviceRemoveError::Busy) => -16,
        driver::LoopbackError::Remove(net::device::NetDeviceRemoveError::AlreadyRemoving) => -114,
    };
    HookError::new(code)
}

#[elm::module]
impl ElmModule for LoopbackElm {
    fn create(_context: &LifecycleContext) -> Result<Self, HookError> {
        Ok(Self { handle: None })
    }

    fn initialize(&mut self, _context: &LifecycleContext) -> HookResult {
        if self.handle.is_some() {
            return Err(HookError::new(-16));
        }
        self.handle = Some(driver::register().map_err(map_net_error)?);
        Ok(())
    }

    fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {
        let Some(handle) = self.handle.as_ref() else {
            return Ok(());
        };
        match handle.unregister() {
            Ok(()) => {
                self.handle = None;
                Ok(())
            }
            Err(error) => Err(map_net_error(error)),
        }
    }
}

#[cfg(not(feature = "elm-integrated"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    elm::runtime::abort_panic()
}
