use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anonlib::{Channel, ChannelTransfer, Image, Process};

#[allow(dead_code)]
mod binding {
    include!(env!("MYGO_PROGRAM_RS"));
}

static TEST_LOCK: Mutex<()> = Mutex::new(());
static NEXT_CALL: AtomicUsize = AtomicUsize::new(0);

const PROCESS: u64 = 0x0000_0001_0000_0001;
const SERVICE: u64 = 0x0000_0001_0000_0002;
const SOURCE_IMAGE: u64 = 0x0000_0001_0000_0003;
const RECEIVED_IMAGE: u64 = 0x0000_0001_0000_0004;

#[repr(C)]
struct NativeResult {
    status: u32,
    reserved: u32,
    value0: u64,
    value1: u64,
}

#[unsafe(no_mangle)]
extern "C" fn mrt_initial_handle(requirement_id: u32) -> u64 {
    match requirement_id {
        binding::MYGO_REQUIREMENT_self_process => PROCESS,
        binding::MYGO_REQUIREMENT_service_channel => SERVICE,
        _ => 0,
    }
}

#[unsafe(no_mangle)]
extern "C" fn mrt_call(
    slot: u64,
    object_handle: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
) -> NativeResult {
    let step = NEXT_CALL.fetch_add(1, Ordering::Relaxed);
    let mut result = NativeResult {
        status: binding::MYGO_STATUS_ok,
        reserved: 0,
        value0: 0,
        value1: 0,
    };
    match step {
        0 => {
            assert_eq!(slot, binding::MYGO_SLOT_image_create);
            assert_eq!(object_handle, PROCESS);
            assert_eq!((arg1, arg2, arg3, arg4), (4, 0, 0, 0));
            result.value0 = SOURCE_IMAGE;
        }
        1 => {
            assert_eq!(slot, binding::MYGO_SLOT_channel_send);
            assert_eq!(object_handle, SERVICE);
            assert_eq!((arg1, arg2, arg3, arg4), (0, 0, 0, 0));
            let message = unsafe { &*(arg0 as *const binding::MygoChannelMessage) };
            assert_eq!(message.data_size, 5);
            assert_eq!(message.handle_count, 1);
            let transfer =
                unsafe { &*(message.handles_ptr as *const binding::MygoChannelHandleTransfer) };
            assert_eq!(transfer.source_handle, SOURCE_IMAGE);
            assert_eq!(transfer.requested_rights, binding::MYGO_RIGHT_load);
            assert_eq!((transfer.flags, transfer.reserved), (0, 0));
        }
        2 => {
            assert_eq!(slot, binding::MYGO_SLOT_channel_receive);
            assert_eq!(object_handle, SERVICE);
            assert_eq!((arg1, arg2, arg3, arg4), (123, 0, 0, 0));
            let message = unsafe { &mut *(arg0 as *mut binding::MygoChannelMessage) };
            assert!(message.data_capacity >= 2);
            assert!(message.handle_capacity >= 1);
            unsafe {
                std::ptr::copy_nonoverlapping(b"ok".as_ptr(), message.data_ptr as *mut u8, 2);
                *(message.handles_ptr as *mut binding::MygoChannelHandleTransfer) =
                    binding::MygoChannelHandleTransfer {
                        source_handle: RECEIVED_IMAGE,
                        requested_rights: binding::MYGO_RIGHT_load,
                        flags: 0,
                        reserved: 0,
                    };
            }
            result.value0 = 2;
            result.value1 = 1;
        }
        3 => {
            assert_eq!(slot, binding::MYGO_SLOT_handle_close);
            assert_eq!(object_handle, RECEIVED_IMAGE);
        }
        4 => {
            assert_eq!(slot, binding::MYGO_SLOT_handle_close);
            assert_eq!(object_handle, SOURCE_IMAGE);
        }
        _ => panic!("unexpected Native call {step}"),
    }
    result
}

#[unsafe(no_mangle)]
extern "C" fn mrt_terminate(_status: u32) -> ! {
    panic!("test must not terminate")
}

#[unsafe(no_mangle)]
extern "C" fn mrt_abort() -> ! {
    panic!("test must not abort")
}

#[test]
fn service_channel_transfers_owned_image_handles_without_closing_the_bootstrap_endpoint() {
    let _guard = TEST_LOCK.lock().unwrap();
    NEXT_CALL.store(0, Ordering::Relaxed);

    let process = Process::current().unwrap();
    let image = Image::create(&process, b"soyo").unwrap();
    let service = Channel::service().unwrap();
    service
        .send_with_handles(b"image", &[ChannelTransfer::copy_image(&image)])
        .unwrap();

    let mut bytes = [0; 8];
    let mut handles = [None];
    let received = service.receive(&mut bytes, &mut handles, 123).unwrap();
    assert_eq!(&bytes[..received.bytes], b"ok");
    assert_eq!(received.handles, 1);
    let received_image = Image::from_received(handles[0].take().unwrap());
    drop(received_image);
    drop(service);
    drop(image);
    assert_eq!(NEXT_CALL.load(Ordering::Relaxed), 5);
}
