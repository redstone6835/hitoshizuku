#![no_std]
#![no_main]

extern crate alloc;

use core::sync::atomic::{AtomicBool, Ordering};

use elm::{ElmModule, HookError, HookResult, LifecycleContext};
#[cfg(not(feature = "elm-integrated"))]
use net::stack::PinnedNetStackEndpoint;
use net::stack::{
    NET_STACK_CALL_STATUS_INVALID, NET_STACK_CALL_STATUS_OK, NET_STACK_ETHERNET_ACCEPTED,
    NET_STACK_ETHERNET_TRUNCATED, NET_STACK_ETHERNET_UNSUPPORTED,
    NET_STACK_ETHERNET_VLAN_UNSUPPORTED, NET_STACK_OP_PROBE, NET_STACK_OP_QUIESCE,
    NET_STACK_OP_WORKER_TURN, NetStackEthernetV1, NetStackHandle, NetStackRegisterErrorKind,
    NetStackRegistration, NetStackRemoveError,
};

use allocator as _;

static QUIESCED: AtomicBool = AtomicBool::new(false);

const fn empty_ethernet() -> NetStackEthernetV1 {
    NetStackEthernetV1 {
        destination: [0; 6],
        source: [0; 6],
        ethertype: 0,
        status: 0,
        reserved: [0; 5],
    }
}

fn ethernet_is_empty(sidecar: &NetStackEthernetV1) -> bool {
    sidecar.destination == [0; 6]
        && sidecar.source == [0; 6]
        && sidecar.ethertype == 0
        && sidecar.status == 0
        && sidecar.reserved == [0; 5]
}

fn worker_turn_header_valid(
    turn: &net::stack::NetStackWorkerTurnV1,
    generation: u64,
) -> bool {
    turn.abi_version == net::stack::NET_STACK_WORKER_TURN_ABI_VERSION
        && turn.struct_size as usize == core::mem::size_of::<net::stack::NetStackWorkerTurnV1>()
        && turn.generation == generation
        && !turn.input.is_null()
        && usize::from(turn.input_count) <= turn.ethernet.len()
        && turn.reserved0 == [0; 6]
        && turn.reserved1 == [0; 2]
}

struct NetStackElm {
    handle: Option<NetStackHandle>,
    boot: Option<net::boot::NetStackBootConfig>,
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
        Ok(Self {
            handle: None,
            boot: None,
        })
    }

    fn initialize(&mut self, _context: &LifecycleContext) -> HookResult {
        if self.handle.is_some() {
            return Err(HookError::new(-16));
        }
        let boot = net::stack::boot_config().ok_or(HookError::new(-19))?;
        if boot.active_cpu_count() == 0 || usize::from(boot.active_cpu_count()) > sched::NR_CPUS {
            return Err(HookError::new(-22));
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
        self.boot = Some(boot);
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
                self.boot = None;
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
        NET_STACK_OP_WORKER_TURN => {
            if QUIESCED.load(Ordering::Acquire) {
                return NET_STACK_CALL_STATUS_INVALID;
            }
            // Safety: host 已把 worker-turn 帧声明为本次 pinned call 的可访问范围；
            // 指针只在同步调用期间借用，ELM 不保存它。
            let turn = unsafe { &mut *frame.worker_turn };
            if !worker_turn_header_valid(turn, frame.generation) || turn.committed != 0 {
                return NET_STACK_CALL_STATUS_INVALID;
            }
            // Safety: host 同时声明了只读 PacketBatch 外壳范围，实际 fragment backing
            // 只会由受能力约束的 copy_packet_out 内核符号读取。
            let input = unsafe { &*turn.input };
            for index in 0..usize::from(turn.input_count) {
                if !ethernet_is_empty(&turn.ethernet[index]) {
                    return NET_STACK_CALL_STATUS_INVALID;
                }
                let mut header = [0u8; 14];
                let sidecar = if !input.copy_packet_out(index, 0, &mut header) {
                    NetStackEthernetV1 {
                        status: NET_STACK_ETHERNET_TRUNCATED,
                        ..empty_ethernet()
                    }
                } else {
                    let ethertype = u16::from_be_bytes([header[12], header[13]]);
                    let status = match ethertype {
                        0x0800 | 0x0806 | 0x86dd => NET_STACK_ETHERNET_ACCEPTED,
                        0x8100 | 0x88a8 => NET_STACK_ETHERNET_VLAN_UNSUPPORTED,
                        _ => NET_STACK_ETHERNET_UNSUPPORTED,
                    };
                    NetStackEthernetV1 {
                        destination: header[0..6].try_into().unwrap(),
                        source: header[6..12].try_into().unwrap(),
                        ethertype,
                        status,
                        reserved: [0; 5],
                    }
                };
                turn.ethernet[index] = sidecar;
                turn.committed = (index + 1) as u8;
            }
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
