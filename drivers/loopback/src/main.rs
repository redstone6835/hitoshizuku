#![no_std]
#![no_main]

extern crate alloc;

mod driver;

use elm::{ElmModule, HookError, HookResult, LifecycleContext};

use allocator as _;

struct LoopbackElm {
    handle: Option<driver::LoopbackHandle>,
}

fn map_net_error(error: net::NetError) -> HookError {
    let code = match error {
        net::NetError::InterfaceNotFound => -19,
        net::NetError::InterfaceExists => -16,
        net::NetError::LinkDown => -100,
        net::NetError::ConnectionRefused => -111,
        net::NetError::TimedOut => -110,
        net::NetError::AddressInUse => -98,
        net::NetError::WouldBlock => -11,
        net::NetError::ConnectionReset => -104,
        net::NetError::Unreachable => -101,
        net::NetError::Closed => -9,
        net::NetError::BufferTooSmall => -75,
        net::NetError::InvalidArgument => -22,
        net::NetError::ResourceExhausted => -12,
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
