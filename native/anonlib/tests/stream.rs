use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use anonlib::stdout;

#[allow(dead_code)]
mod binding {
    include!(env!("MYGO_PROGRAM_RS"));
}

static TEST_LOCK: Mutex<()> = Mutex::new(());
static INITIAL_HANDLE: AtomicU64 = AtomicU64::new(0);
static LAST_REQUIREMENT: AtomicU32 = AtomicU32::new(0);
static CALL_STATUS: AtomicU32 = AtomicU32::new(0);
static CALL_VALUE0: AtomicU64 = AtomicU64::new(0);
static CALL_SLOT: AtomicU64 = AtomicU64::new(0);
static CALL_HANDLE: AtomicU64 = AtomicU64::new(0);
static CALL_POINTER: AtomicUsize = AtomicUsize::new(0);
static CALL_LENGTH: AtomicU64 = AtomicU64::new(0);

#[repr(C)]
struct NativeResult {
    status: u32,
    reserved: u32,
    value0: u64,
    value1: u64,
}

fn reset() {
    INITIAL_HANDLE.store(0, Ordering::Relaxed);
    LAST_REQUIREMENT.store(0, Ordering::Relaxed);
    CALL_STATUS.store(binding::MYGO_STATUS_ok, Ordering::Relaxed);
    CALL_VALUE0.store(0, Ordering::Relaxed);
    CALL_SLOT.store(u64::MAX, Ordering::Relaxed);
    CALL_HANDLE.store(0, Ordering::Relaxed);
    CALL_POINTER.store(0, Ordering::Relaxed);
    CALL_LENGTH.store(0, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
extern "C" fn mrt_initial_handle(requirement_id: u32) -> u64 {
    LAST_REQUIREMENT.store(requirement_id, Ordering::Relaxed);
    INITIAL_HANDLE.load(Ordering::Relaxed)
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
    assert_eq!((arg2, arg3, arg4), (0, 0, 0));
    CALL_SLOT.store(slot, Ordering::Relaxed);
    CALL_HANDLE.store(object_handle, Ordering::Relaxed);
    CALL_POINTER.store(arg0 as usize, Ordering::Relaxed);
    CALL_LENGTH.store(arg1, Ordering::Relaxed);
    NativeResult {
        status: CALL_STATUS.load(Ordering::Relaxed),
        reserved: 0,
        value0: CALL_VALUE0.load(Ordering::Relaxed),
        value1: 0,
    }
}

#[unsafe(no_mangle)]
extern "C" fn mrt_terminate(_status: u32) -> ! {
    panic!("测试不得终止进程")
}

#[unsafe(no_mangle)]
extern "C" fn mrt_abort() -> ! {
    panic!("测试不得进入 abort")
}

#[test]
fn missing_initial_stdout_is_not_fabricated() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset();

    assert!(stdout().is_none());
    assert_eq!(
        LAST_REQUIREMENT.load(Ordering::Relaxed),
        binding::MYGO_REQUIREMENT_stdout
    );
}

#[test]
fn stream_write_uses_bound_slot_and_initial_handle() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset();
    let raw_handle = UINT64_C_1_1;
    INITIAL_HANDLE.store(raw_handle, Ordering::Relaxed);
    CALL_VALUE0.store(5, Ordering::Relaxed);
    let message = b"hello";

    let written = stdout().unwrap().write(message).unwrap();

    assert_eq!(written, message.len());
    assert_eq!(
        CALL_SLOT.load(Ordering::Relaxed),
        binding::MYGO_SLOT_stream_write
    );
    assert_eq!(CALL_HANDLE.load(Ordering::Relaxed), raw_handle);
    assert_eq!(
        CALL_POINTER.load(Ordering::Relaxed),
        message.as_ptr() as usize
    );
    assert_eq!(CALL_LENGTH.load(Ordering::Relaxed), message.len() as u64);
}

#[test]
fn stream_write_preserves_native_error_status() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset();
    INITIAL_HANDLE.store(UINT64_C_1_1, Ordering::Relaxed);
    CALL_STATUS.store(
        binding::MYGO_STATUS_security_rights_denied,
        Ordering::Relaxed,
    );

    let error = stdout().unwrap().write(b"denied").unwrap_err();

    assert_eq!(error.raw(), binding::MYGO_STATUS_security_rights_denied);
}

const UINT64_C_1_1: u64 = 0x0000_0001_0000_0001;
