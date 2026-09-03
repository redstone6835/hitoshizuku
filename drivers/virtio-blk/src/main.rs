#![no_std]
#![no_main]

extern crate alloc;

mod common;
mod mmio;
mod pci;

use alloc::string::String;

use elm::{ElmModule, HookError, HookResult, LifecycleContext};
use general::dev::function::{FunctionProjectionNameAllocError, FunctionProjectionNameAllocator};
use general::dev::pnp::{DriverHandle, PnpError, unregister_driver};

use allocator as _;

static VIRTIO_BLK_PROJECTION_NAMES: FunctionProjectionNameAllocator =
    FunctionProjectionNameAllocator::new("vd");

const VIRTIO_BLK_SECTOR_SIZE: u32 = 512;

fn unregister_tracked_driver(handle: DriverHandle) -> HookResult {
    match unregister_driver(handle) {
        Ok(()) | Err(PnpError::NoDriver) => Ok(()),
        Err(_) => Err(HookError::new(-16)),
    }
}

fn alloc_virtio_blk_dev_name(stable_key: &str) -> Result<String, FunctionProjectionNameAllocError> {
    VIRTIO_BLK_PROJECTION_NAMES
        .try_alloc_stable(stable_key)
        .map(|name| name.into_string())
}

struct VirtioBlock {
    mmio: Option<DriverHandle>,
    pci: Option<DriverHandle>,
}

#[elm::module]
impl ElmModule for VirtioBlock {
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
        let mmio = mmio::register_driver().map_err(|_| HookError::new(-19))?;
        match pci::register_driver() {
            Ok(pci) => {
                self.mmio = Some(mmio);
                self.pci = Some(pci);
                Ok(())
            }
            Err(_) => {
                let _ = unregister_driver(mmio);
                Err(HookError::new(-19))
            }
        }
    }

    fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {
        if let Some(handle) = self.pci {
            unregister_tracked_driver(handle)?;
            self.pci = None;
        }
        if let Some(handle) = self.mmio {
            unregister_tracked_driver(handle)?;
            self.mmio = None;
        }
        Ok(())
    }
}

#[cfg(all(not(feature = "elm-integrated"), target_os = "none"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    elm::runtime::abort_panic()
}
