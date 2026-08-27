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
        #[cfg(not(feature = "elm-integrated"))]
        driver::LoopbackError::Context => -22,
        driver::LoopbackError::Register(
            net::device::NetDeviceRegisterErrorKind::RegistrarNotReady,
        ) => -19,
        driver::LoopbackError::Register(
            net::device::NetDeviceRegisterErrorKind::InvalidRegistration,
        ) => -22,
        driver::LoopbackError::Register(
            net::device::NetDeviceRegisterErrorKind::ResourceExhausted,
        ) => -12,
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
        driver::create_queue().map_err(map_net_error)?;
        match driver::register() {
            Ok(handle) => self.handle = Some(handle),
            Err(error) => {
                driver::destroy_queue();
                return Err(map_net_error(error));
            }
        }
        Ok(())
    }

    fn quiesce(&mut self, _context: &LifecycleContext) -> HookResult {
        driver::quiesce_queue();
        Ok(())
    }

    fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {
        let Some(handle) = self.handle.as_ref() else {
            return Ok(());
        };
        // 自有资源回调会把 host 移除推迟到 finalize，因此在此释放 queue 持有的
        // lease 时，pool 仍然有效。
        driver::destroy_queue();
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
