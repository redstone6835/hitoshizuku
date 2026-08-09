use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use anonlib::HandleTransfer;
use anonlib::{EventPort, Image, Process, SpawnRequest};

#[allow(dead_code)]
mod binding {
    include!(env!("MYGO_PROGRAM_RS"));
}

static TEST_LOCK: Mutex<()> = Mutex::new(());
static NEXT_CALL: AtomicUsize = AtomicUsize::new(0);
static CURRENT_PROCESS: AtomicU64 = AtomicU64::new(0);

const PROCESS: u64 = 0x0000_0001_0000_0001;
const IMAGE: u64 = 0x0000_0001_0000_0002;
const EVENT_PORT: u64 = 0x0000_0001_0000_0003;
const CHILD: u64 = 0x0000_0001_0000_0004;
const STDOUT: u64 = 0x0000_0001_0000_0005;

#[repr(C)]
struct NativeResult {
    status: u32,
    reserved: u32,
    value0: u64,
    value1: u64,
}

fn reset() {
    NEXT_CALL.store(0, Ordering::Relaxed);
    CURRENT_PROCESS.store(PROCESS, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
extern "C" fn mrt_initial_handle(requirement_id: u32) -> u64 {
    match requirement_id {
        binding::MYGO_REQUIREMENT_self_process => CURRENT_PROCESS.load(Ordering::Relaxed),
        binding::MYGO_REQUIREMENT_stdout => STDOUT,
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
            assert_eq!(arg1, 4);
            assert_eq!((arg2, arg3, arg4), (0, 0, 0));
            result.value0 = IMAGE;
        }
        1 => {
            assert_eq!(slot, binding::MYGO_SLOT_event_create);
            assert_eq!(object_handle, PROCESS);
            assert_eq!((arg0, arg1, arg2, arg3, arg4), (4, 0, 0, 0, 0));
            result.value0 = EVENT_PORT;
        }
        2 => {
            assert_eq!(slot, binding::MYGO_SLOT_process_spawn);
            assert_eq!(object_handle, PROCESS);
            assert_eq!(
                arg1,
                core::mem::size_of::<binding::MygoSpawnRequest>() as u64
            );
            assert_eq!((arg2, arg3, arg4), (0, 0, 0));
            let request = unsafe { &*(arg0 as *const binding::MygoSpawnRequest) };
            assert_eq!(request.transfers.count, 1);
            let transfer =
                unsafe { &*(request.transfers.ptr as *const binding::MygoHandleTransfer) };
            assert_eq!(transfer.requirement_id, binding::MYGO_REQUIREMENT_stdout);
            assert_eq!(transfer.source_handle, STDOUT);
            assert_eq!(transfer.requested_rights, binding::MYGO_RIGHT_write);
            assert_eq!(transfer.flags, 0);
            result.value0 = CHILD;
        }
        3 => {
            assert_eq!(slot, binding::MYGO_SLOT_event_bind);
            assert_eq!(object_handle, EVENT_PORT);
            assert_eq!(arg0, CHILD);
            assert_eq!(arg1, binding::MYGO_EVENT_KIND_PROCESS_EXITED as u64);
            assert_eq!(arg2, 0x1234);
            assert_eq!((arg3, arg4), (0, 0));
            result.value0 = 7;
        }
        4 => {
            assert_eq!(slot, binding::MYGO_SLOT_event_wait);
            assert_eq!(object_handle, EVENT_PORT);
            assert_eq!(arg1, 1);
            assert_eq!((arg2, arg3, arg4), (0, 0, 0));
            unsafe {
                *(arg0 as *mut binding::MygoEventRecord) = binding::MygoEventRecord {
                    event_kind: binding::MYGO_EVENT_KIND_PROCESS_EXITED,
                    status: binding::MYGO_STATUS_ok,
                    source_handle: CHILD,
                    sequence: 1,
                    value0: 0x9abc_def0,
                    value1: 0x1234,
                };
            }
            result.value0 = 1;
        }
        5 => {
            assert_eq!(slot, binding::MYGO_SLOT_process_wait);
            assert_eq!(object_handle, CHILD);
            assert_eq!((arg1, arg2, arg3, arg4), (0, 0, 0, 0));
            unsafe {
                *(arg0 as *mut binding::MygoProcessResult) = binding::MygoProcessResult {
                    state: binding::MYGO_PROCESS_STATE_EXITED,
                    flags: 0,
                    exit_code: 0x9abc_def0,
                    fault_kind: 0,
                    detail0: 0,
                    detail1: 0,
                };
            }
        }
        6..=8 => {
            assert_eq!(slot, binding::MYGO_SLOT_handle_close);
            assert!(matches!(object_handle, IMAGE | EVENT_PORT | CHILD));
            assert_eq!((arg0, arg1, arg2, arg3, arg4), (0, 0, 0, 0, 0));
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
fn typed_process_and_event_objects_preserve_the_native_call_contract() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset();

    let parent = Process::current().expect("kernel must grant self process");
    assert!(binding::MYGO_HAS_image_create);
    assert!(binding::MYGO_HAS_process_spawn);
    assert!(binding::MYGO_HAS_process_wait);
    assert!(binding::MYGO_HAS_event_create);
    assert!(binding::MYGO_HAS_event_bind);
    assert!(binding::MYGO_HAS_event_wait);
    let image = Image::create(&parent, b"soyo").expect("image.create must return a handle");
    let event_port = EventPort::create(&parent, 4).expect("event.create must return a handle");
    let stdout = anonlib::stdout().expect("kernel must grant stdout");
    let transfer = HandleTransfer::stdout(&stdout);
    let transfers = [transfer];
    let request = SpawnRequest::new(&image).with_transfers(&transfers);
    let child = parent
        .spawn(&request)
        .expect("process.spawn must return a handle");

    assert_eq!(
        event_port
            .bind_process_exit(&child, 0x1234)
            .expect("event.bind must return a token"),
        7
    );
    let mut records = [Default::default(); 1];
    assert_eq!(event_port.wait(&mut records, 0).unwrap(), 1);
    assert_eq!(records[0].source_handle, CHILD);

    let result = child.wait(0).expect("process.wait must return its result");
    assert_eq!(result.exit_code, 0x9abc_def0);

    drop(child);
    drop(event_port);
    drop(image);
    assert_eq!(NEXT_CALL.load(Ordering::Relaxed), 9);
}
