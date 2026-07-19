#![no_std]
#![no_main]

extern crate alloc;

use core::sync::atomic::{AtomicBool, Ordering};

use elm::{ElmModule, HookError, HookResult, LifecycleContext};
#[cfg(not(feature = "elm-integrated"))]
use net::stack::PinnedNetStackEndpoint;
use net::stack::{
    NET_STACK_CALL_STATUS_INVALID, NET_STACK_CALL_STATUS_OK, NET_STACK_OP_PROBE,
    NET_STACK_OP_QUIESCE, NetStackHandle, NetStackRegisterErrorKind, NetStackRegistration,
    NetStackRemoveError,
};

use allocator as _;

static QUIESCED: AtomicBool = AtomicBool::new(false);

struct NetStackElm {
    handle: Option<NetStackHandle>,
}

fn map_register_error(error: NetStackRegisterErrorKind) -> HookError {
    let code = match error {
        NetStackRegisterErrorKind::RegistrarNotReady => -19,
        NetStackRegisterErrorKind::AlreadyActive => -16,
        NetStackRegisterErrorKind::InvalidRegistration => -22,
        NetStackRegisterErrorKind::ResourceExhausted => -12,
    };
    HookError::new(code)
}

fn map_remove_error(error: NetStackRemoveError) -> HookError {
    let code = match error {
        NetStackRemoveError::NoStack => -19,
        NetStackRemoveError::OwnerMismatch => -1,
        NetStackRemoveError::Busy => -16,
    };
    HookError::new(code)
}

#[elm::module]
impl ElmModule for NetStackElm {
    fn create(_context: &LifecycleContext) -> Result<Self, HookError> {
        Ok(Self { handle: None })
    }

    fn initialize(&mut self, _context: &LifecycleContext) -> HookResult {
        if self.handle.is_some() {
            return Err(HookError::new(-16));
        }
        QUIESCED.store(false, Ordering::Release);
        #[cfg(not(feature = "elm-integrated"))]
        let registration = {
            let endpoint =
                PinnedNetStackEndpoint::current("net.stack.call", "mygo.net.stack-call@1", 1)
                    .ok_or(HookError::new(-22))?;
            NetStackRegistration::pinned(endpoint)
        };
        #[cfg(feature = "elm-integrated")]
        let registration =
            NetStackRegistration::integrated(net_stack_call).ok_or(HookError::new(-22))?;
        let handle = net::stack::register_stack(registration)
            .map_err(|error| map_register_error(error.kind))?;
        self.handle = Some(handle);
        Ok(())
    }

    fn quiesce(&mut self, _context: &LifecycleContext) -> HookResult {
        QUIESCED.store(true, Ordering::Release);
        Ok(())
    }

    fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {
        let Some(handle) = self.handle else {
            return Ok(());
        };
        match net::stack::begin_remove(handle) {
            Ok(()) | Err(NetStackRemoveError::NoStack) => {
                self.handle = None;
                Ok(())
            }
            Err(error) => Err(map_remove_error(error)),
        }
    }
}

#[elm::export(
    name = "net.stack.call",
    contract = "mygo.net.stack-call@1",
    version = 1,
    mode = "direct-pinned",
    visibility = "private"
)]
fn net_stack_call(frame: &mut net::stack::NetStackCallV1) -> i32 {
    if !frame.valid(frame.opcode, frame.generation) || frame.generation == 0 {
        return NET_STACK_CALL_STATUS_INVALID;
    }
    match frame.opcode {
        NET_STACK_OP_PROBE => {
            let quiesced = QUIESCED.load(Ordering::Acquire);
            frame.ready = u8::from(!quiesced);
            frame.quiesced = u8::from(quiesced);
        }
        NET_STACK_OP_QUIESCE => {
            QUIESCED.store(true, Ordering::Release);
            frame.ready = 0;
            frame.quiesced = 1;
        }
        _ => return NET_STACK_CALL_STATUS_INVALID,
    }
    NET_STACK_CALL_STATUS_OK
}

#[cfg(not(feature = "elm-integrated"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    elm::runtime::abort_panic()
}
