#![no_std]
#![no_main]

extern crate alloc;

mod common;
mod mmio;
mod pci;

use elm::{ElmModule, HookError, HookResult, LifecycleContext};
use general::dev::pnp::{DriverHandle, PnpError, unregister_driver};

use allocator as _;

pub(crate) const VIRTIO_NET_DEVICE_NAME: &str = "net0";

struct VirtioNetElm {
    mmio: Option<DriverHandle>,
    pci: Option<DriverHandle>,
}

fn map_pnp_error(_error: PnpError) -> HookError {
    HookError::new(-19)
}

fn unregister_tracked_driver(handle: DriverHandle) -> HookResult {
    match unregister_driver(handle) {
        Ok(()) | Err(PnpError::NoDriver) => Ok(()),
        Err(_) => Err(HookError::new(-16)),
    }
}

#[elm::module]
impl ElmModule for VirtioNetElm {
    fn create(_context: &LifecycleContext) -> Result<Self, HookError> {
        Ok(Self {
            mmio: None,
            pci: None,
        })
    }

    fn initialize(&mut self, _context: &LifecycleContext) -> HookResult {
        if !virtio::framework_ready() {
            return Err(HookError::new(-19));
        }
        let mmio = mmio::register_driver().map_err(map_pnp_error)?;
        match pci::register_driver() {
            Ok(pci) => {
                self.mmio = Some(mmio);
                self.pci = Some(pci);
                Ok(())
            }
            Err(error) => {
                let _ = unregister_driver(mmio);
                Err(map_pnp_error(error))
            }
        }
    }

    fn quiesce(&mut self, _context: &LifecycleContext) -> HookResult {
        common::quiesce_active().map_err(|_| HookError::new(-16))
    }

    fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {
        common::detach_active().map_err(|_| HookError::new(-16))?;
        if let Some(handle) = self.pci {
            unregister_tracked_driver(handle)?;
            self.pci = None;
        }
        if let Some(handle) = self.mmio {
            unregister_tracked_driver(handle)?;
            self.mmio = None;
        }
        common::destroy_active();
        Ok(())
    }
}

#[cfg(all(not(feature = "elm-integrated"), target_os = "none"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    elm::runtime::abort_panic()
}
